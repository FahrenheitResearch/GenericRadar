//! `data_source::warnings::WarningRecord` -> map geometry.
//!
//! The one place the warnings client and the map meet. Two things are decided
//! here, and both are operational judgements rather than plumbing:
//!
//! 1. The **style key**, which picks the colour. A tornado warning and a
//!    tornado emergency are different colours in the NWS colour language, and
//!    the damage-threat escalation subkey is how that is expressed.
//! 2. **Which warnings are drawn at all**: only those in force right now. A
//!    cancelled or expired polygon still in the feed is history, and drawing it
//!    would put a tornado box over a county where the tornado is over.
//!
//! Placement happens once per projection change, not per frame, exactly like
//! the radar site markers: a warning polygon is fixed relative to the
//! projection, so the paint pass only has to apply the camera transform.

use analyst_runtime::WorldPoint;
use color_tables::hazards::hazard_stroke_color;
use data_source::warnings::{Severity, WarningRecord};
use eframe::egui;
use map_scene::RadarProjection;

/// A warning polygon in world kilometres, ready to draw.
#[derive(Clone, Debug)]
pub struct PlacedHazard {
    /// Outline colour, resolved once so the severity tier and the colour can
    /// never disagree.
    pub color: egui::Color32,
    /// Short label drawn at the polygon's top vertex, e.g. `TOR`.
    pub tag: String,
    pub points: Vec<WorldPoint>,
    /// Triangle fill, as indices into `points`. Empty when the ring could not
    /// be triangulated, which leaves the hazard drawn as an outline rather
    /// than not drawn at all.
    pub triangles: Vec<[u32; 3]>,
    /// Storm motion, `(direction it comes FROM in degrees, knots)`.
    pub motion: Option<(u16, u8)>,
    /// Draw thicker. Reserved for the tiers an analyst must not miss.
    pub emphatic: bool,
}

/// Everything in force at `now_rfc3339`, projected into world kilometres.
///
/// The result is ordered least severe first, so the most severe polygon is
/// drawn last and its outline is the one on top where two overlap. A tornado
/// box hidden under a flood box is the failure this ordering exists to prevent.
pub fn place_hazards(
    records: &[WarningRecord],
    now_rfc3339: &str,
    projection: &RadarProjection,
) -> Vec<PlacedHazard> {
    let mut placed: Vec<(u8, PlacedHazard)> = records
        .iter()
        .filter(|record| record.points.len() >= 3)
        .filter(|record| record.is_active_at(now_rfc3339))
        .filter_map(|record| {
            let key = style_key(record);
            let points = project_ring(&record.points, projection)?;
            let [r, g, b, a] = {
                let color = hazard_stroke_color(&key);
                [color.r, color.g, color.b, color.a]
            };
            Some((
                weight(&key),
                PlacedHazard {
                    color: egui::Color32::from_rgba_unmultiplied(r, g, b, a),
                    tag: tag(record),
                    // Triangulated here rather than in the paint pass: the
                    // topology is unchanged by the camera's translate-and-scale,
                    // so it is fixed for as long as the projection is.
                    triangles: triangulate(&points).unwrap_or_default(),
                    points,
                    motion: record.motion,
                    emphatic: matches!(record.severity, Severity::Extreme | Severity::Severe),
                },
            ))
        })
        .collect();
    placed.sort_by_key(|(weight, _)| *weight);
    placed.into_iter().map(|(_, hazard)| hazard).collect()
}

/// Project a whole ring, or nothing.
///
/// A vertex the projection rejects is on the far side of the globe. Keeping the
/// rest of the ring would close the polygon across the gap and draw a line
/// hundreds of kilometres long -- the same artifact the basemap's clipping had
/// to be taught to avoid.
fn project_ring(points: &[(f32, f32)], projection: &RadarProjection) -> Option<Vec<WorldPoint>> {
    points
        .iter()
        .map(|(lon, lat)| projection.try_lon_lat_to_world(f64::from(*lon), f64::from(*lat)))
        .collect()
}

