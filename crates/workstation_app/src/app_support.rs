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

pub(crate) fn source_field_pane_title(
    volume: Option<&RadarVolume>,
    pane: PaneId,
    source: &crate::source_fields::SourceFieldDisplay,
    cut_index: Option<usize>,
) -> String {
    let elevation = volume
        .zip(cut_index)
        .and_then(|(volume, index)| volume.cuts.get(index))
        .map(|cut| format!(" · {:.1}°", cut.elevation_deg))
        .unwrap_or_default();
    let description = source
        .producer_description
        .as_deref()
        .filter(|description| *description != source.producer_name)
        .map(|description| format!(" · metadata: {description}"))
        .unwrap_or_default();
    let units = source.producer_units.as_deref().unwrap_or("not provided");
    format!(
        "{} · {} · SOURCE FIELD{description} · producer unit token: {units}{elevation}",
        pane.get() + 1,
        source.producer_name,
    )
}

/// Keep a dynamic field's exact identity on glass when the selected cut has
/// no finite samples, or when a restored source-field selection is absent
/// from the current file. Falling through `DisplayProduct` would call it REF.
pub(crate) fn unavailable_source_field_pane_title(
    volume: Option<&RadarVolume>,
    pane: PaneId,
    producer_name: &str,
    cut_index: Option<usize>,
) -> String {
    let elevation = volume
        .zip(cut_index)
        .and_then(|(volume, index)| volume.cuts.get(index))
        .map(|cut| format!(" · {:.1}°", cut.elevation_deg))
        .unwrap_or_default();
    format!(
        "{} · {producer_name} · SOURCE FIELD · unavailable{elevation}",
        pane.get() + 1
    )
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

/// The last finished picture compatible with an arriving sweep.
///
/// A matching cut in the previous retained volume keeps its existing
/// preference. At the start of a live session, however, a SAILS repeat may
/// already be arriving before a previous volume exists, and a prior volume can
/// legitimately lack the commanded tilt. In either case an earlier *completed*
/// sweep of the same measured elevation and split-cut leg in this very volume
/// is an honest underpaint. The current frame already owns that volume in an
/// `Arc`, so sharing it with the renderer does not clone any radar data.
pub(crate) fn previous_sweep_for(
    history: &VolumeHistory,
    volume: &RadarVolume,
    cut_index: usize,
    moment: &radar_core::MomentType,
) -> Option<(std::sync::Arc<RadarVolume>, usize)> {
    let selected = history.selected_index()?;
    let target = volume.cuts.get(cut_index)?;
    if let Some(previous) = selected
        .checked_sub(1)
        .and_then(|index| history.frames().get(index))
        && let Some(index) = crate::sweep::matching_cut_index(&previous.volume, target, moment)
    {
        return Some((std::sync::Arc::clone(&previous.volume), index));
    }

    // The caller hands this function a borrowed volume, but the render worker
    // needs shared ownership. Only the selected history frame can supply that
    // ownership without copying an entire radar volume. Identity alone is not
    // enough: successive partial snapshots share a site and timestamp while
    // holding different radials, so the allocation itself must be identical.
    let current = history.current()?;
    if !std::ptr::eq(current.volume.as_ref(), volume) {
        return None;
    }
    let target_elevation = product_engine::capabilities::median_elevation_deg(target)?;
    let target_leg = sweep_leg(target);

    // File order is collection order within one volume. Walking backwards
    // chooses the newest earlier compatible SAILS repeat without remeasuring
    // or sorting every cut in the volume on every animation frame.
    let index =
        volume.cuts[..cut_index]
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, candidate)| {
                if candidate
                    .moments
                    .get(moment)
                    .is_none_or(|grid| grid.radial_count() == 0)
                    || sweep_leg(candidate) != target_leg
                {
                    return None;
                }

                let elevation = product_engine::capabilities::median_elevation_deg(candidate)?;
                if (elevation - target_elevation).abs()
                    > product_engine::capabilities::NOMINAL_ELEVATION_TOLERANCE_DEG
                {
                    return None;
                }

                // Use the reveal's own full-turn measurement rather than a radial
                // count or an end marker: sector scans and interrupted sweeps can
                // carry either without providing a complete underpaint.
                let complete = crate::sweep::SweepAnimator::new()
                    .observe(candidate, std::time::Duration::ZERO)
                    .is_some_and(|state| state.complete);
                complete.then_some(index)
            })?;

    Some((std::sync::Arc::clone(&current.volume), index))
}

