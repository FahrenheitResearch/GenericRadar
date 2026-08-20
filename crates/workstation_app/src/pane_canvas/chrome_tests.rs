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
        annotation: crate::annotation::Annotation::default(),
        units: crate::units::UnitSystem::default(),
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
                    filter_notice: None,
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

// --- Readout & annotation, measured on the frame ---------------------------
//
// These are the end-to-end half of the settings audit: not "the value reached
// a struct" but "the pane painted something different because of it". Every
// assertion here reads the shapes `draw_pane` actually emitted.

/// Ring radii, in screen points, off the emitted circles.
///
/// Filtered by the ring stroke's own width so the origin dot - a filled circle
/// with no stroke - and anything else round stay out of the answer.
fn ring_radii(shapes: &[egui::Shape]) -> Vec<f32> {
    shapes
        .iter()
        .filter_map(|shape| match shape {
            egui::Shape::Circle(circle) if circle.stroke.width == 0.8 => Some(circle.radius),
            _ => None,
        })
        .collect()
}

/// Every string the pass drew, without its colour.
fn strings(shapes: &[egui::Shape]) -> Vec<String> {
    shapes
        .iter()
        .filter_map(|shape| match shape {
            egui::Shape::Text(text) => Some(text.galley.text().to_owned()),
            _ => None,
        })
        .collect()
}

/// The line the corner readout drew, if it drew one. It is the only string
/// that carries a degree sign and a compass letter.
fn corner_readout(shapes: &[egui::Shape]) -> Option<String> {
    strings(shapes)
        .into_iter()
        .find(|text| text.contains('\u{b0}') && (text.contains('N') || text.contains('S')))
}

/// The default pane draws the ladder it has always drawn: six rings at 50,
/// 100, 150, 200, 300 and 400 km, on the default camera's 0.35 km per point.
#[test]
fn the_default_pane_draws_the_ring_ladder_it_always_drew() {
    let map = map_for(MapStylePreset::Slate);
    let radii = ring_radii(&painted(&map));
    let scale = Camera2D::default().km_per_point;
    let expected: Vec<f32> = crate::annotation::SHIPPED_RING_LADDER_KM
        .iter()
        .map(|km| (*km as f32) / scale)
        .collect();
    assert_eq!(
        radii.len(),
        6,
        "the shipped ladder has six rings: {radii:?}"
    );
    for (drawn, want) in radii.iter().zip(expected.iter()) {
        assert!(
            (drawn - want).abs() < 0.01,
            "ring at {drawn} points, expected {want}"
        );
    }
    // And nothing is written on them.
    assert!(
        !strings(&painted(&map))
            .iter()
            .any(|text| text.ends_with(" km")),
        "the shipped pane writes no distance on a ring"
    );
}

/// The ladder setting reaches the picture: a different choice, a different set
/// of circles, measured rather than asserted about a struct.
#[test]
fn changing_the_ring_ladder_changes_the_circles_the_pane_paints() {
    let mut map = map_for(MapStylePreset::Slate);
    map.annotation = crate::annotation::Annotation {
        ring_ladder: crate::annotation::RingLadder::Every100,
        ring_count: 3,
        ..crate::annotation::Annotation::default()
    };
    let radii = ring_radii(&painted(&map));
    let scale = Camera2D::default().km_per_point;
    assert_eq!(radii.len(), 3, "three rings were asked for: {radii:?}");
    for (index, drawn) in radii.iter().enumerate() {
        let want = (100.0 * (index + 1) as f32) / scale;
        assert!(
            (drawn - want).abs() < 0.01,
            "ring {index} at {drawn}, want {want}"
        );
    }

    // Zero rings is a legal answer and leaves the pane with none.
    map.annotation.ring_count = 0;
    assert!(ring_radii(&painted(&map)).is_empty());
}

/// The distance unit reaches the ring LABELS without moving the rings. A ring
/// at 100 km is at 100 km in Nebraska and in Iowa; only the number beside it
/// changes.
#[test]
fn the_distance_unit_relabels_the_rings_without_moving_them() {
    let mut map = map_for(MapStylePreset::Slate);
    map.annotation = crate::annotation::Annotation {
        ring_labels: true,
        ..crate::annotation::Annotation::default()
    };
    let metric = painted(&map);
    assert!(
        strings(&metric).contains(&"50 km".to_owned()),
        "labels on: {:?}",
        strings(&metric)
    );

    map.units = crate::units::UnitSystem {
        distance: crate::units::DistanceUnit::StatuteMiles,
        ..crate::units::UnitSystem::default()
    };
    let imperial = painted(&map);
    // 50 km is 31 statute miles, written where "50 km" was.
    assert!(
        strings(&imperial).contains(&"31 mi".to_owned()),
        "in miles: {:?}",
        strings(&imperial)
    );
    // The named ladder keeps its kilometre radii, so the circles are identical.
    assert_eq!(ring_radii(&metric), ring_radii(&imperial));
}

