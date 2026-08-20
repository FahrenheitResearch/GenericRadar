//! The seam the application actually uses, compiled and exercised here.
//!
//! The application drives this crate from `workstation_app`, which lives
//! outside it, so this crate's own gates cannot catch a call that names a
//! method wrongly or one whose borrows do not work out inside a struct
//! literal. This test performs the same calls, in the same order, with the
//! same types, so the seam is checked by the compiler rather than by eye.

use std::sync::Arc;

use analyst_runtime::{Camera2D, MAX_PANES, PaneId, ViewportMetrics};
use eframe::egui;
use map_scene::{MapChrome, MapGeometry, MapSceneController, RadarProjection, TileFrame};

/// The pane's own `PaneMap`, reduced to the fields the tile layer touches.
/// The point is the *shape*: two `&mut self` calls on the same controller,
/// inside one struct literal, in the order `app.rs` has them.
struct PaneMap {
    geometry: Option<Arc<MapGeometry>>,
    tiles: Option<Arc<TileFrame>>,
    projection: Option<RadarProjection>,
    chrome: MapChrome,
}

#[test]
fn the_app_side_call_sequence_compiles_and_runs() {
    let mut scene = MapSceneController::new(|| {});
    // `app.rs` calls this once per frame, from `fn ui`.
    scene.set_pixels_per_point(1.5);
    assert_eq!(scene.pixels_per_point(), 1.5);
    scene.set_default_anchor();

    let rect = egui::Rect::from_min_size(egui::pos2(0.0, 26.0), egui::vec2(760.0, 480.0));
    for pane_index in 0..MAX_PANES {
        let camera = Camera2D::default();
        // Exactly the shape of the `PaneMap { ... }` literal in `fn canvas`.
        let pane_map = PaneMap {
            geometry: scene.geometry_for_pane(pane_index, camera.sanitized().km_per_point),
            tiles: scene.tiles_for_pane(pane_index, camera, rect),
            projection: scene.projection(),
            chrome: MapChrome::for_style(scene.style()),
        };
        // No provider is selected by default, so the pane is the vector-only
        // pane that shipped. This is the degrade path, asserted rather than
        // assumed.
        assert!(pane_map.tiles.is_none());
        assert!(pane_map.projection.is_some());
        let _ = (pane_map.geometry, pane_map.chrome);
    }
    scene.poll();
}

/// Every provider the picker can offer must return its required credit
/// through the seam the pane draws from - not just the one a test happened to
/// select.
///
/// Attribution is a condition of use for all five, and the failure this
/// catches is a provider added later whose credit string is empty, wrong, or
/// unreachable because the scene forgot to plumb it. The OpenStreetMap wording
/// is pinned exactly: the OSMF licence attribution is the copyright sign
/// followed by "OpenStreetMap contributors", and paraphrasing it is not
/// attribution.
#[test]
fn every_provider_the_picker_offers_has_its_credit_reachable_from_the_scene() {
    let mut scene = MapSceneController::new(|| {});
    scene.set_tiles_offline(true);
    scene.set_default_anchor();
    assert!(
        scene.tile_attribution().is_none(),
        "no imagery must mean no credit drawn"
    );

    let rect = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(900.0, 600.0));
    let mut offered = 0;
    for provider in map_scene::TileProvider::ALL {
        if !scene.tile_provider_permitted(provider) {
            continue;
        }
        offered += 1;
        scene.set_tile_provider(Some(provider));
        let drawn = scene
            .tile_attribution()
            .expect("a selected provider must carry a credit");
        assert_eq!(
            drawn,
            provider.attribution(),
            "{provider:?} draws a different credit from the one it declares"
        );
        assert!(drawn.len() > 12, "{provider:?}: {drawn:?} is not a credit");
        if provider == map_scene::TileProvider::OpenStreetMap {
            assert_eq!(
                drawn, "\u{a9} OpenStreetMap contributors",
                "the OSMF licence attribution may not be reworded"
            );
        } else {
            assert!(
                drawn.contains("USGS"),
                "{provider:?} does not name the service publishing it: {drawn:?}"
            );
        }
        // And it is what the pane will actually draw, which is the frame's own
        // copy rather than the controller's.
        let frame = scene
            .tiles_for_pane(0, Camera2D::default(), rect)
            .expect("a frame, even offline");
        assert_eq!(frame.attribution, provider.attribution());
    }
    assert!(offered >= 4, "only {offered} providers were offered at all");
}

