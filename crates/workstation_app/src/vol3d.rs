//! 3D Volume Explorer: GPU direct-volume rendering of the reflectivity
//! volume plus a radar underlay anchored to the true z=0 ground plane.
//!
//! `render2d::volume_box_resample` builds a Cartesian reflectivity box around
//! the current map center on a background thread. The box is uploaded as an
//! R8 3D texture. A second R8 texture carries the best complete low-level PPI
//! over the same horizontal footprint. The fragment shader can render that
//! PPI or a column-maximum projection on the floor, then ray-march the volume
//! in front of it.
//!
//! Camera parameters are shared by the shader and the CPU annotation
//! projection. Keeping one dynamic z-span and one perspective scale fixes the
//! old apparent "floating floor" caused by the wireframe using viewport width
//! for its vertical projection while the shader used viewport height.

// This module is a near-verbatim port of BowEcho's volume explorer, kept close
// to its upstream so patches to that viewer keep applying to both repositories.
// It therefore exposes the full BowEcho surface - camera modes, floor modes,
// lighting presets - while the workstation's pane drives only part of it, and
// in a binary crate `pub` buys nothing from the `dead_code` lint. Suppressed
// here rather than by deleting the unused half, because deleting it would fork
// the file and end the shared upstream. Anything reported dead in the pane
// itself (`vol3d/pane.rs`) is NOT covered by this and really is dead.
#![allow(dead_code)]

pub mod advanced;
pub mod annotations;
pub mod box_frame;
pub mod camera;
mod controls;
mod lighting;
pub mod pane;

// Part of the ported BowEcho surface: the preset enum is how that viewer's
// UI names its lighting rigs. The workstation pane sets the settings struct
// directly, so the re-export is unused here but must survive for upstream
// patches to keep applying.
#[allow(unused_imports)]
pub use lighting::{Vol3dLightingPreset, Vol3dLightingSettings};

// How big the box is and where it sits. Re-exported so callers see one 3D
// surface rather than having to know which half of it moved out to keep this
// file under the architecture test's module line limit. Some of these names are
// used only by `box_frame` itself and by the pane, hence the allow, exactly as
// for the lighting re-export above.
#[allow(unused_imports)]
pub use box_frame::{
    AUTO_CENTER_CELL_KM, AUTO_CENTER_CORE_DBZ, AUTO_CENTER_ECHO_DBZ, AUTO_CENTER_MIN_CELLS,
    AUTO_CENTER_MIN_HEIGHT_KM, AUTO_CENTER_MIN_SWEEPS, AUTO_CENTER_N, AUTO_CENTER_RANGE_KM,
    AUTO_CENTER_WEIGHT_CEILING_DBZ, AUTO_CENTER_WEIGHT_PER_DB, AUTO_CENTER_WINDOW_HALF_KM,
    BOX_HALF_KM, BOX_SIZE_CHOICES_KM, ClusterRule, EchoComposite, Vol3dBoxCenter, Vol3dCenterKey,
    auto_box_center_km, box_center_key, box_voxel_m, echo_composite,
};

use eframe::egui;
use eframe::egui_wgpu::{self, wgpu};
use lighting::{
    LIGHT_VOLUME_N, LIGHT_VOLUME_NZ, LIGHT_WORKGROUP_SIZE, LIGHTING_ENCODE_MAX, MAX_SHADOW_STEPS,
    combine_cache_revision,
};
use radar_core::{MomentGrid, MomentType, RadarVolume};
use rayon::prelude::*;
use std::sync::{Arc, Mutex, mpsc};

pub const BOX_N: usize = 192;
pub const BOX_NZ: usize = 48;
/// A sharper floor than the volume lattice, while retaining a 512-byte row
/// pitch that is friendly to texture uploads.
pub const FLOOR_N: usize = 512;

pub const BOX_TOP_M: f32 = 18_000.0;

const MAX_SHADER_STEPS: usize = 256;
const UNIFORM_FLOATS: usize = 32;
const UNIFORM_BYTES: u64 = (UNIFORM_FLOATS * std::mem::size_of::<f32>()) as u64;
const LIGHTING_UNIFORM_FLOATS: usize = 32;
const LIGHTING_UNIFORM_BYTES: u64 = (LIGHTING_UNIFORM_FLOATS * std::mem::size_of::<f32>()) as u64;
const LIGHT_VOLUME_SHADER: &str = include_str!("vol3d/light_volume.wgsl");
pub const LIGHTING_MAX_SHADOW_STEPS: u32 = MAX_SHADOW_STEPS;

pub type Vol3dVolumeKey = (String, String, i64, usize, i32, i32, i32, i32);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FloorMode {
    Off,
    LowestTilt,
    ColumnMax,
}