/// The corner readout: its units, its precision, and turning it off.
#[test]
fn the_corner_readout_follows_its_settings() {
    let mut map = populated_map(MapStylePreset::Slate);

    let shipped = corner_readout(&painted_hovered(&map, None))
        .expect("the shipped pane writes a geographic readout under the pointer");
    assert!(shipped.contains(" km "), "shipped readout: {shipped}");
    // Four decimals on the latitude, one on the range - the two literals the
    // format string used to carry.
    assert!(
        shipped.matches('.').count() >= 3,
        "shipped readout: {shipped}"
    );

    map.units = crate::units::UnitSystem {
        distance: crate::units::DistanceUnit::NauticalMiles,
        ..crate::units::UnitSystem::default()
    };
    let nautical = corner_readout(&painted_hovered(&map, None)).expect("still drawn");
    assert!(nautical.contains(" nm "), "nautical readout: {nautical}");
    assert!(!nautical.contains(" km "), "nautical readout: {nautical}");

    // Precision.
    map.units = crate::units::UnitSystem::default();
    map.annotation.coordinate_decimals = 2;
    let coarse = corner_readout(&painted_hovered(&map, None)).expect("still drawn");
    // KTLX is 35.3333625793457 N: four places writes 35.3334, two writes 35.33.
    assert!(coarse.contains("35.33\u{b0}N"), "coarse readout: {coarse}");
    assert!(
        shipped.contains("35.3334\u{b0}N"),
        "shipped readout: {shipped}"
    );

    // And off means off - while the value readout above it is untouched.
    map.annotation = crate::annotation::Annotation {
        corner_readout: crate::annotation::CornerReadout::Off,
        ..crate::annotation::Annotation::default()
    };
    let probe = "REF 52.5 dBZ | 43.1 km 247.4 deg";
    let shapes = painted_hovered(&map, Some(probe));
    assert!(corner_readout(&shapes).is_none(), "readout was turned off");
    assert!(
        strings(&shapes).iter().any(|text| text == probe),
        "the probe readout is a different line and stays"
    );
}

/// The marker and label sizes reach the frame.
#[test]
fn the_site_marker_size_setting_changes_the_boxes_the_pane_paints() {
    let map = populated_map(MapStylePreset::Slate);
    let marker_widths = |map: &PaneMap| -> Vec<f32> {
        let mut widths: Vec<f32> = painted_hovered(map, None)
            .iter()
            .filter_map(|shape| match shape {
                // The marker box is the only rounded rect the pane draws with
                // a three-point halo stroke.
                egui::Shape::Rect(rect) if rect.stroke.width == 3.0 => Some(rect.rect.width()),
                _ => None,
            })
            .collect();
        widths.sort_by(f32::total_cmp);
        widths.dedup();
        widths
    };
    assert_eq!(
        marker_widths(&map),
        vec![10.0],
        "the shipped marker is 10 points across"
    );

    let mut bigger = map;
    bigger.annotation = crate::annotation::Annotation {
        site_marker_points: 18.0,
        ..crate::annotation::Annotation::default()
    };
    assert_eq!(marker_widths(&bigger), vec![18.0]);
}

