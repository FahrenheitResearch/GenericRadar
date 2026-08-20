//! Photograph a real volume through tables the palette editor built, and check
//! the three claims that cannot be checked on a gradient.
//!
//! A colour table is only judged against echo. A strip shows that the ramp is
//! smooth; it does not show that a unit conversion left every gate where it
//! was, that a ramp pair paints the two colours it names, or that the file on
//! disk is the picture the editor previewed. Those are properties of a
//! *rendered scan*, so this renders one.
//!
//! ```text
//! cargo run --release -p workstation_app --example palette_editor_proof -- <level2-file> <out-dir>
//! ```
//!
//! What it asserts, on the volume it is given:
//!
//! 1. **Converting units repaints nothing.** The velocity table is taken from
//!    metres per second through knots and miles per hour and back; every
//!    conversion preserves the engine value of every stop, so the rendered
//!    PNGs must be byte-identical. If a conversion factor is wrong, this is
//!    where it shows up - as a picture, not as a rounding error in a log.
//! 2. **Reinterpreting with `Scale:` does change the picture**, which is the
//!    other half of that distinction. A scale of 2 halves every threshold, so
//!    the echo must come out visibly different. A test that only proved (1)
//!    would also pass if both controls did nothing.
//! 3. **The saved file is the preview.** The edited table is written as a
//!    `.pal`, read back off disk, and rendered again; the two PNGs must be
//!    byte-identical, ramp pairs included.
//! 4. **A shared GR palette survives being opened and saved.** A `.pal` in the
//!    dialect people actually trade - no `Name:` row, both spellings of the
//!    two-colour ramp row - is rendered straight through `ColorTable::parse`,
//!    then through the editor, then through the editor's own re-save. All
//!    three must be the same scan. The editor's internal round trip cannot
//!    catch this on its own: it compares the editor's text against itself and
//!    never against the file that was read.
//! 5. **An awkward name is still a name.** A palette named with a leading and
//!    trailing space and a pasted no-break space in the middle saves, comes
//!    back trimmed, and paints the scan it painted before.
//!
//! It then photographs the editor **window itself**, on the same volume, in
//! both theme variants at both device scales, so that the new window is
//! reviewed the way the rest of the chrome is - by looking at it rather than
//! by trusting that it names the right colour roles. That part needs a GPU and
//! says so and skips if there is none; the three claims above do not.
//!
//! The images are written out either way, so the run is a set of pictures to
//! look at and not only a pass/fail.

// The whole application, exactly as `src/main.rs` compiles it - the same
// construction `theme_gallery` uses, and for the same reason: the editor's
// modules reach `crate::theme` and `crate::app_support`, so the example has to
// present the same crate root the binary does.
#[allow(dead_code)]
#[path = "../src"]
mod source {
    pub mod app;
    pub mod app_support;
    pub mod hazards;
    pub mod legend;
    pub mod live_service;
    pub mod load_service;
    pub mod nearest_site;
    pub mod palette_editor;
    pub mod palettes;
    pub mod pane_canvas;
    pub mod popup;
    pub mod probe;
    pub mod product;
    pub mod product_availability;
    pub mod product_picker;
    pub mod render_service;
    pub mod settings_ui;
    pub mod sites_service;
    pub mod sweep;
    pub mod theme;
    pub mod user_tables;
    pub mod vol3d;
    pub mod vrot;
    pub mod warnings_service;
    pub mod xsection;
}

// Re-exported whole so that every `crate::…` path inside the included
// modules resolves; this example itself only reaches for a few of them.
#[allow(unused_imports)]
pub(crate) use source::{
    app, app_support, hazards, legend, live_service, load_service, nearest_site, palette_editor,
    palettes, pane_canvas, popup, probe, product, product_availability, product_picker,
    render_service, settings_ui, sites_service, sweep, theme, user_tables, vol3d, vrot,
    warnings_service, xsection,
};

use std::path::{Path, PathBuf};

use color_tables::{ColorTableFamily, Rgba8};
use eframe::egui;
use eframe::egui_wgpu::{Renderer, RendererOptions, ScreenDescriptor};
use eframe::wgpu;
use palette_editor::model::{EditorTable, EditorUnits};
use palette_editor::store;
use palette_editor::ui::{PaletteEditorInput, PaletteEditorState, draw_palette_editor};
use radar_core::{MomentType, RadarVolume};
use render2d::RasterOptions;
use theme::Variant;

/// Big enough that a band edge is a band edge and not a rounding artefact.
const RASTER: RasterOptions = RasterOptions {
    width: 900,
    height: 900,
    range_fraction: 94,
};