/// The provider picker's call sequence, from `app_support.rs`.
#[test]
fn the_picker_call_sequence_compiles_and_runs() {
    let mut scene = MapSceneController::new(|| {});
    scene.set_default_anchor();

    let mut provider = scene.tile_provider();
    assert!(provider.is_none(), "no imagery is the shipped default");
    let selected_text = provider
        .map(map_scene::TileProvider::label)
        .unwrap_or("No imagery");
    assert_eq!(selected_text, "No imagery");

    for candidate in map_scene::TileProvider::ALL {
        // What the combo box lists, and what its rows say on hover.
        assert!(!candidate.label().is_empty());
        assert!(!candidate.coverage_note().is_empty());
        assert!(!candidate.attribution().is_empty());
        if !scene.tile_provider_permitted(candidate) {
            // A provider whose terms this configuration cannot satisfy is
            // hidden rather than offered.
            continue;
        }
        provider = Some(candidate);
    }
    if provider != scene.tile_provider() {
        scene.set_tile_provider(provider);
    }
    assert_eq!(scene.tile_provider(), provider);

    // The credit the pane draws. Required, and there is no switch for it.
    let attribution = scene.tile_attribution().expect("a provider is selected");
    assert!(!attribution.is_empty());

    // The scrim slider: read the value in force, write a pin, read it back.
    let mut scrim = scene.tile_scrim();
    assert!((0.0..=1.0).contains(&scrim));
    scrim = 0.5;
    scene.set_tile_scrim(scrim);
    assert_eq!(scene.tile_scrim(), 0.5);

    // Offline is a switch the app may expose; it must never open a socket.
    scene.set_tiles_offline(true);
    assert!(scene.tiles_offline());
    scene.set_tile_provider(None);
    assert!(scene.tile_attribution().is_none());

    let metrics = scene.tile_metrics();
    assert_eq!(metrics.store.downloaded, 0);
    assert_eq!(metrics.store.failed, 0);
}

// ---------------------------------------------------------------------------
// The two functions the handoff adds to `pane_canvas.rs`, VERBATIM.
//
// They are here so the compiler checks the text a human is going to paste. The
// only difference from the snippet is the `PaneMap` above standing in for the
// pane's own, which carries the same `tiles` field.
// ---------------------------------------------------------------------------

/// Queue this pane's raster tile underlay.
///
/// Same shape as `paint_map`: the callback carries a draw list and a camera,
/// and the textures behind it are already on the GPU. Tiles that have not
/// arrived are simply absent from the list, so a cold cache costs nothing here
/// and shows the pane's own ground until they land.
fn paint_tiles(
    painter: &egui::Painter,
    rect: egui::Rect,
    pane: PaneId,
    camera: Camera2D,
    viewport: ViewportMetrics,
    map: &PaneMap,
) {
    let Some(frame) = map.tiles.clone() else {
        return;
    };
    if frame.draws.is_empty() && frame.uploads.is_empty() {
        return;
    }
    let pixels_per_point = viewport.sanitized().pixels_per_point;
    let callback = map_scene::gpu::TilePaintCallback {
        pane_index: pane.index(),
        frame,
        camera,
        viewport,
        rect_px: [
            rect.left() * pixels_per_point,
            rect.top() * pixels_per_point,
            rect.right() * pixels_per_point,
            rect.bottom() * pixels_per_point,
        ],
    };
    painter.add(eframe::egui_wgpu::Callback::new_paint_callback(
        rect, callback,
    ));
}

/// The provider credit, bottom right, on its own plate.
///
/// This is a correctness requirement, not decoration. The OSMF Standard Tile
/// Layer Usage Policy requires the licence attribution to be shown clearly on
/// the map and forbids hiding it beneath UI, behind toggles or off-screen; the
/// USGS National Map services carry their credit in the service's own
/// `copyrightText`. Hence: no toggle, and drawn last of the pane content so
/// nothing covers it.
///
/// On a plate rather than in a `MapChrome` ink because the ground under it is
/// now imagery, not the chrome canvas - a token tuned for a flat dark pane is
/// the wrong contrast reference over an aerial photograph. It sits in the
/// bottom 20 points, which the legend column stops 26 points short of.
fn draw_tile_attribution(painter: &egui::Painter, rect: egui::Rect, map: &PaneMap) {
    let Some(frame) = map.tiles.as_ref() else {
        return;
    };
    let galley = painter.layout_no_wrap(
        frame.attribution.to_owned(),
        egui::FontId::proportional(9.0),
        egui::Color32::from_gray(232),
    );
    let padding = egui::vec2(4.0, 2.0);
    let plate = egui::Rect::from_min_size(
        egui::pos2(
            rect.right() - galley.size().x - padding.x * 2.0 - 4.0,
            rect.bottom() - galley.size().y - padding.y * 2.0 - 4.0,
        ),
        galley.size() + padding * 2.0,
    );
    painter.rect_filled(plate, 2.0, egui::Color32::from_black_alpha(150));
    painter.galley(plate.min + padding, galley, egui::Color32::from_gray(232));
}