/// The marker ceiling keeps the sites nearest the middle of the pane.
///
/// The defect this closes: the loop used to `break` at the ceiling in
/// whatever order the site catalog happened to be in, so an analyst who set
/// "Most markers per pane" to a small number lost arbitrary sites - the one
/// under the pointer could vanish while one in the corner survived, and
/// nothing on screen said why. Read off the frame: the three markers are
/// listed centre, right, left, and a ceiling of one must keep the CENTRE one.
#[test]
fn the_marker_ceiling_keeps_the_sites_nearest_the_middle_of_the_pane() {
    let mut map = populated_map(MapStylePreset::Slate);
    // No labels, so the only strings left from the site layer are the ids the
    // markers themselves carry.
    map.site_labels = SiteLabelMode::Always;

    let ids = |map: &PaneMap| -> Vec<String> {
        strings(&painted_hovered(map, None))
            .into_iter()
            // A site id, not a place name: four characters, all upper case.
            // "Kingfisher" is on this basemap and would otherwise pass a
            // `starts_with('K')` filter.
            .filter(|text| text.len() == 4 && text.chars().all(|c| c.is_ascii_uppercase()))
            .collect()
    };

    let mut all = ids(&map);
    all.sort();
    assert_eq!(
        all,
        ["KACT", "KHOV", "KIDL"],
        "the default ceiling of 250 drops nothing"
    );

    // A ceiling of one. KHOV sits on the pane centre; KACT and KIDL are 20 km
    // either side of it, so nearest-first must keep KHOV. Catalog order would
    // also have kept KHOV here, so the discriminating case is the one below.
    map.annotation = crate::annotation::Annotation {
        site_marker_max: 1,
        ..crate::annotation::Annotation::default()
    };
    assert_eq!(ids(&map), ["KHOV"]);

    // The discriminating case: the catalog's FIRST site is the far one. Under
    // the old `break`-at-the-limit rule this drew KFAR and dropped the site
    // sitting in the middle of the pane.
    map.sites = Arc::from(vec![
        PlacedSite {
            id: "KFAR".to_owned(),
            world: WorldPoint::new(-60.0, 0.0),
        },
        PlacedSite {
            id: "KHOV".to_owned(),
            world: WorldPoint::ORIGIN,
        },
    ]);
    assert_eq!(
        ids(&map),
        ["KHOV"],
        "the ceiling must drop the far marker, not the near one"
    );

    // Two survive out of three, and they are the two nearest the middle.
    map.sites = Arc::from(vec![
        PlacedSite {
            id: "KFAR".to_owned(),
            world: WorldPoint::new(-60.0, 0.0),
        },
        PlacedSite {
            id: "KMID".to_owned(),
            world: WorldPoint::new(10.0, 0.0),
        },
        PlacedSite {
            id: "KHOV".to_owned(),
            world: WorldPoint::ORIGIN,
        },
    ]);
    map.annotation = crate::annotation::Annotation {
        site_marker_max: 2,
        ..crate::annotation::Annotation::default()
    };
    assert_eq!(
        ids(&map),
        ["KMID", "KHOV"],
        "kept nearest-first, painted in catalog order"
    );
}

