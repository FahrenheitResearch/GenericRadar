//! Hierarchy-aware traversal, support display and the analysis render modes of
//! the second-generation volume renderer.
//!
//! Everything here is CPU-side preparation plus the two WGSL fragments that
//! replace the base shader's `fs_main`. The Cartesian reconstruction already
//! runs on a worker thread, so building the conservative hierarchy there costs
//! nothing extra and avoids depending on compute-shader storage-texture
//! features that are not available on every wgpu backend.
//!
//! No meteorological field is changed by this module. It decides where the ray
//! marcher is allowed to skip, how weakly-supported reconstruction is faded,
//! and how a segment of the transfer function is integrated — never what a
//! gate measured.
//!
//! Three properties are load-bearing and are pinned by the tests below:
//!
//! * the min/max hierarchy is CONSERVATIVE, so a skipped brick cannot hide an
//!   echo (built and proven in `render2d::volumetric_support`);
//! * support is a reconstruction DISPLAY AID, never radar QC, confidence, or
//!   uncertainty;
//! * segment preintegration is off for velocity two-box rendering, where the
//!   structure field and the colour field are different quantities.

// Ported surface lands here before the pane drives it, and clippy runs with
// `-D warnings`.
#![allow(dead_code)]

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use eframe::egui_wgpu::wgpu;
use rayon::prelude::*;

// The hierarchy itself is built in `render2d`, where the resampler that feeds
// it already lives. Re-exported so the pane and the GPU upload path have one
// name for it; in a binary crate `pub use` buys nothing from the unused lint.
#[allow(unused_imports)]
pub use render2d::volumetric_support::{
    COARSE_GROUP_X, COARSE_GROUP_Y, COARSE_GROUP_Z, FINE_BRICK, HierarchyDims, VolumeAcceleration,
    build_acceleration,
};

/// Fine hierarchy dimensions for the fixed `BOX_N x BOX_N x BOX_NZ` lattice.
/// The WGSL traverser hard-codes these, so they are asserted at compile time.
pub const FINE_X: usize = super::BOX_N / FINE_BRICK;
pub const FINE_Y: usize = super::BOX_N / FINE_BRICK;
pub const FINE_Z: usize = super::BOX_NZ / FINE_BRICK;
// `div_ceil` on every axis, matching `render2d::volumetric_support::coarse_dims`
// exactly. A floor division here would silently disagree with the builder the
// first time `BOX_N` stopped being a multiple of `FINE_BRICK * COARSE_GROUP_X`,
// and the disagreement would be a hierarchy texture one cell too small.
pub const COARSE_X: usize = FINE_X.div_ceil(COARSE_GROUP_X);
pub const COARSE_Y: usize = FINE_Y.div_ceil(COARSE_GROUP_Y);
pub const COARSE_Z: usize = FINE_Z.div_ceil(COARSE_GROUP_Z);

const _: () =
    assert!(super::BOX_N.is_multiple_of(FINE_BRICK) && super::BOX_NZ.is_multiple_of(FINE_BRICK));
const _: () = assert!(FINE_X == 24 && FINE_Y == 24 && FINE_Z == 6);
const _: () = assert!(COARSE_X == 6 && COARSE_Y == 6 && COARSE_Z == 2);

/// Edge length of the segment-preintegration table.
pub const PREINTEGRATION_N: usize = 256;

/// The one sentence every surface that shows support must carry.
///
/// Support describes the reconstruction geometry — which beams passed near a
/// Cartesian voxel and how far a value had to be carried to reach it. It is
/// not a quality-control product, a confidence, or an error bar, and calling
/// it one would put an authority behind it that the radar never gave.
pub const SUPPORT_DISCLOSURE: &str = "Beam support describes how directly the Cartesian voxel is \
     constrained by the observed tilt stack. It is a display aid for reading the reconstruction, \
     not official radar QC and not a formal uncertainty.";

/// What the ray marcher draws.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Vol3dRenderMode {
    DirectVolume,
    HybridShell,
    Isosurface,
    MaximumProjection,
    OrthogonalSlices,
    SupportInspection,
}

impl Vol3dRenderMode {
    pub const ALL: [Self; 6] = [
        Self::DirectVolume,
        Self::HybridShell,
        Self::Isosurface,
        Self::MaximumProjection,
        Self::OrthogonalSlices,
        Self::SupportInspection,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::DirectVolume => "Direct volume",
            Self::HybridShell => "Hybrid shell + volume",
            Self::Isosurface => "Isosurface",
            Self::MaximumProjection => "Maximum projection",
            Self::OrthogonalSlices => "Orthogonal slices",
            Self::SupportInspection => "Beam-support inspection",
        }
    }

    /// The shader compares against `value +/- 0.5` windows, so these must stay
    /// whole numbers one apart.
    pub fn shader_value(self) -> f32 {
        match self {
            Self::DirectVolume => 0.0,
            Self::HybridShell => 1.0,
            Self::Isosurface => 2.0,
            Self::MaximumProjection => 3.0,
            Self::OrthogonalSlices => 4.0,
            Self::SupportInspection => 5.0,
        }
    }
}

/// How weakly-supported reconstruction is presented.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SupportMode {
    /// Fade reconstruction the beam stack barely constrains. The default,
    /// because an unfaded interpolated storm top looks like an observation.
    HonestFade,
    /// Show the full reconstruction at uniform opacity. Still gated by the
    /// no-data mask; only the fading is disabled.
    FullReconstruction,
    /// Colour by support instead of by the field, to read the beam anatomy
    /// behind an apparent 3D structure.
    Inspect,
}

impl SupportMode {
    pub const ALL: [Self; 3] = [Self::HonestFade, Self::FullReconstruction, Self::Inspect];

    pub fn label(self) -> &'static str {
        match self {
            Self::HonestFade => "Fade weak support",
            Self::FullReconstruction => "Show full reconstruction",
            // User-facing strings follow the application's US spelling; the
            // prose in this repository does not.
            Self::Inspect => "Color by support",
        }
    }

    pub fn shader_value(self) -> f32 {
        match self {
            Self::HonestFade => 0.0,
            Self::FullReconstruction => 1.0,
            Self::Inspect => 2.0,
        }
    }
}

/// Number of floats in the advanced uniform block.
///
/// A multiple of four, because a WGSL uniform buffer is sized in 16-byte rows.
pub const ADVANCED_UNIFORM_FLOATS: usize = 28;
/// Byte size of the advanced uniform buffer.
pub const ADVANCED_UNIFORM_BYTES: u64 =
    (ADVANCED_UNIFORM_FLOATS * std::mem::size_of::<f32>()) as u64;

/// WGSL member order of `AdvancedUniforms`, in the order
/// [`AdvancedParams::shader_uniforms`] packs them. Checked against the shader
/// text itself by [`tests::advanced_uniform_layout_matches_the_shader_struct`].
pub const ADVANCED_UNIFORM_FIELDS: [&str; ADVANCED_UNIFORM_FLOATS] = [
    "render_mode",
    "support_mode",
    "support_floor",
    "support_fade",
    "iso_value",
    "iso_width",
    "jitter_strength",
    "preintegration",
    "crop_x_min",
    "crop_x_max",
    "crop_y_min",
    "crop_y_max",
    "slice_x",
    "slice_y",
    "slice_z",
    "adaptive_strength",
    "reference_path",
    "_advanced_pad_0",
    "_advanced_pad_1",
    "_advanced_pad_2",
    "opacity_ramp_low",
    "opacity_ramp_high",
    "opacity_ramp_gamma",
    "opacity_ramp_floor",
    "opacity_ramp_gain",
    "_advanced_pad_3",
    "_advanced_pad_4",
    "_advanced_pad_5",
];

/// Resource bindings the advanced fragments add to bind group 0, on top of the
/// base shader's 0..=8. The integration side must create the bind group layout
/// in exactly this order.
pub const ADVANCED_BINDINGS: [(&str, u32); 5] = [
    ("t_support", 9),
    ("t_hierarchy_fine", 10),
    ("t_hierarchy_coarse", 11),
    ("t_preintegrated", 12),
    ("ua", 13),
];

/// UI-thread state for the second-generation renderer.
///
/// `iso_value` and `iso_width` are in the ENGINE units OF THE STRUCTURE FIELD —
/// the field `t_volume` carries — because the shader compares them against a
/// `t_volume` sample. For a single-box moment that is just that moment's units,
/// like `Vol3d::threshold_dbz`. In velocity two-box mode the structure box is
/// still reflectivity, so the isosurface is a dBZ surface even though the
/// colour is m/s, and [`AdvancedParams::shader_uniforms`] must be given the
/// REFLECTIVITY range there, not the velocity range.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AdvancedParams {
    pub render_mode: Vol3dRenderMode,
    pub support_mode: SupportMode,
    /// Support below this fraction fades to nothing in `HonestFade`.
    pub support_floor: f32,
    /// Exponent on the faded weight: higher fades harder.
    pub support_fade: f32,
    pub iso_value: f32,
    pub iso_width: f32,
    /// Stable per-pixel sub-voxel offset, 0..1. Removes wood-grain banding.
    pub jitter_strength: f32,
    pub preintegration: bool,
    pub adaptive_strength: f32,
    pub crop_x_min: f32,
    pub crop_x_max: f32,
    pub crop_y_min: f32,
    pub crop_y_max: f32,
    pub slice_x: f32,
    pub slice_y: f32,
    pub slice_z: f32,
    /// Fixed-step, no-hierarchy path kept for A/B verification.
    pub reference_path: bool,
    /// Where the opacity ramp lifts off, in the ENGINE units of the structure
    /// field. Below it every admitted sample absorbs at `opacity_ramp_floor`.
    pub opacity_ramp_low_dbz: f32,
    /// Where the ramp saturates, same units. At and above it a sample absorbs
    /// at `opacity_ramp_gain`.
    pub opacity_ramp_high_dbz: f32,
    /// Exponent between the knees: 1 is linear in dBZ, higher concentrates
    /// opacity into the cores. See [`Self::DEFAULT_OPACITY_RAMP_GAMMA`].
    pub opacity_ramp_gamma: f32,
    /// Extinction multiplier at and below the low knee. Not zero, so a deep
    /// body of weak echo still reads as cloud; equal to `opacity_ramp_gain` it
    /// flattens the ramp to a constant, which is what the renderer did before
    /// the ramp existed.
    pub opacity_ramp_floor: f32,
    /// Extinction multiplier at and above the high knee. Above 1 on purpose:
    /// see [`Self::DEFAULT_OPACITY_RAMP_GAIN`].
    pub opacity_ramp_gain: f32,
}

