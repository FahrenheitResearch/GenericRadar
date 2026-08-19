//! Small pure helpers that `app.rs` uses and does not need to hold.
//!
//! They live here for one honest reason: `app.rs` is the composition root and an
//! architecture test caps every module in this crate at 2000 lines, so a growing
//! toolbar has to displace something. These four were the least coupled things
//! in it - none touches `WorkstationApp` - which makes them the right thing to
//! move and easy to test on their own.

use analyst_runtime::{PaneId, PaneLayout, ViewportMetrics, VolumeHistory};
use eframe::egui;
use radar_core::RadarVolume;

use crate::product::DisplayProduct;
use crate::vol3d::pane::Vol3dCandidate;

/// The basemap look picker.
///
/// `MapSceneController::set_style` had no call site at all, so the map had
/// exactly one appearance - black with slate lines - no matter what the style
/// type could express. `for_style` preselects, because the controller stores a
/// `MapStyle` rather than a preset.
///
/// The settings store is here for exactly one write: a hand-dragged Dim
/// slider. Style and provider are mirrored from the scene every frame by
/// `app.rs`, but the scrim cannot be - while auto-dim is on it is measured
/// from arriving tiles, and mirroring the measurement would silently convert
/// it into a stored manual choice.
pub(crate) fn basemap_picker(
    ui: &mut egui::Ui,
    scene: &mut map_scene::MapSceneController,
    store: &mut settings::SettingsStore,
) {
    let current = scene.style();
    // `recognised` is kept rather than collapsed into `chosen`, because the
    // write-back below has to distinguish "the operator picked something else"
    // from "this style is not in the preset table". Comparing the resolved
    // style to `current` cannot tell those apart, so an unrecognised style
    // would be silently overwritten with Slate on the next toolbar frame - the
    // picker would quietly become a style eraser the moment anything other
    // than itself sets a style.
    let recognised = map_scene::MapStylePreset::for_style(current);
    let mut chosen = recognised.unwrap_or_default();
    egui::ComboBox::from_id_salt("workstation-basemap")
        .selected_text(chosen.label())
        .width(140.0)
        .show_ui(ui, |ui| {
            for preset in map_scene::MapStylePreset::ALL {
                ui.selectable_value(&mut chosen, preset, preset.label());
            }
        })
        .response
        .on_hover_text(
            "Basemap look. Slate Dark is the shipped map; High Contrast is for a lit room or \
             a projector; Daylight is dark ink on a light pane; Minimal thins the lines and \
             holds counties back until twice the zoom.",
        );
    if recognised != Some(chosen) {
        // `set_style` bumps the style clock and drops retained geometry, so the
        // panes rebuild themselves without any extra invalidation here.
        scene.set_style(chosen.style());
    }

    // Ground imagery, which is a different axis from the vector look above:
    // this picker chooses what the boundaries are drawn ON, the combo above
    // chooses how they are drawn. "No imagery" is the shipped behaviour and
    // stays the default, so an offline or firewalled machine is never worse
    // off than it is today.
    //
    // A provider whose terms this build cannot satisfy is not listed at all,
    // rather than listed and then silently refusing to fetch.
    let available: Vec<map_scene::TileProvider> = map_scene::TileProvider::ALL
        .into_iter()
        .filter(|candidate| scene.tile_provider_permitted(*candidate))
        .collect();
    let mut provider = scene.tile_provider();
    egui::ComboBox::from_id_salt("workstation-imagery")
        .selected_text(
            provider
                .map(map_scene::TileProvider::label)
                .unwrap_or("No imagery"),
        )
        .width(170.0)
        .show_ui(ui, |ui| {
            ui.selectable_value(&mut provider, None, "No imagery");
            for candidate in available {
                ui.selectable_value(&mut provider, Some(candidate), candidate.label())
                    .on_hover_text(candidate.coverage_note());
            }
        })
        .response
        .on_hover_text(
            "Raster ground imagery drawn UNDER the radar, with the vector boundaries still \
             drawn over it. USGS layers are U.S. Government works in the public domain; \
             OpenStreetMap is community-run and its tile policy forbids prefetching, so that \
             provider fetches only what is on screen. Coverage is per tile, not per region - \
             a missing tile falls back to a coarser one rather than leaving a hole. \
             Attribution is drawn bottom right and is a condition of use: it is not optional \
             and there is no switch for it.",
        );
    if provider != scene.tile_provider() {
        scene.set_tile_provider(provider);
    }
    if scene.tile_provider().is_some() {
        let mut scrim = scene.tile_scrim();
        if ui
            .add(
                egui::Slider::new(&mut scrim, 0.0..=0.9)
                    .text("Dim")
                    .fixed_decimals(2),
            )
            .on_hover_text(
                "How far the imagery is dimmed towards the pane's own ground, so weak \
                 reflectivity and near-zero velocity stay readable on top of it. The \
                 starting value is measured from the imagery that actually arrived - a \
                 white topographic map needs far more of this than an aerial photograph \
                 does - and choosing a different provider returns it to that measurement.",
            )
            .changed()
        {
            scene.set_tile_scrim(scrim);
            // A hand-set dim is a manual dim, so automatic dimming turns off
            // - which is exactly what the settings window's pair of controls
            // means - and the choice survives the process.
            use crate::settings_ui::catalog::keys::map as k;
            store.set(
                k::CATEGORY,
                k::IMAGERY_DIM,
                settings::SettingValue::Float(f64::from(scrim)),
            );
            store.set(
                k::CATEGORY,
                k::IMAGERY_DIM_AUTO,
                settings::SettingValue::Bool(false),
            );
        }
    }
}