/// A ring label is drawn on a plate, so it survives the place name under it.
///
/// The collision is structural rather than incidental: the labels form a
/// column due north of the radar, and at most sites something on the basemap
/// is written there. In the KDVN proof renders "62 mi" landed on the county
/// name "Dubuque". The label is opt-in, so if it is on it has to be readable.
#[test]
fn a_ring_label_is_written_on_a_plate_so_it_survives_the_map_under_it() {
    let mut map = map_for(MapStylePreset::Slate);
    map.annotation = crate::annotation::Annotation {
        ring_labels: true,
        ..crate::annotation::Annotation::default()
    };
    let shapes = painted(&map);
    let labels: Vec<String> = strings(&shapes)
        .into_iter()
        .filter(|text| text.ends_with(" km"))
        .collect();
    assert_eq!(
        labels,
        ["50 km", "100 km", "150 km", "200 km", "300 km", "400 km"],
        "the ladder labels itself in the analyst's distance unit"
    );

    // One plate per label, in the map's own halo colour, at least as wide as
    // the text it sits under.
    let halo = chrome_color(MapStylePreset::Slate.chrome().label_halo);
    let plates: Vec<f32> = shapes
        .iter()
        .filter_map(|shape| match shape {
            egui::Shape::Rect(rect) if rect.fill == halo && rect.stroke.width == 0.0 => {
                Some(rect.rect.width())
            }
            _ => None,
        })
        .collect();
    assert_eq!(plates.len(), labels.len(), "one plate per ring label");
    for width in &plates {
        assert!(*width > 10.0, "a plate {width} points wide hides nothing");
    }

    // And the shipped pane, which does not label its rings, paints no plate
    // at all - the default frame is unchanged by this.
    let plain = map_for(MapStylePreset::Slate);
    assert!(
        !painted(&plain).iter().any(|shape| matches!(
            shape,
            egui::Shape::Rect(rect) if rect.fill == halo && rect.stroke.width == 0.0
        )),
        "no ring labels, no plates"
    );
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
                    filter_notice: None,
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

/// Who gets a click that lands on the FILTERED band.
///
/// The band is painted last and in its own opaque ground, over everything the
/// pane has already drawn - including the radar site markers, which are
/// hit-tested anywhere in the pane inside a halo of `SITE_MARKER_HALF +
/// SITE_CLICK_SLACK`. So a marker that happens to sit under the band is a
/// target the analyst cannot see, and `draw_pane` used to hand its id back
/// alongside `clear_filter_clicked`. `app.rs` consumes `clicked_site` and
/// `ctrl_clicked_lon_lat` in an else-if chain that the band's own flag does
/// not guard, so one click on the band both cleared the filter and called
/// `start_live` on the invisible marker: the analyst asks to see the echo that
/// was being hidden, and the volume they were reading is replaced by another
/// radar's.
///
/// Every test here is paired with the same click on a pane with no band, which
/// is what stops it passing vacuously: the pair proves the marker really is
/// under that point and really would have been taken.
#[cfg(test)]
mod who_gets_the_band_click {
    use super::*;

    const VIEWPORT: ViewportMetrics = ViewportMetrics {
        width_points: 600.0,
        height_points: 600.0,
        pixels_per_point: 1.0,
    };

    pub(super) const NOTICE: &str =
        "FILTERED - hiding REF below 20 dBZ - click here to show everything";

    /// The centre of the band `draw_filter_band` paints for `PANE`.
    fn band_center() -> egui::Pos2 {
        egui::pos2(
            PANE.center().x,
            PANE.top() + HEADER_HEIGHT + FILTER_BAND_HEIGHT * 0.5,
        )
    }

    /// The Slate map with one site placed exactly under `at`.
    ///
    /// Placed through the camera's own inverse rather than by guessing a
    /// latitude: the question is about screen geometry, and a site that
    /// happened to land two points off would make the whole test vacuous.
    pub(super) fn map_with_a_site_under(at: egui::Pos2) -> PaneMap {
        let mut map = map_for(MapStylePreset::Slate);
        let local = ScreenPoint::new(at.x - PANE.left(), at.y - PANE.top());
        let world = Camera2D::default().screen_to_world(local, VIEWPORT);
        map.sites = Arc::from(vec![PlacedSite {
            id: "kdvn".to_owned(),
            world,
        }]);
        map
    }

    /// One press-and-release on `at`, through three real egui passes.
    ///
    /// Three because egui resolves hover against the rects registered on the
    /// previous pass, and a site marker is only a candidate while the pointer
    /// is over it - so the move has to land before the press.
    fn click_at(
        map: &PaneMap,
        at: egui::Pos2,
        notice: Option<&str>,
        modifiers: egui::Modifiers,
    ) -> PaneInteraction {
        let context = egui::Context::default();
        let mut last = None;
        let passes = [
            (1.0_f64, vec![egui::Event::PointerMoved(at)]),
            (
                1.1,
                vec![
                    egui::Event::PointerMoved(at),
                    egui::Event::PointerButton {
                        pos: at,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers,
                    },
                ],
            ),
            (
                1.2,
                vec![egui::Event::PointerButton {
                    pos: at,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers,
                }],
            ),
        ];
        for (time, events) in passes {
            let input = egui::RawInput {
                time: Some(time),
                // The window system sets the modifier state before it
                // dispatches the button that carries it, and `nearest_site`
                // reads `InputState::modifiers` - a test that set only the
                // event's own would be testing a state no window system
                // produces.
                modifiers,
                events,
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1000.0, 800.0),
                )),
                ..Default::default()
            };
            let mut result = None;
            let _ = context.run_ui(input, |ui| {
                let overlay = PaneOverlay {
                    legend: None,
                    table: None,
                    product_name: "REF",
                    badges: &[],
                    probe: None,
                    filter_notice: notice,
                };
                result = Some(draw_pane(
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
                ));
            });
            last = result;
        }
        last.expect("the pass body runs")
    }

    /// The control: with no band, this exact point IS a site click.
    #[test]
    fn without_the_band_the_marker_under_it_takes_the_click() {
        let at = band_center();
        let map = map_with_a_site_under(at);
        let plain = click_at(&map, at, None, egui::Modifiers::NONE);
        assert_eq!(
            plain.clicked_site.as_deref(),
            Some("kdvn"),
            "the marker is not under the band at all - the pairs below prove nothing"
        );
        assert!(!plain.clear_filter_clicked);

        let ctrl = click_at(&map, at, None, egui::Modifiers::CTRL);
        assert!(
            ctrl.ctrl_clicked_lon_lat.is_some(),
            "Ctrl+click on this point does not reach the radar switch - the pair below \
             proves nothing"
        );
        assert!(!ctrl.clear_filter_clicked);
    }

    /// A plain click on the band clears the filter and does nothing else.
    #[test]
    fn a_click_on_the_band_does_not_also_change_the_radar_site() {
        let at = band_center();
        let map = map_with_a_site_under(at);
        let interaction = click_at(&map, at, Some(NOTICE), egui::Modifiers::NONE);

        assert!(
            interaction.clear_filter_clicked,
            "the band did not take the click at all"
        );
        assert_eq!(
            interaction.clicked_site, None,
            "clearing the filter also loaded whatever site marker the band was painted over"
        );
        assert_eq!(interaction.ctrl_clicked_lon_lat, None);
        assert!(
            !interaction.clicked,
            "the band's click also reached the pane's ordinary click consumers"
        );
    }

    /// And a Ctrl+click on it does not load the nearest S-band radar either.
    #[test]
    fn a_ctrl_click_on_the_band_does_not_also_load_the_nearest_radar() {
        let at = band_center();
        let map = map_with_a_site_under(at);
        let interaction = click_at(&map, at, Some(NOTICE), egui::Modifiers::CTRL);

        assert!(
            interaction.clear_filter_clicked,
            "the band did not take the Ctrl+click at all"
        );
        assert_eq!(
            interaction.ctrl_clicked_lon_lat, None,
            "clearing the filter also swapped the pane onto the nearest S-band radar"
        );
        assert_eq!(interaction.clicked_site, None);
        assert!(!interaction.clicked);
    }

    /// The suppression is the band's, not a blanket ban: a click anywhere else
    /// on a filtered pane still reaches the site marker under it.
    #[test]
    fn a_click_away_from_the_band_still_selects_a_site_on_a_filtered_pane() {
        let at = PANE.center();
        assert!(
            at.y > PANE.top() + HEADER_HEIGHT + FILTER_BAND_HEIGHT,
            "this point is inside the band, so it proves nothing"
        );
        let map = map_with_a_site_under(at);
        let interaction = click_at(&map, at, Some(NOTICE), egui::Modifiers::NONE);

        assert!(!interaction.clear_filter_clicked);
        assert_eq!(
            interaction.clicked_site.as_deref(),
            Some("kdvn"),
            "a filtered pane stopped answering site clicks everywhere, not just on the band"
        );
    }
}