impl Default for AdvancedParams {
    fn default() -> Self {
        Self {
            render_mode: Vol3dRenderMode::DirectVolume,
            support_mode: SupportMode::HonestFade,
            support_floor: 0.18,
            support_fade: 1.0,
            iso_value: 45.0,
            iso_width: 2.0,
            jitter_strength: 0.6,
            preintegration: true,
            adaptive_strength: 0.75,
            crop_x_min: 0.0,
            crop_x_max: 1.0,
            crop_y_min: 0.0,
            crop_y_max: 1.0,
            slice_x: 0.5,
            slice_y: 0.5,
            slice_z: 0.35,
            reference_path: false,
            opacity_ramp_low_dbz: Self::DEFAULT_OPACITY_RAMP_LOW_DBZ,
            opacity_ramp_high_dbz: Self::DEFAULT_OPACITY_RAMP_HIGH_DBZ,
            opacity_ramp_gamma: Self::DEFAULT_OPACITY_RAMP_GAMMA,
            opacity_ramp_floor: Self::DEFAULT_OPACITY_RAMP_FLOOR,
            opacity_ramp_gain: Self::DEFAULT_OPACITY_RAMP_GAIN,
        }
    }
}

impl AdvancedParams {
    /// Where the ramp lifts off, dBZ: about where a reflectivity field stops
    /// being receiver noise and starts being cloud.
    pub const DEFAULT_OPACITY_RAMP_LOW_DBZ: f32 = 5.0;
    /// Where the ramp saturates, dBZ: a hail-bearing core, the thing that has
    /// to read as a solid body and not as a brighter patch of the same haze.
    pub const DEFAULT_OPACITY_RAMP_HIGH_DBZ: f32 = 60.0;
    /// Exponent of the ramp between the knees, chosen against the physics
    /// rather than by eye. For a Marshall-Palmer
    /// drop-size distribution the visible extinction coefficient goes as
    /// `Z^0.406` - Marshall & Palmer 1948 (*The distribution of raindrops with
    /// size*, J. Meteor. 5(4), 165-166) for `Z = 200 R^1.6`, and Atlas 1953
    /// (*Optical extinction by rainfall*, J. Meteor. 10(6), 486-488) for
    /// `sigma ~ R^0.65` - which in dBZ is `10^(0.0406 dBZ)`. With
    /// [`Self::DEFAULT_OPACITY_RAMP_FLOOR`], a power law in normalised dBZ at
    /// this exponent reproduces that curve to inside 14% from 20 to 58 dBZ, and
    /// [`tests::the_default_ramp_tracks_the_marshall_palmer_extinction_law`]
    /// pins it so a "let's make it pop" edit has to argue with a measurement.
    /// Lower it toward 1.0 for a flatter body, raise it to push all but the
    /// cores into haze.
    pub const DEFAULT_OPACITY_RAMP_GAMMA: f32 = 4.2;
    /// Extinction multiplier at and below the low knee. Small but non-zero: a
    /// tall column of 10 dBZ still has to accumulate into a visible cloud edge,
    /// which is the point of rendering a volume instead of a surface. It is
    /// part of the fit above (the physical law does not reach zero either) and
    /// sits on the gain's scale: the fitted shape wants 0.02 OF the gain.
    pub const DEFAULT_OPACITY_RAMP_FLOOR: f32 = 0.07;
    /// Extinction multiplier at and above the high knee, above 1 deliberately.
    /// A ramp normalised to 1 at the core leaves every value below the core
    /// thinner than it was and nothing more solid - the exact opposite of the
    /// complaint this answers, and what the first attempt at it measured. With
    /// a gain the opacity slider still means what it says in the middle of the
    /// ramp (about 45 dBZ here) while cores gain body and weak echo loses it.
    /// It multiplies an optical depth, not an alpha, so above 1 nothing
    /// overflows: a core saturates in fewer samples, which is "solid".
    pub const DEFAULT_OPACITY_RAMP_GAIN: f32 = 3.5;

    /// The extinction multiplier at a physical value of the STRUCTURE field.
    ///
    /// The number the picture is made of: it multiplies the optical depth of
    /// every sample, so the ratio of two is how much more light a core stops
    /// than drizzle does, independent of step size or camera.
    /// `structure_min`/`structure_max` carry the same requirement as
    /// [`Self::shader_uniforms`]: the range of the field in `t_volume`, which
    /// is REFLECTIVITY in velocity two-box mode. Outside that field the ramp
    /// is flat and this returns 1 — see [`Self::packed_ramp_scale`].
    pub fn extinction_multiplier(&self, value: f32, structure_min: f32, structure_max: f32) -> f32 {
        let span = (structure_max - structure_min).abs().max(f32::EPSILON);
        let normalize = |raw: f32| ((raw - structure_min) / span).clamp(0.0, 1.0);
        let (ramp_floor, gain) = self.packed_ramp_scale(structure_min, structure_max);
        opacity_ramp(
            normalize(value),
            normalize(self.opacity_ramp_low_dbz),
            normalize(self.opacity_ramp_high_dbz),
            self.opacity_ramp_gamma,
            ramp_floor,
            gain,
        )
    }

    /// The floor and gain actually sent to the shader: the operator's pair
    /// where the ramp means something, and a flat `(1, 1)` where it does not.
    ///
    /// The ramp is an argument about REFLECTIVITY and about nothing else. Its
    /// knees are in dBZ and its exponent is fitted to a drop-size
    /// distribution, so the whole construction is meaningless the moment
    /// `t_volume` carries a different field — and the 3D explorer will happily
    /// build its box from any product the operator has selected. Applied
    /// blindly, `5..60` normalised against another declared range is not
    /// merely arbitrary, it is wrong in ways that misread the data:
    ///
    /// - **Velocity** (`-64..64` m/s, or `-100..100` unfolded) is SIGNED and
    ///   roughly symmetric about zero, so a ramp that rises with the raw value
    ///   makes outbound flow up to fifty times more opaque than inbound flow of
    ///   the same speed. Half of every couplet — the half a tornado is read
    ///   from — would fade out.
    /// - **Correlation coefficient** (`0.208..1.052`) is entirely below the
    ///   5 dBZ knee once normalised, so every voxel would sit on the floor and
    ///   the field would render about fourteen times more transparent than the
    ///   operator asked for. Differential reflectivity, spectrum width and
    ///   specific differential phase collapse the same way over their working
    ///   ranges.
    /// - **Differential phase** (`0..360 deg`) and the derived volume products
    ///   run the other way: 60 units is a small fraction of their range, so
    ///   nearly every voxel would saturate at the gain and the box would render
    ///   as an opaque brick.
    ///
    /// The one signal this function has for telling the fields apart is the
    /// declared engine range it is already given, and reflectivity's is the
    /// NEXRAD 8-bit encoding domain. It is a deliberately narrow test:
    /// anything it does not recognise gets the flat ramp, which is exactly the
    /// behaviour the renderer had before the ramp existed, so an unrecognised
    /// field can only be unchanged and never wrong.
    /// [`tests::the_ramp_is_flat_for_every_product_that_is_not_reflectivity`]
    /// walks the real product catalog rather than a fixture.
    fn packed_ramp_scale(&self, structure_min: f32, structure_max: f32) -> (f32, f32) {
        if !ramp_applies_to_structure(structure_min, structure_max) {
            return (1.0, 1.0);
        }
        let gain = self.opacity_ramp_gain.max(0.0);
        (self.opacity_ramp_floor.clamp(0.0, gain), gain)
    }

    /// Direct volume rendering with weak-support fading: the operational
    /// default, and the only preset that claims nothing about surfaces.
    pub fn apply_volume_preset(&mut self) {
        self.render_mode = Vol3dRenderMode::DirectVolume;
        self.support_mode = SupportMode::HonestFade;
        self.preintegration = true;
        self.adaptive_strength = 0.75;
        self.jitter_strength = 0.6;
        self.opacity_ramp_low_dbz = Self::DEFAULT_OPACITY_RAMP_LOW_DBZ;
        self.opacity_ramp_high_dbz = Self::DEFAULT_OPACITY_RAMP_HIGH_DBZ;
        self.opacity_ramp_gamma = Self::DEFAULT_OPACITY_RAMP_GAMMA;
        self.opacity_ramp_floor = Self::DEFAULT_OPACITY_RAMP_FLOOR;
        self.opacity_ramp_gain = Self::DEFAULT_OPACITY_RAMP_GAIN;
    }

    pub fn apply_hybrid_preset(&mut self) {
        self.render_mode = Vol3dRenderMode::HybridShell;
        self.support_mode = SupportMode::HonestFade;
        self.preintegration = false;
        self.adaptive_strength = 0.55;
        self.jitter_strength = 0.45;
    }

