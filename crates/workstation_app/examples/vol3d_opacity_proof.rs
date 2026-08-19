//! Photograph a real storm through the shipped 3D volume shader.
//!
//! A transfer function is a claim about what a volume looks like, and the only
//! way to check that claim is to render a real reflectivity box and look at it.
//! This composes the SAME WGSL `advanced::compose_shader` ships, builds the
//! SAME GPU resources `vol3d::init_gpu` builds, and drives the SAME
//! `Vol3dCallback` the pane queues every frame - so what comes back is the
//! application's own picture, not a re-implementation of it.
//!
//! The clear colour is fully transparent and the pipeline blends straight
//! alpha, so the read-back alpha channel is exactly the `accumulated` the ray
//! marcher finished with. The photograph and the opacity measurement therefore
//! come out of one render rather than out of two different code paths.
//!
//! ```text
//! cargo run --release -p workstation_app --example vol3d_opacity_proof --
//!     <out-dir> <name> [level2-file]
//! ```
//!
//! Everything else is an environment variable, so a sweep is a shell loop.
//! `VOL3D_PROOF_RAMP=off` flattens the reflectivity opacity ramp to a constant,
//! which is the renderer's behaviour before that ramp existed, so an A/B pair
//! isolates the ramp and nothing else. `VOL3D_PROOF_FLOOR=off` drops the
//! low-tilt ground sheet, which is a 2D underlay rather than part of the
//! volume and otherwise supplies most of a frame's alpha - dropping it is what
//! makes the number below a measurement OF THE VOLUME.
//! `VOL3D_PROOF_THRESHOLD`, `VOL3D_PROOF_OPACITY` and `VOL3D_PROOF_DIST` move
//! the display threshold in dBZ, the opacity slider, and the camera.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use eframe::egui;
use eframe::egui_wgpu::{
    CallbackTrait, RenderState, RendererOptions, ScreenDescriptor, WgpuConfiguration, wgpu,
};
use radar_core::MomentType;
use render2d::volumetric::InterpPolicy;

// The whole explorer, compiled into this example exactly as the binary compiles
// it. The inline `#[path]` wrapper is what makes `vol3d`'s own child modules
// resolve inside `src/vol3d/`: a `#[path]` module file behaves like a `mod.rs`,
// so pointing straight at `../src/vol3d.rs` would look for `pane.rs` beside it
// in `src/`. `dead_code` is allowed because an example drives a slice of the
// pane and clippy runs this workspace with `-D warnings`.
#[allow(dead_code)]
#[path = "../src"]
mod source {
    pub mod vol3d;
}

use source::vol3d;
use vol3d::advanced;
use vol3d::box_frame::{BOX_HALF_KM, auto_box_center_km};
use vol3d::{
    BOX_N, BOX_NZ, BOX_TOP_M, Vol3d, Vol3dCallback, VolumeBox, lowest_moment_floor,
    normalize_box_with_range,
};

/// 768 * 4 bytes per row is a multiple of the 256-byte alignment
/// `copy_texture_to_buffer` requires, so the readback needs no padding.
const SIDE: u32 = 768;
/// `product_engine`'s declared reflectivity range: what the box is normalised
/// against, and therefore what the shader's 0..1 domain means in dBZ.
const RANGE: (f32, f32) = (-32.0, 94.5);
/// The pane's own dark ground, used to composite the straight-alpha frame into
/// something a human can look at.
const GROUND: [f32; 3] = [0.043, 0.055, 0.078];

fn number(name: &str) -> Option<f32> {
    std::env::var(name).ok().and_then(|raw| raw.parse().ok())
}

fn switched_off(name: &str) -> bool {
    std::env::var(name).is_ok_and(|raw| raw == "off")
}

fn main() {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    let [out_dir, name, rest @ ..] = arguments.as_slice() else {
        eprintln!("usage: vol3d_opacity_proof <out-dir> <name> [level2-file]");
        std::process::exit(2);
    };
    let level2 = rest
        .first()
        .filter(|value| value.as_str() != "-")
        .map(PathBuf::from)
        .or_else(largest_cached_volume);
    let Some(level2) = level2 else {
        eprintln!("no Level II volume given and none cached");
        std::process::exit(1);
    };

    if let Err(message) = run(Path::new(out_dir), name, &level2) {
        eprintln!("{message}");
        std::process::exit(1);
    }
}