/// The filter indicators are legible on EVERY registered theme, and they are
/// legible there because they do not follow the theme at all.
///
/// The pane's furniture - the FILTERED band, the header, the legend's panel -
/// paints its own opaque ground on purpose, so that a warning cannot be tuned
/// down by picking a different look. That is a claim about paint, and it was
/// made when there were two themes and eight arrived. These measure it, with
/// the same WCAG arithmetic `tests/theme_catalog.rs` audits the chrome with,
/// so a number here and a number there mean the same thing.
mod the_filter_indicators_across_every_theme {
    use super::*;
    use crate::theme::{Appearance, catalog};

    fn relative_luminance(color: egui::Color32) -> f64 {
        fn channel(byte: u8) -> f64 {
            let u = f64::from(byte) / 255.0;
            if u <= 0.04045 {
                u / 12.92
            } else {
                ((u + 0.055) / 1.055).powf(2.4)
            }
        }
        0.2126 * channel(color.r()) + 0.7152 * channel(color.g()) + 0.0722 * channel(color.b())
    }

    fn contrast(a: egui::Color32, b: egui::Color32) -> f64 {
        let (la, lb) = (relative_luminance(a), relative_luminance(b));
        let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
        (hi + 0.05) / (lo + 0.05)
    }

    /// A translucent furniture ground over whatever the radar painted under it.
    fn over(ground: egui::Color32, alpha: u8, backing: egui::Color32) -> egui::Color32 {
        let alpha = f32::from(alpha) / 255.0;
        let mix = |g: u8, b: u8| (f32::from(g) * alpha + f32::from(b) * (1.0 - alpha)) as u8;
        egui::Color32::from_rgb(
            mix(ground.r(), backing.r()),
            mix(ground.g(), backing.g()),
            mix(ground.b(), backing.b()),
        )
    }

    /// The two extremes a pane can put behind its furniture: an empty sweep,
    /// and the brightest pixel any colour table produces.
    const BACKINGS: [egui::Color32; 2] = [egui::Color32::BLACK, egui::Color32::WHITE];