    pub fn apply_surface_preset(&mut self) {
        self.render_mode = Vol3dRenderMode::Isosurface;
        self.support_mode = SupportMode::HonestFade;
        self.preintegration = false;
        self.adaptive_strength = 0.35;
        self.jitter_strength = 0.25;
    }

    pub fn apply_support_preset(&mut self) {
        self.render_mode = Vol3dRenderMode::SupportInspection;
        self.support_mode = SupportMode::Inspect;
        self.preintegration = false;
        self.adaptive_strength = 0.5;
        // The opacity ramp is NOT flattened here, and deliberately so. Both
        // inspection presentations must be ungraded — for the same reason
        // `support_weight` does not fade them — but the render-mode and
        // support-mode dropdowns reach them without going through any preset,
        // so a preset that flattened the numbers would leave the two most
        // common routes into the mode still graded by reflectivity. The
        // shader's `opacity_ramp` returns 1 for both instead, which covers
        // every route and leaves the operator's ramp settings intact when they
        // come back out. See
        // [`tests::the_shader_flattens_the_ramp_for_the_inspection_modes`].
    }

    /// Keep the horizontal crop box non-degenerate and ordered.
    pub fn normalized_horizontal_crop(&self) -> (f32, f32, f32, f32) {
        let x0 = self.crop_x_min.clamp(0.0, 0.99);
        let x1 = self.crop_x_max.clamp(x0 + 0.01, 1.0);
        let y0 = self.crop_y_min.clamp(0.0, 0.99);
        let y1 = self.crop_y_max.clamp(y0 + 0.01, 1.0);
        (x0, x1, y0, y1)
    }

    /// Pack the advanced uniform block.
    ///
    /// `structure_min`/`structure_max` are the value range of the box in
    /// `t_volume`, which is what `iso_value` and `iso_width` are normalised
    /// against. In velocity two-box mode that box is REFLECTIVITY: passing the
    /// velocity range there would place the isosurface at an arbitrary dBZ.
    ///
    /// `velocity_two_box` forces preintegration off: the table integrates one
    /// scalar field, and in that mode the structure and the colour come from
    /// two different fields. The shader refuses it as well; doing it here too
    /// means the refusal is visible on the CPU and testable without a GPU.
    pub fn shader_uniforms(
        &self,
        structure_min: f32,
        structure_max: f32,
        velocity_two_box: bool,
    ) -> [f32; ADVANCED_UNIFORM_FLOATS] {
        let span = (structure_max - structure_min).abs().max(f32::EPSILON);
        let (crop_x_min, crop_x_max, crop_y_min, crop_y_max) = self.normalized_horizontal_crop();
        let (ramp_floor, ramp_gain) = self.packed_ramp_scale(structure_min, structure_max);
        [
            self.render_mode.shader_value(),
            self.support_mode.shader_value(),
            self.support_floor.clamp(0.0, 0.95),
            self.support_fade.max(0.05),
            ((self.iso_value - structure_min) / span).clamp(0.0, 1.0),
            (self.iso_width / span).clamp(0.001, 0.25),
            self.jitter_strength.clamp(0.0, 1.0),
            if self.preintegration && !velocity_two_box {
                1.0
            } else {
                0.0
            },
            crop_x_min,
            crop_x_max,
            crop_y_min,
            crop_y_max,
            self.slice_x.clamp(0.0, 1.0),
            self.slice_y.clamp(0.0, 1.0),
            self.slice_z.clamp(0.0, 1.0),
            self.adaptive_strength.clamp(0.0, 1.0),
            if self.reference_path { 1.0 } else { 0.0 },
            0.0,
            0.0,
            0.0,
            // Normalised against the STRUCTURE range for the same reason
            // `iso_value` is: the shader compares them to a `t_volume` sample,
            // which is reflectivity even when the palette is m/s.
            ((self.opacity_ramp_low_dbz - structure_min) / span).clamp(0.0, 1.0),
            ((self.opacity_ramp_high_dbz - structure_min) / span).clamp(0.0, 1.0),
            self.opacity_ramp_gamma.max(0.05),
            ramp_floor,
            ramp_gain,
            0.0,
            0.0,
            0.0,
        ]
    }

    /// The same block, ready for `queue.write_buffer`. `structure_min` and
    /// `structure_max` carry the same requirement as [`Self::shader_uniforms`].
    pub fn shader_uniform_bytes(
        &self,
        structure_min: f32,
        structure_max: f32,
        velocity_two_box: bool,
    ) -> [u8; ADVANCED_UNIFORM_FLOATS * 4] {
        let values = self.shader_uniforms(structure_min, structure_max, velocity_two_box);
        let mut bytes = [0u8; ADVANCED_UNIFORM_FLOATS * 4];
        for (index, value) in values.iter().enumerate() {
            bytes[index * 4..index * 4 + 4].copy_from_slice(&value.to_le_bytes());
        }
        bytes
    }
}

/// Build the hierarchy for the fixed 3D-explorer lattice.
///
/// `normalized` is the STRUCTURE plane — `VolumeBox::data` — which in velocity
/// two-box mode is the reflectivity, never the velocity. Geometry, opacity,
/// hierarchy and support therefore all come from the same field.
pub fn build_box_acceleration(normalized: &[u8], support: &[u8]) -> VolumeAcceleration {
    build_acceleration(normalized, support, super::BOX_N, super::BOX_NZ)
}

/// The shader's transfer gate, mirrored on the CPU for the preintegration
/// table so the two paths agree.
///
/// Returns 0 where the WGSL returns its -1 "does not contribute" sentinel; the
/// caller uses the result as a weight, where the two are equivalent.
fn threshold_strength(value: f32, low: f32, high: f32, mode: f32, width: f32) -> f32 {
    if mode > 1.5 {
        if value <= low {
            return smoothstep(0.0, width, low - value);
        }
        if value >= high {
            return smoothstep(0.0, width, value - high);
        }
        return 0.0;
    }
    if mode > 0.5 {
        if value >= low {
            return 0.0;
        }
        return smoothstep(0.0, width, low - value);
    }
    if value <= low {
        return 0.0;
    }
    smoothstep(low, low + width, value)
}

/// The declared engine range of a NEXRAD reflectivity field, and the only
/// structure domain [`AdvancedParams`]'s opacity ramp is a statement about.
///
/// It is the 8-bit encoding domain, `product_engine`'s
/// `declared_engine_range` for REF and CREF and the reflectivity range the 3D
/// pane substitutes in velocity two-box mode.
/// [`tests::the_reflectivity_structure_range_matches_the_product_catalog`]
/// pins it to the catalog, so the two cannot drift apart silently.
pub const REFLECTIVITY_STRUCTURE_RANGE_DBZ: (f32, f32) = (-32.0, 94.5);

/// Whether the field in `t_volume` is the reflectivity the ramp describes.
///
/// Tolerance is a twentieth of a dBZ: this is an identity check on a declared
/// constant, not a similarity metric.
fn ramp_applies_to_structure(structure_min: f32, structure_max: f32) -> bool {
    (structure_min - REFLECTIVITY_STRUCTURE_RANGE_DBZ.0).abs() < 0.05
        && (structure_max - REFLECTIVITY_STRUCTURE_RANGE_DBZ.1).abs() < 0.05
}

/// The shader's `opacity_ramp`, mirrored on the CPU so the transfer function
/// can be measured and hand-checked without a GPU. Every argument is in the
/// shader's NORMALIZED domain, 0..1 across the structure field's declared
/// range; [`AdvancedParams::extinction_multiplier`] takes dBZ. The result
/// multiplies OPTICAL DEPTH, never a composited alpha - see the WGSL for why
/// that distinction is the correctness argument, and for the Levoy 1988 /
/// Max 1995 / Kniss 2002 lineage of the ramp itself.
fn opacity_ramp(
    structure01: f32,
    low01: f32,
    high01: f32,
    gamma: f32,
    ramp_floor: f32,
    gain: f32,
) -> f32 {
    let low = low01.clamp(0.0, 1.0);
    let high = high01.max(low + 0.0005);
    let gain = gain.max(0.0);
    let ramp_floor = ramp_floor.clamp(0.0, gain);
    let fraction = ((structure01 - low) / (high - low)).clamp(0.0, 1.0);
    let shaped = fraction.powf(gamma.max(0.05));
    ramp_floor + (gain - ramp_floor) * shaped
}