fn run(out_dir: &Path, name: &str, level2: &Path) -> Result<(), String> {
    std::fs::create_dir_all(out_dir).map_err(|error| format!("{}: {error}", out_dir.display()))?;
    let (volume_box, label) = build_box(level2)?;
    println!("volume  {}", level2.display());
    println!("box     {label}");

    let mut vol3d = Vol3d::default();
    if let Some(threshold) = number("VOL3D_PROOF_THRESHOLD") {
        vol3d.threshold_dbz = threshold;
    }
    if let Some(opacity) = number("VOL3D_PROOF_OPACITY") {
        vol3d.opacity = opacity;
    }
    if let Some(dist) = number("VOL3D_PROOF_DIST") {
        vol3d.dist = dist;
    }
    if let Some(density) = number("VOL3D_PROOF_DENSITY") {
        vol3d.density = density;
    }
    if let Some(shading) = number("VOL3D_PROOF_SHADING") {
        vol3d.shading = shading;
    }
    // Quality is the operator's step-count control - 96, 160 or 240 samples
    // across the box - so an A/B across it is the step-length invariance the
    // optical-depth form claims, measured on the GPU rather than argued.
    if let Ok(raw) = std::env::var("VOL3D_PROOF_QUALITY") {
        vol3d.quality = match raw.as_str() {
            "draft" => vol3d::Vol3dQuality::Draft,
            "high" => vol3d::Vol3dQuality::High,
            _ => vol3d::Vol3dQuality::Balanced,
        };
    }
    if let Ok(raw) = std::env::var("VOL3D_PROOF_MODE") {
        vol3d.advanced.render_mode = advanced::Vol3dRenderMode::ALL
            .into_iter()
            .find(|mode| mode.label().to_lowercase().starts_with(&raw))
            .unwrap_or(advanced::Vol3dRenderMode::DirectVolume);
    }
    if let Ok(raw) = std::env::var("VOL3D_PROOF_SUPPORT") {
        vol3d.advanced.support_mode = match raw.as_str() {
            "full" => advanced::SupportMode::FullReconstruction,
            "inspect" => advanced::SupportMode::Inspect,
            _ => advanced::SupportMode::HonestFade,
        };
    }
    // `Below` is the mode that makes no-data dangerous: an unobserved voxel
    // stores 0, which reads as a legitimate low value and would paint the whole
    // empty box unless `support_value` gates it first.
    if let Ok(raw) = std::env::var("VOL3D_PROOF_THRESHOLD_MODE") {
        vol3d.threshold_mode = match raw.as_str() {
            "below" => vol3d::Vol3dThresholdMode::Below,
            "outside" => vol3d::Vol3dThresholdMode::Outside,
            _ => vol3d::Vol3dThresholdMode::Above,
        };
    }
    if switched_off("VOL3D_PROOF_PREINTEGRATION") {
        vol3d.advanced.preintegration = false;
    }
    if switched_off("VOL3D_PROOF_JITTER") {
        vol3d.advanced.jitter_strength = 0.0;
    }
    if switched_off("VOL3D_PROOF_ADAPTIVE") {
        vol3d.advanced.adaptive_strength = 0.0;
    }
    let floor_on = !switched_off("VOL3D_PROOF_FLOOR");
    if !floor_on {
        vol3d.floor_mode = vol3d::FloorMode::Off;
    }
    let ramp_on = !switched_off("VOL3D_PROOF_RAMP");
    if !ramp_on {
        // A constant ramp: the exact behaviour before the reflectivity opacity
        // ramp existed, reached through the shipped uniform rather than through
        // a second copy of the shader.
        vol3d.advanced.opacity_ramp_gain = 1.0;
        vol3d.advanced.opacity_ramp_floor = 1.0;
    }
    if let Some(low) = number("VOL3D_PROOF_RAMP_LOW") {
        vol3d.advanced.opacity_ramp_low_dbz = low;
    }
    if let Some(high) = number("VOL3D_PROOF_RAMP_HIGH") {
        vol3d.advanced.opacity_ramp_high_dbz = high;
    }
    if let Some(gamma) = number("VOL3D_PROOF_RAMP_GAMMA") {
        vol3d.advanced.opacity_ramp_gamma = gamma;
    }
    if let Some(value) = number("VOL3D_PROOF_RAMP_FLOOR") {
        vol3d.advanced.opacity_ramp_floor = value;
    }
    if let Some(gain) = number("VOL3D_PROOF_RAMP_GAIN") {
        vol3d.advanced.opacity_ramp_gain = gain;
    }
    println!(
        "params  threshold {:.0} dBZ  opacity {:.2}  density {:.2}  {} steps  {}  {}  \
         floor {}  ramp {}",
        vol3d.threshold_dbz,
        vol3d.opacity,
        vol3d.density,
        vol3d.quality.steps(),
        vol3d.advanced.render_mode.label(),
        vol3d.advanced.support_mode.label(),
        if floor_on { "on" } else { "off" },
        if ramp_on { "on" } else { "off (constant)" }
    );
    print_ramp_table(&vol3d.advanced);

    let harness = Harness::new().ok_or_else(|| "no wgpu adapter".to_owned())?;
    let lut = palette();
    let threshold01 = (vol3d.threshold_dbz - RANGE.0) / (RANGE.1 - RANGE.0);
    let table = advanced::build_preintegrated_lut(&lut, threshold01, -1.0, 0.0, vol3d.opacity);
    vol3d.lut_rgba = lut.clone();
    if let Ok(mut pending) = vol3d.pending.lock() {
        pending.volume = Some(volume_box);
        pending.lut = Some(lut);
        pending.preintegrated = Some(table);
    }
    // STRUCTURE range on both arguments, exactly as the pane passes it: in
    // two-box mode `t_volume` is still reflectivity even though the palette is
    // m/s, so the ramp knees normalise against dBZ and not against velocity.
    let two_box = std::env::var("VOL3D_PROOF_TWOBOX").is_ok();
    let uniforms = vol3d.advanced.shader_uniforms(RANGE.0, RANGE.1, two_box);
    let pixels = harness.frame(&callback_for(&vol3d, uniforms));
    report(name, &pixels);
    let path = save(out_dir, &pixels, &format!("{name}.png"))?;
    println!("wrote   {}", path.display());

    // The A/B in one process and one GPU state, so the pair cannot differ by
    // anything except the ramp: the flat ramp is what the renderer did before
    // the ramp existed.
    if let Ok(raw) = std::env::var("VOL3D_PROOF_AB") {
        let mut flat = vol3d.advanced;
        // A number is a comparison GAIN, which is the monotonicity probe: more
        // extinction everywhere must never make a pixel less opaque, and a
        // compositing path that lets alpha leave 0..1 fails exactly that.
        // Anything else is the flat ramp, the renderer before this existed.
        if let Ok(gain) = raw.parse::<f32>() {
            flat.opacity_ramp_gain = gain;
        } else {
            flat.opacity_ramp_floor = 1.0;
            flat.opacity_ramp_gain = 1.0;
        }
        let flat_pixels = harness.frame(&callback_for(
            &vol3d,
            flat.shader_uniforms(RANGE.0, RANGE.1, two_box),
        ));
        report("flat", &flat_pixels);
        difference(&flat_pixels, &pixels);
        let path = save(out_dir, &flat_pixels, &format!("{name}-flat.png"))?;
        println!("wrote   {}", path.display());
    }
    Ok(())
}