fn main() {
    let mut args = std::env::args_os().skip(1).map(PathBuf::from);
    let (Some(input), Some(out_dir)) = (args.next(), args.next()) else {
        eprintln!(
            "usage: cargo run --release -p workstation_app --example palette_editor_proof \
             -- <level2-file> <out-dir>"
        );
        std::process::exit(2);
    };
    if let Err(error) = run(&input, &out_dir) {
        eprintln!("palette editor proof failed: {error}");
        std::process::exit(1);
    }
}

fn run(input: &Path, out_dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    std::fs::create_dir_all(out_dir)?;
    let volume = nexrad_io::decode_volume_from_path(input)?;
    println!(
        "{} · {} cuts · {}",
        input.display(),
        volume.cuts.len(),
        volume.site.id
    );

    reflectivity_proof(&volume, out_dir)?;
    velocity_proof(&volume, out_dir)?;
    shared_palette_proof(&volume, out_dir)?;
    awkward_name_proof(&volume, out_dir)?;
    photograph_window(&volume, out_dir);
    Ok(())
}

/// A palette shared in the GR dialect, opened and saved, paints the same scan.
///
/// The two readers used to disagree about how many components a ramp-pair end
/// colour has - the editor sized it from the row key, the shipped parser from
/// what is left on the line - so a `Color4:` row with a three-component end
/// lost its ramp and a `Color:` row with a four-component end had its ramp
/// target forced opaque. On a gradient strip that is a slightly different
/// shade; on echo it is the difference between a core that fades and a core
/// that steps. Both forms are on the fixture, so both are on the picture.
fn shared_palette_proof(
    volume: &RadarVolume,
    out_dir: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    // Written the way a shared GR palette is written: no `Name:` row, no
    // `Mode:` row, both two-colour forms, one flat row.
    let shared = "\
Product: BR
Color4: 5 0 0 0 0 30 80 160
Color: 25 30 180 60 240 230 40 180
Color4: 45 255 0 220 255 255 255 255 255
Color4: 70 255 255 255 255
";
    let path = out_dir.join("shared-gr.pal");
    std::fs::write(&path, shared)?;
    let opened = store::load(&path)?;

    // What the renderer would paint from the file itself, with no editor in
    // the way: the reference every reading of it has to match.
    let reference_table = color_tables::ColorTable::parse("shared-gr", shared)?;
    let cut = first_cut(volume, &MomentType::Reflectivity)?;
    let reference = render2d::render_moment_image_with_table(
        volume,
        cut,
        MomentType::Reflectivity,
        RASTER,
        Some(&reference_table),
    )?;
    reference.save(out_dir.join("shared_gr_reference.png"))?;
    let reference = reference.into_raw();

    let opened_pixels = render(
        volume,
        &MomentType::Reflectivity,
        &opened,
        out_dir,
        "shared_gr_opened",
    )?;
    if opened_pixels != reference {
        return Err(format!(
            "opening the shared palette repaints {} pixels against the file itself",
            differing(&reference, &opened_pixels)
        )
        .into());
    }

    // And saving it back is not a repaint either - which is the half a
    // round-trip check inside the editor cannot see, because it compares the
    // editor's own text against itself.
    store::save(&opened, &path)?;
    let resaved = render(
        volume,
        &MomentType::Reflectivity,
        &store::load(&path)?,
        out_dir,
        "shared_gr_resaved",
    )?;
    if resaved != reference {
        return Err(format!(
            "saving the shared palette repaints {} pixels",
            differing(&reference, &resaved)
        )
        .into());
    }
    println!("  shared GR palette: opened and re-saved, pixel-identical to the file itself");
    Ok(())
}

/// A name is not typed clean, and a table whose name has a space around it
/// still saves and still paints.
///
/// A trailing space used to make Save fail outright - and fail blaming the
/// colours - because the writer trimmed the `Name:` row while the table handed
/// to the parser kept the untrimmed name. The editor became read-only until
/// the space was found and deleted.
fn awkward_name_proof(
    volume: &RadarVolume,
    out_dir: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let installed = color_tables::builtin_reflectivity_table();
    let mut table = EditorTable::from_color_table(ColorTableFamily::Reflectivity, &installed);
    let clean = render(
        volume,
        &MomentType::Reflectivity,
        &table,
        out_dir,
        "name_clean",
    )?;

    // A trailing space, and a no-break space where a paste would put one.
    table.name = " Storm\u{a0}Detail v2 ".to_owned();
    let path = out_dir.join("awkward-name.pal");
    store::save(&table, &path)?;
    let reloaded = store::load(&path)?;
    if reloaded.name != "Storm Detail v2" {
        return Err(format!("the saved name came back as {:?}", reloaded.name).into());
    }
    let after = render(
        volume,
        &MomentType::Reflectivity,
        &reloaded,
        out_dir,
        "name_awkward",
    )?;
    if after != clean {
        return Err(format!(
            "a name with a space around it repainted {} pixels",
            differing(&clean, &after)
        )
        .into());
    }
    println!("  a name typed with spaces around it saves, reloads trimmed, and paints the same");
    Ok(())
}

