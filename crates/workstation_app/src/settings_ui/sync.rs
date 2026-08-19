//! Conversions between live application state and the persisted snapshot.
//!
//! Everything here is a pure function over types from `analyst_runtime`,
//! `render2d` and `settings`, so it is testable without a window and usable
//! from `app.rs` as one-line calls. The string vocabularies (`"four"`,
//! `"cut"`, `"smooth"`) are defined here and resolved defensively: an
//! unknown string keeps the current behaviour, never breaks a pane.
//!
//! Product ids are deliberately NOT resolved here. The snapshot stores the
//! raw registry id; `app.rs` resolves it through
//! `DisplayProduct::try_from_product_id` on load so an unknown product
//! resets to the default *with a visible status line* - that resolution
//! needs the product registry, which this module does not reach.

use analyst_runtime::{PaneId, PaneLayout, StormMotionIntent, TiltSelection, WorkspaceState};
use radar_core::ProductId;
use render2d::DisplayQuality;
use settings::{PaneSnapshot, WorkspaceSnapshot};

// --- layout -----------------------------------------------------------------

pub fn layout_id(layout: PaneLayout) -> &'static str {
    match layout {
        PaneLayout::One => "one",
        PaneLayout::TwoVertical => "two-vertical",
        PaneLayout::TwoHorizontal => "two-horizontal",
        PaneLayout::Four => "four",
    }
}

pub fn layout_from_id(id: &str) -> Option<PaneLayout> {
    [
        PaneLayout::One,
        PaneLayout::TwoVertical,
        PaneLayout::TwoHorizontal,
        PaneLayout::Four,
    ]
    .into_iter()
    .find(|layout| layout_id(*layout) == id)
}

// --- display quality --------------------------------------------------------

/// The id stored for a quality preset. `None` for a custom quality no preset
/// names - the store then keeps whatever id it already had, which on reload
/// resolves to the nearest thing the analyst chose last.
pub fn quality_id(quality: DisplayQuality) -> Option<&'static str> {
    if quality == DisplayQuality::NATIVE {
        Some("native")
    } else if quality == DisplayQuality::SMOOTH {
        Some("smooth")
    } else if quality == DisplayQuality::HIGH {
        Some("high")
    } else if quality == DisplayQuality::ULTRA {
        Some("ultra")
    } else {
        None
    }
}

pub fn quality_from_id(id: &str) -> Option<DisplayQuality> {
    match id {
        "native" => Some(DisplayQuality::NATIVE),
        "smooth" => Some(DisplayQuality::SMOOTH),
        "high" => Some(DisplayQuality::HIGH),
        "ultra" => Some(DisplayQuality::ULTRA),
        _ => None,
    }
}

// --- tilt -------------------------------------------------------------------

fn tilt_to_snapshot(tilt: TiltSelection) -> (&'static str, Option<f64>) {
    match tilt {
        TiltSelection::LowestAvailable => ("lowest", None),
        TiltSelection::CutIndex(index) => ("cut", Some(f64::from(index))),
        TiltSelection::NearestElevationTenths(tenths) => {
            // Stored in degrees, not tenths: the file is for humans too.
            ("nearest", Some(f64::from(tenths) / 10.0))
        }
    }
}

fn tilt_from_snapshot(mode: Option<&str>, value: Option<f64>) -> Option<TiltSelection> {
    match mode? {
        "lowest" => Some(TiltSelection::LowestAvailable),
        "cut" => {
            let index = value?.clamp(0.0, f64::from(u16::MAX)).round();
            Some(TiltSelection::CutIndex(index as u16))
        }
        "nearest" => {
            let degrees = value?;
            if !degrees.is_finite() {
                return None;
            }
            let tenths = (degrees * 10.0).clamp(f64::from(i16::MIN), f64::from(i16::MAX));
            Some(TiltSelection::NearestElevationTenths(tenths.round() as i16))
        }
        _ => None,
    }
}

// --- the workspace ----------------------------------------------------------