/// Run both of them for real, inside an egui pass, on a pane with no imagery
/// and then on one with a frame.
#[test]
fn the_pane_canvas_snippet_compiles_and_draws() {
    let mut scene = MapSceneController::new(|| {});
    scene.set_default_anchor();
    scene.set_tile_provider(Some(map_scene::TileProvider::UsgsTopo));
    scene.set_tiles_offline(true);
    let rect = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(760.0, 480.0));
    let camera = Camera2D::default();
    let map = PaneMap {
        geometry: None,
        tiles: scene.tiles_for_pane(0, camera, rect),
        projection: scene.projection(),
        chrome: MapChrome::for_style(scene.style()),
    };
    assert!(
        map.tiles.is_some(),
        "offline still produces a frame to draw"
    );
    let attribution = map.tiles.as_ref().expect("frame").attribution;
    assert!(attribution.contains("USGS"), "{attribution}");

    let context = egui::Context::default();
    let mut shapes = 0;
    // Two passes: the first egui pass builds the font atlas, and a pane is
    // never a session's first frame.
    for _ in 0..2 {
        let output = context.run_ui(egui::RawInput::default(), |ui| {
            let painter = ui.painter_at(rect);
            let viewport = ViewportMetrics {
                width_points: rect.width(),
                height_points: rect.height(),
                pixels_per_point: 1.0,
            };
            let pane = PaneId::new(0).expect("pane");
            paint_tiles(&painter, rect, pane, camera, viewport, &map);
            draw_tile_attribution(&painter, rect, &map);
        });
        shapes = output.shapes.len();
    }
    // The credit is drawn: a plate and its text, on a pane that has imagery.
    // This is the assertion that stops the attribution from being quietly
    // dropped, which would make the layer unshippable.
    assert!(shapes >= 2, "the attribution never reached the paint list");

    let empty = PaneMap {
        geometry: None,
        tiles: None,
        projection: scene.projection(),
        chrome: MapChrome::for_style(scene.style()),
    };
    let output = context.run_ui(egui::RawInput::default(), |ui| {
        let painter = ui.painter_at(rect);
        let viewport = ViewportMetrics {
            width_points: rect.width(),
            height_points: rect.height(),
            pixels_per_point: 1.0,
        };
        draw_tile_attribution(&painter, rect, &empty);
        paint_tiles(
            &painter,
            rect,
            PaneId::new(0).expect("pane"),
            camera,
            viewport,
            &empty,
        );
    });
    assert!(
        output.shapes.is_empty(),
        "a pane with no imagery drew {} shapes",
        output.shapes.len()
    );
}

// ---------------------------------------------------------------------------
// The block the handoff adds to `app_support.rs::basemap_picker`, VERBATIM.
// Wrapped in a function here only so it can be run; in `app_support.rs` it is
// the tail of the existing `basemap_picker` and `scene` is already its
// parameter.
// ---------------------------------------------------------------------------
fn imagery_picker(ui: &mut egui::Ui, scene: &mut MapSceneController) {
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
        }
    }
}

#[test]
fn the_picker_block_compiles_and_draws() {
    let mut scene = MapSceneController::new(|| {});
    scene.set_default_anchor();
    scene.set_tiles_offline(true);
    let context = egui::Context::default();
    for _ in 0..2 {
        let output = context.run_ui(egui::RawInput::default(), |ui| {
            imagery_picker(ui, &mut scene);
        });
        assert!(!output.shapes.is_empty(), "the picker drew nothing");
    }
    // With imagery selected the slider appears beside the combo box.
    scene.set_tile_provider(Some(map_scene::TileProvider::UsgsImagery));
    let with_slider = context.run_ui(egui::RawInput::default(), |ui| {
        imagery_picker(ui, &mut scene);
    });
    scene.set_tile_provider(None);
    let without = context.run_ui(egui::RawInput::default(), |ui| {
        imagery_picker(ui, &mut scene);
    });
    assert!(with_slider.shapes.len() > without.shapes.len());
}