fn first_cut(
    volume: &RadarVolume,
    moment: &MomentType,
) -> Result<usize, Box<dyn std::error::Error>> {
    volume
        .cuts
        .iter()
        .position(|cut| cut.moments.contains_key(moment))
        .ok_or_else(|| format!("this volume carries no {moment} sweep").into())
}

/// Reflectivity: an edit an analyst would actually make, then the file.
fn reflectivity_proof(
    volume: &RadarVolume,
    out_dir: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let installed = color_tables::builtin_reflectivity_table();
    let mut table = EditorTable::from_color_table(ColorTableFamily::Reflectivity, &installed);
    // Under the name the editor itself would give this, not the preset's. Edit
    // on a shipped row opens a COPY - "AWIPS Wilson REF copy" - because the
    // shipped palette wins its own name everywhere: the restore path searches
    // the catalogue before the analyst's directory, and the picker row for
    // that name offers Edit on the preset rather than on the analyst's file.
    // A save under the preset's own name is refused for exactly that reason,
    // so the bench edits what an analyst would really have on screen.
    table.name = format!("{} copy", table.name);
    let before = render(
        volume,
        &MomentType::Reflectivity,
        &table,
        out_dir,
        "ref_before",
    )?;

    // Three edits, of the three kinds the editor offers.
    //
    // A colour: the stop nearest 50 dBZ - the top of heavy rain and the bottom
    // of the hail signature - goes hard magenta, which is a change nobody can
    // miss in a core.
    let core = nearest_stop(&table, 50.0);
    if let Some(stop) = table.stop_mut(core) {
        stop.color = Rgba8::opaque(255, 0, 220);
    }
    // A ramp pair on the stop below it, so the approach to that core is a
    // gradient inside one row rather than a step at its edge.
    let approach = nearest_stop(&table, 40.0);
    if let Some(stop) = table.stop_mut(approach) {
        stop.ramp_end = Some(Rgba8::opaque(20, 20, 20));
    }
    // And a stop inserted midway, which must not repaint anything by itself.
    table.insert_after(approach);
    let after = render(
        volume,
        &MomentType::Reflectivity,
        &table,
        out_dir,
        "ref_after",
    )?;
    println!(
        "  reflectivity: {} pixels changed by the edit ({:.2}% of the scan)",
        differing(&before, &after),
        100.0 * differing(&before, &after) as f32 / (before.len() / 4) as f32
    );
    if before == after {
        return Err("editing a stop's colour changed nothing on real echo".into());
    }

    // Claim 3: the file is the preview.
    let path = out_dir.join("edited-reflectivity.pal");
    store::save(&table, &path)?;
    let reloaded = store::load(&path)?;
    let from_file = render(
        volume,
        &MomentType::Reflectivity,
        &reloaded,
        out_dir,
        "ref_from_saved_file",
    )?;
    if from_file != after {
        return Err(format!(
            "the saved .pal paints a different scan from the editor preview ({} pixels differ)",
            differing(&after, &from_file)
        )
        .into());
    }
    println!(
        "  {} reloads to the identical scan, ramp pair included",
        path.display()
    );
    Ok(())
}

/// Velocity: the units-versus-scale distinction, as two pictures.
fn velocity_proof(volume: &RadarVolume, out_dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let installed = color_tables::builtin_velocity_table();
    let mut table = EditorTable::from_color_table(ColorTableFamily::Velocity, &installed);
    let before = render(volume, &MomentType::Velocity, &table, out_dir, "vel_before")?;

    // Claim 1: a conversion preserves the physical meaning of every colour, so
    // it must not move one pixel. Through knots and back, because a factor
    // applied once in the wrong direction still round-trips.
    table.set_units(EditorUnits::Knots);
    let in_knots = render(volume, &MomentType::Velocity, &table, out_dir, "vel_knots")?;
    if in_knots != before {
        return Err(format!(
            "converting m/s to kt repainted {} pixels; it must repaint none",
            differing(&before, &in_knots)
        )
        .into());
    }
    table.set_units(EditorUnits::MilesPerHour);
    table.set_units(EditorUnits::MetresPerSecond);
    let back = render(volume, &MomentType::Velocity, &table, out_dir, "vel_back")?;
    if back != before {
        return Err(format!(
            "m/s -> kt -> mph -> m/s repainted {} pixels",
            differing(&before, &back)
        )
        .into());
    }
    println!("  velocity: m/s -> kt -> mph -> m/s is pixel-identical on real echo");

    // Claim 2: a scale reinterprets, and reinterpreting is visible.
    table.set_scale(Some(2.0));
    let scaled = render(volume, &MomentType::Velocity, &table, out_dir, "vel_scale2")?;
    if scaled == back {
        return Err("a scale of 2 changed nothing; the two controls are not distinct".into());
    }
    println!(
        "  velocity: Scale 2 repaints {} pixels ({:.1}% of the scan) - the numbers stayed, \
         their meaning halved",
        differing(&back, &scaled),
        100.0 * differing(&back, &scaled) as f32 / (back.len() / 4) as f32
    );
    Ok(())
}

