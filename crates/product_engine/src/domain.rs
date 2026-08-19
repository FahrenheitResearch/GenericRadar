//! The value domain of a product: what it means, what it may read, and what
//! range of numbers is physically possible.
//!
//! A [`DisplayDomain`] is the single object that answers "what unit is this
//! stored in, what unit does an analyst read, and what is the sane range" for
//! one product. Colour tables, legends, probes, and the plausibility gate all
//! read the same one, so they cannot disagree about a product's meaning.

use crate::units::{AffineTransform, DisplayUnit, PhysicalUnit};

/// A closed interval of engine values.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ValueRange {
    pub min: f32,
    pub max: f32,
}

impl ValueRange {
    pub const fn new(min: f32, max: f32) -> Self {
        Self { min, max }
    }

    pub fn contains(self, value: f32) -> bool {
        value >= self.min && value <= self.max
    }

    pub fn span(self) -> f32 {
        self.max - self.min
    }

    /// The overlap of two ranges, or `None` when they do not overlap.
    ///
    /// The legend uses this to clip a product's declared domain to the part of
    /// the palette that actually has ink in it. Returning `None` rather than an
    /// inverted range matters: an inverted range drawn as a bar is a picture of
    /// nothing that still looks like a scale.
    pub fn intersect(self, other: Self) -> Option<Self> {
        let min = self.min.max(other.min);
        let max = self.max.min(other.max);
        (min <= max).then_some(Self { min, max })
    }
}

/// What a plausibility check concluded about a value or a whole field.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlausibilityDisposition {
    /// Inside the soft range. Install it.
    Pass,
    /// Outside the soft range but inside the hard range. Install it with a
    /// visible badge; unusual weather is real and must still be drawable.
    Warn,
    /// Outside the hard range, or not a finite number. Do not install it.
    Reject,
}

/// Broad engineering bounds that catch a unit or reference-frame error.
///
/// These are not warning thresholds and not climatology. `hard` answers "could
/// a radar have produced this at all"; `soft` answers "is this unusual enough
/// that an analyst should be told". A field that trips `hard` is not rare
/// weather, it is a bug: a metres/kilofeet mix-up, an MSL height compared with
/// an above-radar height, or a display conversion applied before storage.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlausibleRange {
    pub soft_min: f32,
    pub soft_max: f32,
    pub hard_min: f32,
    pub hard_max: f32,
}

impl PlausibleRange {
    pub const fn new(soft_min: f32, soft_max: f32, hard_min: f32, hard_max: f32) -> Self {
        Self {
            soft_min,
            soft_max,
            hard_min,
            hard_max,
        }
    }

    pub fn classify(self, value: f32) -> PlausibilityDisposition {
        if !value.is_finite() {
            return PlausibilityDisposition::Reject;
        }
        if value < self.hard_min || value > self.hard_max {
            return PlausibilityDisposition::Reject;
        }
        if value < self.soft_min || value > self.soft_max {
            return PlausibilityDisposition::Warn;
        }
        PlausibilityDisposition::Pass
    }

    /// True when the soft range sits inside the hard range, which is the only
    /// ordering that makes `classify` mean anything.
    pub fn is_well_ordered(self) -> bool {
        self.hard_min <= self.soft_min
            && self.soft_min <= self.soft_max
            && self.soft_max <= self.hard_max
    }
}

/// How many intervals a legend should aim for on this product's bar.
///
/// A hint, not a demand: the tick ladder reduces the count when labels would
/// collide, and a product with a naturally coarse domain reads better with
/// fewer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TickHint {
    pub target_intervals: u8,
}

impl TickHint {
    pub const DEFAULT: Self = Self {
        target_intervals: 6,
    };

    pub const fn intervals(target_intervals: u8) -> Self {
        Self { target_intervals }
    }
}

/// Everything about one product's numbers.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DisplayDomain {
    /// The unit the grid is stored in and the colour table is sampled in.
    pub engine_unit: PhysicalUnit,
    /// The unit the legend and probe label.
    pub display_unit: DisplayUnit,
    pub display_from_engine: AffineTransform,
    /// The meaningful span of the product, in engine units. The legend clips
    /// this against the palette's inked span rather than drawing either alone.
    pub declared_engine_range: ValueRange,
    pub plausible: PlausibleRange,
    pub tick_hint: TickHint,
    /// Decimal places for a readout. Reflectivity wants one, RHO wants three.
    pub decimals: u8,
}