/// WGSL `smoothstep`: the Hermite `3t^2 - 2t^3` blend on a clamped parameter.
fn smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    let t = ((x - edge0) / (edge1 - edge0).max(f32::EPSILON)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// The shader's `support_weight`, mirrored on the CPU so the display rule can
/// be reasoned about and hand-checked without a GPU.
///
/// `support01` is the sampled support texel in 0..1. Two things are
/// load-bearing here and are pinned by the tests:
///
/// * a zero sample weighs nothing in EVERY mode — support is the no-data mask
///   before it is a display weight;
/// * only `HonestFade` fades. `FullReconstruction` shows the reconstruction at
///   uniform opacity by definition, and `Inspect` must too: fading the mode
///   whose whole job is to expose the cone of silence, the wide tilt gaps and
///   the top extrapolation, by exactly the quantity it paints, would hide them.
///
/// This is a presentation weight, never a confidence, a QC flag or an error bar.
fn support_display_weight(support01: f32, mode: SupportMode, floor: f32, fade: f32) -> f32 {
    if support01 <= 0.0001 {
        return 0.0;
    }
    if mode != SupportMode::HonestFade {
        return 1.0;
    }
    let normalized = smoothstep(floor.clamp(0.0, 0.95), 1.0, support01);
    normalized.max(0.0001).powf(fade.max(0.05))
}

/// Linear sample of a 256-entry RGBA8 palette, matching the GPU's filtered
/// `t_lut` fetch.
fn lut_sample(lut: &[u8], value: f32) -> [f32; 4] {
    if lut.len() < 256 * 4 {
        return [0.0; 4];
    }
    let x = value.clamp(0.0, 1.0) * 255.0;
    let i0 = x.floor() as usize;
    let i1 = (i0 + 1).min(255);
    let t = x - i0 as f32;
    let mut out = [0.0f32; 4];
    for (channel, slot) in out.iter_mut().enumerate() {
        let a = f32::from(lut[i0 * 4 + channel]) / 255.0;
        let b = f32::from(lut[i1 * 4 + channel]) / 255.0;
        *slot = a + (b - a) * t;
    }
    out
}

/// Build the 256x256 segment-preintegration table.
///
/// Entry `(start, end)` integrates the transfer function along the straight
/// line from normalized value `start` to `end` over one REFERENCE segment.
/// RGB is stored straight-alpha; alpha is that reference segment's opacity,
/// which the shader then Beer-Lambert corrects for the actual ray step and
/// density. This is the classical preintegrated transfer function of Engel,
/// Kraus & Ertl 2001 (*High-Quality Pre-Integrated Volume Rendering Using
/// Hardware-Accelerated Pixel Shading*, HWWS '01, 9-16), evaluated numerically
/// rather than analytically because the threshold gate is not integrable in
/// closed form.
///
/// This table depends on the palette, the thresholds and the opacity. It is
/// NOT the spatial hierarchy and rebuilding it does not rebuild that; see
/// [`preintegration_signature`] for the cache key.
pub fn build_preintegrated_lut(
    lut: &[u8],
    threshold_low: f32,
    threshold_high: f32,
    threshold_mode: f32,
    opacity: f32,
) -> Vec<u8> {
    const SUBSTEPS: usize = 16;
    let mut out = vec![0u8; PREINTEGRATION_N * PREINTEGRATION_N * 4];
    out.par_chunks_exact_mut(4)
        .enumerate()
        .for_each(|(index, texel)| {
            let start = (index / PREINTEGRATION_N) as f32;
            let end = (index % PREINTEGRATION_N) as f32;
            let mut color = [0.0f32; 3];
            let mut accumulated = 0.0f32;
            for step in 0..SUBSTEPS {
                let fraction = (step as f32 + 0.5) / SUBSTEPS as f32;
                let value = (start + (end - start) * fraction) / 255.0;
                let palette = lut_sample(lut, value);
                let transfer =
                    threshold_strength(value, threshold_low, threshold_high, threshold_mode, 0.08);
                let raw = (palette[3] * opacity * transfer).clamp(0.0, 0.9999);
                let alpha = 1.0 - (1.0 - raw).powf(1.0 / SUBSTEPS as f32);
                let remaining = 1.0 - accumulated;
                for (channel, slot) in color.iter_mut().enumerate() {
                    *slot += remaining * alpha * palette[channel];
                }
                accumulated += remaining * alpha;
            }
            if accumulated > 1.0e-6 {
                for (channel, slot) in texel[..3].iter_mut().enumerate() {
                    *slot = ((color[channel] / accumulated).clamp(0.0, 1.0) * 255.0).round() as u8;
                }
            }
            texel[3] = (accumulated.clamp(0.0, 1.0) * 255.0).round() as u8;
        });
    out
}

/// Cache key for [`build_preintegrated_lut`]. Camera motion is deliberately
/// not an input: moving the camera must never rebuild anything.
pub fn preintegration_signature(
    lut: &[u8],
    threshold_low: f32,
    threshold_high: f32,
    threshold_mode: f32,
    opacity: f32,
) -> u64 {
    let mut hasher = DefaultHasher::new();
    lut.hash(&mut hasher);
    threshold_low.to_bits().hash(&mut hasher);
    threshold_high.to_bits().hash(&mut hasher);
    threshold_mode.to_bits().hash(&mut hasher);
    opacity.to_bits().hash(&mut hasher);
    hasher.finish()
}

/// Traversal helpers, bindings and the advanced uniform block.
pub const ADVANCED_SHADER_HELPERS: &str = include_str!("advanced_shader_helpers.wgsl");
/// The second-generation `fs_main`.
pub const ADVANCED_FS_MAIN: &str = include_str!("advanced_fs_main.wgsl");

/// The five GPU resources the second-generation renderer adds to bind group 0.
///
/// They live here rather than in `vol3d.rs` for two reasons: the module that
/// decides the formats is the one that has to keep them right, and `vol3d.rs`
/// sits close enough to its 2000-line module ceiling that the descriptors would
/// not fit beside the base ones.
/// Support-plane format. **Unorm, never Srgb** — see [`AdvancedResources::new`].
pub const SUPPORT_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::R8Unorm;
/// Hierarchy min/max format. **Unorm, never Srgb.**
pub const HIERARCHY_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
/// Preintegration-table format. **Unorm, never Srgb.**
pub const PREINTEGRATION_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

pub struct AdvancedResources {
    pub support: wgpu::Texture,
    pub hierarchy_fine: wgpu::Texture,
    pub hierarchy_coarse: wgpu::Texture,
    pub preintegrated: wgpu::Texture,
    pub uniforms: wgpu::Buffer,
}

/// Views of [`AdvancedResources`], held by the caller across the bind-group
/// build because [`wgpu::BindGroupEntry`] borrows them.
pub struct AdvancedViews {
    support: wgpu::TextureView,
    hierarchy_fine: wgpu::TextureView,
    hierarchy_coarse: wgpu::TextureView,
    preintegrated: wgpu::TextureView,
}

/// A 3D texture that the fragment stage samples or loads.
fn volume_texture(
    device: &wgpu::Device,
    label: &str,
    format: wgpu::TextureFormat,
    size: (u32, u32, u32),
) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width: size.0,
            height: size.1,
            depth_or_array_layers: size.2,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D3,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    })
}

fn sampled_texture_entry(
    binding: u32,
    dimension: wgpu::TextureViewDimension,
) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: dimension,
            multisampled: false,
        },
        count: None,
    }
}

impl AdvancedResources {
    /// **Every format here is Unorm, and none of them may become Srgb.**
    ///
    /// The hierarchy textures carry conservative min/max BOUNDS on the stored
    /// 0..=255 field, compared in the shader against thresholds that live in the
    /// same domain. An Srgb view returns those bytes gamma-DECODED, so the
    /// bounds arrive smaller than the values they are supposed to bound, the
    /// skip test starts answering "cannot contribute" for cells that can, and
    /// the traverser drops storms — with no validation error and no compile
    /// error, because Srgb and Unorm are both filterable float textures. The
    /// same argument applies to the support plane, which is a mask, and to the
    /// preintegration table, whose alpha is an optical depth rather than a
    /// colour.
    pub fn new(device: &wgpu::Device) -> Self {
        let support = volume_texture(
            device,
            "vol3d-support",
            SUPPORT_FORMAT,
            (
                super::BOX_N as u32,
                super::BOX_N as u32,
                super::BOX_NZ as u32,
            ),
        );
        let hierarchy_fine = volume_texture(
            device,
            "vol3d-hierarchy-fine",
            HIERARCHY_FORMAT,
            (FINE_X as u32, FINE_Y as u32, FINE_Z as u32),
        );
        let hierarchy_coarse = volume_texture(
            device,
            "vol3d-hierarchy-coarse",
            HIERARCHY_FORMAT,
            (COARSE_X as u32, COARSE_Y as u32, COARSE_Z as u32),
        );
        let preintegrated = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("vol3d-preintegrated"),
            size: wgpu::Extent3d {
                width: PREINTEGRATION_N as u32,
                height: PREINTEGRATION_N as u32,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: PREINTEGRATION_FORMAT,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("vol3d-advanced-uniforms"),
            size: ADVANCED_UNIFORM_BYTES,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Self {
            support,
            hierarchy_fine,
            hierarchy_coarse,
            preintegrated,
            uniforms,
        }
    }

    pub fn views(&self) -> AdvancedViews {
        AdvancedViews {
            support: self.support.create_view(&Default::default()),
            hierarchy_fine: self.hierarchy_fine.create_view(&Default::default()),
            hierarchy_coarse: self.hierarchy_coarse.create_view(&Default::default()),
            preintegrated: self.preintegrated.create_view(&Default::default()),
        }
    }

    /// Layout entries for bindings 9..=13, in the order [`ADVANCED_BINDINGS`]
    /// pins. Appended to the base shader's 0..=8.
    pub fn layout_entries() -> [wgpu::BindGroupLayoutEntry; ADVANCED_BINDINGS.len()] {
        [
            sampled_texture_entry(ADVANCED_BINDINGS[0].1, wgpu::TextureViewDimension::D3),
            sampled_texture_entry(ADVANCED_BINDINGS[1].1, wgpu::TextureViewDimension::D3),
            sampled_texture_entry(ADVANCED_BINDINGS[2].1, wgpu::TextureViewDimension::D3),
            sampled_texture_entry(ADVANCED_BINDINGS[3].1, wgpu::TextureViewDimension::D2),
            wgpu::BindGroupLayoutEntry {
                binding: ADVANCED_BINDINGS[4].1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            },
        ]
    }

    pub fn bind_group_entries<'a>(
        &'a self,
        views: &'a AdvancedViews,
    ) -> [wgpu::BindGroupEntry<'a>; ADVANCED_BINDINGS.len()] {
        [
            wgpu::BindGroupEntry {
                binding: ADVANCED_BINDINGS[0].1,
                resource: wgpu::BindingResource::TextureView(&views.support),
            },
            wgpu::BindGroupEntry {
                binding: ADVANCED_BINDINGS[1].1,
                resource: wgpu::BindingResource::TextureView(&views.hierarchy_fine),
            },
            wgpu::BindGroupEntry {
                binding: ADVANCED_BINDINGS[2].1,
                resource: wgpu::BindingResource::TextureView(&views.hierarchy_coarse),
            },
            wgpu::BindGroupEntry {
                binding: ADVANCED_BINDINGS[3].1,
                resource: wgpu::BindingResource::TextureView(&views.preintegrated),
            },
            wgpu::BindGroupEntry {
                binding: ADVANCED_BINDINGS[4].1,
                resource: self.uniforms.as_entire_binding(),
            },
        ]
    }