    /// The band's own pair clears AAA, and it is the same pair whatever theme
    /// is installed.
    ///
    /// Both halves matter. The ratio alone would pass a band rewired to the
    /// theme's error ink that happened to clear 7:1 on the themes somebody
    /// checked; the invariance alone would pass a band that was consistently
    /// unreadable.
    #[test]
    fn the_bands_pair_is_the_same_on_every_theme_and_clears_aaa() {
        let context = egui::Context::default();
        for theme in catalog::THEMES {
            crate::theme::apply(&context, &Appearance::by_id(theme.id));
            let ratio = contrast(FILTER_BAND_INK, FILTER_BAND_GROUND);
            assert!(
                ratio >= 7.0,
                "{}: the FILTERED band reads at {ratio:.2}:1, under the 7:1 the one \
                 indicator the safety rule names is held to",
                theme.id
            );
            assert_eq!(
                (FILTER_BAND_GROUND.a(), FILTER_BAND_INK.a()),
                (255, 255),
                "{}: the band went translucent, so its contrast is now whatever \
                 reflectivity happens to be underneath it",
                theme.id
            );
        }
    }

    /// The pane header carries the engine's filter line, and it stays readable
    /// over an empty sweep and over a 70 dBZ core alike.
    ///
    /// Its ground is translucent, so the real contrast is against the
    /// composite rather than against the constant. 218 of 255 is enough that
    /// even pure white underneath leaves a dark bar; this is what says so
    /// rather than a sentence claiming it.
    #[test]
    fn the_pane_headers_two_inks_stay_readable_over_any_echo() {
        for backing in BACKINGS {
            let ground = over(egui::Color32::from_rgb(4, 7, 10), 218, backing);
            for (name, ink) in [
                ("title", HEADER_TITLE_COLOR),
                ("status", HEADER_STATUS_COLOR),
            ] {
                let ratio = contrast(ink, ground);
                assert!(
                    ratio >= 4.5,
                    "the header's {name} reads at {ratio:.2}:1 over {backing:?}, under the \
                     4.5:1 floor - and the status is where the engine says how many gates \
                     it removed"
                );
            }
        }
    }

    /// And the pane paints the SAME filter band, byte for byte, whatever theme
    /// is installed.
    ///
    /// Read off real egui passes rather than off the constants, so it covers
    /// the whole path: a band that started taking its colour from `MapChrome`
    /// or from the theme's error ink fails here even though the constants
    /// above are untouched. That is a plausible future edit - it looks like an
    /// improvement - and it would make the loudest warning in the application
    /// quiet on some themes and not others.
    #[test]
    fn every_theme_paints_the_same_filter_band() {
        let map = super::who_gets_the_band_click::map_with_a_site_under(PANE.center());
        let mut first: Option<(&str, Vec<(egui::Rect, egui::Color32)>)> = None;
        for theme in catalog::THEMES {
            let context = egui::Context::default();
            crate::theme::apply(&context, &Appearance::by_id(theme.id));
            let mut bands = Vec::new();
            for _ in 0..2 {
                let output = context.run_ui(egui::RawInput::default(), |ui| {
                    egui::CentralPanel::default().show_inside(ui, |ui| {
                        let overlay = PaneOverlay {
                            legend: None,
                            table: None,
                            product_name: "REF",
                            badges: &[],
                            probe: None,
                            filter_notice: Some(super::who_gets_the_band_click::NOTICE),
                        };
                        draw_pane(
                            ui,
                            PaneId::new(0).expect("pane 0"),
                            PANE,
                            true,
                            Camera2D::default(),
                            NavTuning::default(),
                            None,
                            &map,
                            "1 - REF (dBZ)",
                            "",
                            &overlay,
                        );
                    });
                });
                bands = output
                    .shapes
                    .into_iter()
                    .filter_map(|clipped| match clipped.shape {
                        egui::Shape::Rect(rect)
                            if (rect.rect.height() - FILTER_BAND_HEIGHT).abs() < 0.5 =>
                        {
                            Some((rect.rect, rect.fill))
                        }
                        _ => None,
                    })
                    .collect();
            }
            assert!(
                !bands.is_empty(),
                "{}: a filtered pane painted no band at all",
                theme.id
            );
            match first {
                None => first = Some((theme.id, bands)),
                Some((other, ref expected)) => assert_eq!(
                    &bands, expected,
                    "{} paints a different FILTERED band from {other}. The band is \
                     furniture, not a mark on the map: it must not follow the theme, or a \
                     warning becomes something an analyst can tune down",
                    theme.id
                ),
            }
        }
    }
}