/// The same moment-based split-leg classification as `product_engine`.
///
/// A Doppler sweep can carry REF as well as VEL. Matching on REF alone would
/// silently paint its short-range field underneath a long-range surveillance
/// sweep, so the entire leg - not merely the requested moment - has to agree.
fn sweep_leg(cut: &radar_core::ElevationCut) -> product_engine::CutLeg {
    use radar_core::MomentType;

    let has_velocity = cut.moments.contains_key(&MomentType::Velocity);
    let has_dual_pol = cut
        .moments
        .contains_key(&MomentType::DifferentialReflectivity)
        || cut
            .moments
            .contains_key(&MomentType::CorrelationCoefficient)
        || cut.moments.contains_key(&MomentType::DifferentialPhase);

    match (has_velocity, has_dual_pol) {
        (true, true) => product_engine::CutLeg::Combined,
        (true, false) => product_engine::CutLeg::Doppler,
        (false, _) => product_engine::CutLeg::Surveillance,
    }
}

/// File extensions the open and drop paths accept, lowercase and without the
/// dot.
///
/// The list spans every container the io crate's routing seam can identify,
/// so a drop that mixes a volume with the screenshot next to it opens the
/// volume rather than the screenshot.
///
/// * NEXRAD Archive II: `ar2v`, `v06`, `v08`, `raw`, and the `gz`/`bz2`
///   wrappers those arrive in.
/// * GR2Analyst-convention exports: `msg31`.
/// * HDF5, carrying ODIM_H5: `h5`, `hdf`, `hd5`.
/// * Classic netCDF, carrying CfRadial 1.x: `nc`.
/// * Mobile deployment bundles: `zip`.
///
/// DORADE sweepfiles are not on the list because they have no extension in
/// any useful sense - `swp.1090509143923.NOXPRVP.0.0.5_PPI_v1` parses as an
/// extension of `5_PPI_v1` - so they are matched by their `swp.` naming
/// convention instead; see [`looks_like_radar_file`].
const RADAR_FILE_EXTENSIONS: &[&str] = &[
    "ar2v", "bz2", "gz", "h5", "hd5", "hdf", "mat", "msg31", "nc", "raw", "v06", "v08", "zip",
];

/// Whether a path is worth handing to the decoder.
///
/// A file with no extension passes. That is not laxity: NEXRAD volumes are
/// routinely stored with the site and timestamp as the entire name
/// (`KTLX19990503_233631`), and the S3 archive objects carry no extension at
/// all, so refusing them would reject the most common real input. Content is
/// what actually decides - the io crate sniffs magic bytes - and this
/// predicate exists only to pick the plausible file out of a multi-file drop
/// and to keep an obviously wrong one from clearing the session.
pub(crate) fn looks_like_radar_file(path: &std::path::Path) -> bool {
    // A DORADE sweepfile's name is a dotted field list, so its "extension"
    // is whatever happens to follow the last dot. The io crate owns the
    // naming convention; asking it keeps one definition of the shape.
    if nexrad_io::dorade::looks_like_dorade_path(path) {
        return true;
    }
    match path.extension().and_then(|extension| extension.to_str()) {
        None => true,
        Some(extension) => RADAR_FILE_EXTENSIONS
            .iter()
            .any(|known| extension.eq_ignore_ascii_case(known)),
    }
}

/// Pick the radar candidates out of a drop.
///
/// Dropping a selection rather than a single file is normal. Every plausible
/// radar path is retained for a file playlist while a screenshot or notes
/// beside them are ignored.
///
/// If nothing in the drop looks like radar data, the first path is returned
/// anyway. A decoder error naming the file the analyst dropped is more use
/// than a drop target that silently does nothing, which reads as a bug.
pub(crate) fn choose_dropped_radar_files(
    paths: impl IntoIterator<Item = std::path::PathBuf>,
) -> Vec<std::path::PathBuf> {
    let mut all = Vec::new();
    let mut radar = Vec::new();
    for path in paths {
        if looks_like_radar_file(&path) {
            radar.push(path.clone());
        }
        all.push(path);
    }
    if radar.is_empty() {
        all.into_iter().take(1).collect()
    } else {
        radar
    }
}

