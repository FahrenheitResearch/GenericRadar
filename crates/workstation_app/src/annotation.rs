//! What the pane writes on top of the radar, and how precisely.
//!
//! Range rings, site markers and their identifiers, and the two corner
//! readouts were eight hard-coded numbers spread through `pane_canvas.rs`: a
//! ring ladder of `[50, 100, 150, 200, 300, 400]` km, a five-point marker
//! half-size, an eleven-point label, a forty-site declutter rule, a
//! two-hundred-and-fifty-marker ceiling, one decimal on the range and four on
//! the latitude. Every one of them is a defensible default and none of them is
//! a law, which is what makes them settings rather than constants.
//!
//! [`Annotation::default`] is those eight numbers exactly, so a build with no
//! settings file paints the pane it always painted. The pane's tests read the
//! shapes back out of a real egui frame and pin that.
//!
//! Distances here are kilometres, always, because that is what the camera and
//! the radar work in. The analyst's chosen distance unit ([`crate::units`])
//! decides only what a ring's LABEL says and what a readout writes - except
//! for the numbered ring spacings, which step in the analyst's unit and are
//! converted back to kilometres before anything is drawn. See
//! [`RingLadder::radii_km`].

use crate::units::DistanceUnit;

/// The shipped ring ladder, in kilometres.
///
/// Not a uniform spacing: 50 km steps out to 200, then 100 km steps to 400.
/// The close rings are where a warning-decision distance is read and the far
/// ones only need to say "this is a long way away", and the ladder has looked
/// like this since the first pane. It is offered as a named choice rather than
/// being reconstructed out of a spacing and a count, because no spacing and
/// count can reproduce it.
pub const SHIPPED_RING_LADDER_KM: &[f64] = &[50.0, 100.0, 150.0, 200.0, 300.0, 400.0];

/// Which distance rings are drawn about the radar.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum RingLadder {
    /// [`SHIPPED_RING_LADDER_KM`], in kilometres, whatever the distance unit
    /// is. A named ladder keeps its numbers.
    #[default]
    Shipped,
    /// Evenly spaced rings, stepping in the analyst's chosen distance unit -
    /// so `Every50` in statute miles is a ring every 50 miles, not every
    /// 50 km relabelled.
    Every25,
    Every50,
    Every100,
    Every200,
}

impl RingLadder {
    pub const ALL: [Self; 5] = [
        Self::Shipped,
        Self::Every25,
        Self::Every50,
        Self::Every100,
        Self::Every200,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Self::Shipped => "shipped",
            Self::Every25 => "every-25",
            Self::Every50 => "every-50",
            Self::Every100 => "every-100",
            Self::Every200 => "every-200",
        }
    }

    /// The even step, in the analyst's distance unit, or `None` for the named
    /// ladder.
    fn step(self) -> Option<f64> {
        match self {
            Self::Shipped => None,
            Self::Every25 => Some(25.0),
            Self::Every50 => Some(50.0),
            Self::Every100 => Some(100.0),
            Self::Every200 => Some(200.0),
        }
    }

    /// Resolve a stored id; anything unrecognised is the shipped ladder.
    pub fn from_id(id: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|ladder| ladder.id() == id)
            .unwrap_or_default()
    }

    /// The ring radii to draw, in kilometres, nearest first.
    ///
    /// `count` bounds the list at both ends of the argument: the shipped
    /// ladder is truncated to its first `count` entries, and an even step
    /// produces exactly `count` rings. That is what makes "rings" a single
    /// honest number in the menu rather than two that interact.
    pub fn radii_km(self, count: usize, unit: DistanceUnit) -> Vec<f64> {
        match self.step() {
            None => SHIPPED_RING_LADDER_KM.iter().copied().take(count).collect(),
            Some(step) => (1..=count)
                .map(|index| unit.to_km(step * index as f64))
                .collect(),
        }
    }
}

/// What the bottom-left corner of a pane writes while the pointer is over it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CornerReadout {
    /// Range, azimuth and latitude/longitude - what the pane has always
    /// written when it has a projection to write it with.
    #[default]
    RangeAzimuthAndCoordinates,
    /// Range and azimuth alone: the radar-local half, for someone working
    /// entirely in the radar's own frame.
    RangeAndAzimuth,
    /// Latitude and longitude alone, for reading a position off to somebody.
    CoordinatesOnly,
    /// Nothing. The pane keeps the probe readout above it either way; this
    /// turns off the geographic line, not the value under the cursor.
    Off,
}

impl CornerReadout {
    pub const ALL: [Self; 4] = [
        Self::RangeAzimuthAndCoordinates,
        Self::RangeAndAzimuth,
        Self::CoordinatesOnly,
        Self::Off,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Self::RangeAzimuthAndCoordinates => "range-azimuth-coords",
            Self::RangeAndAzimuth => "range-azimuth",
            Self::CoordinatesOnly => "coords",
            Self::Off => "off",
        }
    }

    pub fn from_id(id: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|choice| choice.id() == id)
            .unwrap_or_default()
    }

    pub fn shows_range(self) -> bool {
        matches!(
            self,
            Self::RangeAzimuthAndCoordinates | Self::RangeAndAzimuth
        )
    }

    pub fn shows_coordinates(self) -> bool {
        matches!(
            self,
            Self::RangeAzimuthAndCoordinates | Self::CoordinatesOnly
        )
    }
}