impl DisplayDomain {
    pub fn to_display(&self, engine_value: f32) -> f64 {
        self.display_from_engine.to_display(engine_value)
    }

    pub fn to_engine(&self, display_value: f64) -> f32 {
        self.display_from_engine.to_engine(display_value)
    }

    /// A number formatted for a readout, without its unit.
    pub fn format_display_value(&self, engine_value: f32) -> String {
        format!(
            "{:.*}",
            usize::from(self.decimals),
            self.to_display(engine_value)
        )
    }

    /// A number formatted for a readout, with its unit. Dimensionless products
    /// get no trailing space.
    pub fn format_display(&self, engine_value: f32) -> String {
        let value = self.format_display_value(engine_value);
        let unit = self.display_unit.label();
        if unit.is_empty() {
            value
        } else {
            format!("{value} {unit}")
        }
    }

    pub fn classify(&self, engine_value: f32) -> PlausibilityDisposition {
        self.plausible.classify(engine_value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::units::METERS_TO_KILOFEET;

    fn echo_top_domain() -> DisplayDomain {
        DisplayDomain {
            engine_unit: PhysicalUnit::Meters,
            display_unit: DisplayUnit::Kilofeet,
            display_from_engine: AffineTransform::scaled(METERS_TO_KILOFEET),
            declared_engine_range: ValueRange::new(0.0, 21_000.0),
            plausible: PlausibleRange::new(0.0, 22_000.0, 0.0, 30_000.0),
            tick_hint: TickHint::DEFAULT,
            decimals: 1,
        }
    }

    #[test]
    fn an_echo_top_formats_in_kilofeet_but_classifies_in_metres() {
        let domain = echo_top_domain();
        assert_eq!(domain.format_display(12_000.0), "39.4 kft");
        assert_eq!(
            domain.classify(12_000.0),
            PlausibilityDisposition::Pass,
            "12 000 m is an ordinary echo top and must not warn"
        );
    }

    #[test]
    fn an_echo_top_stored_in_kilofeet_by_mistake_is_not_rejected_by_range_alone() {
        // 12 000 m stored as 39.37 kft is still inside 0..30 000, so the range
        // gate alone cannot catch this. It is caught by the type of the field
        // and by the legend test, which is why both exist.
        let domain = echo_top_domain();
        assert_eq!(domain.classify(39.37), PlausibilityDisposition::Pass);
    }

    #[test]
    fn a_negative_echo_top_is_rejected() {
        assert_eq!(
            echo_top_domain().classify(-1.0),
            PlausibilityDisposition::Reject
        );
    }

    #[test]
    fn an_unusually_tall_echo_top_warns_instead_of_being_dropped() {
        assert_eq!(
            echo_top_domain().classify(24_000.0),
            PlausibilityDisposition::Warn
        );
    }

    #[test]
    fn a_non_finite_value_is_rejected() {
        let domain = echo_top_domain();
        assert_eq!(domain.classify(f32::NAN), PlausibilityDisposition::Reject);
        assert_eq!(
            domain.classify(f32::INFINITY),
            PlausibilityDisposition::Reject
        );
    }

    #[test]
    fn overlapping_ranges_intersect_to_the_shared_span() {
        let declared = ValueRange::new(-32.0, 94.5);
        let inked = ValueRange::new(-10.0, 75.0);
        assert_eq!(
            declared.intersect(inked),
            Some(ValueRange::new(-10.0, 75.0)),
            "the legend must clip to the part of the palette that has ink"
        );
    }

    #[test]
    fn disjoint_ranges_do_not_intersect() {
        let declared = ValueRange::new(0.0, 1.05);
        let inked = ValueRange::new(-32.0, -10.0);
        assert_eq!(
            declared.intersect(inked),
            None,
            "a palette that misses the product domain must say so, not draw an inverted bar"
        );
    }

    #[test]
    fn a_touching_pair_of_ranges_intersects_at_one_value() {
        let left = ValueRange::new(0.0, 5.0);
        let right = ValueRange::new(5.0, 9.0);
        assert_eq!(left.intersect(right), Some(ValueRange::new(5.0, 5.0)));
    }
}