/// Fill triangles for a simple polygon, by ear clipping.
///
/// A fan from vertex zero -- which is what `egui::Shape::convex_polygon` does
/// -- spills outside any concave ring, and warning polygons are routinely
/// concave where a forecaster cut around a county line. This is the same ear
/// clip the legacy viewer fills its hazards with, so both draw a concave
/// warning the same shape.
///
/// Returns `None` for a degenerate or self-intersecting ring; the caller draws
/// the outline alone rather than an invented fill.
fn triangulate(points: &[WorldPoint]) -> Option<Vec<[u32; 3]>> {
    if points.len() < 3 || points.len() > u32::MAX as usize {
        return None;
    }
    // Test the AREA, not its sign. `f64::signum` answers 1.0 for +0.0, so a
    // guard written as `area.signum() == 0.0` never fires and a zero-area ring
    // walks straight into the clipping loop.
    let area = signed_area(points);
    if area == 0.0 {
        return None;
    }
    let winding = area.signum();

    let mut remaining: Vec<usize> = (0..points.len()).collect();
    let mut triangles = Vec::with_capacity(points.len() - 2);
    // Each pass must remove at least one vertex, so a ring this loop cannot
    // reduce is one ear clipping does not handle -- bounded, never spinning.
    let max_passes = points.len() * points.len();
    let mut passes = 0_usize;

    while remaining.len() > 3 && passes < max_passes {
        passes += 1;
        let mut clipped = false;
        for current in 0..remaining.len() {
            let previous = remaining[(current + remaining.len() - 1) % remaining.len()];
            let index = remaining[current];
            let next = remaining[(current + 1) % remaining.len()];
            if !is_ear(points, &remaining, previous, index, next, winding) {
                continue;
            }
            triangles.push([previous as u32, index as u32, next as u32]);
            remaining.remove(current);
            clipped = true;
            break;
        }
        if !clipped {
            return None;
        }
    }

    if remaining.len() == 3 {
        triangles.push([
            remaining[0] as u32,
            remaining[1] as u32,
            remaining[2] as u32,
        ]);
    }
    (!triangles.is_empty()).then_some(triangles)
}

fn is_ear(
    points: &[WorldPoint],
    remaining: &[usize],
    previous: usize,
    index: usize,
    next: usize,
    winding: f64,
) -> bool {
    let (a, b, c) = (points[previous], points[index], points[next]);
    let cross = cross(a, b, c);
    if cross == 0.0 || cross.signum() != winding {
        return false;
    }
    !remaining.iter().any(|candidate| {
        let candidate = *candidate;
        candidate != previous
            && candidate != index
            && candidate != next
            && in_triangle(points[candidate], a, b, c)
    })
}

fn signed_area(points: &[WorldPoint]) -> f64 {
    let mut area = 0.0;
    for index in 0..points.len() {
        let current = points[index];
        let next = points[(index + 1) % points.len()];
        area += current.east_km * next.north_km - next.east_km * current.north_km;
    }
    area * 0.5
}

fn cross(a: WorldPoint, b: WorldPoint, c: WorldPoint) -> f64 {
    (b.east_km - a.east_km) * (c.north_km - a.north_km)
        - (b.north_km - a.north_km) * (c.east_km - a.east_km)
}

fn in_triangle(point: WorldPoint, a: WorldPoint, b: WorldPoint, c: WorldPoint) -> bool {
    let ab = cross(a, b, point);
    let bc = cross(b, c, point);
    let ca = cross(c, a, point);
    (ab >= 0.0 && bc >= 0.0 && ca >= 0.0) || (ab <= 0.0 && bc <= 0.0 && ca <= 0.0)
}

/// Draw-order weight: higher paints later, i.e. on top.
fn weight(style_key: &str) -> u8 {
    match style_key {
        "tornado/catastrophic" => 6,
        "tornado/considerable" => 5,
        "tornado" => 4,
        "severe-thunderstorm/destructive" | "severe-thunderstorm/considerable" => 3,
        "severe-thunderstorm" => 2,
        "flash-flood/catastrophic" | "flash-flood/considerable" => 2,
        _ => 1,
    }
}