/// The Map menu body: basemap look, ground imagery, and the dim slider.
///
/// Menu rows rather than nested combo boxes: a combo inside a menu popup
/// closes the menu under the analyst's pointer. `for_style` preselects,
/// because the controller stores a `MapStyle` rather than a preset.
///
/// The settings store is here for exactly one write: a hand-dragged Dim
/// slider. Style and provider are mirrored from the scene every frame by
/// `app.rs`, but the scrim cannot be - while auto-dim is on it is measured
/// from arriving tiles, and mirroring the measurement would silently convert
/// it into a stored manual choice.
pub(crate) fn basemap_menu(
    ui: &mut egui::Ui,
    scene: &mut map_scene::MapSceneController,
    store: &mut settings::SettingsStore,
) {
    ui.set_min_width(240.0);
    let current = scene.style();
    // `recognised` is kept rather than collapsed into a default, because the
    // write-back below has to distinguish "the operator picked something else"
    // from "this style is not in the preset table". Without that, an
    // unrecognised style would be silently overwritten with Slate - the menu
    // would quietly become a style eraser the moment anything other than
    // itself sets a style.
    let recognised = map_scene::MapStylePreset::for_style(current);
    ui.label("Basemap");
    for preset in map_scene::MapStylePreset::ALL {
        if ui
            .selectable_label(recognised == Some(preset), preset.label())
            .clicked()
            && recognised != Some(preset)
        {
            // `set_style` bumps the style clock and drops retained geometry,
            // so the panes rebuild themselves without extra invalidation.
            scene.set_style(preset.style());
        }
    }

    ui.separator();
    // Ground imagery: what the boundaries are drawn ON. "No imagery" is the
    // shipped behaviour and the default, so an offline machine is never worse
    // off. A provider whose terms this build cannot satisfy is not listed at
    // all, rather than listed and then silently refusing to fetch.
    ui.label("Ground imagery");
    let provider = scene.tile_provider();
    if ui
        .selectable_label(provider.is_none(), "No imagery")
        .clicked()
        && provider.is_some()
    {
        scene.set_tile_provider(None);
    }
    for candidate in map_scene::TileProvider::ALL {
        if !scene.tile_provider_permitted(candidate) {
            continue;
        }
        if ui
            .selectable_label(provider == Some(candidate), candidate.label())
            .on_hover_text(candidate.coverage_note())
            .clicked()
            && provider != Some(candidate)
        {
            scene.set_tile_provider(Some(candidate));
        }
    }
    if scene.tile_provider().is_some() {
        let mut scrim = scene.tile_scrim();
        if ui
            .add(
                egui::Slider::new(&mut scrim, 0.0..=0.9)
                    .text("Dim")
                    .fixed_decimals(2),
            )
            .on_hover_text(
                "How far the imagery is dimmed towards the pane's own ground,                  so weak reflectivity and near-zero velocity stay readable on                  top of it. The starting value is measured from the imagery                  that actually arrived.",
            )
            .changed()
        {
            scene.set_tile_scrim(scrim);
            // A hand-set dim is a manual dim, so automatic dimming turns off
            // - which is exactly what the settings window's pair of controls
            // means - and the choice survives the process.
            use crate::settings_ui::catalog::keys::map as k;
            store.set(
                k::CATEGORY,
                k::IMAGERY_DIM,
                settings::SettingValue::Float(f64::from(scrim)),
            );
            store.set(
                k::CATEGORY,
                k::IMAGERY_DIM_AUTO,
                settings::SettingValue::Bool(false),
            );
        }
    }
}

