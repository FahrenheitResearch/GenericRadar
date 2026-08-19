//! What the pane actually paints, read off a real egui frame.
//!
//! A sibling module rather than a `mod tests` inside `pane_canvas.rs`, because
//! that file is at 1 900-odd lines against the 2 000-line cap that
//! `tests/architecture.rs` enforces and these tests are the part of it most
//! likely to keep growing.
//!
//! The bug they close: the pane hard-coded `rgb(6, 9, 13)` for its ground,
//! `rgb(214, 222, 232)` over `rgba(0, 0, 0, 190)` for its place names, and
//! another six constants for its rings, readouts and site markers - so
//! `Daylight` painted dark ink onto a permanently dark pane and read as a
//! blank screen, and once the ground was fixed the near-white readouts
//! vanished into the near-white pane instead. Everything here drives
//! `draw_pane` through a real egui pass and inspects the shapes it emitted, so
//! it measures the paint rather than the intention.

use super::*;
use analyst_runtime::{Generation, GeometryCacheKey, LodBucket};
use map_scene::MapStylePreset;

/// KTLX (Twin Lakes, Oklahoma) as the radar reports itself in the `RVOL`
/// block of the real archive file `KTLX20260817_165447_RT346_V06` - the
/// field `install_loaded_volume` hands to `set_radar_anchor`. Real place
/// labels exist here, which is what makes the label assertions meaningful.
const KTLX: (f64, f64) = (35.3333625793457, -97.27776336669922);
const PANE: egui::Rect = egui::Rect {
    min: egui::pos2(0.0, 0.0),
    max: egui::pos2(600.0, 600.0),
};

fn map_for(preset: MapStylePreset) -> PaneMap {
    let projection = RadarProjection::new(KTLX.0, KTLX.1);
    let scale = Camera2D::default().km_per_point;
    let geometry = map_scene::build_geometry(&map_scene::MapBuildRequest {
        key: GeometryCacheKey {
            dataset: Generation::new(1),
            projection: Generation::new(1),
            style: Generation::new(1),
            lod: LodBucket::ideal(scale, map_scene::LOD_REFERENCE_KM_PER_POINT),
        },
        dataset: map_scene::MapDataset::from_generated(Generation::new(1)),
        projection,
        style: preset.style(),
    });
    PaneMap {
        geometry: Some(Arc::new(geometry)),
        projection: Some(projection),
        tiles: None,
        chrome: preset.chrome(),
        sites: Arc::from(Vec::new()),
        site_labels: SiteLabelMode::default(),
        active_site: None,
        hazards: Arc::from(Vec::new()),
    }
}

/// Every shape one `draw_pane` emitted. Two passes, because the first egui
/// pass builds the font atlas and a pane is never a session's first frame.
fn painted(map: &PaneMap) -> Vec<egui::Shape> {
    let context = egui::Context::default();
    let mut shapes = Vec::new();
    for _ in 0..2 {
        let output = context.run_ui(egui::RawInput::default(), |ui| {
            egui::CentralPanel::default().show_inside(ui, |ui| {
                let overlay = PaneOverlay {
                    legend: None,
                    table: None,
                    product_name: "REF",
                    badges: &[],
                    probe: None,
                };
                draw_pane(
                    ui,
                    PaneId::new(0).expect("pane 0"),
                    PANE,
                    true,
                    Camera2D::default(),
                    NavTuning::default(),
                    None,
                    map,
                    "1 - REF (dBZ)",
                    "",
                    &overlay,
                );
            });
        });
        shapes = output.shapes.into_iter().map(|c| c.shape).collect();
    }
    shapes
}

/// The ground the pane cleared to: the rect covering the whole pane.
fn ground(shapes: &[egui::Shape]) -> egui::Color32 {
    shapes
        .iter()
        .find_map(|shape| match shape {
            egui::Shape::Rect(rect) if rect.rect == PANE => Some(rect.fill),
            _ => None,
        })
        .expect("the pane painted no background")
}

/// Every colour the pass used for text.
fn text_colors(shapes: &[egui::Shape]) -> Vec<egui::Color32> {
    let mut colors: Vec<egui::Color32> = shapes
        .iter()
        .filter_map(|shape| match shape {
            egui::Shape::Text(text) => Some(text.fallback_color),
            _ => None,
        })
        .collect();
    colors.sort_by_key(|color| color.to_array());
    colors.dedup();
    colors
}