/// The colour key for a record: family, plus a damage-threat escalation subkey
/// when the bulletin carried one.
///
/// The subkeys are exactly the ones `color_tables::hazards` declares --
/// inventing a key here silently falls through to the generic colour, which is
/// what `every_emitted_style_key_is_a_real_one` exists to catch.
fn style_key(record: &WarningRecord) -> String {
    let family = match record.phenomenon.as_str() {
        "TO" if record.significance == "A" => return "watch/tornado".to_owned(),
        "SV" if record.significance == "A" => return "watch/severe-thunderstorm".to_owned(),
        _ if record.significance == "A" => return "watch".to_owned(),
        "TO" => "tornado",
        "SV" => "severe-thunderstorm",
        "FF" => "flash-flood",
        "FA" | "FL" => "flood",
        "FW" | "RF" => "fire-weather",
        "MA" | "SM" => "special-marine",
        "SQ" => "snow-squall",
        // Statements carry polygons but no VTEC; they get their own muted
        // colour rather than the unknown-key yellow.
        "SPS" | "MWS" => "special-weather",
        _ => return "other".to_owned(),
    };

    let escalation = record.damage_threat.as_deref().map(str::to_ascii_uppercase);
    match (family, escalation.as_deref()) {
        ("tornado", Some("CATASTROPHIC")) => "tornado/catastrophic".to_owned(),
        ("tornado", Some("CONSIDERABLE")) => "tornado/considerable".to_owned(),
        ("severe-thunderstorm", Some("DESTRUCTIVE")) => {
            "severe-thunderstorm/destructive".to_owned()
        }
        ("severe-thunderstorm", Some("CONSIDERABLE")) => {
            "severe-thunderstorm/considerable".to_owned()
        }
        ("flash-flood", Some("CATASTROPHIC")) => "flash-flood/catastrophic".to_owned(),
        ("flash-flood", Some("CONSIDERABLE")) => "flash-flood/considerable".to_owned(),
        ("flood", Some("CATASTROPHIC")) => "flood/catastrophic".to_owned(),
        ("flood", Some("CONSIDERABLE")) => "flood/considerable".to_owned(),
        _ => family.to_owned(),
    }
}