/// Every volume the 3D box may be built from, oldest first.
///
/// The whole history rather than just the displayed frame: a live volume two
/// tilts into a fourteen-tilt VCP reconstructs to two disconnected shells,
/// which reads as a storm with two layers rather than as a box still filling.
/// The pane picks the most complete one and says which it used.
pub(crate) fn vol3d_candidates(history: &VolumeHistory) -> Vec<Vol3dCandidate<'_>> {
    let selected = history.selected_index();
    history
        .frames()
        .iter()
        .enumerate()
        .map(|(index, frame)| Vol3dCandidate {
            volume: &frame.volume,
            displayed: Some(index) == selected,
        })
        .collect()
}

pub(crate) fn layout_label(layout: PaneLayout) -> &'static str {
    match layout {
        PaneLayout::One => "1 pane",
        PaneLayout::TwoHorizontal => "2 horizontal",
        PaneLayout::TwoVertical => "2 vertical",
        PaneLayout::Four => "4 panes",
    }
}

pub(crate) fn pane_title(
    volume: Option<&RadarVolume>,
    pane: PaneId,
    product: DisplayProduct,
    cut_index: Option<usize>,
) -> String {
    let elevation = volume
        .zip(cut_index)
        .and_then(|(volume, index)| volume.cuts.get(index))
        .map(|cut| format!(" · {:.1}°", cut.elevation_deg))
        .unwrap_or_default();
    // The unit comes from the registry, the same place the colour table and
    // the plausibility gate read theirs, so the header cannot advertise one
    // unit while the pixels are sampled in another. Dimensionless products get
    // no parentheses rather than an empty pair.
    let unit = product.domain().display_unit.label();
    let unit = if unit.is_empty() {
        String::new()
    } else {
        format!(" ({unit})")
    };
    format!("{} · {}{}{}", pane.get() + 1, product.id(), unit, elevation)
}

pub(crate) fn viewport_changed(previous: ViewportMetrics, current: ViewportMetrics) -> bool {
    (previous.width_points - current.width_points).abs() >= 0.5
        || (previous.height_points - current.height_points).abs() >= 0.5
        || previous.pixels_per_point.to_bits() != current.pixels_per_point.to_bits()
}

pub(crate) fn color_image_from_rgba(width: u32, height: u32, rgba: &[u8]) -> egui::ColorImage {
    let expected = width as usize * height as usize * 4;
    assert_eq!(rgba.len(), expected, "invalid renderer RGBA buffer length");
    let pixels = rgba
        .chunks_exact(4)
        .map(|pixel| egui::Color32::from_rgba_unmultiplied(pixel[0], pixel[1], pixel[2], pixel[3]))
        .collect();
    egui::ColorImage::new([width as usize, height as usize], pixels)
}

/// Hover text for the warnings toggle.
///
/// The drawn count matters: the service can hold warnings the pane is not
/// showing, so "12 active" alone would let an analyst believe a polygon is on
/// screen when the layer is off or the hazard fell outside the view.
pub(crate) fn warnings_hover(detail: &str, show_warnings: bool, drawn: usize) -> String {
    if show_warnings {
        format!("{detail} · {drawn} drawn here")
    } else {
        format!("{detail} · hidden")
    }
}

/// The same cut of the frame before the selected one, for a sweep blend.
///
/// Returns `None` at the start of history, when the previous frame never
/// collected this cut, or when the cut cannot be matched by geometry - all of
/// which mean "draw this tilt on its own" rather than "error".
pub(crate) fn previous_sweep_for(
    history: &VolumeHistory,
    volume: &RadarVolume,
    cut_index: usize,
    moment: &radar_core::MomentType,
) -> Option<(std::sync::Arc<RadarVolume>, usize)> {
    let selected = history.selected_index()?;
    let previous = history.frames().get(selected.checked_sub(1)?)?;
    let target = volume.cuts.get(cut_index)?;
    let index = crate::sweep::matching_cut_index(&previous.volume, target, moment)?;
    Some((std::sync::Arc::clone(&previous.volume), index))
}