/// How far the after moved from the before, per pixel, in opacity points.
///
/// A mean that barely moves can hide a picture that changed everywhere, and a
/// picture that did not change at all can hide behind a mean that moved: this
/// reports the signed distribution, which cannot do either.
fn difference(before: &[u8], after: &[u8]) {
    let mut gained = 0_u32;
    let mut lost = 0_u32;
    let mut total = 0.0_f64;
    let mut worst = 0.0_f32;
    let mut solid_before = 0_u32;
    let mut solid_after = 0_u32;
    for (a, b) in before.chunks_exact(4).zip(after.chunks_exact(4)) {
        let (x, y) = (f32::from(a[3]) / 255.0, f32::from(b[3]) / 255.0);
        let delta = y - x;
        if delta > 0.01 {
            gained += 1;
        }
        if delta < -0.01 {
            lost += 1;
        }
        if delta.abs() > worst.abs() {
            worst = delta;
        }
        total += f64::from(delta);
        solid_before += u32::from(x > 0.9);
        solid_after += u32::from(y > 0.9);
    }
    let pixels = f64::from(SIDE) * f64::from(SIDE);
    println!(
        "  change: mean {:+.4}  gained {:5.2}%  lost {:5.2}%  worst {worst:+.3}  \
         solid {:5.2}% -> {:5.2}%",
        total / pixels,
        f64::from(gained) * 100.0 / pixels,
        f64::from(lost) * 100.0 / pixels,
        f64::from(solid_before) * 100.0 / pixels,
        f64::from(solid_after) * 100.0 / pixels,
    );
}

