//! Display units: how a distance, an altitude and a time are WRITTEN.
//!
//! Nothing here touches a stored or decoded number. Every radar quantity in
//! this workspace stays in the unit its decoder produced - slant range in
//! metres, beam height in metres, volume time in UTC - and this module is the
//! last thing that runs before those numbers become characters on a pane. That
//! split is the whole point: a session switched to statute miles must read
//! differently and probe the same gate, and a session switched back must be
//! bit-for-bit where it started.
//!
//! # Why the conversions are written as exact constants
//!
//! All three conversion factors are definitions rather than measurements, so
//! they are written to their full exact value and never to a rounded one:
//!
//! * 1 international mile = 1.609 344 km exactly - the International Yard and
//!   Pound Agreement of 1959 (National Bureau of Standards, *Federal
//!   Register* **24**, 5348, 1 July 1959), which defines the yard as 0.9144 m.
//! * 1 international nautical mile = 1.852 km exactly - adopted by the First
//!   International Extraordinary Hydrographic Conference, Monaco, 1929, and
//!   carried into the SI brochure's table of non-SI units.
//! * 1 international foot = 0.3048 m exactly - the same 1959 agreement.
//!
//! The U.S. survey foot (1200/3937 m) is deliberately NOT offered. It differs
//! from the international foot by 2 parts per million, which at the 460 km a
//! WSR-88D reaches is under a metre and far below any beam's own uncertainty,
//! and offering the choice would put a decision in front of an analyst that
//! cannot change what they see.
//!
//! # Rounding
//!
//! A converted number keeps the decimal count the call site asked for while it
//! stays in a "large" unit - kilometres, miles, nautical miles - because those
//! all sit within a factor of two of each other and a readout tuned to one
//! reads correctly in the others. Feet and metres are different in kind: a
//! beam at 0.73 km is 2392 ft, and printing "2391.73 ft" would claim a
//! centimetre of precision the radar has never had. Both of those round to
//! whole units. See [`AltitudeUnit::decimals_for`].

use chrono::{DateTime, Local, SecondsFormat, Utc};

/// Kilometres in one international statute mile. Exact; see the module docs.
pub const KM_PER_STATUTE_MILE: f64 = 1.609_344;
/// Kilometres in one international nautical mile. Exact; see the module docs.
pub const KM_PER_NAUTICAL_MILE: f64 = 1.852;
/// Metres in one international foot. Exact; see the module docs.
pub const METRES_PER_FOOT: f64 = 0.304_8;

/// The unit ground distances are written in: ranges, ring radii, the length of
/// a cross-section line.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DistanceUnit {
    /// What every distance in this application has always been written in.
    #[default]
    Kilometres,
    StatuteMiles,
    NauticalMiles,
}

impl DistanceUnit {
    pub const ALL: [Self; 3] = [Self::Kilometres, Self::StatuteMiles, Self::NauticalMiles];

    /// The stored choice id. These strings are a persistence contract.
    pub fn id(self) -> &'static str {
        match self {
            Self::Kilometres => "km",
            Self::StatuteMiles => "mi",
            Self::NauticalMiles => "nm",
        }
    }

    /// What is written after the number.
    pub fn label(self) -> &'static str {
        self.id()
    }

    /// Resolve a stored id. Anything this build does not recognise - a value
    /// written by a later one, or a hand-edited file - reads as the shipped
    /// unit rather than picking a unit on the analyst's behalf.
    pub fn from_id(id: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|unit| unit.id() == id)
            .unwrap_or_default()
    }

    /// Kilometres in, this unit out. Named `convert_` rather than `from_`
    /// because it takes the unit as `self`: `DistanceUnit::StatuteMiles
    /// .convert_km(100.0)` is "100 km, in miles".
    pub fn convert_km(self, km: f64) -> f64 {
        match self {
            Self::Kilometres => km,
            Self::StatuteMiles => km / KM_PER_STATUTE_MILE,
            Self::NauticalMiles => km / KM_PER_NAUTICAL_MILE,
        }
    }

    /// This unit in, kilometres out. The exact inverse of [`Self::convert_km`],
    /// which is what lets a ring ladder be stated in the analyst's unit and
    /// still be drawn on the radar's own kilometre grid.
    pub fn to_km(self, value: f64) -> f64 {
        match self {
            Self::Kilometres => value,
            Self::StatuteMiles => value * KM_PER_STATUTE_MILE,
            Self::NauticalMiles => value * KM_PER_NAUTICAL_MILE,
        }
    }

    /// `"41.1 km"`. `decimals` is the call site's own precision, unchanged by
    /// the unit: all three distance units are within a factor of two of each
    /// other, so a readout tuned for kilometres reads correctly in the others.
    pub fn format_km(self, km: f64, decimals: u8) -> String {
        format!(
            "{:.*} {}",
            usize::from(decimals),
            self.convert_km(km),
            self.label()
        )
    }
}