/// The first candidate, retained for single-file decoder-path tests.
#[cfg(test)]
pub(crate) fn choose_dropped_radar_file(
    paths: impl IntoIterator<Item = std::path::PathBuf>,
) -> Option<std::path::PathBuf> {
    choose_dropped_radar_files(paths).into_iter().next()
}

#[cfg(test)]
mod tests {
    use super::*;
    use analyst_runtime::{FrameOrigin, FrameStage, VolumeFrame};
    use chrono::{TimeZone, Utc};
    use radar_core::{
        ElevationCut, GateRange, MomentGrid, MomentType, RadarSite, Radial, RadialStatus,
    };
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    fn blend_volume(minute: u32) -> RadarVolume {
        RadarVolume::new(
            RadarSite::new("KTLX"),
            Utc.with_ymd_and_hms(2026, 8, 24, 18, minute, 0)
                .single()
                .expect("valid sweep-blend fixture time"),
        )
    }

    fn add_blend_cut(
        volume: &mut RadarVolume,
        stored_elevation_deg: f32,
        measured_elevation_deg: f32,
        elevation_number: u8,
        radial_count: usize,
        moments: &[MomentType],
    ) {
        let gate_range = GateRange {
            first_gate_m: 250,
            gate_spacing_m: 250,
            gate_count: 1,
        };
        let mut cut = ElevationCut::new(stored_elevation_deg, Some(elevation_number));
        for index in 0..radial_count {
            cut.radials.push(Radial {
                azimuth_deg: index as f32,
                elevation_deg: measured_elevation_deg,
                time_offset_ms: index as i32 * 100,
                gate_range: gate_range.clone(),
                nyquist_velocity_mps: Some(25.0),
                // Deliberately present even on a sector fixture: a marker
                // alone cannot make 120 degrees a complete underpaint.
                radial_status: Some(if index + 1 == radial_count {
                    RadialStatus::EndElevation
                } else {
                    RadialStatus::Intermediate
                }),
            });
        }
        for moment in moments {
            let mut grid = MomentGrid::new_u8(
                moment.clone(),
                gate_range.clone(),
                2.0,
                66.0,
                Some(0),
                Some(1),
            );
            for index in 0..radial_count {
                grid.push_u8_row_slice(index, &[2])
                    .expect("valid one-gate fixture radial");
            }
            cut.moments.insert(moment.clone(), grid);
        }
        volume.cuts.push(cut);
    }

    fn install_blend_volume(history: &mut VolumeHistory, volume: Arc<RadarVolume>) {
        history.install(VolumeFrame::new(
            volume,
            FrameOrigin::Live,
            FrameStage::Partial,
            "live-sweep-fixture",
        ));
    }

    #[test]
    fn an_arriving_sails_repeat_underpaints_from_the_newest_completed_compatible_leg() {
        let mut volume = blend_volume(5);
        // Stored first-radial angles are intentionally misleading; measured
        // medians put both surveillance sweeps in the target's real group.
        add_blend_cut(&mut volume, 0.72, 0.50, 1, 360, &[MomentType::Reflectivity]);
        add_blend_cut(
            &mut volume,
            0.48,
            0.51,
            2,
            360,
            &[MomentType::Reflectivity, MomentType::Velocity],
        );
        add_blend_cut(&mut volume, 0.78, 0.53, 3, 360, &[MomentType::Reflectivity]);
        add_blend_cut(&mut volume, 0.52, 0.50, 4, 120, &[MomentType::Reflectivity]);
        add_blend_cut(&mut volume, 0.91, 0.90, 5, 360, &[MomentType::Reflectivity]);
        add_blend_cut(&mut volume, 0.10, 0.50, 6, 60, &[MomentType::Reflectivity]);
        let volume = Arc::new(volume);
        let mut history = VolumeHistory::default();
        install_blend_volume(&mut history, Arc::clone(&volume));

        let (underpaint, index) =
            previous_sweep_for(&history, volume.as_ref(), 5, &MomentType::Reflectivity)
                .expect("the current volume contains a completed compatible SAILS sweep");

        assert!(
            Arc::ptr_eq(&underpaint, &volume),
            "underpainting a repeat must share the retained snapshot, never clone radar data"
        );
        assert_eq!(
            index, 2,
            "skip newer wrong-tilt and unfinished sweeps and preserve the surveillance leg"
        );
    }