/// The four constants this module used to hard-code, reproduced exactly
/// through the conversion the chrome now goes through. If
/// `LayerColor::to_rgba8` ever rounded differently the shipped map would
/// shift by a byte and nothing else would notice.
#[test]
fn slate_chrome_is_byte_identical_to_the_constants_it_replaced() {
    let slate = MapStylePreset::Slate.chrome();
    assert_eq!(
        chrome_color(slate.canvas),
        egui::Color32::from_rgb(6, 9, 13),
        "pane background and hazard-tag halo"
    );
    assert_eq!(
        chrome_color(slate.label_ink),
        egui::Color32::from_rgb(214, 222, 232),
        "place-label ink"
    );
    assert_eq!(
        chrome_color(slate.label_halo),
        egui::Color32::from_rgba_unmultiplied(0, 0, 0, 190),
        "place-label halo"
    );
    // A `PaneMap` built without a scene is still the shipped look.
    assert_eq!(PaneMap::default().chrome, slate);
}

/// Ground, ink and halo all follow the chosen preset - measured on the
/// emitted shapes. Slate's ground is still the colour that shipped, and
/// Daylight's is light, which the hard-coded background could never be.
#[test]
fn the_pane_paints_the_chosen_presets_ground_ink_and_halo() {
    for preset in MapStylePreset::ALL {
        let map = map_for(preset);
        assert!(
            !map.geometry.as_ref().expect("geometry").labels.is_empty(),
            "no label candidates near KTLX to test with"
        );
        let shapes = painted(&map);
        let ground = ground(&shapes);
        assert_eq!(
            ground,
            chrome_color(preset.chrome().canvas),
            "{} painted another look's ground",
            preset.id()
        );
        match preset {
            MapStylePreset::Slate => {
                assert_eq!(ground, egui::Color32::from_rgb(6, 9, 13), "Slate moved")
            }
            MapStylePreset::Daylight => {
                assert_eq!(ground, egui::Color32::from_rgb(232, 236, 239));
                assert!(ground.r() > 200, "Daylight is still dark: {ground:?}");
            }
            _ => {}
        }
        let colors = text_colors(&shapes);
        for (what, color) in [
            ("ink", chrome_color(preset.chrome().label_ink)),
            ("halo", chrome_color(preset.chrome().label_halo)),
        ] {
            assert!(
                colors.contains(&color),
                "{} drew no label {what} in {color:?}; saw {colors:?}",
                preset.id()
            );
        }
    }
}

/// A pane with something of every kind on it: three site markers in the three
/// states, one warning polygon with a tag, and a pointer sitting on the pane
/// so the geographic readout draws at all.
///
/// The empty `PaneMap` the tests above use exercises the background and the
/// place labels and nothing else, which is exactly how six hard-coded colours
/// survived the first pass at this bug.
fn populated_map(preset: MapStylePreset) -> PaneMap {
    let mut map = map_for(preset);
    map.sites = Arc::from(vec![
        // Under the pointer: the hovered state.
        PlacedSite {
            id: "KHOV".to_owned(),
            world: WorldPoint::ORIGIN,
        },
        // The tuned site, well clear of the pointer's hit slack.
        PlacedSite {
            id: "KACT".to_owned(),
            world: WorldPoint::new(20.0, 0.0),
        },
        PlacedSite {
            id: "KIDL".to_owned(),
            world: WorldPoint::new(-20.0, 0.0),
        },
    ]);
    map.active_site = Some("KACT".to_owned());
    map.hazards = Arc::from(vec![PlacedHazard {
        color: egui::Color32::from_rgb(255, 60, 60),
        tag: "TOR".to_owned(),
        points: vec![
            WorldPoint::new(-12.0, 12.0),
            WorldPoint::new(12.0, 12.0),
            WorldPoint::new(0.0, 24.0),
        ],
        triangles: vec![[0, 1, 2]],
        motion: None,
        emphatic: true,
    }]);
    map
}

/// Where the pointer sits: the pane centre, which is also the radar and the
/// first site marker, because `Camera2D::default` is centred on the origin.
const POINTER: egui::Pos2 = egui::pos2(300.0, 300.0);