    pub fn write_uniforms(&self, queue: &wgpu::Queue, values: &[f32; ADVANCED_UNIFORM_FLOATS]) {
        queue.write_buffer(&self.uniforms, 0, &pack_uniforms(values));
    }

    /// Upload the support plane and both hierarchy levels for a newly arrived
    /// box.
    ///
    /// `acceleration` is `None` for the placeholder empty box. Zeroes go up in
    /// that case rather than nothing at all: leaving the previous box's support
    /// resident would let a stale no-data mask authorise the new box's zeroed
    /// voxels, and `Below` would then paint the whole lattice solid.
    pub fn write_acceleration(
        &self,
        queue: &wgpu::Queue,
        acceleration: Option<&VolumeAcceleration>,
    ) {
        let fine_len = FINE_X * FINE_Y * FINE_Z * 4;
        let coarse_len = COARSE_X * COARSE_Y * COARSE_Z * 4;
        let support_len = super::BOX_N * super::BOX_N * super::BOX_NZ;
        let zeros;
        let (support, fine, coarse) = match acceleration {
            Some(acceleration)
                if acceleration.support.len() == support_len
                    && acceleration.fine_minmax.len() == fine_len
                    && acceleration.coarse_minmax.len() == coarse_len =>
            {
                (
                    acceleration.support.as_slice(),
                    acceleration.fine_minmax.as_slice(),
                    acceleration.coarse_minmax.as_slice(),
                )
            }
            _ => {
                zeros = vec![0u8; support_len];
                (
                    &zeros[..support_len],
                    &zeros[..fine_len],
                    &zeros[..coarse_len],
                )
            }
        };
        write_3d(
            queue,
            &self.support,
            support,
            super::BOX_N as u32,
            (
                super::BOX_N as u32,
                super::BOX_N as u32,
                super::BOX_NZ as u32,
            ),
        );
        write_3d(
            queue,
            &self.hierarchy_fine,
            fine,
            (FINE_X * 4) as u32,
            (FINE_X as u32, FINE_Y as u32, FINE_Z as u32),
        );
        write_3d(
            queue,
            &self.hierarchy_coarse,
            coarse,
            (COARSE_X * 4) as u32,
            (COARSE_X as u32, COARSE_Y as u32, COARSE_Z as u32),
        );
    }

    /// Upload the segment-preintegration table. Palette/threshold/opacity
    /// driven, so it rides with the LUT and never with the box.
    pub fn write_preintegration(&self, queue: &wgpu::Queue, table: &[u8]) {
        if table.len() != PREINTEGRATION_N * PREINTEGRATION_N * 4 {
            return;
        }
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.preintegrated,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            table,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some((PREINTEGRATION_N * 4) as u32),
                rows_per_image: Some(PREINTEGRATION_N as u32),
            },
            wgpu::Extent3d {
                width: PREINTEGRATION_N as u32,
                height: PREINTEGRATION_N as u32,
                depth_or_array_layers: 1,
            },
        );
    }
}

fn write_3d(
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    data: &[u8],
    bytes_per_row: u32,
    size: (u32, u32, u32),
) {
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        data,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(bytes_per_row),
            rows_per_image: Some(size.1),
        },
        wgpu::Extent3d {
            width: size.0,
            height: size.1,
            depth_or_array_layers: size.2,
        },
    );
}

/// Little-endian bytes of the advanced uniform block, ready for
/// `queue.write_buffer`.
pub fn pack_uniforms(values: &[f32; ADVANCED_UNIFORM_FLOATS]) -> [u8; ADVANCED_UNIFORM_FLOATS * 4] {
    let mut bytes = [0u8; ADVANCED_UNIFORM_FLOATS * 4];
    for (index, value) in values.iter().enumerate() {
        bytes[index * 4..index * 4 + 4].copy_from_slice(&value.to_le_bytes());
    }
    bytes
}

/// Compose the shipped fragment program from the base shader's prelude.
///
/// The prelude keeps everything the port does not touch — the `Uniforms`
/// block, the nine base bindings, `vs_main`, `box_intersect`, `column_max`,
/// `threshold_strength`, and critically `shaded_rgb`, which reads the cached
/// positive log-SH lighting volume. Only the base `fs_main` is replaced, so
/// the lighting contract survives by construction rather than by review.
pub fn compose_shader(prelude: &str) -> String {
    // Match the attribute only at the start of a line, so a comment mentioning
    // it cannot truncate the prelude early.
    let head = match prelude.find("\n@fragment") {
        Some(offset) => &prelude[..offset],
        None => prelude,
    };
    let mut out =
        String::with_capacity(head.len() + ADVANCED_SHADER_HELPERS.len() + ADVANCED_FS_MAIN.len());
    out.push_str(head);
    out.push('\n');
    out.push_str(ADVANCED_SHADER_HELPERS);
    out.push('\n');
    out.push_str(ADVANCED_FS_MAIN);
    out
}