    #[test]
    fn a_matching_previous_volume_retains_its_existing_underpaint_preference() {
        let mut previous = blend_volume(0);
        add_blend_cut(&mut previous, 0.5, 0.5, 7, 360, &[MomentType::Reflectivity]);
        let previous = Arc::new(previous);

        let mut current = blend_volume(5);
        add_blend_cut(&mut current, 0.5, 0.5, 1, 360, &[MomentType::Reflectivity]);
        add_blend_cut(&mut current, 0.5, 0.5, 7, 60, &[MomentType::Reflectivity]);
        let current = Arc::new(current);

        let mut history = VolumeHistory::default();
        install_blend_volume(&mut history, Arc::clone(&previous));
        install_blend_volume(&mut history, Arc::clone(&current));

        let (underpaint, index) =
            previous_sweep_for(&history, current.as_ref(), 1, &MomentType::Reflectivity)
                .expect("the previous retained volume carries the matching tilt");

        assert!(
            Arc::ptr_eq(&underpaint, &previous),
            "a same-volume fallback must not replace an already suitable prior volume"
        );
        assert_eq!(index, 0);
    }

    #[test]
    fn a_prior_volume_without_this_tilt_falls_back_to_the_current_completed_sweep() {
        let mut previous = blend_volume(0);
        add_blend_cut(&mut previous, 1.8, 1.8, 9, 360, &[MomentType::Reflectivity]);
        let previous = Arc::new(previous);

        let mut current = blend_volume(5);
        add_blend_cut(&mut current, 0.5, 0.5, 1, 360, &[MomentType::Reflectivity]);
        add_blend_cut(&mut current, 0.5, 0.5, 2, 60, &[MomentType::Reflectivity]);
        let current = Arc::new(current);

        let mut history = VolumeHistory::default();
        install_blend_volume(&mut history, previous);
        install_blend_volume(&mut history, Arc::clone(&current));

        let (underpaint, index) =
            previous_sweep_for(&history, current.as_ref(), 1, &MomentType::Reflectivity)
                .expect("the prior volume's missing tilt cannot hide a valid current-volume sweep");

        assert!(Arc::ptr_eq(&underpaint, &current));
        assert_eq!(index, 0);
    }

    #[test]
    fn unfinished_wrong_leg_and_wrong_elevation_sweeps_are_never_underpaint() {
        let mut volume = blend_volume(5);
        add_blend_cut(&mut volume, 0.9, 0.9, 1, 360, &[MomentType::Reflectivity]);
        add_blend_cut(
            &mut volume,
            0.5,
            0.5,
            2,
            360,
            &[MomentType::Reflectivity, MomentType::Velocity],
        );
        add_blend_cut(&mut volume, 0.5, 0.5, 3, 120, &[MomentType::Reflectivity]);
        add_blend_cut(&mut volume, 0.5, 0.5, 4, 60, &[MomentType::Reflectivity]);
        let volume = Arc::new(volume);
        let mut history = VolumeHistory::default();
        install_blend_volume(&mut history, Arc::clone(&volume));

        assert!(
            previous_sweep_for(&history, volume.as_ref(), 3, &MomentType::Reflectivity).is_none(),
            "a matching REF key, misleading sweep-end marker, or nearby cut is not enough"
        );
    }

    #[test]
    fn a_different_snapshot_with_the_same_identity_cannot_borrow_the_history_arc() {
        let mut volume = blend_volume(5);
        add_blend_cut(&mut volume, 0.5, 0.5, 1, 360, &[MomentType::Reflectivity]);
        add_blend_cut(&mut volume, 0.5, 0.5, 2, 60, &[MomentType::Reflectivity]);
        let volume = Arc::new(volume);
        let different_snapshot = volume.as_ref().clone();
        let mut history = VolumeHistory::default();
        install_blend_volume(&mut history, volume);

        assert!(
            previous_sweep_for(&history, &different_snapshot, 1, &MomentType::Reflectivity,)
                .is_none(),
            "matching site/time does not make a different partial snapshot safe to share"
        );
    }

    #[test]
    fn a_drop_prefers_the_radar_file_over_whatever_came_with_it() {
        let dropped = ["notes.txt", "screenshot.png", "KTLX19990503_233631"]
            .into_iter()
            .map(PathBuf::from);
        assert_eq!(
            choose_dropped_radar_file(dropped),
            Some(PathBuf::from("KTLX19990503_233631"))
        );
    }