/// Every shape one `draw_pane` emitted with the pointer resting on the pane.
///
/// A pointer is not decoration here: `draw_cursor_readout` returns before
/// painting anything unless the pane is hovered, and the hovered site state
/// does not exist without one.
fn painted_hovered(map: &PaneMap, probe: Option<&str>) -> Vec<egui::Shape> {
    let context = egui::Context::default();
    let mut shapes = Vec::new();
    for _ in 0..2 {
        let input = egui::RawInput {
            events: vec![egui::Event::PointerMoved(POINTER)],
            ..Default::default()
        };
        let output = context.run_ui(input, |ui| {
            egui::CentralPanel::default().show_inside(ui, |ui| {
                let overlay = PaneOverlay {
                    legend: None,
                    table: None,
                    product_name: "REF",
                    badges: &[],
                    probe,
                };
                draw_pane(
                    ui,
                    PaneId::new(0).expect("pane 0"),
                    PANE,
                    true,
                    Camera2D::default(),
                    NavTuning::default(),
                    None,
                    map,
                    "1 - REF (dBZ)",
                    "",
                    &overlay,
                );
            });
        });
        shapes = output.shapes.into_iter().map(|c| c.shape).collect();
    }
    shapes
}

/// Every colour the pass used, whatever kind of shape carried it.
///
/// Deliberately indiscriminate: the question these tests ask is whether a
/// given byte pattern reached the frame at all, and a colour that has moved
/// from a stroke to a fill is still the same look.
fn every_color(shapes: &[egui::Shape]) -> Vec<egui::Color32> {
    fn walk(shape: &egui::Shape, colors: &mut Vec<egui::Color32>) {
        match shape {
            egui::Shape::Text(text) => colors.push(text.fallback_color),
            egui::Shape::Rect(rect) => {
                colors.push(rect.fill);
                colors.push(rect.stroke.color);
            }
            egui::Shape::Circle(circle) => {
                colors.push(circle.fill);
                colors.push(circle.stroke.color);
            }
            egui::Shape::LineSegment { stroke, .. } => colors.push(stroke.color),
            egui::Shape::Path(path) => {
                colors.push(path.fill);
                // A `PathStroke` may carry a callback instead of a colour;
                // only the solid case has bytes to compare.
                if let egui::epaint::ColorMode::Solid(color) = path.stroke.color {
                    colors.push(color);
                }
            }
            egui::Shape::Mesh(mesh) => {
                colors.extend(mesh.vertices.iter().map(|vertex| vertex.color));
            }
            // egui nests whenever a helper emits more than one shape at once,
            // so a non-recursive walk would quietly miss whole widgets.
            egui::Shape::Vec(nested) => {
                for shape in nested {
                    walk(shape, colors);
                }
            }
            _ => {}
        }
    }
    let mut colors = Vec::new();
    for shape in shapes {
        walk(shape, &mut colors);
    }
    colors.sort_by_key(|color| color.to_array());
    colors.dedup();
    colors
}