/// The measured shape of the transfer function, in the units an operator reads.
fn print_ramp_table(params: &advanced::AdvancedParams) {
    println!(
        "ramp    {:.0}..{:.0} dBZ, gamma {:.2}, floor {:.3}, gain {:.2}",
        params.opacity_ramp_low_dbz,
        params.opacity_ramp_high_dbz,
        params.opacity_ramp_gamma,
        params.opacity_ramp_floor,
        params.opacity_ramp_gain
    );
    const SAMPLES: [f32; 10] = [10.0, 20.0, 30.0, 35.0, 40.0, 45.0, 50.0, 55.0, 60.0, 65.0];
    print!("dBZ     ");
    for dbz in SAMPLES {
        print!("{dbz:>7.0}");
    }
    println!();
    print!("k       ");
    for dbz in SAMPLES {
        print!(
            "{:>7.3}",
            params.extinction_multiplier(dbz, RANGE.0, RANGE.1)
        );
    }
    println!();
}

/// The pane's own box build: resample the tilt stack, normalise against the
/// declared range, build the conservative hierarchy from the structure plane,
/// and raster the low-tilt floor.
fn build_box(path: &Path) -> Result<(VolumeBox, String), String> {
    let volume = nexrad_io::decode_volume_from_path(path)
        .map_err(|error| format!("could not decode {}: {error}", path.display()))?;
    let moment = MomentType::Reflectivity;
    // The velocity two-box path, which the pane cannot reach today because it
    // hard-codes `velocity_mode: 0.0`. Driving it here is the only way to
    // photograph it: `t_volume` keeps the REFLECTIVITY structure and `t_color`
    // carries the second moment, so the opacity ramp has to read the first and
    // the palette the second. `VOL3D_PROOF_TWOBOX=ref` puts reflectivity in
    // BOTH boxes, which is the discriminator - if the ramp really reads the
    // structure plane, its effect on alpha must be identical between the two.
    let colour_moment = match std::env::var("VOL3D_PROOF_TWOBOX").as_deref() {
        Ok("on") => Some((MomentType::Velocity, (-100.0_f32, 100.0_f32))),
        Ok("ref") => Some((MomentType::Reflectivity, RANGE)),
        _ => None,
    };
    let (east_km, north_km) = auto_box_center_km(&volume).unwrap_or((0.0, 0.0));
    let resampled = render2d::volumetric::volume_box_resample_moment_with_support(
        &volume,
        &moment,
        InterpPolicy::LinearAngle,
        east_km,
        north_km,
        BOX_HALF_KM,
        BOX_N,
        BOX_NZ,
        BOX_TOP_M,
    )
    .ok_or_else(|| format!("no reflectivity volume in {}", path.display()))?;
    let mut volume_box =
        normalize_box_with_range(&resampled.values, BOX_N, BOX_NZ, RANGE.0, RANGE.1);
    if let Some((colour_moment, colour_range)) = colour_moment {
        let coloured = render2d::volumetric::volume_box_resample_moment_with_support(
            &volume,
            &colour_moment,
            InterpPolicy::LinearAngle,
            east_km,
            north_km,
            BOX_HALF_KM,
            BOX_N,
            BOX_NZ,
            BOX_TOP_M,
        )
        .ok_or_else(|| format!("no {colour_moment:?} volume in {}", path.display()))?;
        volume_box.color_data = Some(vol3d::normalize_values(
            &coloured.values,
            colour_range.0,
            colour_range.1,
        ));
    }
    volume_box.acceleration = Some(advanced::build_box_acceleration(
        &volume_box.data,
        &resampled.support,
    ));
    if let Some((floor_data, elevation_deg)) = lowest_moment_floor(
        &volume,
        &moment,
        east_km,
        north_km,
        BOX_HALF_KM,
        RANGE.0,
        RANGE.1,
    ) {
        volume_box.floor_data = Some(floor_data);
        volume_box.floor_elevation_deg = Some(elevation_deg);
    }
    let finite = || resampled.values.iter().copied().filter(|v| v.is_finite());
    let peak = finite().fold(f32::NEG_INFINITY, f32::max);
    let over_35 = finite().filter(|value| *value >= 35.0).count();
    let over_50 = finite().filter(|value| *value >= 50.0).count();
    let label = format!(
        "{} {} centred {east_km:+.1} km E {north_km:+.1} km N, peak {peak:.1} dBZ, \
         {over_35} voxels over 35 dBZ, {over_50} over 50",
        volume.site.id,
        volume.volume_time.to_rfc3339()
    );
    Ok((volume_box, label))
}