    #[test]
    fn a_drop_of_only_wrong_files_still_reports_one_so_the_error_names_it() {
        let dropped = ["notes.txt", "screenshot.png"]
            .into_iter()
            .map(PathBuf::from);
        assert_eq!(
            choose_dropped_radar_file(dropped),
            Some(PathBuf::from("notes.txt"))
        );
    }

    #[test]
    fn an_empty_drop_loads_nothing() {
        assert_eq!(choose_dropped_radar_file(Vec::new()), None);
    }

    #[test]
    fn an_unavailable_source_title_never_falls_through_to_ref() {
        let pane = PaneId::new(0).expect("first pane");
        assert_eq!(
            unavailable_source_field_pane_title(None, pane, "VL1_CRR", None),
            "1 · VL1_CRR · SOURCE FIELD · unavailable"
        );
    }

    #[test]
    fn every_radar_file_in_a_drop_becomes_a_playlist_candidate_in_drop_order() {
        let dropped = [
            "notes.txt",
            "KTLX19990503_233631",
            "screenshot.png",
            "KTLX19990503_234201",
        ]
        .into_iter()
        .map(PathBuf::from);
        assert_eq!(
            choose_dropped_radar_files(dropped),
            vec![
                PathBuf::from("KTLX19990503_233631"),
                PathBuf::from("KTLX19990503_234201"),
            ]
        );
    }

    #[test]
    fn accepts_every_container_the_routing_seam_can_identify() {
        for name in [
            "KDVN20260819_192802_V06",
            "volume.ar2v",
            "volume.ar2v.gz",
            "volume.V08",
            "scan.RAW",
            "export.msg31",
            "bejab.pvol.h5",
            "T_PAGZ35.hdf",
            "polrad.hd5",
            "cfrad.20211011_223602_DOW8_RHI.nc",
            "iq_OUPRIME_20100510_224711_e00.12_prt2.mat",
            "deployment.zip",
            "volume.bz2",
            // Real DORADE sweepfiles, whose names are dotted field lists.
            // `Path::extension` reads the first as "5_PPI_v1", which is on
            // no allowlist and never will be.
            "swp.1090509143923.NOXPRVP.0.0.5_PPI_v1",
            "swp.1260521225514.COW2.229.1.0_SUR_v215",
        ] {
            assert!(
                looks_like_radar_file(Path::new(name)),
                "{name} should be accepted"
            );
        }
    }

    #[test]
    fn rejects_files_that_are_plainly_not_radar_volumes() {
        for name in ["screenshot.png", "notes.txt", "colors.json", "report.pdf"] {
            assert!(
                !looks_like_radar_file(Path::new(name)),
                "{name} should be rejected"
            );
        }
    }

    /// A DORADE sweep dropped alongside its notes opens the sweep.
    ///
    /// This is the case the extension allowlist cannot serve: without the
    /// naming rule the sweepfile reads as extension "5_PPI_v1", loses to the
    /// `.txt` sitting next to it, and the analyst gets a decode failure on a
    /// text file instead of the scan they dropped.
    #[test]
    fn a_dorade_sweepfile_wins_a_drop_over_the_notes_beside_it() {
        let dropped = [
            "deployment/notes.txt",
            "deployment/swp.1090509143923.NOXPRVP.0.0.5_PPI_v1",
        ]
        .into_iter()
        .map(PathBuf::from);
        assert_eq!(
            choose_dropped_radar_file(dropped),
            Some(PathBuf::from(
                "deployment/swp.1090509143923.NOXPRVP.0.0.5_PPI_v1"
            ))
        );
    }

    #[test]
    fn a_matlab_iq_cube_wins_a_drop_over_non_radar_files() {
        let dropped = ["case/notes.txt", "case/scan.mat", "case/screenshot.png"]
            .into_iter()
            .map(PathBuf::from);
        assert_eq!(
            choose_dropped_radar_file(dropped),
            Some(PathBuf::from("case/scan.mat"))
        );
    }

    #[test]
    fn extensionless_archive_names_are_accepted_because_that_is_how_they_ship() {
        // The public Level II archive stores objects under exactly this name.
        assert!(looks_like_radar_file(Path::new(
            "C:/data/KTLX19990503_233631"
        )));
    }
}