impl FloorMode {
    pub const ALL: [Self; 3] = [Self::Off, Self::LowestTilt, Self::ColumnMax];

    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::LowestTilt => "Lowest tilt PPI",
            Self::ColumnMax => "Column max",
        }
    }

    pub fn shader_value(self) -> f32 {
        match self {
            Self::Off => 0.0,
            Self::LowestTilt => 1.0,
            Self::ColumnMax => 2.0,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Vol3dThresholdMode {
    Above,
    Below,
    Outside,
}

impl Vol3dThresholdMode {
    pub fn shader_value(self) -> f32 {
        match self {
            Self::Above => 0.0,
            Self::Below => 1.0,
            Self::Outside => 2.0,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Vol3dQuality {
    Draft,
    Balanced,
    High,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Vol3dCameraMode {
    Orbit,
    Fly,
}

impl Vol3dCameraMode {
    pub fn shader_value(self) -> f32 {
        match self {
            Self::Orbit => 0.0,
            Self::Fly => 1.0,
        }
    }
}

impl Vol3dQuality {
    pub const ALL: [Self; 3] = [Self::Draft, Self::Balanced, Self::High];

    pub fn label(self) -> &'static str {
        match self {
            Self::Draft => "Draft",
            Self::Balanced => "Balanced",
            Self::High => "High",
        }
    }

    pub fn steps(self) -> usize {
        match self {
            Self::Draft => 96,
            Self::Balanced => 160,
            Self::High => 240,
        }
    }
}

/// UI-thread state.
pub struct Vol3d {
    pub open: bool,
    pub yaw: f32,
    pub pitch: f32,
    pub dist: f32,
    pub camera_mode: Vol3dCameraMode,
    pub fly_x: f32,
    pub fly_y: f32,
    pub fly_z: f32,
    pub fly_speed: f32,
    pub threshold_dbz: f32,
    pub threshold_mode: Vol3dThresholdMode,
    /// Velocity mode only: reflectivity structure gate (dBZ). The volume body
    /// takes its shape from where reflectivity exceeds this, and the signed
    /// velocity is painted only inside that body.
    pub vel_ref_gate_dbz: f32,
    /// Velocity mode only: 0 shows all flow at the reflectivity opacity (broad
    /// flow), 1 fades near-zero flow and lets strong inbound/outbound couplets
    /// dominate.
    pub vel_couplet_emphasis: f32,
    /// True when the currently uploaded box is a velocity two-box pair
    /// (reflectivity structure + velocity color). Drives the shader mode flag.
    pub velocity_color_active: bool,
    pub opacity: f32,
    /// Optical-depth multiplier. Opacity controls each sample; density controls
    /// how quickly repeated samples accumulate into a solid echo body.
    pub density: f32,
    /// Blend from untouched palette color (0) to cached SH lighting (1).
    pub shading: f32,
    /// Positive-energy environment, key-light, and pseudo-shadow controls.
    pub lighting: Vol3dLightingSettings,
    pub quality: Vol3dQuality,
    pub floor_mode: FloorMode,
    pub floor_opacity: f32,
    pub floor_threshold_dbz: f32,
    pub floor_threshold_mode: Vol3dThresholdMode,
    /// Purely visual vertical exaggeration. 1x preserves physical proportions
    /// at every box size, because `zspan` normalises by `top_km / half_km`, so
    /// the multiplier means the same thing however wide the box is.
    ///
    /// The default is 1.5, down from 2.0, and the box size is why. Exaggeration
    /// scales the storm, not the box: at 2x a 30 km wide, 15 km deep supercell
    /// renders 1.00 x 1.00 display units - exactly as tall as it is wide. In the
    /// old 120 km box that storm was a lump occupying a quarter of the frame and
    /// the 2x lie was easy to discount. In the 60 km default box the storm IS
    /// the picture, and reading its depth off the height scale is the main thing
    /// the pane is for. 1.5x renders that same storm 1.00 wide x 0.75 tall: the
    /// vertical structure operators look for (overhang, BWER, the leaning
    /// updraft) is still stretched half again, but the storm still reads as
    /// broader than deep, which is the truth.
    pub vertical_exaggeration: f32,
    /// Perspective coefficient used by the shader and CPU projection.
    pub fov_scale: f32,
    /// Camera look-at height, km above the floor.
    pub focus_height_km: f32,
    /// Visible vertical slab, km above the radar.
    pub clip_bottom_km: f32,
    pub clip_top_km: f32,
    pub show_grid: bool,
    pub show_box: bool,
    pub show_labels: bool,
    pub resample_rx: Option<mpsc::Receiver<Option<VolumeBox>>>,
    /// (site id, product label, volume_time_ms, source volume pointer, top
    /// product elevation in tenths of a degree, box centre east in tenths of a
    /// km, box centre north in tenths of a km, box half km). Top-tilt and
    /// pointer inclusion make completing/replaced live volumes re-trigger the
    /// resample; the centre is in the key because the box has to rebuild when
    /// it moves, and must NOT rebuild when it has not moved.
    pub volume_key: Option<Vol3dVolumeKey>,
    /// Top product elevation of the last fully-built box. Live partial volumes
    /// do not replace a complete box with a one-sector fragment.
    pub last_top_deg: f32,
    /// Box half-width, km. Half of one of [`BOX_SIZE_CHOICES_KM`].
    pub box_half_km: f32,
    /// How [`Vol3d::resolve_box_center`] places the box each volume.
    pub box_center_mode: Vol3dBoxCenter,
    /// Identity of the volume the storm centre was last measured from, so the
    /// gate scan happens once per volume instead of once per frame.
    pub auto_center_key: Option<Vol3dCenterKey>,
    /// Part of the ported BowEcho surface: a map target picked on that viewer's
    /// radar canvas. The workstation places the box with [`Vol3dBoxCenter`]
    /// instead, in km east and north of the radar, so nothing reads this yet.
    pub box_target_lonlat: Option<(f32, f32)>,
    /// Product label used by the currently selected 3D volume.
    pub product_label: String,
    /// Resolved box centre relative to the radar, km east and north. The single
    /// source of truth: the resample, the floor raster, the resample key and the
    /// radar marker all read these, so they cannot disagree about where the box
    /// is.
    pub box_center_east_km: f32,
    pub box_center_north_km: f32,
    pub status: String,
    /// Last uploaded reflectivity LUT signature.
    pub lut_signature: u64,
    /// Uploads waiting for the GPU, drained in `prepare`.
    pub pending: Arc<Mutex<PendingUploads>>,
    /// Inline advanced control groups. These are not popup menus because egui
    /// combo boxes close immediately when nested inside a parent popup.
    pub show_volume_controls: bool,
    pub show_floor_controls: bool,
    pub show_lighting_controls: bool,
    pub show_analysis_controls: bool,
    /// Render mode, support presentation, crop box and the rest of the
    /// second-generation renderer's uniform-only state.
    pub advanced: advanced::AdvancedParams,
    /// The 256-entry RGBA8 palette exactly as it was last uploaded. The
    /// preintegration table integrates the transfer function over a segment, so
    /// it needs the palette itself, not the `ColorTable` it came from.
    pub lut_rgba: Vec<u8>,
    /// Cache key of the preintegration table currently on the GPU
    /// (`advanced::preintegration_signature`). Camera motion is deliberately
    /// not an input, so orbiting rebuilds nothing.
    pub preintegration_signature: u64,
}

#[derive(Default)]
pub struct PendingUploads {
    pub volume: Option<VolumeBox>,
    pub lut: Option<Vec<u8>>,
    /// Segment-preintegration table, `PREINTEGRATION_N` square RGBA8. Driven by
    /// the palette, the thresholds and the opacity, so it travels with the LUT
    /// and never with the box.
    pub preintegrated: Option<Vec<u8>>,
}

pub struct VolumeBox {
    pub data: Vec<u8>,
    pub n: usize,
    pub nz: usize,
    /// Optional co-located color box, same `n`/`nz` lattice as `data`. Present
    /// only in velocity mode: `data` then carries the REFLECTIVITY structure
    /// (opacity/threshold/shading) while `color_data` carries the signed
    /// velocity that drives the LUT color. `None` = single-box (reflectivity
    /// and every other moment): color comes from `data` itself, exactly as
    /// before.
    pub color_data: Option<Vec<u8>>,
    /// Raw normalized dBZ for the low-level floor. Rows run south-to-north,
    /// exactly like the y dimension of `volume_box_resample`.
    pub floor_data: Option<Vec<u8>>,
    pub floor_elevation_deg: Option<f32>,
    /// Beam-stack support plane plus the conservative min/max hierarchy for
    /// this box, built on the resample worker beside the box itself.
    ///
    /// It rides WITH the box because it is a function of the box and of nothing
    /// else: no camera, opacity or palette input can reach the builder, which is
    /// how "camera motion, opacity-only changes and palette changes must not
    /// rebuild the hierarchy" is enforced rather than merely intended.
    pub acceleration: Option<render2d::volumetric_support::VolumeAcceleration>,
}

type CameraBasis = ([f32; 3], [f32; 3], [f32; 3], [f32; 3]);

impl Default for Vol3d {
    fn default() -> Self {
        Self {
            open: false,
            yaw: 0.6,
            pitch: 0.45,
            dist: 2.4,
            camera_mode: Vol3dCameraMode::Orbit,
            fly_x: 0.0,
            fly_y: -2.4,
            fly_z: 1.0,
            fly_speed: 1.2,
            // The Storm-structure preset's numbers, as the DEFAULT. At the old
            // 35 dBZ threshold the reflectivity opacity ramp had almost no
            // range left to grade and the box opened as a uniform translucent
            // slab; at 12 dBZ the weak echo becomes the cloud body and the
            // cores read as solid masses inside it - measured and LOOKED AT on
            // KMKX 2026-08-18 20:31Z and KUDX 2026-08-19 04:37Z. The owner's
            // requirement, verbatim: "having my 3d look like a realistic
            // cloud is important to me."
            threshold_dbz: 12.0,
            threshold_mode: Vol3dThresholdMode::Above,
            vel_ref_gate_dbz: 15.0,
            vel_couplet_emphasis: 0.35,
            velocity_color_active: false,
            opacity: 0.28,
            density: 0.78,
            shading: 0.9,
            lighting: Vol3dLightingSettings::default(),
            quality: Vol3dQuality::Balanced,
            floor_mode: FloorMode::LowestTilt,
            floor_opacity: 0.82,
            floor_threshold_dbz: 0.0,
            floor_threshold_mode: Vol3dThresholdMode::Above,
            vertical_exaggeration: 1.5,
            fov_scale: 0.7,
            focus_height_km: 6.0,
            clip_bottom_km: 0.0,
            clip_top_km: BOX_TOP_M / 1000.0,
            show_grid: true,
            show_box: true,
            show_labels: true,
            resample_rx: None,
            volume_key: None,
            last_top_deg: 0.0,
            box_half_km: BOX_HALF_KM,
            box_center_mode: Vol3dBoxCenter::Storm,
            auto_center_key: None,
            box_target_lonlat: None,
            product_label: "REF".to_owned(),
            box_center_east_km: 0.0,
            box_center_north_km: 0.0,
            status: String::new(),
            lut_signature: 0,
            pending: Arc::new(Mutex::new(PendingUploads::default())),
            show_volume_controls: false,
            show_floor_controls: false,
            show_lighting_controls: false,
            show_analysis_controls: false,
            advanced: advanced::AdvancedParams::default(),
            lut_rgba: Vec::new(),
            preintegration_signature: 0,
        }
    }
}

impl Vol3d {
    pub fn top_km(&self) -> f32 {
        BOX_TOP_M / 1000.0
    }

    /// Display-space box height. One horizontal world unit equals
    /// `box_half_km`; therefore top_km / half_km is the physically correct
    /// height and the user multiplier is a stable exaggeration across sizes.
    ///
    /// The clamp is a guard against a degenerate box, not a policy: it has to
    /// sit outside everything the UI can ask for, or it silently rewrites the
    /// exaggeration the operator selected. The widest box the picker offers is
    /// 360 km ([`BOX_SIZE_CHOICES_KM`]) and the exaggeration slider bottoms out
    /// at 0.5x, which is 18 / 180 * 0.5 = 0.05 - under the 0.06 floor this
    /// carried before, so at that one corner the box was drawn 20% taller than
    /// asked for and `zspan() * half_km / top_km` no longer recovered the
    /// multiplier. [`tests::vertical_exaggeration_is_independent_of_box_size`]
    /// now sweeps the whole offered domain rather than a sample of it.
    pub fn zspan(&self) -> f32 {
        (self.top_km() / self.box_half_km.max(1.0) * self.vertical_exaggeration).clamp(0.02, 24.0)
    }

    pub fn focus_height_fraction(&self) -> f32 {
        (self.focus_height_km / self.top_km().max(0.1)).clamp(0.0, 1.0)
    }

    pub fn normalized_clip(&self) -> (f32, f32) {
        let top = self.top_km().max(0.1);
        let low = (self.clip_bottom_km / top).clamp(0.0, 0.98);
        let high = (self.clip_top_km / top).clamp(low + 0.01, 1.0);
        (low, high)
    }

    pub fn orbit_distance(&self) -> f32 {
        self.dist.max(self.zspan() * 0.45 + 1.25)
    }

    fn orbit_center(&self) -> [f32; 3] {
        [0.0, 0.0, self.zspan() * self.focus_height_fraction()]
    }

    pub fn orbit_eye(&self) -> [f32; 3] {
        let center = self.orbit_center();
        let (cy, sy) = (self.yaw.cos(), self.yaw.sin());
        let (cp, sp) = (self.pitch.cos(), self.pitch.sin());
        let dist = self.orbit_distance();
        [
            center[0] + dist * cy * cp,
            center[1] + dist * sy * cp,
            center[2] + dist * sp,
        ]
    }

    fn fly_forward(&self) -> [f32; 3] {
        let (cy, sy) = (self.yaw.cos(), self.yaw.sin());
        let (cp, sp) = (self.pitch.cos(), self.pitch.sin());
        [-cy * cp, -sy * cp, -sp]
    }

    fn camera_basis(&self) -> Option<CameraBasis> {
        let (eye, mut fwd) = match self.camera_mode {
            Vol3dCameraMode::Orbit => {
                let center = self.orbit_center();
                let eye = self.orbit_eye();
                (
                    eye,
                    [center[0] - eye[0], center[1] - eye[1], center[2] - eye[2]],
                )
            }
            Vol3dCameraMode::Fly => ([self.fly_x, self.fly_y, self.fly_z], self.fly_forward()),
        };
        let fwd_len = (fwd[0] * fwd[0] + fwd[1] * fwd[1] + fwd[2] * fwd[2]).sqrt();
        if !fwd_len.is_finite() || fwd_len <= 1.0e-6 {
            return None;
        }
        for component in &mut fwd {
            *component /= fwd_len;
        }
        let mut right = [fwd[1], -fwd[0], 0.0];
        let right_len = (right[0] * right[0] + right[1] * right[1]).sqrt();
        if !right_len.is_finite() || right_len <= 1.0e-6 {
            return None;
        }
        right[0] /= right_len;
        right[1] /= right_len;
        let up = [
            right[1] * fwd[2] - right[2] * fwd[1],
            right[2] * fwd[0] - right[0] * fwd[2],
            right[0] * fwd[1] - right[1] * fwd[0],
        ];
        Some((eye, fwd, right, up))
    }

    pub fn enter_orbit_mode(&mut self) {
        self.camera_mode = Vol3dCameraMode::Orbit;
        self.pitch = self.pitch.clamp(0.03, 1.50);
    }

    pub fn enter_fly_mode(&mut self) {
        if self.camera_mode != Vol3dCameraMode::Fly {
            let eye = self.orbit_eye();
            self.fly_x = eye[0];
            self.fly_y = eye[1];
            self.fly_z = eye[2];
        }
        self.camera_mode = Vol3dCameraMode::Fly;
        self.pitch = self.pitch.clamp(-1.45, 1.45);
    }

    pub fn reset_fly_eye_from_orbit(&mut self) {
        let eye = self.orbit_eye();
        self.fly_x = eye[0];
        self.fly_y = eye[1];
        self.fly_z = eye[2];
    }

    pub fn fly_dolly(&mut self, amount: f32) {
        let fwd = self.fly_forward();
        self.fly_x += fwd[0] * amount;
        self.fly_y += fwd[1] * amount;
        self.fly_z = (self.fly_z + fwd[2] * amount).clamp(-1.0, self.zspan() + 1.0);
    }

    pub fn apply_fly_movement(&mut self, strafe: f32, forward: f32, vertical: f32, dt: f32) {
        if strafe == 0.0 && forward == 0.0 && vertical == 0.0 {
            return;
        }
        let fwd = self.fly_forward();
        let mut right = [fwd[1], -fwd[0], 0.0];
        let right_len = (right[0] * right[0] + right[1] * right[1]).sqrt();
        if right_len > 1.0e-6 {
            right[0] /= right_len;
            right[1] /= right_len;
        }
        let speed = self.fly_speed.max(0.05) * dt.max(0.0);
        self.fly_x += (right[0] * strafe + fwd[0] * forward) * speed;
        self.fly_y += (right[1] * strafe + fwd[1] * forward) * speed;
        self.fly_z =
            (self.fly_z + (fwd[2] * forward + vertical) * speed).clamp(-1.0, self.zspan() + 1.0);
    }

    pub fn reset_camera(&mut self) {
        self.yaw = 0.6;
        self.pitch = 0.45;
        self.dist = 2.4;
        self.fov_scale = 0.7;
        self.focus_height_km = 6.0;
        self.enter_orbit_mode();
        self.reset_fly_eye_from_orbit();
    }

    pub fn top_view(&mut self) {
        self.yaw = 0.0;
        self.pitch = 1.50;
        self.dist = 2.45;
        self.focus_height_km = 0.0;
        self.enter_orbit_mode();
    }

    pub fn low_view(&mut self) {
        self.yaw = 0.6;
        self.pitch = 0.20;
        self.dist = 2.7;
        self.focus_height_km = 4.0;
        self.enter_orbit_mode();
    }

    pub fn apply_balanced_preset(&mut self) {
        self.threshold_dbz = 25.0;
        self.opacity = 0.48;
        self.density = 1.0;
        self.shading = 0.65;
        self.quality = Vol3dQuality::Balanced;
    }

    pub fn apply_structure_preset(&mut self) {
        self.threshold_dbz = 12.0;
        self.opacity = 0.28;
        self.density = 0.78;
        self.shading = 0.90;
        self.quality = Vol3dQuality::High;
    }

    pub fn apply_core_preset(&mut self) {
        self.threshold_dbz = 40.0;
        self.opacity = 0.72;
        self.density = 1.35;
        self.shading = 0.48;
        self.quality = Vol3dQuality::Balanced;
    }

    /// Velocity two-box preset: fill the whole precipitation body with its
    /// signed velocity color (no couplet emphasis, low reflectivity gate).
    /// Velocity structure is fine-grained, so all velocity presets ray-march at
    /// High quality to keep the volume smooth.
    pub fn apply_velocity_broad_preset(&mut self) {
        self.vel_ref_gate_dbz = 12.0;
        self.vel_couplet_emphasis = 0.0;
        self.opacity = 0.50;
        self.density = 1.00;
        self.shading = 0.55;
        self.quality = Vol3dQuality::High;
    }

    /// Velocity two-box preset: emphasize the couplet — fade weak flow, keep
    /// the color inside the storm cores.
    pub fn apply_velocity_couplet_preset(&mut self) {
        self.vel_ref_gate_dbz = 20.0;
        self.vel_couplet_emphasis = 0.60;
        self.opacity = 0.70;
        self.density = 1.10;
        self.shading = 0.60;
        self.quality = Vol3dQuality::High;
    }

    /// Velocity two-box preset: strongest reflectivity cores + strongest flow
    /// only, for picking out mesocyclone/shear signatures.
    pub fn apply_velocity_shear_preset(&mut self) {
        self.vel_ref_gate_dbz = 25.0;
        self.vel_couplet_emphasis = 0.85;
        self.opacity = 0.85;
        self.density = 1.30;
        self.shading = 0.55;
        self.quality = Vol3dQuality::High;
    }

    /// Exact CPU companion to the WGSL camera projection. Returning `None`
    /// avoids drawing annotation lines through the camera or behind it.
    pub fn project_point(&self, rect: egui::Rect, point: [f32; 3]) -> Option<egui::Pos2> {
        let (eye, fwd, right, up) = self.camera_basis()?;
        let delta = [point[0] - eye[0], point[1] - eye[1], point[2] - eye[2]];
        let depth = delta[0] * fwd[0] + delta[1] * fwd[1] + delta[2] * fwd[2];
        if !depth.is_finite() || depth <= 1.0e-4 {
            return None;
        }
        let x = (delta[0] * right[0] + delta[1] * right[1] + delta[2] * right[2])
            / depth
            / self.fov_scale.max(0.05);
        let y = (delta[0] * up[0] + delta[1] * up[1] + delta[2] * up[2])
            / depth
            / self.fov_scale.max(0.05);
        if !x.is_finite() || !y.is_finite() {
            return None;
        }
        // The shader's horizontal aspect correction makes both axes use the
        // viewport HEIGHT in pixel space. The old width-based y scale was the
        // source of the apparent floating ground plane on wide panes.
        let scale = rect.height() * 0.5;
        Some(rect.center() + egui::vec2(x, -y) * scale)
    }
}

/// Normalize a slab of physical values into 0..255 texels against an explicit
/// range. Non-finite (no-data) maps to 0. Shared by the structure box and the
/// velocity color box so both lattices are normalized the same way.
pub fn normalize_values(values: &[f32], value_min: f32, value_max: f32) -> Vec<u8> {
    values
        .iter()
        .map(|value| normalize_value(*value, value_min, value_max))
        .collect()
}

pub fn normalize_box_with_range(
    values: &[f32],
    n: usize,
    nz: usize,
    value_min: f32,
    value_max: f32,
) -> VolumeBox {
    VolumeBox {
        data: normalize_values(values, value_min, value_max),
        n,
        nz,
        color_data: None,
        // Upload an empty floor with every new volume so a failed low-tilt
        // raster can never leave the prior scan painted underneath.
        floor_data: Some(vec![0; FLOOR_N.saturating_mul(FLOOR_N)]),
        floor_elevation_deg: None,
        acceleration: None,
    }
}

pub fn empty_box() -> VolumeBox {
    VolumeBox {
        data: vec![0; BOX_N.saturating_mul(BOX_N).saturating_mul(BOX_NZ)],
        n: BOX_N,
        nz: BOX_NZ,
        color_data: None,
        floor_data: Some(vec![0; FLOOR_N.saturating_mul(FLOOR_N)]),
        floor_elevation_deg: None,
        acceleration: None,
    }
}

fn normalize_value(value: f32, value_min: f32, value_max: f32) -> u8 {
    if value.is_finite() {
        let span = (value_max - value_min).abs().max(f32::EPSILON);
        (((value - value_min) / span).clamp(0.0, 1.0) * 255.0).round() as u8
    } else {
        0
    }
}

#[derive(Clone, Copy)]
struct FloorAzimuthSample {
    azimuth_deg: f32,
    row: usize,
    left_half_width_deg: f32,
    right_half_width_deg: f32,
}

fn angle_distance_deg(a: f32, b: f32) -> f32 {
    let delta = (a - b).abs().rem_euclid(360.0);
    delta.min(360.0 - delta)
}

fn signed_angle_delta_deg(from: f32, to: f32) -> f32 {
    (to - from + 540.0).rem_euclid(360.0) - 180.0
}

fn floor_azimuth_samples(
    volume: &RadarVolume,
    cut_index: usize,
    grid: &MomentGrid,
) -> Vec<FloorAzimuthSample> {
    let Some(cut) = volume.cuts.get(cut_index) else {
        return Vec::new();
    };
    let mut base = grid
        .radial_indices
        .iter()
        .enumerate()
        .filter_map(|(row, radial_index)| {
            let azimuth_deg = cut
                .radials
                .get(*radial_index)?
                .azimuth_deg
                .rem_euclid(360.0);
            azimuth_deg.is_finite().then_some((azimuth_deg, row))
        })
        .collect::<Vec<_>>();
    base.sort_by(|left, right| left.0.total_cmp(&right.0));
    if base.is_empty() {
        return Vec::new();
    }

    (0..base.len())
        .map(|index| {
            let (azimuth_deg, row) = base[index];
            let previous = base[(index + base.len() - 1) % base.len()].0;
            let next = base[(index + 1) % base.len()].0;
            let left_gap = (azimuth_deg - previous).rem_euclid(360.0);
            let right_gap = (next - azimuth_deg).rem_euclid(360.0);
            FloorAzimuthSample {
                azimuth_deg,
                row,
                // A 3 degree ceiling keeps sector-scan edges from smearing
                // across their unobserved azimuth gap.
                left_half_width_deg: (0.5 * left_gap).clamp(0.05, 3.0),
                right_half_width_deg: (0.5 * right_gap).clamp(0.05, 3.0),
            }
        })
        .collect()
}

fn nearest_floor_row(samples: &[FloorAzimuthSample], azimuth_deg: f32) -> Option<usize> {
    if samples.is_empty() {
        return None;
    }
    let index = samples.partition_point(|sample| sample.azimuth_deg < azimuth_deg);
    let low = if index == 0 {
        samples.len() - 1
    } else {
        index - 1
    };
    let high = if index >= samples.len() { 0 } else { index };
    let candidate = if angle_distance_deg(samples[low].azimuth_deg, azimuth_deg)
        <= angle_distance_deg(samples[high].azimuth_deg, azimuth_deg)
    {
        samples[low]
    } else {
        samples[high]
    };
    let signed = signed_angle_delta_deg(candidate.azimuth_deg, azimuth_deg);
    let allowed = if signed < 0.0 {
        candidate.left_half_width_deg
    } else {
        candidate.right_half_width_deg
    };
    (signed.abs() <= allowed + 0.02).then_some(candidate.row)
}

pub fn lowest_moment_floor(
    volume: &RadarVolume,
    moment: &MomentType,
    center_east_km: f32,
    center_north_km: f32,
    half_km: f32,
    value_min: f32,
    value_max: f32,
) -> Option<(Vec<u8>, f32)> {
    if !half_km.is_finite() || half_km <= 0.0 {
        return None;
    }
    let candidates = volume
        .cuts
        .iter()
        .enumerate()
        .filter_map(|(index, cut)| {
            let grid = cut.moments.get(moment)?;
            (cut.elevation_deg.is_finite() && grid.radial_count() > 0).then_some((
                index,
                cut.elevation_deg,
                grid.radial_count(),
                grid,
            ))
        })
        .collect::<Vec<_>>();
    let max_rows = candidates.iter().map(|candidate| candidate.2).max()?;
    let minimum_usable_rows = (max_rows / 2).max(8).min(max_rows);
    let (cut_index, elevation_deg, _, grid) = candidates
        .into_iter()
        .filter(|candidate| candidate.2 >= minimum_usable_rows)
        .min_by(|left, right| {
            left.1
                .total_cmp(&right.1)
                .then_with(|| right.2.cmp(&left.2))
        })?;
    let azimuth_samples = floor_azimuth_samples(volume, cut_index, grid);
    if azimuth_samples.is_empty() || grid.gate_range.gate_count == 0 {
        return None;
    }
    let first_gate_m = grid.gate_range.first_gate_m as f32;
    let gate_spacing_m = grid.gate_range.gate_spacing_m.max(1) as f32;
    let gate_count = grid.gate_range.gate_count;
    let span_km = half_km * 2.0;
    let denominator = (FLOOR_N - 1).max(1) as f32;
    let mut data = vec![0u8; FLOOR_N * FLOOR_N];
    data.par_chunks_mut(FLOOR_N)
        .enumerate()
        .for_each(|(row_index, output_row)| {
            // row 0 = south, matching the volume texture's +Y convention.
            let north_km = center_north_km - half_km + span_km * row_index as f32 / denominator;
            for (column_index, output) in output_row.iter_mut().enumerate() {
                let east_km =
                    center_east_km - half_km + span_km * column_index as f32 / denominator;
                let range_m = east_km.hypot(north_km) * 1000.0;
                let gate_float = (range_m - first_gate_m) / gate_spacing_m;
                if !gate_float.is_finite() {
                    continue;
                }
                let gate = gate_float.round() as isize;
                if gate < 0 || gate as usize >= gate_count {
                    continue;
                }
                let azimuth_deg = east_km.atan2(north_km).to_degrees().rem_euclid(360.0);
                let Some(row) = nearest_floor_row(&azimuth_samples, azimuth_deg) else {
                    continue;
                };
                let Some(value) = grid
                    .scaled_value(row, gate as usize)
                    .filter(|value| value.is_finite())
                else {
                    continue;
                };
                *output = normalize_value(value, value_min, value_max);
            }
        });
    Some((data, elevation_deg))
}

const SHADER: &str = r#"
struct Uniforms {
    yaw: f32,
    pitch: f32,
    dist: f32,
    threshold: f32,

    opacity: f32,
    aspect: f32,
    floor_opacity: f32,
    floor_mode: f32,

    zspan: f32,
    fov_scale: f32,
    steps: f32,
    density: f32,

    shading: f32,
    clip_low: f32,
    clip_high: f32,
    floor_threshold: f32,

    focus_height: f32,
    camera_mode: f32,
    fly_x: f32,
    fly_y: f32,

    fly_z: f32,
    threshold_high: f32,
    threshold_mode: f32,
    floor_threshold_high: f32,

    floor_threshold_mode: f32,
    // Velocity two-box mode: t_volume carries the REFLECTIVITY structure and
    // t_color carries the signed velocity. velocity_mode > 0.5 switches on the
    // reflectivity-driven opacity + velocity-driven color path.
    velocity_mode: f32,
    // Normalized reflectivity structure gate (0..1). Below this, transparent.
    ref_gate: f32,
    // 0 = show all flow at the reflectivity opacity, 1 = fade weak flow so
    // strong inbound/outbound couplets dominate.
    couplet_emphasis: f32,

    // Fragment-only controls for the precomputed lighting texture.
    rim_strength: f32,
    light_enabled: f32,
    light_decode_scale: f32,
    _lighting_pad: f32,
};

@group(0) @binding(0) var<uniform> u: Uniforms;
@group(0) @binding(1) var t_volume: texture_3d<f32>;
@group(0) @binding(2) var s_volume: sampler;
@group(0) @binding(3) var t_lut: texture_2d<f32>;
@group(0) @binding(4) var s_lut: sampler;
@group(0) @binding(5) var t_floor: texture_2d<f32>;
@group(0) @binding(6) var s_floor: sampler;
@group(0) @binding(7) var t_color: texture_3d<f32>;
@group(0) @binding(8) var t_light: texture_3d<f32>;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> VsOut {
    var out: VsOut;
    let p = vec2<f32>(f32((vi << 1u) & 2u), f32(vi & 2u));
    out.uv = p;
    out.pos = vec4<f32>(p * 2.0 - 1.0, 0.0, 1.0);
    return out;
}

const MAX_STEPS: i32 = 256;
const FLOOR_COLUMN_STEPS: i32 = 48;
fn box_intersect(
    ro: vec3<f32>,
    rd: vec3<f32>,
    bmin: vec3<f32>,
    bmax: vec3<f32>
) -> vec2<f32> {
    let inv = 1.0 / rd;
    let t0 = (bmin - ro) * inv;
    let t1 = (bmax - ro) * inv;
    let tmin = min(t0, t1);
    let tmax = max(t0, t1);
    return vec2<f32>(
        max(max(tmin.x, tmin.y), tmin.z),
        min(min(tmax.x, tmax.y), tmax.z)
    );
}

fn shaded_rgb(uvw: vec3<f32>, ray_dir: vec3<f32>, base: vec3<f32>) -> vec3<f32> {
    if (u.shading <= 0.001 || u.light_enabled < 0.5) {
        return base;
    }
    let cached = textureSampleLevel(t_light, s_volume, uvw, 0.0);
    let decoded_normal = cached.rgb * 2.0 - vec3<f32>(1.0);
    var rim = 0.0;
    if (dot(decoded_normal, decoded_normal) > 0.01) {
        let normal = normalize(decoded_normal);
        let rim_facing = 1.0 - abs(dot(normal, -ray_dir));
        rim = max(u.rim_strength, 0.0) * rim_facing * rim_facing;
    }
    let lighting = clamp(cached.a * u.light_decode_scale + rim, 0.20, 2.0);
    // Scalar illumination preserves the exact reflectivity/velocity palette hue.
    return base * mix(1.0, lighting, clamp(u.shading, 0.0, 1.0));
}

fn column_max(uv: vec2<f32>) -> f32 {
    var maximum = 0.0;
    let low = clamp(u.clip_low, 0.0, 0.99);
    let high = clamp(u.clip_high, low + 0.01, 1.0);
    for (var i = 0; i < FLOOR_COLUMN_STEPS; i = i + 1) {
        let fraction = (f32(i) + 0.5) / f32(FLOOR_COLUMN_STEPS);
        let z = mix(low, high, fraction);
        maximum = max(
            maximum,
            textureSampleLevel(t_volume, s_volume, vec3<f32>(uv, z), 0.0).r
        );
    }
    return maximum;
}

fn threshold_strength(value: f32, low: f32, high: f32, mode: f32, width: f32) -> f32 {
    if (mode > 1.5) {
        if (value <= low) {
            return smoothstep(0.0, width, low - value);
        }
        if (value >= high) {
            return smoothstep(0.0, width, value - high);
        }
        return -1.0;
    }
    if (mode > 0.5) {
        if (value >= low) {
            return -1.0;
        }
        return smoothstep(0.0, width, low - value);
    }
    if (value <= low) {
        return -1.0;
    }
    return smoothstep(low, low + width, value);
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let cy = cos(u.yaw);
    let sy = sin(u.yaw);
    let cp = cos(u.pitch);
    let sp = sin(u.pitch);
    let center = vec3<f32>(0.0, 0.0, u.zspan * clamp(u.focus_height, 0.0, 1.0));
    var eye = center + u.dist * vec3<f32>(cy * cp, sy * cp, sp);
    var fwd = normalize(center - eye);
    if (u.camera_mode > 0.5) {
        eye = vec3<f32>(u.fly_x, u.fly_y, u.fly_z);
        fwd = normalize(vec3<f32>(-cy * cp, -sy * cp, -sp));
    }
    let right = normalize(cross(fwd, vec3<f32>(0.0, 0.0, 1.0)));
    let up = cross(right, fwd);
    let ndc = (in.uv * 2.0 - 1.0) * vec2<f32>(u.aspect, 1.0);
    let rd = normalize(fwd + u.fov_scale * (ndc.x * right + ndc.y * up));

    let clip_low_z = clamp(u.clip_low, 0.0, 0.99) * u.zspan;
    let clip_high_z = clamp(u.clip_high, u.clip_low + 0.01, 1.0) * u.zspan;
    let hit = box_intersect(
        eye,
        rd,
        vec3<f32>(-1.0, -1.0, clip_low_z),
        vec3<f32>(1.0, 1.0, clip_high_z)
    );

    var color = vec3<f32>(0.0);
    var accumulated = 0.0;
    if (hit.y > max(hit.x, 0.0)) {
        let t0 = max(hit.x, 0.0);
        let step_count = i32(clamp(u.steps, 32.0, 256.0));
        let dt = (hit.y - t0) / f32(step_count);
        for (var i = 0; i < MAX_STEPS; i = i + 1) {
            if (i >= step_count) {
                break;
            }
            let point = eye + rd * (t0 + (f32(i) + 0.5) * dt);
            let uvw = vec3<f32>(
                (point.x + 1.0) * 0.5,
                (point.y + 1.0) * 0.5,
                point.z / u.zspan
            );
            let structure = textureSampleLevel(t_volume, s_volume, uvw, 0.0).r;
            var transfer = 0.0;
            var lut_coord = structure;
            var emphasis = 1.0;
            if (u.velocity_mode > 0.5) {
                // Structure and opacity from the co-located reflectivity box;
                // color from the signed velocity box. The volume body takes its
                // shape from where precipitation actually is, and each voxel is
                // painted with its velocity.
                transfer = smoothstep(u.ref_gate, u.ref_gate + 0.08, structure);
                let vel = textureSampleLevel(t_color, s_volume, uvw, 0.0).r;
                let vmag = clamp(abs(vel - 0.5) * 2.0, 0.0, 1.0);
                emphasis = mix(1.0, vmag, clamp(u.couplet_emphasis, 0.0, 1.0));
                lut_coord = vel;
            } else {
                transfer = threshold_strength(
                    structure,
                    u.threshold,
                    u.threshold_high,
                    u.threshold_mode,
                    0.08
                );
                lut_coord = structure;
            }
            if (transfer <= 0.0) {
                continue;
            }
            let palette = textureSampleLevel(t_lut, s_lut, vec2<f32>(lut_coord, 0.5), 0.0);
            var alpha = palette.a * u.opacity * transfer * emphasis;
            alpha = 1.0 - pow(
                max(1.0 - alpha, 0.0001),
                dt * 28.0 * max(u.density, 0.05)
            );
            let sample_rgb = shaded_rgb(uvw, rd, palette.rgb);
            color = color + (1.0 - accumulated) * alpha * sample_rgb;
            accumulated = accumulated + (1.0 - accumulated) * alpha;
            if (accumulated > 0.985) {
                break;
            }
        }
    }

    // Ground underlay at the exact z=0 box floor. With the camera constrained
    // above the box this intersection lies behind all volume samples, so
    // ordinary front-to-back compositing gives correct occlusion.
    if (u.floor_mode > 0.5 && abs(rd.z) > 0.00001) {
        let floor_t = -eye.z / rd.z;
        if (floor_t > 0.0) {
            let point = eye + rd * floor_t;
            if (abs(point.x) <= 1.0 && abs(point.y) <= 1.0) {
                let floor_uv = vec2<f32>(
                    (point.x + 1.0) * 0.5,
                    (point.y + 1.0) * 0.5
                );
                var value = textureSampleLevel(t_floor, s_floor, floor_uv, 0.0).r;
                // Column-max projects the reflectivity structure column onto the
                // floor; in velocity mode that column lives in t_volume and
                // would be miscolored by the velocity LUT, so fall back to the
                // lowest-tilt velocity PPI there.
                if (u.floor_mode > 1.5 && u.velocity_mode < 0.5) {
                    value = column_max(floor_uv);
                }
                let floor_transfer = threshold_strength(
                    value,
                    u.floor_threshold,
                    u.floor_threshold_high,
                    u.floor_threshold_mode,
                    0.04
                );
                if (floor_transfer > 0.0) {
                    let palette = textureSampleLevel(
                        t_lut,
                        s_lut,
                        vec2<f32>(value, 0.5),
                        0.0
                    );
                    let alpha = palette.a * u.floor_opacity * floor_transfer;
                    color = color + (1.0 - accumulated) * alpha * palette.rgb;
                    accumulated = accumulated + (1.0 - accumulated) * alpha;
                }
            }
        }
    }

    if (accumulated <= 0.00001) {
        return vec4<f32>(0.0);
    }
    // The render target uses straight-alpha blending. The ray marcher builds
    // a premultiplied front-to-back color, so unpremultiply once here rather
    // than letting the fixed-function blend multiply alpha a second time.
    return vec4<f32>(color / accumulated, accumulated);
}
"#;

/// GPU resources, created once at startup and stored in egui-wgpu's callback
/// resource typemap.
pub struct Vol3dResources {
    pipeline: wgpu::RenderPipeline,
    bind_group: wgpu::BindGroup,
    uniforms: wgpu::Buffer,
    volume_tex: wgpu::Texture,
    lut_tex: wgpu::Texture,
    floor_tex: wgpu::Texture,
    color_tex: wgpu::Texture,
    light_pipeline: wgpu::ComputePipeline,
    light_bind_group: wgpu::BindGroup,
    light_uniforms: wgpu::Buffer,
    advanced: advanced::AdvancedResources,
    volume_generation: u64,
    last_lighting_revision: Option<u64>,
}

/// One-time GPU setup (eframe custom-3D pattern: call from the app
/// constructor with `cc.wgpu_render_state`).
pub fn init_gpu(render_state: &egui_wgpu::RenderState) {
    let device = &render_state.device;
    // The base prelude keeps `Uniforms`, the nine base bindings, `vs_main`,
    // `box_intersect`, `column_max`, `threshold_strength` and `shaded_rgb` —
    // the cached positive log-SH light fetch — and only its `fs_main` is
    // replaced. The lighting contract therefore survives by construction.
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("vol3d"),
        source: wgpu::ShaderSource::Wgsl(advanced::compose_shader(SHADER).into()),
    });
    let light_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("vol3d-light-volume"),
        source: wgpu::ShaderSource::Wgsl(LIGHT_VOLUME_SHADER.into()),
    });
    let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("vol3d-uniforms"),
        size: UNIFORM_BYTES,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let light_uniforms = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("vol3d-light-uniforms"),
        size: LIGHTING_UNIFORM_BYTES,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let volume_tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("vol3d-volume"),
        size: wgpu::Extent3d {
            width: BOX_N as u32,
            height: BOX_N as u32,
            depth_or_array_layers: BOX_NZ as u32,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D3,
        format: wgpu::TextureFormat::R8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let lut_tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("vol3d-lut"),
        size: wgpu::Extent3d {
            width: 256,
            height: 1,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let floor_tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("vol3d-floor"),
        size: wgpu::Extent3d {
            width: FLOOR_N as u32,
            height: FLOOR_N as u32,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::R8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    // Velocity color box: same lattice as the volume, sampled only when the
    // shader is in velocity mode. Zeroed until a velocity two-box arrives.
    let color_tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("vol3d-color"),
        size: wgpu::Extent3d {
            width: BOX_N as u32,
            height: BOX_N as u32,
            depth_or_array_layers: BOX_NZ as u32,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D3,
        format: wgpu::TextureFormat::R8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let light_tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("vol3d-light-volume"),
        size: wgpu::Extent3d {
            width: LIGHT_VOLUME_N,
            height: LIGHT_VOLUME_N,
            depth_or_array_layers: LIGHT_VOLUME_NZ,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D3,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::STORAGE_BINDING,
        view_formats: &[],
    });
    // Support mask, both conservative min/max hierarchy levels and the segment
    // preintegration table. Formats are pinned in `advanced` and are Unorm on
    // purpose; see `AdvancedResources::new`.
    let advanced_resources = advanced::AdvancedResources::new(device);
    let advanced_views = advanced_resources.views();
    // Named because the bind-group entries now live in a `Vec` that outlives
    // the initialiser expression; an inline `create_view` would be a temporary
    // freed before `create_bind_group` reads it.
    let volume_view = volume_tex.create_view(&Default::default());
    let lut_view = lut_tex.create_view(&Default::default());
    let floor_view = floor_tex.create_view(&Default::default());
    let color_view = color_tex.create_view(&Default::default());
    let light_view = light_tex.create_view(&Default::default());
    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("vol3d-sampler"),
        address_mode_u: wgpu::AddressMode::ClampToEdge,
        address_mode_v: wgpu::AddressMode::ClampToEdge,
        address_mode_w: wgpu::AddressMode::ClampToEdge,
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        ..Default::default()
    });
    let mut layout_entries: Vec<wgpu::BindGroupLayoutEntry> = vec![
        wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        },
        wgpu::BindGroupLayoutEntry {
            binding: 1,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D3,
                multisampled: false,
            },
            count: None,
        },
        wgpu::BindGroupLayoutEntry {
            binding: 2,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
            count: None,
        },
        wgpu::BindGroupLayoutEntry {
            binding: 3,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        },
        wgpu::BindGroupLayoutEntry {
            binding: 4,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
            count: None,
        },
        wgpu::BindGroupLayoutEntry {
            binding: 5,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        },
        wgpu::BindGroupLayoutEntry {
            binding: 6,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
            count: None,
        },
        wgpu::BindGroupLayoutEntry {
            binding: 7,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D3,
                multisampled: false,
            },
            count: None,
        },
        wgpu::BindGroupLayoutEntry {
            binding: 8,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D3,
                multisampled: false,
            },
            count: None,
        },
    ];
    layout_entries.extend(advanced::AdvancedResources::layout_entries());
    let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("vol3d-bgl"),
        entries: &layout_entries,
    });
    let mut bind_entries: Vec<wgpu::BindGroupEntry<'_>> = vec![
        wgpu::BindGroupEntry {
            binding: 0,
            resource: uniforms.as_entire_binding(),
        },
        wgpu::BindGroupEntry {
            binding: 1,
            resource: wgpu::BindingResource::TextureView(&volume_view),
        },
        wgpu::BindGroupEntry {
            binding: 2,
            resource: wgpu::BindingResource::Sampler(&sampler),
        },
        wgpu::BindGroupEntry {
            binding: 3,
            resource: wgpu::BindingResource::TextureView(&lut_view),
        },
        wgpu::BindGroupEntry {
            binding: 4,
            resource: wgpu::BindingResource::Sampler(&sampler),
        },
        wgpu::BindGroupEntry {
            binding: 5,
            resource: wgpu::BindingResource::TextureView(&floor_view),
        },
        wgpu::BindGroupEntry {
            binding: 6,
            resource: wgpu::BindingResource::Sampler(&sampler),
        },
        wgpu::BindGroupEntry {
            binding: 7,
            resource: wgpu::BindingResource::TextureView(&color_view),
        },
        wgpu::BindGroupEntry {
            binding: 8,
            resource: wgpu::BindingResource::TextureView(&light_view),
        },
    ];
    bind_entries.extend(advanced_resources.bind_group_entries(&advanced_views));
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("vol3d-bg"),
        layout: &layout,
        entries: &bind_entries,
    });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("vol3d-pl"),
        bind_group_layouts: &[Some(&layout)],
        immediate_size: 0,
    });
    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("vol3d-pipeline"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs_main"),
            buffers: &[],
            compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs_main"),
            targets: &[Some(wgpu::ColorTargetState {
                format: render_state.target_format,
                blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: Default::default(),
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    });

    let light_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("vol3d-light-bgl"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D3,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 2,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 3,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::StorageTexture {
                    access: wgpu::StorageTextureAccess::WriteOnly,
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    view_dimension: wgpu::TextureViewDimension::D3,
                },
                count: None,
            },
        ],
    });
    let light_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("vol3d-light-bg"),
        layout: &light_layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: light_uniforms.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::TextureView(
                    &volume_tex.create_view(&Default::default()),
                ),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: wgpu::BindingResource::Sampler(&sampler),
            },
            wgpu::BindGroupEntry {
                binding: 3,
                resource: wgpu::BindingResource::TextureView(
                    &light_tex.create_view(&Default::default()),
                ),
            },
        ],
    });
    let light_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("vol3d-light-pl"),
        bind_group_layouts: &[Some(&light_layout)],
        immediate_size: 0,
    });
    let light_pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some("vol3d-light-pipeline"),
        layout: Some(&light_pipeline_layout),
        module: &light_shader,
        entry_point: Some("cs_main"),
        compilation_options: Default::default(),
        cache: None,
    });
    render_state
        .renderer
        .write()
        .callback_resources
        .insert(Vol3dResources {
            pipeline,
            bind_group,
            uniforms,
            volume_tex,
            lut_tex,
            floor_tex,
            color_tex,
            light_pipeline,
            light_bind_group,
            light_uniforms,
            advanced: advanced_resources,
            volume_generation: 0,
            last_lighting_revision: None,
        });
}