/// The largest cached live volume: a large file is a full VCP over a real
/// storm, which is what a volume renderer has to be judged on.
fn largest_cached_volume() -> Option<PathBuf> {
    let cache = PathBuf::from(std::env::var_os("LOCALAPPDATA")?)
        .join("FahrenheitResearch")
        .join("RadarWorkstation")
        .join("cache")
        .join("level2-live");
    let mut best: Option<(u64, PathBuf)> = None;
    for entry in std::fs::read_dir(cache).ok()?.flatten() {
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        if best.as_ref().is_none_or(|(size, _)| metadata.len() > *size) {
            best = Some((metadata.len(), entry.path()));
        }
    }
    best.map(|(_, path)| path)
}

/// The reflectivity palette exactly as `pane::update_lut` builds it.
fn palette() -> Vec<u8> {
    let tables = color_tables::ColorTableSet::default();
    let table = tables.for_family(color_tables::ColorTableFamily::Reflectivity);
    let mut lut = vec![0_u8; 256 * 4];
    for (index, pixel) in lut.chunks_exact_mut(4).enumerate() {
        let value = RANGE.0 + (RANGE.1 - RANGE.0) * (index as f32 / 255.0);
        pixel.copy_from_slice(&table.color_for_value(value));
    }
    lut
}

fn callback_for(
    vol3d: &Vol3d,
    uniforms: [f32; advanced::ADVANCED_UNIFORM_FLOATS],
) -> Vol3dCallback {
    let span = RANGE.1 - RANGE.0;
    Vol3dCallback {
        yaw: vol3d.yaw,
        pitch: vol3d.pitch,
        dist: vol3d.dist,
        camera_mode: vol3d.camera_mode,
        fly_x: vol3d.fly_x,
        fly_y: vol3d.fly_y,
        fly_z: vol3d.fly_z,
        threshold01: (vol3d.threshold_dbz - RANGE.0) / span,
        threshold_high01: -1.0,
        threshold_mode: vol3d.threshold_mode,
        opacity: vol3d.opacity,
        aspect: 1.0,
        floor_opacity: vol3d.floor_opacity,
        floor_mode: vol3d.floor_mode,
        zspan: vol3d.zspan(),
        fov_scale: vol3d.fov_scale,
        quality: vol3d.quality,
        density: vol3d.density,
        shading: vol3d.shading,
        lighting: vol3d.lighting,
        clip_low: 0.0,
        clip_high: 1.0,
        floor_threshold01: (vol3d.floor_threshold_dbz - RANGE.0) / span,
        floor_threshold_high01: -1.0,
        floor_threshold_mode: vol3d.floor_threshold_mode,
        focus_height: vol3d.focus_height_fraction(),
        velocity_mode: if std::env::var("VOL3D_PROOF_TWOBOX").is_ok() {
            1.0
        } else {
            0.0
        },
        ref_gate: (number("VOL3D_PROOF_REF_GATE").unwrap_or(15.0) - RANGE.0) / span,
        couplet_emphasis: number("VOL3D_PROOF_COUPLET").unwrap_or(0.0),
        advanced: uniforms,
        pending: Arc::clone(&vol3d.pending),
    }
}

/// Composite the premultiplied frame over the pane's ground and write it.
fn save(out_dir: &Path, pixels: &[u8], name: &str) -> Result<PathBuf, String> {
    let path = out_dir.join(name);
    let mut rgb = Vec::with_capacity(pixels.len() / 4 * 3);
    for texel in pixels.chunks_exact(4) {
        let alpha = f32::from(texel[3]) / 255.0;
        for (channel, ground) in GROUND.iter().enumerate() {
            let over = f32::from(texel[channel]) / 255.0 + ground * (1.0 - alpha);
            rgb.push((over.clamp(0.0, 1.0) * 255.0).round() as u8);
        }
    }
    let image = image::RgbImage::from_raw(SIDE, SIDE, rgb).ok_or("frame is not RGB")?;
    image
        .save(&path)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    Ok(path)
}