/// Every string this frame drew, with the colour it was drawn in and where.
///
/// Recursive for the same reason `every_color` is: `Shape::Vec` nests.
fn texts(shapes: &[egui::Shape]) -> Vec<(egui::Color32, String)> {
    fn walk(shape: &egui::Shape, out: &mut Vec<(egui::Color32, String)>) {
        match shape {
            egui::Shape::Text(text) => {
                out.push((text.fallback_color, text.galley.text().to_owned()));
            }
            egui::Shape::Vec(nested) => {
                for shape in nested {
                    walk(shape, out);
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    for shape in shapes {
        walk(shape, &mut out);
    }
    out
}

/// The eight marks a pane paints straight onto its own ground, and the chrome
/// token each one must come from.
fn bare_canvas_marks(chrome: map_scene::MapChrome) -> [(&'static str, egui::Color32); 8] {
    [
        ("label ink", chrome_color(chrome.label_ink)),
        ("label halo", chrome_color(chrome.label_halo)),
        ("cursor readout", chrome_color(chrome.readout_ink)),
        ("probe readout", chrome_color(chrome.probe_ink)),
        ("range ring", chrome_color(chrome.range_ring)),
        ("radar origin dot", chrome_color(chrome.origin_dot)),
        ("idle site", chrome_color(chrome.site_ink)),
        ("active site", chrome_color(chrome.site_active_ink)),
    ]
}

/// Everything the pane draws on bare canvas follows the chosen look - not just
/// the background and the place names.
///
/// The hovered site is checked separately below, because it is the one mark
/// whose presence depends on where the pointer is rather than on what is on
/// the map.
#[test]
fn every_mark_the_pane_paints_on_its_ground_comes_from_the_chrome() {
    for preset in MapStylePreset::ALL {
        let map = populated_map(preset);
        let shapes = painted_hovered(&map, Some("47.5 dBZ"));
        let colors = every_color(&shapes);
        for (what, color) in bare_canvas_marks(preset.chrome()) {
            assert!(
                colors.contains(&color),
                "{} painted no {what} in {color:?}",
                preset.id()
            );
        }
        assert!(
            colors.contains(&chrome_color(preset.chrome().site_hover_ink)),
            "{} painted no hovered site marker",
            preset.id()
        );
    }
}

/// The test that would have caught the six colours the first pass left behind.
///
/// `Daylight` is the only look whose ground inverts, so every one of Slate's
/// furniture constants is wrong on it - and wrong in the specific way that
/// hides it, since all eight are near-white and so is the Daylight pane. If
/// any of them still reaches the frame, some paint site is still reading a
/// literal instead of the chrome, and this fails naming it.
///
/// Written against the chrome rather than against pasted literals so the two
/// cannot drift, but `slate_chrome_is_every_constant_the_pane_replaced` in
/// `style_presets.rs` pins those same values to the literals, so the pair of
/// tests together is a check against the constants themselves.
#[test]
fn the_daylight_pane_contains_none_of_the_shipped_dark_look() {
    let map = populated_map(MapStylePreset::Daylight);
    let colors = every_color(&painted_hovered(&map, Some("47.5 dBZ")));
    let slate = MapStylePreset::Slate.chrome();
    for (what, color) in bare_canvas_marks(slate) {
        assert!(
            !colors.contains(&color),
            "the Daylight pane still paints Slate's {what} ({color:?}): that site \
             is reading a hard-coded colour"
        );
    }
    assert!(
        !colors.contains(&chrome_color(slate.site_hover_ink)),
        "the Daylight pane still paints Slate's hovered site marker"
    );
}

/// A warning tag is drawn with a one-pixel halo so it reads over a 70 dBZ
/// core, and that halo is the pane's own ground. On a light pane it has to
/// become a light halo or the tag loses its outline against everything except
/// the radar.
///
/// This site has no test of its own otherwise: the `PaneMap` the other tests
/// build carries no hazards, so reverting this one line to its literal used to
/// leave the whole crate green.
#[test]
fn the_hazard_tag_halo_is_the_panes_own_ground() {
    for preset in MapStylePreset::ALL {
        let map = populated_map(preset);
        let shapes = painted_hovered(&map, None);
        let ground = chrome_color(preset.chrome().canvas);
        let halo_draws = texts(&shapes)
            .into_iter()
            .filter(|(color, drawn)| *color == ground && drawn == "TOR")
            .count();
        assert_eq!(
            halo_draws,
            4,
            "{} drew {halo_draws} of the four TOR halo offsets in its own ground {ground:?}",
            preset.id()
        );
    }
}

/// The first link in the chain, driven rather than read.
///
/// Everything else in this file starts from a `PaneMap` that already carries a
/// chrome. The step before that - an operator opening the toolbar's basemap
/// combo box and clicking a name - was the one part of the chain nobody had
/// executed: `basemap_menu` had been verified by reading it and by an
/// equivalence test on `MapStylePreset::for_style`, which is not the same as
/// proving the widget fires. These click a real `egui::ComboBox` with
/// synthetic pointer events and check what came out the far end.
///
/// They live beside the pane's paint tests, rather than in `app_support.rs`
/// where the picker does, because that file was off limits to this change.
#[cfg(test)]
mod the_picker_itself {
    use super::*;

    /// Where a piece of text was drawn, if this frame drew it.
    ///
    /// Clicking a widget needs a point inside it, and a row's own label is the
    /// only handle available from outside: neither `basemap_menu` nor
    /// `ComboBox` hands back a rectangle, and hard-coding a guess at the
    /// popup's layout would be a test of egui's spacing constants rather than
    /// of the picker.
    fn text_position(shapes: &[egui::Shape], wanted: &str) -> Option<egui::Pos2> {
        fn walk(shape: &egui::Shape, wanted: &str) -> Option<egui::Pos2> {
            match shape {
                egui::Shape::Text(text) if text.galley.text().trim() == wanted => {
                    Some(text.galley.rect.translate(text.pos.to_vec2()).center())
                }
                egui::Shape::Vec(nested) => nested.iter().find_map(|s| walk(s, wanted)),
                _ => None,
            }
        }
        shapes.iter().find_map(|shape| walk(shape, wanted))
    }

    fn pointer(position: egui::Pos2, pressed: bool) -> Vec<egui::Event> {
        vec![
            egui::Event::PointerMoved(position),
            egui::Event::PointerButton {
                pos: position,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            },
        ]
    }

    /// One frame of the toolbar picker; returns the shapes it emitted.
    fn frame(
        context: &egui::Context,
        scene: &mut map_scene::MapSceneController,
        events: Vec<egui::Event>,
    ) -> Vec<egui::Shape> {
        let input = egui::RawInput {
            events,
            ..Default::default()
        };
        // A store at a path that never exists and is never saved: the picker
        // needs one for the Dim slider's persistence write, and these tests
        // only assert on the shapes.
        let mut store = settings::SettingsStore::open(
            std::env::temp_dir().join("radar-workstation-chrome-tests-no-settings.json"),
        );
        let output = context.run_ui(input, |ui| {
            crate::app_support::basemap_menu(ui, scene, &mut store);
        });
        output.shapes.into_iter().map(|c| c.shape).collect()
    }

    /// Press and release on a point, the way `product_picker`'s tests do.
    fn click(
        context: &egui::Context,
        scene: &mut map_scene::MapSceneController,
        position: egui::Pos2,
    ) {
        frame(context, scene, pointer(position, true));
        frame(context, scene, pointer(position, false));
    }

    /// Open the picker and click one preset by the name shown in the list.
    ///
    /// Several frames rather than one call, because that is what the widget
    /// needs: the popup does not exist until the button has been clicked, and
    /// its rows have no position until it has been laid out once.
    fn choose(
        context: &egui::Context,
        scene: &mut map_scene::MapSceneController,
        preset: MapStylePreset,
    ) {
        let showing = MapStylePreset::for_style(scene.style())
            .expect("the controller is holding a preset style")
            .label();
        let closed = frame(context, scene, Vec::new());
        let button = text_position(&closed, showing)
            .unwrap_or_else(|| panic!("the closed combo drew no {showing:?} label to click"));
        click(context, scene, button);

        for _ in 0..8 {
            let open = frame(context, scene, Vec::new());
            if let Some(row) = text_position(&open, preset.label()) {
                click(context, scene, row);
                // One more idle frame, so the write-back at the end of
                // `basemap_menu` has run with the new selection in hand.
                frame(context, scene, Vec::new());
                return;
            }
        }
        panic!("the popup never drew a row labelled {:?}", preset.label());
    }

    /// Click each preset in the real combo box and watch the controller move.
    ///
    /// This is the measurement the chain was missing at its top end: not
    /// "`set_style` works when called", but "the widget calls it".
    #[test]
    fn clicking_a_row_sets_the_style_and_the_chrome_the_pane_will_paint() {
        for preset in MapStylePreset::ALL {
            // A fresh controller per preset, so every click starts from the
            // shipped look and has to travel the whole way.
            let context = egui::Context::default();
            let mut scene = map_scene::MapSceneController::new(|| {});
            assert_eq!(
                scene.style(),
                MapStylePreset::Slate.style(),
                "a new controller no longer starts on the shipped look"
            );

            choose(&context, &mut scene, preset);

            assert_eq!(
                scene.style(),
                preset.style(),
                "clicking {:?} did not reach set_style",
                preset.label()
            );
            // And the chrome the pane is handed for that style is this
            // preset's, which is where the rest of this file starts.
            assert_eq!(
                map_scene::MapChrome::for_style(scene.style()),
                preset.chrome(),
                "clicking {:?} would paint another look's ground",
                preset.label()
            );
        }
    }

    /// Two choices in a row, so the picker is shown to move *between* looks
    /// rather than only away from the default - and to leave a choice alone on
    /// the frames in between.
    #[test]
    fn a_second_choice_replaces_the_first_and_neither_drifts_back() {
        let context = egui::Context::default();
        let mut scene = map_scene::MapSceneController::new(|| {});

        choose(&context, &mut scene, MapStylePreset::Daylight);
        assert_eq!(scene.style(), MapStylePreset::Daylight.style());

        // Twenty untouched frames. The picker runs every frame the toolbar
        // does, so one that rewrote the style when nobody clicked would undo
        // the operator's choice a frame later.
        for _ in 0..20 {
            frame(&context, &mut scene, Vec::new());
        }
        assert_eq!(
            scene.style(),
            MapStylePreset::Daylight.style(),
            "the picker walked the style back on its own"
        );

        choose(&context, &mut scene, MapStylePreset::HighContrast);
        assert_eq!(scene.style(), MapStylePreset::HighContrast.style());
    }
}