/// The three or four characters drawn at the top of the polygon.
///
/// Warning-room shorthand, not the full headline: at 9pt over a storm core,
/// anything longer is unreadable and covers the thing it is describing.
fn tag(record: &WarningRecord) -> String {
    let base = match record.phenomenon.as_str() {
        "TO" => "TOR",
        "SV" => "SVR",
        "FF" => "FFW",
        "FA" | "FL" => "FLW",
        "FW" | "RF" => "FIRE",
        "MA" | "SM" => "MAR",
        "SQ" => "SQW",
        "SPS" | "MWS" => "SPS",
        other => other,
    };
    match record.significance.as_str() {
        "A" => format!("{base}-A"),
        _ => base.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use color_tables::hazards::{HAZARD_ESCALATIONS, HAZARD_FAMILIES};

    const NOW: &str = "2026-08-16T20:30:00Z";

    fn record(phenomenon: &str, significance: &str) -> WarningRecord {
        WarningRecord {
            event_id: format!("KOUN.O.{phenomenon}.{significance}.0001"),
            office: "KOUN".to_owned(),
            phenomenon: phenomenon.to_owned(),
            significance: significance.to_owned(),
            action: "NEW".to_owned(),
            event_family: "test".to_owned(),
            headline: None,
            valid_start: Some("2026-08-16T20:00:00Z".to_owned()),
            valid_end: Some("2026-08-16T21:00:00Z".to_owned()),
            severity: Severity::Moderate,
            tornado: None,
            hail_inches: None,
            wind_mph: None,
            damage_threat: None,
            points: vec![(-97.5, 35.2), (-97.0, 35.2), (-97.0, 35.6)],
            bbox: Some([-97.5, 35.2, -97.0, 35.6]),
            motion: Some((230, 40)),
            ugcs: Vec::new(),
        }
    }

    /// KTLX, so the test polygon is a few tens of kilometres from the anchor.
    fn projection() -> RadarProjection {
        RadarProjection::new(35.3333, -97.2778)
    }

    #[test]
    fn a_warning_in_force_becomes_a_polygon_in_world_kilometres() {
        let placed = place_hazards(&[record("TO", "W")], NOW, &projection());
        assert_eq!(placed.len(), 1);
        assert_eq!(placed[0].tag, "TOR");
        assert_eq!(placed[0].points.len(), 3);
        assert_eq!(placed[0].motion, Some((230, 40)));
        // Within a couple of hundred kilometres of the radar, not at the origin
        // and not off in a different hemisphere.
        for point in &placed[0].points {
            assert!(
                point.east_km.abs() < 200.0 && point.north_km.abs() < 200.0,
                "{point:?}"
            );
        }
    }

    /// Every key this module can emit must be one `color_tables::hazards`
    /// actually declares. A typo falls through to the generic yellow and
    /// nobody notices until it matters.
    #[test]
    fn every_emitted_style_key_is_a_real_one() {
        let known: Vec<&str> = HAZARD_FAMILIES
            .iter()
            .chain(HAZARD_ESCALATIONS.iter())
            .copied()
            .collect();
        for phenomenon in [
            "TO", "SV", "FF", "FA", "FL", "FW", "MA", "SQ", "SPS", "MWS", "ZZ",
        ] {
            for significance in ["W", "A", "Y", "S"] {
                for threat in [
                    None,
                    Some("CONSIDERABLE"),
                    Some("DESTRUCTIVE"),
                    Some("CATASTROPHIC"),
                ] {
                    let mut record = record(phenomenon, significance);
                    record.damage_threat = threat.map(str::to_owned);
                    let key = style_key(&record);
                    assert!(
                        known.contains(&key.as_str()),
                        "{phenomenon}/{significance}/{threat:?} produced '{key}', \
                         which color_tables::hazards does not declare"
                    );
                }
            }
        }
    }

    #[test]
    fn escalations_pick_the_escalated_colour() {
        let mut record = record("TO", "W");
        record.damage_threat = Some("CATASTROPHIC".to_owned());
        assert_eq!(style_key(&record), "tornado/catastrophic");
        assert_ne!(
            place_hazards(&[record], NOW, &projection())[0].color,
            place_hazards(&[super::tests::record("TO", "W")], NOW, &projection())[0].color,
            "a tornado emergency must not paint as an ordinary tornado warning"
        );

        // The bulletin tag is case-insensitive.
        let mut record = super::tests::record("SV", "W");
        record.damage_threat = Some("destructive".to_owned());
        assert_eq!(style_key(&record), "severe-thunderstorm/destructive");
    }

    #[test]
    fn watches_are_a_different_colour_from_warnings() {
        assert_eq!(style_key(&record("TO", "A")), "watch/tornado");
        assert_eq!(style_key(&record("SV", "A")), "watch/severe-thunderstorm");
        assert_eq!(tag(&record("TO", "A")), "TOR-A");
        assert_ne!(
            hazard_stroke_color("watch/tornado"),
            hazard_stroke_color("tornado")
        );
    }

    /// History must not be drawn. A cancelled tornado box over a county where
    /// the tornado is over is worse than no box.
    #[test]
    fn cancelled_expired_and_future_warnings_are_not_drawn() {
        for action in ["CAN", "EXP", "UPG"] {
            let mut record = record("TO", "W");
            record.action = action.to_owned();
            assert!(
                place_hazards(&[record], NOW, &projection()).is_empty(),
                "a {action} warning was drawn"
            );
        }
        let mut past = record("TO", "W");
        past.valid_end = Some("2026-08-16T20:15:00Z".to_owned());
        assert!(place_hazards(&[past], NOW, &projection()).is_empty());

        let mut future = record("TO", "W");
        future.valid_start = Some("2026-08-16T22:00:00Z".to_owned());
        assert!(place_hazards(&[future], NOW, &projection()).is_empty());
    }

    #[test]
    fn a_ugc_only_product_with_no_polygon_is_skipped() {
        let mut record = record("TO", "W");
        record.points.clear();
        assert!(place_hazards(&[record], NOW, &projection()).is_empty());

        // Two points is a line, not a polygon.
        let mut record = super::tests::record("TO", "W");
        record.points.truncate(2);
        assert!(place_hazards(&[record], NOW, &projection()).is_empty());
    }

    /// A ring with one unprojectable vertex is dropped whole, rather than
    /// closing across the gap and drawing a line to the horizon.
    #[test]
    fn a_ring_that_reaches_the_far_side_of_the_globe_is_dropped_whole() {
        let mut record = record("TO", "W");
        // Antipodal to KTLX: the azimuthal-equidistant projection has no
        // finite answer there.
        record.points.push((82.7222, -35.3333));
        assert!(place_hazards(&[record], NOW, &projection()).is_empty());
    }

    /// The most severe polygon must be drawn LAST, so its outline wins where
    /// two overlap.
    #[test]
    fn the_worst_warning_is_painted_on_top() {
        let mut tornado = record("TO", "W");
        tornado.damage_threat = Some("CATASTROPHIC".to_owned());
        let expected = hazard_stroke_color("tornado/catastrophic");

        let placed = place_hazards(
            &[record("FF", "W"), tornado, record("SV", "W")],
            NOW,
            &projection(),
        );
        assert_eq!(placed.len(), 3);
        let last = placed.last().expect("three hazards");
        assert_eq!(
            (last.color.r(), last.color.g(), last.color.b()),
            (expected.r, expected.g, expected.b)
        );
    }

    fn world(east_km: f64, north_km: f64) -> WorldPoint {
        WorldPoint { east_km, north_km }
    }

    fn triangle_area(a: WorldPoint, b: WorldPoint, c: WorldPoint) -> f64 {
        cross(a, b, c).abs() * 0.5
    }

    /// A forecaster cutting a warning around a county line leaves a notch, and
    /// a triangle fan from vertex zero fills straight across it. The fill must
    /// cover the ring's own area and no more.
    #[test]
    fn a_concave_ring_is_filled_without_spilling_across_its_notch() {
        // A U: 4 x 4 with a 2 x 3 bite taken out of the top.
        let ring = [
            world(0.0, 0.0),
            world(4.0, 0.0),
            world(4.0, 4.0),
            world(3.0, 4.0),
            world(3.0, 1.0),
            world(1.0, 1.0),
            world(1.0, 4.0),
            world(0.0, 4.0),
        ];
        let ring_area = signed_area(&ring).abs();
        assert!((ring_area - 10.0).abs() < 1e-9, "ring area is {ring_area}");

        let triangles = triangulate(&ring).expect("a simple concave ring triangulates");
        assert_eq!(triangles.len(), ring.len() - 2);
        let filled: f64 = triangles
            .iter()
            .map(|[a, b, c]| triangle_area(ring[*a as usize], ring[*b as usize], ring[*c as usize]))
            .sum();
        assert!(
            (filled - ring_area).abs() < 1e-9,
            "fill covers {filled} against a ring of {ring_area}"
        );

        // The fan the naive path would build covers a different area entirely,
        // which is the whole reason this triangulation exists.
        let fanned: f64 = (1..ring.len() - 1)
            .map(|index| triangle_area(ring[0], ring[index], ring[index + 1]))
            .sum();
        assert!(
            (fanned - ring_area).abs() > 1.0,
            "a vertex-zero fan covered {fanned}, so this test proves nothing"
        );
    }

    #[test]
    fn a_real_warning_polygon_is_fully_triangulated() {
        // The live KMOB severe-thunderstorm warning of 2026-08-17T21:16Z, with
        // its repeated closing vertex already dropped.
        let mut record = record("SV", "W");
        record.points = vec![
            (-89.08, 30.8),
            (-88.9, 30.79),
            (-88.88, 30.71),
            (-88.88, 30.68),
            (-89.09, 30.7),
        ];
        let placed = place_hazards(&[record], NOW, &projection());
        assert_eq!(placed[0].points.len(), 5);
        assert_eq!(
            placed[0].triangles.len(),
            3,
            "n vertices give n-2 triangles"
        );
        for [a, b, c] in &placed[0].triangles {
            for index in [a, b, c] {
                assert!((*index as usize) < placed[0].points.len());
            }
        }
    }

    /// A ring with no area has no fill, and still draws its outline. Losing the
    /// fill is a cosmetic loss; losing the tornado box is not.
    ///
    /// This is also the guard that catches the `signum` trap: `f64::signum`
    /// answers 1.0 for +0.0, so a winding-based zero check silently passes a
    /// degenerate ring into the clipping loop.
    #[test]
    fn a_zero_area_ring_keeps_its_outline_and_gets_no_fill() {
        // A bow tie: its two lobes cancel, so the signed area is exactly zero.
        let bow_tie = [
            world(0.0, 0.0),
            world(2.0, 2.0),
            world(2.0, 0.0),
            world(0.0, 2.0),
        ];
        assert_eq!(signed_area(&bow_tie), 0.0);
        assert_eq!(
            signed_area(&bow_tie).signum(),
            1.0,
            "signum reports +1 for zero, which is why the guard tests the area"
        );
        assert!(triangulate(&bow_tie).is_none());

        // Three collinear points: an area-free sliver.
        let collinear = [world(0.0, 0.0), world(1.0, 1.0), world(2.0, 2.0)];
        assert!(triangulate(&collinear).is_none());

        // End to end: a ring whose vertices all coincide has no fill, and the
        // hazard is still handed to the map so its outline and tag draw.
        let mut record = record("TO", "W");
        record.points = vec![(-97.5, 35.2); 3];
        let placed = place_hazards(&[record], NOW, &projection());
        assert_eq!(placed.len(), 1, "the hazard is still drawn");
        assert!(placed[0].triangles.is_empty());
        assert_eq!(placed[0].points.len(), 3);
    }

    #[test]
    fn extreme_and_severe_get_the_heavy_stroke() {
        for (severity, want) in [
            (Severity::Extreme, true),
            (Severity::Severe, true),
            (Severity::Moderate, false),
            (Severity::Minor, false),
        ] {
            let mut record = record("TO", "W");
            record.severity = severity;
            assert_eq!(
                place_hazards(&[record], NOW, &projection())[0].emphatic,
                want,
                "{severity:?}"
            );
        }
    }
}