/// What the volume's opacity actually came out as, over the whole frame.
///
/// `painted` counts every pixel the volume touched at all; the quantiles say
/// how that opacity is distributed, which is the difference between a solid
/// core inside a wispy body and one uniform sheet of haze.
fn report(label: &str, pixels: &[u8]) {
    let mut alphas: Vec<f32> = pixels
        .chunks_exact(4)
        .map(|texel| f32::from(texel[3]) / 255.0)
        .filter(|alpha| *alpha > 0.02)
        .collect();
    let total = f64::from(SIDE) * f64::from(SIDE);
    if alphas.is_empty() {
        println!("{label:>10}: nothing painted");
        return;
    }
    alphas.sort_by(f32::total_cmp);
    let quantile = |fraction: f64| alphas[((alphas.len() - 1) as f64 * fraction) as usize];
    let mean = alphas.iter().sum::<f32>() / alphas.len() as f32;
    let solid = alphas.iter().filter(|alpha| **alpha > 0.9).count();
    println!(
        "{label:>10}: painted {:5.2}%  mean {mean:.3}  p10 {:.3}  p50 {:.3}  p90 {:.3}  \
         over 0.9 {:5.2}% of frame",
        alphas.len() as f64 * 100.0 / total,
        quantile(0.10),
        quantile(0.50),
        quantile(0.90),
        solid as f64 * 100.0 / total
    );
}

fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(value) = future.as_mut().poll(&mut context) {
            return value;
        }
        std::thread::yield_now();
    }
}

struct Harness {
    state: RenderState,
    target: wgpu::Texture,
    view: wgpu::TextureView,
    readback: wgpu::Buffer,
}

impl Harness {
    /// `None` when the machine has no wgpu adapter, so a headless node fails
    /// loudly instead of pretending it looked at a picture.
    fn new() -> Option<Self> {
        let config = WgpuConfiguration::default();
        let instance = block_on(config.wgpu_setup.new_instance());
        let state = block_on(RenderState::create(
            &config,
            &instance,
            None,
            RendererOptions::default(),
        ))
        .ok()?;
        println!("adapter {}", state.adapter.get_info().name);
        vol3d::init_gpu(&state);
        let target = state.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("vol3d-proof-target"),
            size: wgpu::Extent3d {
                width: SIDE,
                height: SIDE,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: state.target_format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = target.create_view(&wgpu::TextureViewDescriptor::default());
        let readback = state.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("vol3d-proof-readback"),
            size: u64::from(SIDE) * u64::from(SIDE) * 4,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        Some(Self {
            state,
            target,
            view,
            readback,
        })
    }

    fn frame(&self, callback: &Vol3dCallback) -> Vec<u8> {
        let device = &self.state.device;
        let queue = &self.state.queue;
        let screen = ScreenDescriptor {
            size_in_pixels: [SIDE, SIDE],
            pixels_per_point: 1.0,
        };
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("vol3d-proof"),
        });
        {
            let mut renderer = self.state.renderer.write();
            let extra = callback.prepare(
                device,
                queue,
                &screen,
                &mut encoder,
                &mut renderer.callback_resources,
            );
            assert!(
                extra.is_empty(),
                "the vol3d callback queued command buffers"
            );
        }
        {
            let renderer = self.state.renderer.read();
            let mut pass = encoder
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("vol3d-proof-pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &self.view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            // Transparent, so the read-back alpha IS the ray
                            // marcher's accumulated opacity.
                            load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                })
                .forget_lifetime();
            let whole = egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(SIDE as f32, SIDE as f32),
            );
            pass.set_viewport(0.0, 0.0, SIDE as f32, SIDE as f32, 0.0, 1.0);
            callback.paint(
                egui::PaintCallbackInfo {
                    viewport: whole,
                    clip_rect: whole,
                    pixels_per_point: 1.0,
                    screen_size_px: [SIDE, SIDE],
                },
                &mut pass,
                &renderer.callback_resources,
            );
        }
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &self.target,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &self.readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(SIDE * 4),
                    rows_per_image: Some(SIDE),
                },
            },
            wgpu::Extent3d {
                width: SIDE,
                height: SIDE,
                depth_or_array_layers: 1,
            },
        );
        queue.submit([encoder.finish()]);
        let slice = self.readback.slice(..);
        let (sender, receiver) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = sender.send(result);
        });
        self.state
            .device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("device poll");
        receiver.recv().expect("map callback").expect("map read");
        let pixels = slice.get_mapped_range().to_vec();
        self.readback.unmap();
        pixels
    }
}