/// Snapshot the parts of the workspace the file persists. Palettes are
/// captured separately (`palettes::capture_palettes`) because they live on
/// the colour table set, not on `WorkspaceState`; the caller composes the
/// two into one `WorkspaceSnapshot`.
pub fn capture_workspace(workspace: &WorkspaceState) -> WorkspaceSnapshot {
    let panes = (0..analyst_runtime::MAX_PANES)
        .filter_map(|index| PaneId::new(index as u8))
        .map(|pane| {
            let intent = workspace.pane(pane);
            let (tilt_mode, tilt_value) = tilt_to_snapshot(intent.tilt);
            PaneSnapshot {
                product: Some(intent.product.0.clone()),
                tilt_mode: Some(tilt_mode.to_owned()),
                tilt_value,
                center_east_km: Some(intent.camera.center_east_km),
                center_north_km: Some(intent.camera.center_north_km),
                km_per_point: Some(f64::from(intent.camera.km_per_point)),
                rotation_rad: Some(f64::from(intent.camera.rotation_rad)),
                camera_linked: Some(intent.links.camera.is_some()),
                ..Default::default()
            }
        })
        .collect();
    WorkspaceSnapshot {
        layout: Some(layout_id(workspace.layout).to_owned()),
        active_pane: Some(workspace.active_pane.get()),
        panes,
        ..Default::default()
    }
}

/// Restore a snapshot into a live workspace. Every field is optional and
/// resolved defensively; anything missing or unrecognised leaves the
/// workspace as it was. Cameras pass through `Camera2D::sanitized`, so a
/// hand-edited file cannot install a NaN camera.
///
/// Product ids are installed raw - see the module doc for why `app.rs`
/// validates them afterwards.
pub fn apply_workspace_snapshot(snapshot: &WorkspaceSnapshot, workspace: &mut WorkspaceState) {
    if let Some(layout) = snapshot.layout.as_deref().and_then(layout_from_id) {
        workspace.set_layout(layout);
    }
    for (index, pane_snapshot) in snapshot.panes.iter().enumerate() {
        let Some(pane) = u8::try_from(index).ok().and_then(PaneId::new) else {
            // A future build with more panes than this one: the extras are
            // preserved in the file and ignored here.
            break;
        };
        let intent = workspace.pane_mut(pane);
        if let Some(product) = &pane_snapshot.product {
            intent.product = ProductId(product.clone());
        }
        if let Some(tilt) =
            tilt_from_snapshot(pane_snapshot.tilt_mode.as_deref(), pane_snapshot.tilt_value)
        {
            intent.tilt = tilt;
        }
        let mut camera = intent.camera;
        if let Some(east) = pane_snapshot.center_east_km {
            camera.center_east_km = east;
        }
        if let Some(north) = pane_snapshot.center_north_km {
            camera.center_north_km = north;
        }
        if let Some(km_per_point) = pane_snapshot.km_per_point {
            camera.km_per_point = km_per_point as f32;
        }
        if let Some(rotation) = pane_snapshot.rotation_rad {
            camera.rotation_rad = rotation as f32;
        }
        intent.camera = camera.sanitized();
        if let Some(linked) = pane_snapshot.camera_linked {
            intent.links.camera = linked.then_some(0);
        }
    }
    // Active pane last, after the panes exist in their restored shape.
    if let Some(active) = snapshot.active_pane.and_then(PaneId::new) {
        workspace.set_active(active);
    }
}

/// The storm motion the Analysis page's two sliders describe.
pub fn storm_motion_from_settings(direction_from_deg: f64, speed_mps: f64) -> StormMotionIntent {
    StormMotionIntent {
        direction_from_deg: direction_from_deg.rem_euclid(360.0) as f32,
        speed_mps: speed_mps.clamp(0.0, 100.0) as f32,
    }
}