/// The stop whose value is closest to `target`, in display units.
fn nearest_stop(table: &EditorTable, target: f32) -> palette_editor::model::StopId {
    table
        .stops()
        .iter()
        .min_by(|left, right| {
            (left.value - target)
                .abs()
                .total_cmp(&(right.value - target).abs())
        })
        .expect("a colour table has stops")
        .id
}

fn render(
    volume: &RadarVolume,
    moment: &MomentType,
    table: &EditorTable,
    out_dir: &Path,
    name: &str,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let built = table.to_color_table()?;
    let cut = volume
        .cuts
        .iter()
        .position(|cut| cut.moments.contains_key(moment))
        .ok_or_else(|| format!("this volume carries no {moment} sweep"))?;
    let image = render2d::render_moment_image_with_table(
        volume,
        cut,
        moment.clone(),
        RASTER,
        Some(&built),
    )?;
    let path = out_dir.join(format!("{name}.png"));
    image.save(&path)?;
    println!("    wrote {} (cut {cut}, {moment})", path.display());
    Ok(image.into_raw())
}

fn differing(left: &[u8], right: &[u8]) -> usize {
    left.chunks_exact(4)
        .zip(right.chunks_exact(4))
        .filter(|(left, right)| left != right)
        .count()
}

// ---------------------------------------------------------------------------
// The window, in both theme variants, on the same volume.
// ---------------------------------------------------------------------------

/// Window size for the photographs, in points. Wide enough that the stop list
/// and the preview column both get their real width rather than the narrow
/// fallback, which is what a reviewer needs to see.
///
/// 1024 and not a rounder 1000 because the readback below copies whole rows:
/// wgpu requires `bytes_per_row` to be a multiple of 256, so the pixel width
/// has to be a multiple of 64 at every scale this photographs at.
const SHOT_POINTS: egui::Vec2 = egui::vec2(1024.0, 820.0);

/// The pixel format the offscreen target uses. `Rgba8Unorm` rather than
/// `Rgba8UnormSrgb` because egui writes gamma-space bytes into it, so a
/// read-back triple IS the `Color32` egui asked for - the same choice, for the
/// same reason, as `examples/theme_gallery.rs`.
const TARGET_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

/// Photograph the editor in both variants at 1x and 2x.
///
/// Skips rather than fails without a GPU: the three numeric claims above are
/// the part that must hold on every machine, and a headless build node that
/// cannot rasterise should still be able to run them.
fn photograph_window(volume: &RadarVolume, out_dir: &Path) {
    let instance = wgpu::Instance::default();
    let Ok(adapter) =
        pollster_block(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
    else {
        println!(
            "  SKIPPED the window photographs: no wgpu adapter on this machine. \
             The three claims above did run."
        );
        return;
    };
    println!("  adapter: {:?}", adapter.get_info());
    let Ok((device, queue)) = pollster_block(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("palette editor shots"),
        ..Default::default()
    })) else {
        println!("  SKIPPED the window photographs: no wgpu device.");
        return;
    };

    for (variant, name) in [(Variant::Dark, "dark"), (Variant::Light, "light")] {
        for scale in [1.0_f32, 2.0] {
            let (width, height, pixels) = render_window(&device, &queue, volume, variant, scale);
            let file = out_dir.join(format!("editor_{name}_{scale}x.png"));
            image::RgbaImage::from_raw(width, height, pixels)
                .expect("readback size matches the target")
                .save(&file)
                .expect("write PNG");
            println!("    wrote {}", file.display());
        }
    }
}