/// Every annotation knob the pane reads, resolved once per settings change.
///
/// [`Default`] is the shipped pane exactly. Each field names the constant it
/// replaced so the two can be checked against each other by eye.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Annotation {
    /// Was the literal `RANGE_RINGS_KM` ladder.
    pub ring_ladder: RingLadder,
    /// Was `RANGE_RINGS_KM.len()`, which is 6.
    pub ring_count: usize,
    /// New, and off: the shipped pane has never written a number on a ring.
    pub ring_labels: bool,
    /// Full size of a site marker box, points. Was `SITE_MARKER_HALF * 2.0`.
    pub site_marker_points: f32,
    /// Point size of the identifier written beside a marker. Was the literal
    /// `FontId::monospace(11.0)`.
    pub site_label_points: f32,
    /// How many sites may be on screen before the automatic label rule stops
    /// labelling them all. Was the literal `map.sites.len() <= 40`.
    pub site_declutter_max: usize,
    /// Ceiling on markers drawn in one pane. Was `MAX_SITE_MARKERS`.
    pub site_marker_max: usize,
    /// Decimals on a distance in either corner readout. Was the `:.1` in the
    /// pane's and the probe's format strings.
    pub range_decimals: u8,
    /// Decimals on a latitude or longitude. Was the `:.4`.
    pub coordinate_decimals: u8,
    /// What the geographic corner readout writes.
    pub corner_readout: CornerReadout,
}

impl Default for Annotation {
    fn default() -> Self {
        Self {
            ring_ladder: RingLadder::Shipped,
            ring_count: SHIPPED_RING_LADDER_KM.len(),
            ring_labels: false,
            site_marker_points: 10.0,
            site_label_points: 11.0,
            site_declutter_max: 40,
            site_marker_max: 250,
            range_decimals: 1,
            coordinate_decimals: 4,
            corner_readout: CornerReadout::default(),
        }
    }
}

impl Annotation {
    /// Half the marker box, which is what the painter and the hit test both
    /// want. Named rather than inlined because the two must never drift.
    pub fn site_marker_half(&self) -> f32 {
        self.site_marker_points * 0.5
    }