/// The reflectivity opacity ramp's own test suite. A sibling file, because it
/// is one coherent argument and it is longer than the code it checks.
#[cfg(test)]
mod ramp_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn composed() -> String {
        compose_shader(super::super::SHADER)
    }

    fn parse(source: &str) -> naga::Module {
        naga::front::wgsl::parse_str(source)
            .unwrap_or_else(|error| panic!("composed vol3d WGSL failed to parse: {error:?}"))
    }

    #[test]
    fn composed_shader_parses_and_validates() {
        let source = composed();
        // One fragment entry point: the base fs_main must have been replaced,
        // not appended to. Count declarations, not mentions in comments.
        let entry_points = source
            .lines()
            .filter(|line| line.trim_start().starts_with("@fragment"))
            .count();
        assert_eq!(entry_points, 1);
        let bodies = source
            .lines()
            .filter(|line| line.trim_start().starts_with("fn fs_main"))
            .count();
        assert_eq!(bodies, 1);
        assert!(source.contains("fn vs_main"), "vertex stage was truncated");
        assert!(
            source.contains("fn shaded_rgb"),
            "the log-SH lighting fetch must survive the port"
        );

        let module = parse(&source);
        let mut validator = naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::all(),
        );
        validator
            .validate(&module)
            .unwrap_or_else(|error| panic!("composed vol3d WGSL failed to validate: {error:?}"));
    }

    #[test]
    fn advanced_uniform_layout_matches_the_shader_struct() {
        let module = parse(&composed());
        let members = module
            .types
            .iter()
            .find(|(_, ty)| ty.name.as_deref() == Some("AdvancedUniforms"))
            .and_then(|(_, ty)| match &ty.inner {
                naga::TypeInner::Struct { members, .. } => Some(members.clone()),
                _ => None,
            })
            .expect("AdvancedUniforms is declared by the helper fragment");

        let names: Vec<&str> = members
            .iter()
            .map(|member| member.name.as_deref().unwrap_or(""))
            .collect();
        assert_eq!(names, ADVANCED_UNIFORM_FIELDS.to_vec());
        for (index, member) in members.iter().enumerate() {
            assert_eq!(
                member.offset as usize,
                index * 4,
                "field {} is not where shader_uniforms writes it",
                names[index]
            );
        }
    }

    #[test]
    fn the_bind_group_layout_is_built_in_the_order_the_shader_pins() {
        // The integration side appends these to the base shader's 0..=8, so a
        // reordering here would bind the hierarchy where the support mask is
        // read: no validation error, a silently wrong image.
        let entries = AdvancedResources::layout_entries();
        assert_eq!(entries.len(), ADVANCED_BINDINGS.len());
        for (entry, (name, binding)) in entries.iter().zip(ADVANCED_BINDINGS) {
            assert_eq!(entry.binding, binding, "{name} moved slot");
            assert_eq!(entry.visibility, wgpu::ShaderStages::FRAGMENT);
        }
        // 9..=11 are 3D textures, 12 is the 2D preintegration table, 13 is the
        // uniform block.
        let dimension = |entry: &wgpu::BindGroupLayoutEntry| match entry.ty {
            wgpu::BindingType::Texture { view_dimension, .. } => Some(view_dimension),
            _ => None,
        };
        assert_eq!(dimension(&entries[0]), Some(wgpu::TextureViewDimension::D3));
        assert_eq!(dimension(&entries[1]), Some(wgpu::TextureViewDimension::D3));
        assert_eq!(dimension(&entries[2]), Some(wgpu::TextureViewDimension::D3));
        assert_eq!(dimension(&entries[3]), Some(wgpu::TextureViewDimension::D2));
        assert!(matches!(
            entries[4].ty,
            wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                ..
            }
        ));
    }

    #[test]
    fn none_of_the_advanced_textures_may_be_srgb() {
        // An Srgb view gamma-DECODES the stored bytes. The hierarchy carries
        // conservative BOUNDS on the 0..=255 field and the shader compares them
        // against thresholds in that same stored domain, so decoded bounds stop
        // bounding, the skip test starts culling cells that can contribute, and
        // storms vanish with no compile or validation error. The support plane
        // is a mask and the preintegration alpha is an optical depth; neither is
        // a colour either.
        for (label, format) in [
            ("support", SUPPORT_FORMAT),
            ("hierarchy", HIERARCHY_FORMAT),
            ("preintegration", PREINTEGRATION_FORMAT),
        ] {
            assert!(!format.is_srgb(), "{label} texture became Srgb");
        }
        assert_eq!(SUPPORT_FORMAT, wgpu::TextureFormat::R8Unorm);
        assert_eq!(HIERARCHY_FORMAT, wgpu::TextureFormat::Rgba8Unorm);
        assert_eq!(PREINTEGRATION_FORMAT, wgpu::TextureFormat::Rgba8Unorm);
    }

    #[test]
    fn packed_uniforms_are_little_endian_in_declaration_order() {
        let values = AdvancedParams::default().shader_uniforms(0.0, 80.0, false);
        let bytes = pack_uniforms(&values);
        assert_eq!(bytes.len() as u64, ADVANCED_UNIFORM_BYTES);
        for (index, value) in values.iter().enumerate() {
            let slot: [u8; 4] = bytes[index * 4..index * 4 + 4].try_into().unwrap();
            assert_eq!(f32::from_le_bytes(slot), *value);
        }
    }

    #[test]
    fn advanced_bindings_are_declared_where_the_bind_group_expects_them() {
        let module = parse(&composed());
        for (name, binding) in ADVANCED_BINDINGS {
            let variable = module
                .global_variables
                .iter()
                .find(|(_, global)| global.name.as_deref() == Some(name))
                .map(|(_, global)| global)
                .unwrap_or_else(|| panic!("{name} is not declared"));
            let resource = variable
                .binding
                .as_ref()
                .unwrap_or_else(|| panic!("{name} has no resource binding"));
            assert_eq!(resource.group, 0);
            assert_eq!(resource.binding, binding, "{name} moved binding slot");
        }
    }

    #[test]
    fn shader_hierarchy_dimensions_match_the_rust_constants() {
        let source = composed();
        assert!(
            source.contains(&format!(
                "const FINE_DIMS: vec3<i32> = vec3<i32>({FINE_X}, {FINE_Y}, {FINE_Z});"
            )),
            "the WGSL fine dimensions no longer match BOX_N / BOX_NZ"
        );
        assert!(
            source.contains(&format!(
                "const COARSE_DIMS: vec3<i32> = vec3<i32>({COARSE_X}, {COARSE_Y}, {COARSE_Z});"
            )),
            "the WGSL coarse dimensions no longer match BOX_N / BOX_NZ"
        );
        let dims =
            render2d::volumetric_support::fine_dims(super::super::BOX_N, super::super::BOX_NZ);
        assert_eq!(
            dims,
            HierarchyDims {
                x: FINE_X,
                y: FINE_Y,
                z: FINE_Z
            }
        );
    }

    #[test]
    fn no_data_is_rejected_before_any_threshold_mode_can_see_it() {
        // The support gate must sit ahead of the transfer function, or the
        // stored 0 of an unobserved voxel reads as a real low value and Below
        // and Outside paint the empty half of the box.
        let fs_main = ADVANCED_FS_MAIN;
        let gate = fs_main
            .find("if (support <= 0.0001) {")
            .expect("the traversal loop must gate on the support mask");
        let transfer = fs_main
            .find("transfer = threshold_strength(")
            .expect("the traversal loop must apply the threshold transfer");
        assert!(
            gate < transfer,
            "the no-data gate must precede the threshold transfer"
        );
        assert!(
            ADVANCED_SHADER_HELPERS.contains("if (range.a <= 0.0001) {"),
            "the hierarchy skip must reject cells with no support at all"
        );
    }

    #[test]
    fn preintegration_is_refused_for_the_velocity_two_box_path() {
        let params = AdvancedParams {
            preintegration: true,
            ..Default::default()
        };
        let single = params.shader_uniforms(0.0, 80.0, false);
        let two_box = params.shader_uniforms(-100.0, 100.0, true);
        let slot = ADVANCED_UNIFORM_FIELDS
            .iter()
            .position(|field| *field == "preintegration")
            .expect("the field exists");
        assert_eq!(single[slot], 1.0);
        assert_eq!(two_box[slot], 0.0);
        // And the shader refuses it independently of the CPU.
        assert!(ADVANCED_FS_MAIN.contains("&& u.velocity_mode < 0.5"));
    }

    #[test]
    fn shader_uniforms_normalise_the_isosurface_into_the_shader_domain() {
        let params = AdvancedParams {
            iso_value: 45.0,
            iso_width: 2.0,
            ..Default::default()
        };
        // 0..80 dBZ: 45 dBZ sits at 45/80 = 0.5625, a 2 dBZ shell is 0.025.
        let uniforms = params.shader_uniforms(0.0, 80.0, false);
        assert!((uniforms[4] - 0.5625).abs() < 1.0e-6);
        assert!((uniforms[5] - 0.025).abs() < 1.0e-6);
        // Out-of-range isosurfaces clamp rather than sampling off the palette.
        let clamped = AdvancedParams {
            iso_value: 500.0,
            ..params
        }
        .shader_uniforms(0.0, 80.0, false);
        assert_eq!(clamped[4], 1.0);
    }

    #[test]
    fn crop_box_stays_ordered_and_non_degenerate() {
        let params = AdvancedParams {
            crop_x_min: 0.8,
            crop_x_max: 0.2,
            crop_y_min: -3.0,
            crop_y_max: 9.0,
            ..Default::default()
        };
        let (x0, x1, y0, y1) = params.normalized_horizontal_crop();
        assert!((x0 - 0.8).abs() < 1.0e-6);
        assert!((x1 - 0.81).abs() < 1.0e-6);
        assert_eq!(y0, 0.0);
        assert_eq!(y1, 1.0);
    }

    #[test]
    fn threshold_transfer_matches_the_shader_hermite_blend() {
        // smoothstep(a, a + 0.08, a + 0.04) has t = 0.5, so the Hermite blend
        // 3t^2 - 2t^3 is 0.25 * 3 - 0.125 * 2 = 0.5. Above, Below and the two
        // wings of Outside all use the same 0.08 ramp.
        assert!((threshold_strength(0.24, 0.20, -1.0, 0.0, 0.08) - 0.5).abs() < 1.0e-6);
        assert_eq!(threshold_strength(0.20, 0.20, -1.0, 0.0, 0.08), 0.0);
        assert!((threshold_strength(0.16, 0.20, -1.0, 1.0, 0.08) - 0.5).abs() < 1.0e-6);
        assert_eq!(threshold_strength(0.25, 0.20, -1.0, 1.0, 0.08), 0.0);
        assert_eq!(threshold_strength(0.50, 0.20, 0.80, 2.0, 0.08), 0.0);
        assert!((threshold_strength(0.84, 0.20, 0.80, 2.0, 0.08) - 0.5).abs() < 1.0e-6);
        assert!((threshold_strength(0.16, 0.20, 0.80, 2.0, 0.08) - 0.5).abs() < 1.0e-6);
        // Fully past the ramp saturates rather than growing without bound.
        assert_eq!(threshold_strength(0.99, 0.20, -1.0, 0.0, 0.08), 1.0);
    }

    #[test]
    fn preintegrated_segment_reproduces_its_own_constant_value() {
        // Flat palette (rgb 90, opaque) and opacity 0.6. A degenerate segment
        // start = end = 200 holds one value, so the sixteen substeps compound
        // back to exactly the reference opacity 0.6 -> round(153.0) = 153, and
        // the straight-alpha colour is the palette itself -> 90.
        let lut: Vec<u8> = (0..256).flat_map(|_| [90u8, 90, 90, 255]).collect();
        let table = build_preintegrated_lut(&lut, 0.0, -1.0, 0.0, 0.6);
        assert_eq!(table.len(), PREINTEGRATION_N * PREINTEGRATION_N * 4);
        let texel = (200 * PREINTEGRATION_N + 200) * 4;
        assert_eq!(&table[texel..texel + 4], &[90, 90, 90, 153]);
    }

    #[test]
    fn preintegrated_segment_below_the_threshold_is_fully_transparent() {
        let lut: Vec<u8> = (0..256).flat_map(|_| [90u8, 90, 90, 255]).collect();
        // Above 0.5 normalized: value 10/255 = 0.039 never reaches the ramp.
        let table = build_preintegrated_lut(&lut, 0.5, -1.0, 0.0, 0.6);
        let texel = (10 * PREINTEGRATION_N + 10) * 4;
        assert_eq!(&table[texel..texel + 4], &[0, 0, 0, 0]);
        // A segment climbing across the threshold does accumulate.
        let crossing = (10 * PREINTEGRATION_N + 250) * 4;
        assert!(table[crossing + 3] > 0);
    }

    #[test]
    fn preintegrated_table_is_row_start_column_end() {
        // The table is uploaded row-major with a 256-texel row pitch, so texel
        // `index` sits at (x = index % 256, y = index / 256) and the build
        // writes `index = start * 256 + end`: row is the segment START. The
        // shader must therefore put the CURRENT value on the horizontal axis.
        //
        // Front-to-back compositing weights the start of the segment more, so
        // a ramp palette makes the two orientations distinguishable — while
        // the opacity, which is order-independent, must come out identical.
        let lut: Vec<u8> = (0..256)
            .flat_map(|index| [index as u8, index as u8, index as u8, 255])
            .collect();
        let table = build_preintegrated_lut(&lut, 0.0, -1.0, 0.0, 0.6);
        let rising = (50 * PREINTEGRATION_N + 200) * 4;
        let falling = (200 * PREINTEGRATION_N + 50) * 4;

        assert!(
            table[rising] < 125 && table[rising] > 50,
            "a 50 -> 200 segment must sit between its ends, nearer the start"
        );
        assert!(
            table[falling] > 125 && table[falling] < 200,
            "a 200 -> 50 segment must sit between its ends, nearer the start"
        );
        assert_eq!(
            table[rising + 3],
            table[falling + 3],
            "reversing a segment changes its colour but never its opacity"
        );
        assert!(
            ADVANCED_FS_MAIN.contains("vec2<f32>(structure, previous_structure)"),
            "the shader lookup must put the current value on the column axis"
        );
    }

    #[test]
    fn preintegration_signature_tracks_palette_threshold_and_opacity() {
        let lut: Vec<u8> = (0..256).flat_map(|i| [i as u8, 0, 0, 255]).collect();
        let base = preintegration_signature(&lut, 0.2, 0.8, 0.0, 0.5);
        assert_eq!(base, preintegration_signature(&lut, 0.2, 0.8, 0.0, 0.5));
        assert_ne!(base, preintegration_signature(&lut, 0.3, 0.8, 0.0, 0.5));
        assert_ne!(base, preintegration_signature(&lut, 0.2, 0.8, 0.0, 0.6));
        assert_ne!(base, preintegration_signature(&lut, 0.2, 0.8, 1.0, 0.5));
        let other: Vec<u8> = (0..256).flat_map(|i| [0, i as u8, 0, 255]).collect();
        assert_ne!(base, preintegration_signature(&other, 0.2, 0.8, 0.0, 0.5));
    }

    #[test]
    fn render_and_support_modes_land_inside_the_shader_windows() {
        for (index, mode) in Vol3dRenderMode::ALL.iter().enumerate() {
            assert_eq!(mode.shader_value(), index as f32);
            assert!(!mode.label().is_empty());
        }
        for (index, mode) in SupportMode::ALL.iter().enumerate() {
            assert_eq!(mode.shader_value(), index as f32);
        }
        assert_eq!(
            Vol3dRenderMode::SupportInspection.label(),
            "Beam-support inspection"
        );
    }

    #[test]
    fn support_disclosure_never_claims_qc_or_confidence() {
        let text = SUPPORT_DISCLOSURE.to_ascii_lowercase();
        assert!(text.contains("display aid"));
        assert!(text.contains("not official radar qc"));
        assert!(text.contains("not a formal uncertainty"));
        assert!(
            !text.contains("confidence"),
            "support must never be labelled a confidence"
        );
        assert!(!text.contains("quality control"));
    }

    #[test]
    fn reference_path_disables_hierarchy_and_jitter_but_nothing_else() {
        let params = AdvancedParams {
            reference_path: true,
            ..Default::default()
        };
        let slot = ADVANCED_UNIFORM_FIELDS
            .iter()
            .position(|field| *field == "reference_path")
            .expect("the field exists");
        assert_eq!(params.shader_uniforms(0.0, 80.0, false)[slot], 1.0);
        assert_eq!(
            AdvancedParams::default().shader_uniforms(0.0, 80.0, false)[slot],
            0.0
        );
        // The reference branch must gate the two skips, the jitter and the
        // adaptive step, and must still honour the support mask.
        assert_eq!(ADVANCED_FS_MAIN.matches("if (!reference) {").count(), 4);
        assert!(ADVANCED_FS_MAIN.contains("let reference = ua.reference_path > 0.5;"));
    }

    #[test]
    fn hierarchy_build_ignores_everything_a_camera_or_palette_can_change() {
        // Contract: camera motion, opacity and palette must not rebuild the
        // hierarchy. The only way to guarantee that is for the builder to have
        // no way to see them, so this pins the call shape.
        let voxels = super::super::BOX_N * super::super::BOX_N * super::super::BOX_NZ;
        let mut normalized = vec![0u8; voxels];
        let mut support = vec![0u8; voxels];
        let index = 9 * super::super::BOX_N * super::super::BOX_N + 25 * super::super::BOX_N + 17;
        normalized[index] = 231;
        support[index] = 207;

        let accel = build_box_acceleration(&normalized, &support);
        assert_eq!(accel.fine_dims.x, FINE_X);
        assert_eq!(accel.fine_dims.z, FINE_Z);
        assert_eq!(accel.fine_minmax.len(), FINE_X * FINE_Y * FINE_Z * 4);
        assert_eq!(
            accel.coarse_minmax.len(),
            COARSE_X * COARSE_Y * COARSE_Z * 4
        );
        let cell = accel
            .fine_dims
            .index(17 / FINE_BRICK, 25 / FINE_BRICK, 9 / FINE_BRICK)
            * 4;
        assert!(accel.fine_minmax[cell] <= 231 && accel.fine_minmax[cell + 1] >= 231);
        assert!(accel.fine_minmax[cell + 3] >= 207);
        assert!(accel.empty_fine_fraction < 1.0);
    }

    /// Everything after the fragment attribute: the ported code, with the base
    /// prelude excluded so a contract check cannot be satisfied by the text the
    /// port did not write.
    fn fragment_half(source: &str) -> &str {
        let offset = source
            .find("\n@fragment")
            .expect("the composed shader has a fragment stage");
        &source[offset..]
    }

    fn prelude_half(source: &str) -> &str {
        let offset = source
            .find("\n@fragment")
            .expect("the composed shader has a fragment stage");
        &source[..offset]
    }

    #[test]
    fn composed_shader_validates_without_any_optional_capability() {
        // `Capabilities::all()` will validate a shader that needs a feature the
        // adapter may not have. The port must not need one: it adds two 3D
        // texture loads, one 3D sample, one 2D sample and a uniform block.
        let module = parse(&composed());
        let mut validator = naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::empty(),
        );
        validator.validate(&module).unwrap_or_else(|error| {
            panic!("composed vol3d WGSL needs an optional capability: {error:?}")
        });
    }

    #[test]
    fn no_per_sample_gradient_lighting_survives_in_the_fragment_stage() {
        // Contract 1. The light volume is computed ONCE per voxel by
        // `light_volume.wgsl` (finite differences, positive log-SH ambient,
        // `exp(clamp(log_sh_l3(normal), -8, 4))`) and cached in `t_light`. The
        // ray marcher may only read that cache through `shaded_rgb`. Six
        // gradient taps per ray sample, or `abs(N dot L)` two-sided light, are
        // exactly the regression this port must not reintroduce.
        let source = composed();
        let fragment = fragment_half(&source);
        for banned in [
            "abs(dot(",
            "dpdx",
            "dpdy",
            "fwidth",
            // Implicit-derivative sampling; every fetch here is explicit-LOD.
            "textureSample(",
            "textureGather",
        ] {
            assert!(
                !fragment.contains(banned),
                "the ported fragment stage reintroduced `{banned}`"
            );
        }
        assert!(
            !fragment.contains("t_light"),
            "the fragment stage must reach the light cache only through shaded_rgb"
        );
        let prelude = prelude_half(&source);
        assert_eq!(
            prelude.matches("textureSampleLevel(t_light").count(),
            1,
            "shaded_rgb is the single light-cache fetch"
        );
        // One shaded colour per compositing path: orthogonal slices, the
        // isosurface shell, the direct-volume sample and the maximum
        // projection. If a path stops shading, this count moves.
        assert_eq!(
            fragment.matches("shaded_rgb(").count(),
            4,
            "every colour path must go through the cached-lighting shading"
        );
    }

    #[test]
    fn velocity_two_box_takes_structure_from_reflectivity_and_only_colour_from_velocity() {
        // Contract 2. `t_volume` is the reflectivity structure box and drives
        // the transfer function, the opacity, the isosurface, the hierarchy and
        // the support. `t_color` supplies a palette coordinate and nothing else.
        let fragment = ADVANCED_FS_MAIN;
        for line in fragment
            .lines()
            .filter(|line| line.contains("textureSampleLevel(t_color"))
        {
            let trimmed = line.trim();
            assert!(
                trimmed.starts_with("lut_coord = ")
                    || trimmed.starts_with("let velocity = ")
                    || trimmed.starts_with("surface_lut = "),
                "a velocity fetch escaped the palette-coordinate role: {trimmed}"
            );
        }
        // The velocity fetch feeds the LUT coordinate and the couplet emphasis,
        // never the transfer gate.
        assert_eq!(
            fragment
                .matches("transfer = smoothstep(u.ref_gate, u.ref_gate + 0.08, structure)")
                .count(),
            2,
            "both the slice path and the traversal loop gate on the reflectivity structure"
        );
        assert!(
            !fragment.contains("smoothstep(u.ref_gate, u.ref_gate + 0.08, velocity)"),
            "the reflectivity gate must never be applied to the velocity field"
        );
        // The hierarchy tests the structure plane's maximum against the same
        // gate, so the skip test and the transfer function cannot disagree.
        assert!(
            ADVANCED_SHADER_HELPERS.contains("return range.g > u.ref_gate;"),
            "the velocity skip test must read the structure plane's maximum"
        );
        // And the box the hierarchy is built from is the structure plane:
        // `build_box_acceleration` has no velocity parameter to be handed by
        // mistake, and its only field input is `VolumeBox::data`.
        assert!(
            ADVANCED_SHADER_HELPERS.contains("fn range_can_contribute(range: vec4<f32>) -> bool"),
            "the skip test takes one range, from the one structure hierarchy"
        );
    }

    #[test]
    fn the_skip_test_never_culls_on_the_minimum_support_channel() {
        // A coarse cell's `b` channel is a minimum over OBSERVED children, not
        // a bound over the cell, so culling on it would be unsound. Nothing
        // does, and this fails if anything starts.
        for source in [ADVANCED_SHADER_HELPERS, ADVANCED_FS_MAIN] {
            for banned in ["range.b", "fine.b", "coarse.b"] {
                assert!(
                    !source.contains(banned),
                    "the traverser started reading the minimum-support channel via `{banned}`"
                );
            }
        }
        // The only two range channels the adaptive step reads are the value
        // bounds, which ARE conservative over the cell plus its apron.
        assert!(ADVANCED_FS_MAIN.contains("let interval_width = fine.g - fine.r;"));
    }

    #[test]
    fn support_weight_mirrors_the_shader_rule() {
        // The Rust mirror below is only trustworthy while it matches the WGSL,
        // so pin the three branches it reproduces.
        let helpers = ADVANCED_SHADER_HELPERS;
        let start = helpers
            .find("fn support_weight(value: f32) -> f32 {")
            .expect("the shader declares support_weight");
        let body = &helpers[start..];
        let end = body.find("\n}").expect("the function closes") + 2;
        let body = &body[..end];
        assert!(body.contains("if (value <= 0.0001) {\n        return 0.0;\n    }"));
        assert!(
            body.contains("if (ua.support_mode > 0.5) {\n        return 1.0;\n    }"),
            "both non-fading support modes must return uniform weight"
        );
        assert!(
            !body.contains("ua.support_mode < 1.5"),
            "fading `Color by support` by support erases the anatomy it exists to show"
        );
        assert!(body.contains("smoothstep(floor_value, 1.0, value)"));
        assert!(body.contains("pow(max(normalized, 0.0001), max(ua.support_fade, 0.05))"));
    }

    #[test]
    fn no_data_is_transparent_in_every_support_mode() {
        for mode in SupportMode::ALL {
            assert_eq!(support_display_weight(0.0, mode, 0.18, 1.0), 0.0);
            assert_eq!(support_display_weight(0.00005, mode, 0.18, 1.0), 0.0);
        }
    }

    #[test]
    fn inspecting_support_no_longer_fades_out_the_weakest_reconstruction() {
        // 0.12 is the score a voxel gets above the highest cut once the
        // extrapolation term has run out (`0.12 * 255 = 31`), and 0.18 is the
        // downward-extrapolation floor. At the default support floor of 0.18
        // both land at or below the smoothstep's lower edge, so `HonestFade`
        // takes them to the 0.0001 clamp: invisible. That is the point of
        // `HonestFade` and a defect in the mode named for inspecting them.
        let top_extrapolation = 31.0 / 255.0;
        assert!(
            support_display_weight(top_extrapolation, SupportMode::HonestFade, 0.18, 1.0) <= 0.0001
        );
        assert_eq!(
            support_display_weight(top_extrapolation, SupportMode::Inspect, 0.18, 1.0),
            1.0
        );
        assert_eq!(
            support_display_weight(
                top_extrapolation,
                SupportMode::FullReconstruction,
                0.18,
                1.0
            ),
            1.0
        );
    }

    #[test]
    fn honest_fade_follows_the_hand_computed_hermite_ramp() {
        // floor 0.18, value 0.59: t = (0.59 - 0.18) / 0.82 = 0.5, and the
        // Hermite blend 3t^2 - 2t^3 at t = 0.5 is 0.75 - 0.25 = 0.5. With the
        // default fade exponent of 1.0 the weight is that blend unchanged.
        let weight = support_display_weight(0.59, SupportMode::HonestFade, 0.18, 1.0);
        assert!((weight - 0.5).abs() < 1.0e-6, "{weight}");
        // A fade exponent of 2 squares it: 0.25.
        let harder = support_display_weight(0.59, SupportMode::HonestFade, 0.18, 2.0);
        assert!((harder - 0.25).abs() < 1.0e-6, "{harder}");
    }

    #[test]
    fn advanced_constants_agree_with_the_hierarchy_builder() {
        // The GPU textures are sized from these constants and the data that
        // fills them is sized by `coarse_dims`. A floor/ceil disagreement here
        // would upload a hierarchy one cell short of the texture.
        let fine =
            render2d::volumetric_support::fine_dims(super::super::BOX_N, super::super::BOX_NZ);
        let coarse =
            render2d::volumetric_support::coarse_dims(super::super::BOX_N, super::super::BOX_NZ);
        assert_eq!(
            fine,
            HierarchyDims {
                x: FINE_X,
                y: FINE_Y,
                z: FINE_Z
            }
        );
        assert_eq!(
            coarse,
            HierarchyDims {
                x: COARSE_X,
                y: COARSE_Y,
                z: COARSE_Z
            }
        );
    }

    #[test]
    fn the_shipped_disclosure_reads_as_one_sentence_pair() {
        // The `\` continuations in the const must not leave doubled spaces or
        // stray indentation in a string that goes on screen.
        assert!(!SUPPORT_DISCLOSURE.contains("  "), "{SUPPORT_DISCLOSURE}");
        assert!(!SUPPORT_DISCLOSURE.contains('\n'));
        assert!(SUPPORT_DISCLOSURE.starts_with("Beam support describes"));
        assert!(SUPPORT_DISCLOSURE.ends_with("not a formal uncertainty."));
    }

    #[test]
    fn nothing_the_operator_reads_calls_support_a_quality_or_accuracy_claim() {
        // Contract 4, applied to every string this module can put on screen.
        // The labels must not use any of these words at all; the disclosure is
        // allowed exactly the two that it denies.
        let labels: Vec<&str> = Vol3dRenderMode::ALL
            .iter()
            .map(|mode| mode.label())
            .chain(SupportMode::ALL.iter().map(|mode| mode.label()))
            .collect();
        for text in labels {
            let lower = text.to_ascii_lowercase();
            for banned in [
                "confidence",
                "uncertain",
                "accuracy",
                "accurate",
                "error bar",
                "quality",
                "qc",
            ] {
                assert!(
                    !lower.contains(banned),
                    "user-visible label overclaims with `{banned}`: {text}"
                );
            }
        }
        let lower = SUPPORT_DISCLOSURE.to_ascii_lowercase();
        for banned in ["confidence", "accuracy", "accurate", "error bar", "quality"] {
            assert!(!lower.contains(banned), "the disclosure says `{banned}`");
        }
        // `uncertainty` and `QC` appear once each, inside the clause that
        // refuses them.
        assert_eq!(lower.matches("uncertainty").count(), 1);
        assert_eq!(lower.matches("qc").count(), 1);
        assert!(lower.contains("not a formal uncertainty"));
        assert!(lower.contains("not official radar qc"));
    }

    #[test]
    fn the_isosurface_normalises_against_the_structure_range_even_in_two_box_mode() {
        // The shader compares `iso_value` against a `t_volume` sample, and in
        // velocity two-box mode `t_volume` is reflectivity. A 45 dBZ shell is
        // therefore packed against the 0..80 dBZ reflectivity range even though
        // the palette is a signed-velocity ramp: 45 / 80 = 0.5625.
        let params = AdvancedParams {
            iso_value: 45.0,
            iso_width: 2.0,
            ..Default::default()
        };
        let two_box = params.shader_uniforms(0.0, 80.0, true);
        assert!((two_box[4] - 0.5625).abs() < 1.0e-6);
        assert!((two_box[5] - 0.025).abs() < 1.0e-6);
        // Handing it the VELOCITY range instead would put the shell at
        // (45 + 100) / 200 = 0.725 of the reflectivity ramp, about 58 dBZ.
        // That is the mistake the parameter names exist to prevent.
        let wrong = params.shader_uniforms(-100.0, 100.0, true);
        assert!((wrong[4] - 0.725).abs() < 1.0e-6);
    }

    /// What an opacity drag actually costs the UI thread.
    ///
    /// Contract 6 says opacity must not rebuild the SPATIAL hierarchy, and it
    /// does not. It does rebuild this 256x256x16 table, on the frame thread,
    /// and "it is cached" is not an answer while the slider is moving: every
    /// frame of the drag is a cache miss. Printed rather than asserted, because
    /// a wall-clock threshold in a shared gate is a flake generator — but a
    /// number a reviewer can look at is not.
    #[ignore = "wall-clock measurement; run with --ignored --nocapture"]
    #[test]
    fn a_preintegration_rebuild_is_cheap_enough_to_sit_on_the_frame_thread() {
        let lut: Vec<u8> = (0..256)
            .flat_map(|index| [index as u8, 200 - index as u8 / 2, 90, 255])
            .collect();
        // Warm the rayon pool so the first measurement is not thread startup.
        let _ = build_preintegrated_lut(&lut, 0.2, -1.0, 0.0, 0.5);
        let mut worst = std::time::Duration::ZERO;
        let mut total = std::time::Duration::ZERO;
        const RUNS: u32 = 24;
        for run in 0..RUNS {
            // A different opacity each time, exactly as a slider drag does, so
            // the signature misses on every iteration.
            let opacity = 0.3 + f32::from(run as u16) * 0.01;
            let started = std::time::Instant::now();
            let table = build_preintegrated_lut(&lut, 0.2, -1.0, 0.0, opacity);
            let elapsed = started.elapsed();
            assert_eq!(table.len(), PREINTEGRATION_N * PREINTEGRATION_N * 4);
            worst = worst.max(elapsed);
            total += elapsed;
        }
        println!(
            "preintegration rebuild: mean {:.2} ms, worst {:.2} ms over {RUNS} runs \
             ({PREINTEGRATION_N}x{PREINTEGRATION_N} x 16 substeps)",
            total.as_secs_f64() * 1000.0 / f64::from(RUNS),
            worst.as_secs_f64() * 1000.0
        );
    }
}