/// Per-frame paint callback: uniforms + pending texture uploads in `prepare`,
/// one fullscreen-triangle draw in `paint`.
pub struct Vol3dCallback {
    pub yaw: f32,
    pub pitch: f32,
    pub dist: f32,
    pub camera_mode: Vol3dCameraMode,
    pub fly_x: f32,
    pub fly_y: f32,
    pub fly_z: f32,
    pub threshold01: f32,
    pub threshold_high01: f32,
    pub threshold_mode: Vol3dThresholdMode,
    pub opacity: f32,
    pub aspect: f32,
    pub floor_opacity: f32,
    pub floor_mode: FloorMode,
    pub zspan: f32,
    pub fov_scale: f32,
    pub quality: Vol3dQuality,
    pub density: f32,
    pub shading: f32,
    pub lighting: Vol3dLightingSettings,
    pub clip_low: f32,
    pub clip_high: f32,
    pub floor_threshold01: f32,
    pub floor_threshold_high01: f32,
    pub floor_threshold_mode: Vol3dThresholdMode,
    pub focus_height: f32,
    /// 0 = single-box (reflectivity and every other moment), 1 = velocity
    /// two-box (reflectivity structure + velocity color).
    pub velocity_mode: f32,
    /// Normalized reflectivity structure gate for velocity mode.
    pub ref_gate: f32,
    /// Couplet emphasis 0..1 for velocity mode.
    pub couplet_emphasis: f32,
    /// The advanced uniform block, already packed by
    /// `advanced::AdvancedParams::shader_uniforms`. Uniform-only state, so it
    /// is rewritten every frame and rebuilds nothing.
    pub advanced: [f32; advanced::ADVANCED_UNIFORM_FLOATS],
    pub pending: Arc<Mutex<PendingUploads>>,
}