    /// The rings to draw, in kilometres.
    pub fn ring_radii_km(&self, unit: DistanceUnit) -> Vec<f64> {
        self.ring_ladder.radii_km(self.ring_count, unit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_annotation_is_the_ladder_the_pane_always_drew() {
        let annotation = Annotation::default();
        assert_eq!(
            annotation.ring_radii_km(DistanceUnit::Kilometres),
            SHIPPED_RING_LADDER_KM.to_vec()
        );
        // And the unit does not touch the named ladder: a session in statute
        // miles still gets rings at 50 km, relabelled, not rings at 50 mi.
        assert_eq!(
            annotation.ring_radii_km(DistanceUnit::StatuteMiles),
            SHIPPED_RING_LADDER_KM.to_vec()
        );
        assert_eq!(annotation.site_marker_half(), 5.0);
        assert_eq!(annotation.site_label_points, 11.0);
        assert_eq!(annotation.site_declutter_max, 40);
        assert_eq!(annotation.site_marker_max, 250);
        assert_eq!(annotation.range_decimals, 1);
        assert_eq!(annotation.coordinate_decimals, 4);
        assert!(!annotation.ring_labels);
    }

    #[test]
    fn an_even_ladder_steps_in_the_analysts_own_unit() {
        let annotation = Annotation {
            ring_ladder: RingLadder::Every50,
            ring_count: 3,
            ..Annotation::default()
        };
        assert_eq!(
            annotation.ring_radii_km(DistanceUnit::Kilometres),
            vec![50.0, 100.0, 150.0]
        );
        // 50 statute miles is 80.4672 km, exactly.
        let miles = annotation.ring_radii_km(DistanceUnit::StatuteMiles);
        assert_eq!(miles.len(), 3);
        assert!((miles[0] - 80.467_2).abs() < 1e-9, "got {}", miles[0]);
        assert!((miles[2] - 241.401_6).abs() < 1e-9, "got {}", miles[2]);
    }

    #[test]
    fn the_ring_count_bounds_both_ladder_kinds() {
        // The named ladder truncates rather than inventing a seventh ring.
        let shipped = RingLadder::Shipped.radii_km(2, DistanceUnit::Kilometres);
        assert_eq!(shipped, vec![50.0, 100.0]);
        let past_the_end = RingLadder::Shipped.radii_km(40, DistanceUnit::Kilometres);
        assert_eq!(past_the_end.len(), SHIPPED_RING_LADDER_KM.len());
        // An even step produces exactly the count asked for.
        assert_eq!(
            RingLadder::Every100
                .radii_km(4, DistanceUnit::Kilometres)
                .len(),
            4
        );
        // Zero rings is a legitimate answer and must not panic.
        assert!(
            RingLadder::Every25
                .radii_km(0, DistanceUnit::Kilometres)
                .is_empty()
        );
    }

    #[test]
    fn a_stored_id_this_build_does_not_know_reads_as_the_shipped_choice() {
        assert_eq!(RingLadder::from_id("every-7"), RingLadder::Shipped);
        assert_eq!(
            CornerReadout::from_id("mgrs"),
            CornerReadout::RangeAzimuthAndCoordinates
        );
        for ladder in RingLadder::ALL {
            assert_eq!(RingLadder::from_id(ladder.id()), ladder);
        }
        for readout in CornerReadout::ALL {
            assert_eq!(CornerReadout::from_id(readout.id()), readout);
        }
    }

    /// The catalog's numbers against this module's, field by field.
    ///
    /// The catalog cannot see this module - it is compiled by the `settings`
    /// crate's preview harness too - so its defaults are written out by hand
    /// there. This is what stops the two copies drifting: a default changed in
    /// one place and not the other fails here rather than on someone's screen.
    #[test]
    fn every_catalog_default_is_the_shipped_pane() {
        use crate::settings_ui::catalog::{keys, registry};
        let registry = registry();
        let store = settings::SettingsStore::open(
            std::env::temp_dir().join("annotation-pin-never-written.json"),
        );
        let category = keys::annotation::CATEGORY;
        let int = |id: &str| store.effective_int(&registry, category, id);
        let float = |id: &str| store.effective_float(&registry, category, id) as f32;
        let text = |id: &str| store.effective_text(&registry, category, id);

        let shipped = Annotation::default();
        assert_eq!(
            RingLadder::from_id(&text(keys::annotation::RING_LADDER)),
            shipped.ring_ladder
        );
        assert_eq!(
            int(keys::annotation::RING_COUNT) as usize,
            shipped.ring_count
        );
        assert_eq!(
            store.effective_bool(&registry, category, keys::annotation::RING_LABELS),
            shipped.ring_labels
        );
        assert_eq!(
            float(keys::annotation::SITE_MARKER_SIZE),
            shipped.site_marker_points
        );
        assert_eq!(
            float(keys::annotation::SITE_LABEL_SIZE),
            shipped.site_label_points
        );
        assert_eq!(
            int(keys::annotation::SITE_DECLUTTER_MAX) as usize,
            shipped.site_declutter_max
        );
        assert_eq!(
            int(keys::annotation::SITE_MARKER_MAX) as usize,
            shipped.site_marker_max
        );
        assert_eq!(
            int(keys::annotation::RANGE_DECIMALS) as u8,
            shipped.range_decimals
        );
        assert_eq!(
            int(keys::annotation::COORDINATE_DECIMALS) as u8,
            shipped.coordinate_decimals
        );
        assert_eq!(
            CornerReadout::from_id(&text(keys::annotation::CORNER_READOUT)),
            shipped.corner_readout
        );
    }

    /// And every option the two menus offer resolves to an enum here, so a
    /// choice cannot be offered that nothing can act on.
    #[test]
    fn every_menu_option_matches_an_enum() {
        use crate::settings_ui::catalog::{keys, registry};
        let registry = registry();
        let ids = |id: &str| -> Vec<String> {
            match &registry
                .setting(keys::annotation::CATEGORY, id)
                .unwrap_or_else(|| panic!("the catalog declares annotation/{id}"))
                .kind
            {
                settings::SettingKind::Choice { options, .. } => {
                    options.iter().map(|option| option.id.clone()).collect()
                }
                other => panic!("annotation/{id} is {other:?}, not a choice"),
            }
        };
        assert_eq!(
            ids(keys::annotation::RING_LADDER),
            RingLadder::ALL
                .iter()
                .map(|ladder| ladder.id().to_owned())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            ids(keys::annotation::CORNER_READOUT),
            CornerReadout::ALL
                .iter()
                .map(|readout| readout.id().to_owned())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn the_corner_readout_choices_cover_each_half_exactly_once() {
        assert!(CornerReadout::RangeAzimuthAndCoordinates.shows_range());
        assert!(CornerReadout::RangeAzimuthAndCoordinates.shows_coordinates());
        assert!(CornerReadout::RangeAndAzimuth.shows_range());
        assert!(!CornerReadout::RangeAndAzimuth.shows_coordinates());
        assert!(!CornerReadout::CoordinatesOnly.shows_range());
        assert!(CornerReadout::CoordinatesOnly.shows_coordinates());
        assert!(!CornerReadout::Off.shows_range());
        assert!(!CornerReadout::Off.shows_coordinates());
    }
}