/// The unit heights above the radar (or above sea level) are written in.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum AltitudeUnit {
    /// What every beam height in this application has always been written in.
    #[default]
    Kilometres,
    Feet,
    Metres,
}

impl AltitudeUnit {
    pub const ALL: [Self; 3] = [Self::Kilometres, Self::Feet, Self::Metres];

    pub fn id(self) -> &'static str {
        match self {
            Self::Kilometres => "km",
            Self::Feet => "ft",
            Self::Metres => "m",
        }
    }

    pub fn label(self) -> &'static str {
        self.id()
    }

    pub fn from_id(id: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|unit| unit.id() == id)
            .unwrap_or_default()
    }

    /// Metres in, this unit out. Same naming as [`DistanceUnit::convert_km`].
    pub fn convert_metres(self, metres: f64) -> f64 {
        match self {
            Self::Kilometres => metres / 1000.0,
            Self::Feet => metres / METRES_PER_FOOT,
            Self::Metres => metres,
        }
    }

    /// This unit in, metres out. The exact inverse of
    /// [`Self::convert_metres`], and the counterpart of
    /// [`DistanceUnit::to_km`].
    ///
    /// It exists for the same reason that one does: a LADDER is chosen in the
    /// unit the analyst reads - rungs every 5 000 ft, not every 1 524 m
    /// relabelled - and then has to be placed on a picture whose axis is
    /// metres. See `xsection`'s height ladder.
    pub fn to_metres(self, value: f64) -> f64 {
        match self {
            Self::Kilometres => value * 1000.0,
            Self::Feet => value * METRES_PER_FOOT,
            Self::Metres => value,
        }
    }

    /// How many decimals to print. Kilometres keep the call site's own
    /// precision - one or two places on a number around 10 - while feet and
    /// metres round to whole units, because 0.73 km is 2392 ft and a
    /// hundredth of a foot is four orders of magnitude finer than any beam.
    pub fn decimals_for(self, requested: u8) -> u8 {
        match self {
            Self::Kilometres => requested,
            Self::Feet | Self::Metres => 0,
        }
    }

    /// `"0.73 km"`, `"2392 ft"`.
    pub fn format_metres(self, metres: f64, decimals: u8) -> String {
        format!(
            "{:.*} {}",
            usize::from(self.decimals_for(decimals)),
            self.convert_metres(metres),
            self.label()
        )
    }
}

/// Which clock a volume time is written against.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TimeZoneChoice {
    /// What every time in this application has always been written in, and
    /// what the volume header actually holds.
    #[default]
    Utc,
    /// This machine's own zone, as the operating system reports it.
    Local,
}

impl TimeZoneChoice {
    pub const ALL: [Self; 2] = [Self::Utc, Self::Local];

    pub fn id(self) -> &'static str {
        match self {
            Self::Utc => "utc",
            Self::Local => "local",
        }
    }

    pub fn from_id(id: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|zone| zone.id() == id)
            .unwrap_or_default()
    }
}

/// Whether an hour is written 00-23 or 12-hour with a meridiem.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ClockFormat {
    #[default]
    TwentyFourHour,
    TwelveHour,
}

impl ClockFormat {
    pub const ALL: [Self; 2] = [Self::TwentyFourHour, Self::TwelveHour];

    pub fn id(self) -> &'static str {
        match self {
            Self::TwentyFourHour => "24h",
            Self::TwelveHour => "12h",
        }
    }

    pub fn from_id(id: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|clock| clock.id() == id)
            .unwrap_or_default()
    }
}

/// The complete set of display-unit choices, resolved once per settings change
/// and read by every surface that writes a number.
///
/// [`Default`] is the application's shipped behaviour exactly - kilometres,
/// kilometres, UTC, 24-hour - so a build with no settings file writes what
/// every previous build wrote, character for character. A test pins that
/// against the exact `to_rfc3339_opts` call the status line used to make.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct UnitSystem {
    pub distance: DistanceUnit,
    pub altitude: AltitudeUnit,
    pub zone: TimeZoneChoice,
    pub clock: ClockFormat,
}