impl egui_wgpu::CallbackTrait for Vol3dCallback {
    fn prepare(
        &self,
        _device: &wgpu::Device,
        queue: &wgpu::Queue,
        _screen: &egui_wgpu::ScreenDescriptor,
        encoder: &mut wgpu::CommandEncoder,
        resources: &mut egui_wgpu::CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        let Some(resources) = resources.get_mut::<Vol3dResources>() else {
            return Vec::new();
        };
        let uniforms: [f32; UNIFORM_FLOATS] = [
            self.yaw,
            self.pitch,
            self.dist,
            self.threshold01,
            self.opacity,
            self.aspect,
            self.floor_opacity,
            self.floor_mode.shader_value(),
            self.zspan,
            self.fov_scale,
            self.quality.steps().min(MAX_SHADER_STEPS) as f32,
            self.density,
            self.shading,
            self.clip_low,
            self.clip_high,
            self.floor_threshold01,
            self.focus_height,
            self.camera_mode.shader_value(),
            self.fly_x,
            self.fly_y,
            self.fly_z,
            self.threshold_high01,
            self.threshold_mode.shader_value(),
            self.floor_threshold_high01,
            self.floor_threshold_mode.shader_value(),
            self.velocity_mode,
            self.ref_gate,
            self.couplet_emphasis,
            self.lighting.rim_strength,
            if self.lighting.enabled { 1.0 } else { 0.0 },
            LIGHTING_ENCODE_MAX,
            0.0,
        ];
        let mut bytes = [0u8; UNIFORM_FLOATS * std::mem::size_of::<f32>()];
        for (index, value) in uniforms.iter().enumerate() {
            bytes[index * 4..index * 4 + 4].copy_from_slice(&value.to_le_bytes());
        }
        queue.write_buffer(&resources.uniforms, 0, &bytes);
        resources.advanced.write_uniforms(queue, &self.advanced);

        let mut uploaded_volume = false;
        if let Ok(mut pending) = self.pending.lock() {
            if let Some(volume) = pending.volume.take() {
                uploaded_volume = true;
                queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: &resources.volume_tex,
                        mip_level: 0,
                        origin: wgpu::Origin3d::ZERO,
                        aspect: wgpu::TextureAspect::All,
                    },
                    &volume.data,
                    wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(volume.n as u32),
                        rows_per_image: Some(volume.n as u32),
                    },
                    wgpu::Extent3d {
                        width: volume.n as u32,
                        height: volume.n as u32,
                        depth_or_array_layers: volume.nz as u32,
                    },
                );
                if let Some(color_data) = volume.color_data {
                    queue.write_texture(
                        wgpu::TexelCopyTextureInfo {
                            texture: &resources.color_tex,
                            mip_level: 0,
                            origin: wgpu::Origin3d::ZERO,
                            aspect: wgpu::TextureAspect::All,
                        },
                        &color_data,
                        wgpu::TexelCopyBufferLayout {
                            offset: 0,
                            bytes_per_row: Some(volume.n as u32),
                            rows_per_image: Some(volume.n as u32),
                        },
                        wgpu::Extent3d {
                            width: volume.n as u32,
                            height: volume.n as u32,
                            depth_or_array_layers: volume.nz as u32,
                        },
                    );
                }
                // With the box and only with the box. A `None` acceleration
                // uploads zeroes rather than nothing, so a stale no-data mask
                // can never outlive the box it was built for.
                resources
                    .advanced
                    .write_acceleration(queue, volume.acceleration.as_ref());
                if let Some(floor_data) = volume.floor_data {
                    queue.write_texture(
                        wgpu::TexelCopyTextureInfo {
                            texture: &resources.floor_tex,
                            mip_level: 0,
                            origin: wgpu::Origin3d::ZERO,
                            aspect: wgpu::TextureAspect::All,
                        },
                        &floor_data,
                        wgpu::TexelCopyBufferLayout {
                            offset: 0,
                            bytes_per_row: Some(FLOOR_N as u32),
                            rows_per_image: Some(FLOOR_N as u32),
                        },
                        wgpu::Extent3d {
                            width: FLOOR_N as u32,
                            height: FLOOR_N as u32,
                            depth_or_array_layers: 1,
                        },
                    );
                }
            }
            if let Some(lut) = pending.lut.take() {
                queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: &resources.lut_tex,
                        mip_level: 0,
                        origin: wgpu::Origin3d::ZERO,
                        aspect: wgpu::TextureAspect::All,
                    },
                    &lut,
                    wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(256 * 4),
                        rows_per_image: Some(1),
                    },
                    wgpu::Extent3d {
                        width: 256,
                        height: 1,
                        depth_or_array_layers: 1,
                    },
                );
            }
            if let Some(table) = pending.preintegrated.take() {
                resources.advanced.write_preintegration(queue, &table);
            }
        }
        if uploaded_volume {
            resources.volume_generation = resources.volume_generation.wrapping_add(1);
        }

        if self.lighting.enabled && self.shading > 0.001 {
            let settings_revision = self.lighting.cache_revision(
                self.threshold01,
                self.threshold_high01,
                self.threshold_mode.shader_value(),
                self.velocity_mode,
                self.ref_gate,
                self.zspan,
            );
            let desired_revision =
                combine_cache_revision(settings_revision, resources.volume_generation);
            if resources.last_lighting_revision != Some(desired_revision) {
                let light = self.lighting.light_direction();
                let coefficients = self.lighting.log_sh_coefficients();
                let lighting_uniforms: [f32; LIGHTING_UNIFORM_FLOATS] = [
                    light[0],
                    light[1],
                    light[2],
                    0.0,
                    self.lighting.ambient_strength,
                    self.lighting.key_strength,
                    self.lighting.shadow_strength,
                    self.lighting.shadow_density,
                    self.threshold01,
                    self.threshold_high01,
                    self.threshold_mode.shader_value(),
                    self.ref_gate,
                    self.zspan,
                    self.lighting.shadow_steps.clamp(1, MAX_SHADOW_STEPS) as f32,
                    self.velocity_mode,
                    LIGHTING_ENCODE_MAX,
                    coefficients[0],
                    coefficients[1],
                    coefficients[2],
                    coefficients[3],
                    coefficients[4],
                    coefficients[5],
                    coefficients[6],
                    coefficients[7],
                    coefficients[8],
                    coefficients[9],
                    coefficients[10],
                    coefficients[11],
                    coefficients[12],
                    coefficients[13],
                    coefficients[14],
                    coefficients[15],
                ];
                let mut lighting_bytes =
                    [0u8; LIGHTING_UNIFORM_FLOATS * std::mem::size_of::<f32>()];
                for (index, value) in lighting_uniforms.iter().enumerate() {
                    lighting_bytes[index * 4..index * 4 + 4].copy_from_slice(&value.to_le_bytes());
                }
                queue.write_buffer(&resources.light_uniforms, 0, &lighting_bytes);

                {
                    let mut compute_pass =
                        encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                            label: Some("vol3d-light-volume"),
                            timestamp_writes: None,
                        });
                    compute_pass.set_pipeline(&resources.light_pipeline);
                    compute_pass.set_bind_group(0, &resources.light_bind_group, &[]);
                    compute_pass.dispatch_workgroups(
                        LIGHT_VOLUME_N.div_ceil(LIGHT_WORKGROUP_SIZE),
                        LIGHT_VOLUME_N.div_ceil(LIGHT_WORKGROUP_SIZE),
                        LIGHT_VOLUME_NZ.div_ceil(LIGHT_WORKGROUP_SIZE),
                    );
                }
                resources.last_lighting_revision = Some(desired_revision);
            }
        }
        Vec::new()
    }

    fn paint(
        &self,
        _info: egui::PaintCallbackInfo,
        render_pass: &mut wgpu::RenderPass<'static>,
        resources: &egui_wgpu::CallbackResources,
    ) {
        let Some(resources) = resources.get::<Vol3dResources>() else {
            return;
        };
        render_pass.set_pipeline(&resources.pipeline);
        render_pass.set_bind_group(0, &resources.bind_group, &[]);
        render_pass.draw(0..3, 0..1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_projection_is_aspect_stable() {
        let explorer = Vol3d::default();
        let square = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(640.0, 640.0));
        let wide = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1_200.0, 640.0));
        let point = [0.35, -0.40, 0.0];
        let square_position = explorer
            .project_point(square, point)
            .expect("visible point");
        let wide_position = explorer.project_point(wide, point).expect("visible point");
        let square_offset = square_position - square.center();
        let wide_offset = wide_position - wide.center();
        assert!((square_offset.x - wide_offset.x).abs() < 1.0e-3);
        assert!((square_offset.y - wide_offset.y).abs() < 1.0e-3);
    }

    #[test]
    fn vertical_exaggeration_is_independent_of_box_size() {
        let mut explorer = Vol3d {
            vertical_exaggeration: 2.5,
            ..Default::default()
        };
        for half_km in [10.0, 20.0, 60.0, 120.0, 180.0] {
            explorer.box_half_km = half_km;
            let recovered = explorer.zspan() * half_km / explorer.top_km();
            assert!((recovered - 2.5).abs() < 1.0e-4);
        }
    }

    #[test]
    fn orbit_distance_backs_out_for_tall_selected_cell_boxes() {
        let explorer = Vol3d {
            box_half_km: 6.0,
            vertical_exaggeration: 2.5,
            dist: 1.2,
            ..Default::default()
        };

        assert!(explorer.zspan() > 1.6);
        assert!(explorer.orbit_distance() > explorer.dist);
    }

    #[test]
    fn fly_camera_starts_from_orbit_eye_and_sees_box_center() {
        let mut explorer = Vol3d::default();
        let orbit_eye = explorer.orbit_eye();
        explorer.enter_fly_mode();

        assert_eq!(explorer.camera_mode, Vol3dCameraMode::Fly);
        assert!((explorer.fly_x - orbit_eye[0]).abs() < 1.0e-5);
        assert!((explorer.fly_y - orbit_eye[1]).abs() < 1.0e-5);
        assert!((explorer.fly_z - orbit_eye[2]).abs() < 1.0e-5);

        let rect = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(800.0, 600.0));
        assert!(
            explorer
                .project_point(rect, explorer.orbit_center())
                .is_some()
        );
    }

    #[test]
    fn normalize_values_maps_range_and_rejects_nan() {
        // Signed velocity range: min -> 0, zero -> mid, max -> 255.
        let values = [-100.0, 0.0, 100.0, f32::NAN, -250.0, 999.0];
        let out = normalize_values(&values, -100.0, 100.0);
        assert_eq!(out[0], 0);
        assert_eq!(out[1], 128); // 0 m/s is the diverging-colormap center
        assert_eq!(out[2], 255);
        assert_eq!(out[3], 0); // no-data
        assert_eq!(out[4], 0); // clamped below min
        assert_eq!(out[5], 255); // clamped above max
    }

    #[test]
    fn normalize_box_data_matches_helper() {
        let values = vec![-40.0, -10.0, 0.0, 25.0, 60.0, f32::NAN];
        let vbox = normalize_box_with_range(&values, 1, values.len(), -50.0, 60.0);
        assert_eq!(vbox.data, normalize_values(&values, -50.0, 60.0));
        assert!(vbox.color_data.is_none(), "single box has no color plane");
    }

    #[test]
    fn two_box_color_plane_matches_structure_dims() {
        // In velocity mode the structure (reflectivity) box and the velocity
        // color box must share the same lattice so the shader can sample both
        // at one uvw.
        let refl = vec![5.0f32; BOX_N * BOX_N * BOX_NZ];
        let vel = vec![3.0f32; BOX_N * BOX_N * BOX_NZ];
        let mut vbox = normalize_box_with_range(&refl, BOX_N, BOX_NZ, 0.0, 80.0);
        vbox.color_data = Some(normalize_values(&vel, -100.0, 100.0));
        assert_eq!(vbox.color_data.as_ref().unwrap().len(), vbox.data.len());
    }

    #[test]
    fn velocity_presets_are_ordered() {
        let mut v = Vol3d::default();
        v.apply_velocity_broad_preset();
        let broad = (v.vel_ref_gate_dbz, v.vel_couplet_emphasis, v.opacity);
        v.apply_velocity_couplet_preset();
        let couplet = (v.vel_ref_gate_dbz, v.vel_couplet_emphasis, v.opacity);
        v.apply_velocity_shear_preset();
        let shear = (v.vel_ref_gate_dbz, v.vel_couplet_emphasis, v.opacity);

        // Reflectivity gate, couplet emphasis, and opacity all tighten as the
        // preset moves from broad flow toward strong-shear isolation.
        assert!(broad.0 < couplet.0 && couplet.0 < shear.0);
        assert!(broad.1 < couplet.1 && couplet.1 < shear.1);
        assert!(broad.2 < couplet.2 && couplet.2 < shear.2);
        // Emphasis is a 0..1 mix factor; broad flow applies none.
        assert_eq!(broad.1, 0.0);
        for emphasis in [broad.1, couplet.1, shear.1] {
            assert!((0.0..=1.0).contains(&emphasis));
        }
    }

    #[test]
    fn default_velocity_fields_are_sane() {
        let v = Vol3d::default();
        assert!(v.vel_ref_gate_dbz > 0.0 && v.vel_ref_gate_dbz < 80.0);
        assert!((0.0..=1.0).contains(&v.vel_couplet_emphasis));
        assert!(!v.velocity_color_active);
    }

    #[test]
    fn shaders_parse_and_validate() {
        // The build nodes are headless: wgpu never builds these pipelines, so
        // Naga validates both embedded shaders without requiring an adapter.
        for (label, source) in [("raymarch", SHADER), ("light-volume", LIGHT_VOLUME_SHADER)] {
            let module = naga::front::wgsl::parse_str(source)
                .unwrap_or_else(|error| panic!("vol3d {label} WGSL failed to parse: {error:?}"));
            let mut validator = naga::valid::Validator::new(
                naga::valid::ValidationFlags::all(),
                naga::valid::Capabilities::all(),
            );
            validator
                .validate(&module)
                .unwrap_or_else(|error| panic!("vol3d {label} WGSL failed to validate: {error:?}"));
        }
    }
}
