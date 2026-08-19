//! Distinguishes rendering artefacts from the source data's own geometry.
//!
//! Long straight segments appear near river mouths in the rendered map. This
//! checks whether they are present in the compiled-in dataset before the
//! renderer touches it, so a data property is never mistaken for a bug in
//! projection, clipping or simplification.

use analyst_runtime::Generation;
use map_scene::dataset::{MapDataset, MapLayer};
use map_scene::projection::RadarProjection;

/// KAKQ, the site the long segments were observed around.
const KAKQ: (f64, f64) = (36.9840, -77.0074);

#[test]
fn report_long_source_segments_near_kakq() {
    let dataset = MapDataset::from_generated(Generation::new(1));
    let projection = RadarProjection::new(KAKQ.0, KAKQ.1);

    let mut long_segments = Vec::new();
    let mut examined = 0_usize;

    for line in dataset
        .lines
        .iter()
        .filter(|line| line.layer == MapLayer::County)
    {
        let mut previous: Option<[f64; 2]> = None;
        for (lon, lat) in line.points {
            let world = projection.lon_lat_to_world(f64::from(*lon), f64::from(*lat));
            let point = [world.east_km, world.north_km];
            // Only the neighbourhood actually on screen in the capture.
            let near = point[0].abs() < 150.0 && point[1].abs() < 150.0;
            if let Some(previous_point) = previous
                && near
            {
                examined += 1;
                let length = (point[0] - previous_point[0]).hypot(point[1] - previous_point[1]);
                if length > 20.0 {
                    long_segments.push((length, *lon, *lat));
                }
            }
            previous = Some(point);
        }
    }

    long_segments.sort_by(|a, b| b.0.partial_cmp(&a.0).expect("finite lengths"));
    println!(
        "county segments within 150 km of KAKQ: {examined}, longer than 20 km: {}",
        long_segments.len()
    );
    for (length, lon, lat) in long_segments.iter().take(10) {
        println!("  {length:.1} km segment ending at ({lon:.4}, {lat:.4})");
    }

    assert!(
        examined > 1_000,
        "the audit found no county data to examine"
    );
    // Long straight chords are a property of the decimated source boundaries,
    // most visibly where they cross water. They are recorded here so the
    // renderer is not "fixed" for faithfully drawing them.
    assert!(
        !long_segments.is_empty(),
        "source boundaries near KAKQ no longer contain long straight segments; \
         if the dataset was replaced, the rendered map's straight runs should be \
         re-examined rather than assumed to be source geometry"
    );
}