/// One full pass of the real editor window, rasterised offscreen.
fn render_window(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    volume: &RadarVolume,
    variant: Variant,
    scale: f32,
) -> (u32, u32, Vec<u8>) {
    let width_px = (SHOT_POINTS.x * scale) as u32;
    let height_px = (SHOT_POINTS.y * scale) as u32;

    let context = egui::Context::default();
    theme::apply(&context, variant);
    context.set_pixels_per_point(scale);
    let mut state = PaletteEditorState::default();
    state.edit_or_duplicate(
        ColorTableFamily::Reflectivity,
        &color_tables::builtin_reflectivity_table(),
        // A shipped preset, so the window opens on a copy - which is the state
        // worth photographing, since it is the one every Edit on a built-in
        // row produces.
        true,
    );
    // The AWIPS table is ramp pairs the whole way down, so one row has its
    // second colour cleared before the shutter. The photograph then shows both
    // states of that column - a set pair and the placeholder that says there
    // is none - which is what a reviewer needs to tell them apart at a glance.
    if let Some(table) = state.table_mut()
        && let Some(id) = table.stops().get(6).map(|stop| stop.id)
        && let Some(stop) = table.stop_mut(id)
    {
        stop.ramp_end = None;
    }

    // Four passes: the scale lands on the first, the auto-sized window settles
    // over the next two, and the preview's texture is uploaded in the delta of
    // whichever pass first built it. The deltas are accumulated across all of
    // them - dropping the early ones leaves every mesh sampling a texture that
    // was never uploaded and the frame comes back black.
    let mut textures = eframe::epaint::textures::TexturesDelta::default();
    let mut last = None;
    for _ in 0..4 {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, SHOT_POINTS)),
            ..Default::default()
        };
        let mut output = context.run_ui(input, |ui| {
            theme::paint_root_ground(ui);
            draw_palette_editor(
                ui.ctx(),
                PaletteEditorInput {
                    state: &mut state,
                    volume: Some(volume),
                },
            );
        });
        textures.append(std::mem::take(&mut output.textures_delta));
        last = Some(output);
    }
    let output = last.expect("at least one pass ran");
    let clipped = context.tessellate(output.shapes, output.pixels_per_point);
    assert!(
        !clipped.is_empty(),
        "the editor tessellated nothing; the photograph would be a lie"
    );

    let mut renderer = Renderer::new(device, TARGET_FORMAT, RendererOptions::PREDICTABLE);
    for (id, delta) in &textures.set {
        renderer.update_texture(device, queue, *id, delta);
    }
    let pixels = rasterise(
        device,
        queue,
        &mut renderer,
        &clipped,
        width_px,
        height_px,
        scale,
    );
    for id in &textures.free {
        renderer.free_texture(id);
    }
    (width_px, height_px, pixels)
}

#[allow(clippy::too_many_arguments)]
fn rasterise(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    renderer: &mut Renderer,
    clipped: &[egui::ClippedPrimitive],
    width_px: u32,
    height_px: u32,
    scale: f32,
) -> Vec<u8> {
    let target = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("palette editor target"),
        size: wgpu::Extent3d {
            width: width_px,
            height: height_px,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: TARGET_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = target.create_view(&wgpu::TextureViewDescriptor::default());
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("palette editor readback"),
        size: u64::from(width_px) * u64::from(height_px) * 4,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let screen = ScreenDescriptor {
        size_in_pixels: [width_px, height_px],
        pixels_per_point: scale,
    };
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("palette editor"),
    });
    let extra = renderer.update_buffers(device, queue, &mut encoder, clipped, &screen);
    assert!(extra.is_empty(), "no paint callbacks in the editor");
    {
        let mut pass = encoder
            .begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("palette editor pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            })
            .forget_lifetime();
        renderer.render(&mut pass, clipped, &screen);
    }
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &target,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &readback,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(width_px * 4),
                rows_per_image: Some(height_px),
            },
        },
        wgpu::Extent3d {
            width: width_px,
            height: height_px,
            depth_or_array_layers: 1,
        },
    );
    queue.submit([encoder.finish()]);

    let slice = readback.slice(..);
    let (sender, receiver) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |result| {
        let _ = sender.send(result);
    });
    device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("device poll");
    receiver.recv().expect("map callback").expect("map read");
    let pixels = slice.get_mapped_range().to_vec();
    readback.unmap();
    pixels
}

/// Drive a future to completion on this thread; wgpu's native backends resolve
/// adapter and device requests without needing a waker.
fn pollster_block<F: std::future::Future>(future: F) -> F::Output {
    use std::task::{Context, Poll, Waker};
    let mut future = std::pin::pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(value) = future.as_mut().poll(&mut context) {
            return value;
        }
        std::thread::yield_now();
    }
}