/// A window geometry snapshot from live viewport numbers, refusing degenerate
/// sizes so a minimised window is never persisted as the size to reopen at.
pub fn window_snapshot(
    outer_position: Option<(f32, f32)>,
    inner_size: Option<(f32, f32)>,
    maximized: bool,
) -> Option<settings::WindowSnapshot> {
    let (width, height) = inner_size?;
    if !(width >= 320.0 && height >= 240.0) {
        return None;
    }
    Some(settings::WindowSnapshot {
        x: outer_position.map(|(x, _)| x),
        y: outer_position.map(|(_, y)| y),
        width: Some(width),
        height: Some(height),
        maximized,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use analyst_runtime::Camera2D;

    #[test]
    fn layout_and_quality_ids_round_trip() {
        for layout in [
            PaneLayout::One,
            PaneLayout::TwoVertical,
            PaneLayout::TwoHorizontal,
            PaneLayout::Four,
        ] {
            assert_eq!(layout_from_id(layout_id(layout)), Some(layout));
        }
        for quality in [
            DisplayQuality::NATIVE,
            DisplayQuality::SMOOTH,
            DisplayQuality::HIGH,
            DisplayQuality::ULTRA,
        ] {
            let id = quality_id(quality).expect("preset has an id");
            assert_eq!(quality_from_id(id), Some(quality));
        }
        assert_eq!(layout_from_id("septagon"), None);
        assert_eq!(quality_from_id("septagon"), None);
    }

    #[test]
    fn a_real_workspace_survives_capture_and_apply() {
        let mut original = WorkspaceState::default();
        original.set_layout(PaneLayout::Four);
        let pane1 = PaneId::new(1).expect("pane 1");
        original.set_active(pane1);
        {
            let intent = original.pane_mut(pane1);
            intent.product = ProductId("DVEL".to_owned());
            intent.tilt = TiltSelection::CutIndex(3);
            intent.camera = Camera2D {
                center_east_km: -42.5,
                center_north_km: 17.25,
                km_per_point: 0.22,
                rotation_rad: 0.0,
            };
            intent.links.camera = None;
        }
        {
            let pane2 = PaneId::new(2).expect("pane 2");
            let intent = original.pane_mut(pane2);
            intent.tilt = TiltSelection::NearestElevationTenths(9);
        }

        let snapshot = capture_workspace(&original);
        let mut restored = WorkspaceState::default();
        apply_workspace_snapshot(&snapshot, &mut restored);

        assert_eq!(restored.layout, PaneLayout::Four);
        assert_eq!(restored.active_pane, pane1);
        let intent = restored.pane(pane1);
        assert_eq!(intent.product.0, "DVEL");
        assert_eq!(intent.tilt, TiltSelection::CutIndex(3));
        assert_eq!(intent.camera.center_east_km, -42.5);
        assert_eq!(intent.camera.center_north_km, 17.25);
        assert_eq!(intent.camera.km_per_point, 0.22);
        assert_eq!(intent.links.camera, None);
        assert_eq!(
            restored.pane(PaneId::new(2).expect("pane 2")).tilt,
            TiltSelection::NearestElevationTenths(9)
        );
    }

    #[test]
    fn a_hand_edited_nan_camera_is_sanitized_not_installed() {
        let snapshot = WorkspaceSnapshot {
            panes: vec![settings::PaneSnapshot {
                center_east_km: Some(f64::NAN),
                km_per_point: Some(-3.0),
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut workspace = WorkspaceState::default();
        apply_workspace_snapshot(&snapshot, &mut workspace);
        let camera = workspace.pane(PaneId::new(0).expect("pane 0")).camera;
        assert!(camera.center_east_km.is_finite());
        assert!(camera.km_per_point > 0.0, "{}", camera.km_per_point);
    }

    #[test]
    fn an_unknown_tilt_mode_or_layout_keeps_current_behaviour() {
        let snapshot = WorkspaceSnapshot {
            layout: Some("hexadecagon".to_owned()),
            panes: vec![settings::PaneSnapshot {
                tilt_mode: Some("astral".to_owned()),
                tilt_value: Some(2.0),
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut workspace = WorkspaceState::default();
        let before_layout = workspace.layout;
        let before_tilt = workspace.pane(PaneId::new(0).expect("pane 0")).tilt;
        apply_workspace_snapshot(&snapshot, &mut workspace);
        assert_eq!(workspace.layout, before_layout);
        assert_eq!(
            workspace.pane(PaneId::new(0).expect("pane 0")).tilt,
            before_tilt
        );
    }

    #[test]
    fn nearest_tilt_survives_the_degree_round_trip() {
        let (mode, value) = tilt_to_snapshot(TiltSelection::NearestElevationTenths(35));
        assert_eq!(mode, "nearest");
        assert_eq!(value, Some(3.5));
        assert_eq!(
            tilt_from_snapshot(Some(mode), value),
            Some(TiltSelection::NearestElevationTenths(35))
        );
    }

    #[test]
    fn degenerate_window_sizes_are_refused() {
        assert!(window_snapshot(None, Some((0.0, 0.0)), false).is_none());
        assert!(window_snapshot(None, None, false).is_none());
        let snapshot =
            window_snapshot(Some((10.0, 20.0)), Some((1280.0, 800.0)), true).expect("valid");
        assert_eq!(snapshot.width, Some(1280.0));
        assert!(snapshot.maximized);
    }
}