impl UnitSystem {
    /// `"41.1 km"` - a ground distance, from kilometres.
    pub fn distance(self, km: f64, decimals: u8) -> String {
        self.distance.format_km(km, decimals)
    }

    /// `"0.73 km"` - a height, from metres.
    pub fn altitude(self, metres: f64, decimals: u8) -> String {
        self.altitude.format_metres(metres, decimals)
    }

    /// A volume time, written for a human.
    ///
    /// The UTC 24-hour case is byte-identical to
    /// `time.to_rfc3339_opts(SecondsFormat::Secs, true)`, which is the call
    /// every status line in the application used to make directly; the other
    /// three cases carry an explicit offset or `Z` so a screenshot can never
    /// be read against the wrong clock. Seconds are always shown: a volume
    /// time is an instant a radar recorded, not a wall clock.
    pub fn time(self, time: DateTime<Utc>) -> String {
        match (self.zone, self.clock) {
            (TimeZoneChoice::Utc, ClockFormat::TwentyFourHour) => {
                time.to_rfc3339_opts(SecondsFormat::Secs, true)
            }
            (TimeZoneChoice::Utc, ClockFormat::TwelveHour) => {
                time.format("%Y-%m-%d %I:%M:%S %p Z").to_string()
            }
            (TimeZoneChoice::Local, ClockFormat::TwentyFourHour) => time
                .with_timezone(&Local)
                .format("%Y-%m-%d %H:%M:%S %:z")
                .to_string(),
            (TimeZoneChoice::Local, ClockFormat::TwelveHour) => time
                .with_timezone(&Local)
                .format("%Y-%m-%d %I:%M:%S %p %:z")
                .to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kdvn_volume_time() -> DateTime<Utc> {
        // The real volume this wave was proved on: KDVN, 2026-08-19 19:28 UTC.
        "2026-08-19T19:28:23Z"
            .parse::<DateTime<Utc>>()
            .expect("a literal RFC 3339 stamp parses")
    }

    #[test]
    fn the_default_unit_system_is_what_the_application_already_wrote() {
        let units = UnitSystem::default();
        assert_eq!(units.distance, DistanceUnit::Kilometres);
        assert_eq!(units.altitude, AltitudeUnit::Kilometres);
        // The two formatters, character for character against the format
        // strings the pane and the probe carried before this module existed.
        assert_eq!(units.distance(41.14, 1), "41.1 km");
        assert_eq!(units.altitude(729.0, 2), "0.73 km");
        // And the exact call `timeline_status` used to make.
        let time = kdvn_volume_time();
        assert_eq!(
            units.time(time),
            time.to_rfc3339_opts(SecondsFormat::Secs, true)
        );
    }

    #[test]
    fn distance_converts_to_the_defined_factors_and_back() {
        // 100 km is 62.137... statute miles and 53.995... nautical miles.
        assert!((DistanceUnit::StatuteMiles.convert_km(100.0) - 62.137_119_223_733_4).abs() < 1e-9);
        assert!(
            (DistanceUnit::NauticalMiles.convert_km(100.0) - 53.995_680_345_572_4).abs() < 1e-9
        );
        // The 460 km surveillance fence, in every unit, back to kilometres.
        for unit in DistanceUnit::ALL {
            let there = unit.convert_km(460.0);
            let back = unit.to_km(there);
            assert!(
                (back - 460.0).abs() < 1e-9,
                "{} round trip lost {}",
                unit.id(),
                (back - 460.0).abs()
            );
        }
    }

    #[test]
    fn altitude_converts_to_the_defined_factors() {
        // The 1959 foot: 3048 m is 10 000 ft exactly.
        assert!((AltitudeUnit::Feet.convert_metres(3_048.0) - 10_000.0).abs() < 1e-9);
        assert!((AltitudeUnit::Metres.convert_metres(12_000.0) - 12_000.0).abs() < 1e-12);
        assert!((AltitudeUnit::Kilometres.convert_metres(12_000.0) - 12.0).abs() < 1e-12);
        // A 12 km echo top, in feet: 39 370.08...
        assert!((AltitudeUnit::Feet.convert_metres(12_000.0) - 39_370.078_740_157_5).abs() < 1e-6);
    }

    /// A height ladder is chosen in the analyst's unit and drawn on a metre
    /// axis, so the two directions must be exact inverses.
    #[test]
    fn altitude_round_trips_through_metres() {
        // 5 000 ft is 1 524 m exactly, by the 1959 foot.
        assert!((AltitudeUnit::Feet.to_metres(5_000.0) - 1_524.0).abs() < 1e-9);
        for unit in AltitudeUnit::ALL {
            for metres in [0.0, 729.0, 12_000.0, 18_000.0] {
                let back = unit.to_metres(unit.convert_metres(metres));
                assert!(
                    (back - metres).abs() < 1e-9,
                    "{} lost {} at {metres} m",
                    unit.id(),
                    (back - metres).abs()
                );
            }
        }
    }

    #[test]
    fn feet_and_metres_round_to_whole_units_however_many_decimals_were_asked_for() {
        // 0.73 km ARL, the beam height in the probe readout's own doc example.
        assert_eq!(AltitudeUnit::Feet.format_metres(729.0, 2), "2392 ft");
        assert_eq!(AltitudeUnit::Metres.format_metres(729.0, 2), "729 m");
        // Kilometres keep what the call site asked for.
        assert_eq!(AltitudeUnit::Kilometres.format_metres(729.0, 2), "0.73 km");
        assert_eq!(AltitudeUnit::Kilometres.format_metres(729.0, 1), "0.7 km");
    }

    #[test]
    fn a_stored_id_this_build_does_not_know_reads_as_the_shipped_unit() {
        assert_eq!(DistanceUnit::from_id("furlong"), DistanceUnit::Kilometres);
        assert_eq!(AltitudeUnit::from_id("fathom"), AltitudeUnit::Kilometres);
        assert_eq!(TimeZoneChoice::from_id("mars"), TimeZoneChoice::Utc);
        assert_eq!(ClockFormat::from_id("decimal"), ClockFormat::TwentyFourHour);
        // And every id this build DOES write round-trips.
        for unit in DistanceUnit::ALL {
            assert_eq!(DistanceUnit::from_id(unit.id()), unit);
        }
        for unit in AltitudeUnit::ALL {
            assert_eq!(AltitudeUnit::from_id(unit.id()), unit);
        }
    }

    #[test]
    fn the_twelve_hour_utc_clock_names_the_same_instant() {
        let units = UnitSystem {
            clock: ClockFormat::TwelveHour,
            ..UnitSystem::default()
        };
        // 19:28:23Z is 07:28:23 PM.
        assert_eq!(units.time(kdvn_volume_time()), "2026-08-19 07:28:23 PM Z");
    }

    #[test]
    fn local_time_carries_an_explicit_offset_so_a_screenshot_cannot_be_misread() {
        let units = UnitSystem {
            zone: TimeZoneChoice::Local,
            ..UnitSystem::default()
        };
        let written = units.time(kdvn_volume_time());
        // The offset is this machine's, so the test asserts the SHAPE rather
        // than a zone: a stamp with no offset is the failure that matters.
        //
        // It is asserted on the TAIL and not on the whole string. `contains('-')`
        // would have been satisfied by the hyphens in "2026-08-19" and would
        // have passed against a stamp carrying no offset at all, which is
        // exactly the failure this test exists for.
        let offset = written
            .rsplit_once(' ')
            .map(|(_, tail)| tail)
            .unwrap_or_default();
        assert!(
            offset.starts_with('+') || offset.starts_with('-'),
            "local time must end in a signed offset, got {written:?}"
        );
        assert!(!written.ends_with('Z'), "local time must not claim UTC");
        // Same instant, read back.
        let parsed = DateTime::parse_from_str(&written, "%Y-%m-%d %H:%M:%S %:z")
            .expect("the local 24-hour form parses back");
        assert_eq!(parsed.with_timezone(&Utc), kdvn_volume_time());
    }

    #[test]
    fn a_statute_mile_readout_is_the_same_gate_written_differently() {
        // The invariant the whole module exists for: conversion is display
        // only, so the kilometre value the probe sampled survives untouched.
        let sampled_km = 41.14_f64;
        let miles = DistanceUnit::StatuteMiles;
        assert_eq!(miles.format_km(sampled_km, 1), "25.6 mi");
        assert!((miles.to_km(miles.convert_km(sampled_km)) - sampled_km).abs() < 1e-12);
    }
}
