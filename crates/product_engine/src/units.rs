//! Physical units for stored values, and the display units an analyst reads.
//!
//! Engine units and display units are deliberately different types. Every grid
//! is stored, colorized, integrated, and range-checked in engine units; a
//! display unit appears only at a formatting boundary. `Knots` and `Kilofeet`
//! are therefore absent from [`PhysicalUnit`], so "store this field in knots"
//! is not a mistake that can be made quietly — it does not compile.
//!
//! The failure this guards against is specific and hard to see. An echo top of
//! 12 000 m stored by mistake in kilofeet is 39.37, which is finite, positive,
//! and inside every naive sanity check; it simply paints the wrong colour and
//! reads out a storm 8 km shorter than it is. The same trap exists for VIL
//! density (a factor of 1000), MESH (25.4), and the shear derivatives (1000).

/// A unit a value may actually be stored in.
///
/// Deliberately narrow. If a unit is not on this list, no grid may hold it.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PhysicalUnit {
    /// Logarithmic reflectivity factor.
    Dbz,
    /// Uncalibrated power relative to one squared stored I/Q unit.
    RelativeIqPowerDb,
    /// Received power referenced to one milliwatt.
    Dbm,
    /// Velocity, spectrum width, and every wind-like quantity.
    MetersPerSecond,
    /// Differential reflectivity.
    Decibels,
    /// Correlation coefficient: a ratio with no unit.
    Dimensionless,
    /// Differential phase.
    Degrees,
    /// Specific differential phase.
    DegreesPerKilometer,
    /// Heights. Always paired with a reference frame by the caller.
    Meters,
    /// Vertically integrated liquid.
    KilogramsPerSquareMeter,
    /// VIL density.
    KilogramsPerCubicMeter,
    /// Hail size estimates.
    Millimeters,
    /// Probabilities expressed 0..100 rather than 0..1.
    Percent,
    /// Velocity derivatives: azimuthal shear and radial divergence.
    PerSecond,
    /// Severe hail index.
    JoulesPerMeterPerSecond,
}

impl PhysicalUnit {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Dbz => "dBZ",
            Self::RelativeIqPowerDb => "dB re stored I/Q unit²",
            Self::Dbm => "dBm",
            Self::MetersPerSecond => "m/s",
            Self::Decibels => "dB",
            Self::Dimensionless => "",
            Self::Degrees => "deg",
            Self::DegreesPerKilometer => "deg/km",
            Self::Meters => "m",
            Self::KilogramsPerSquareMeter => "kg/m2",
            Self::KilogramsPerCubicMeter => "kg/m3",
            Self::Millimeters => "mm",
            Self::Percent => "%",
            Self::PerSecond => "1/s",
            Self::JoulesPerMeterPerSecond => "J/m/s",
        }
    }
}

/// A unit a value may be shown in.
///
/// A superset of [`PhysicalUnit`] in spirit but not in type: the extra members
/// are the ones that exist only for reading, and giving them their own enum is
/// what stops them reaching storage.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DisplayUnit {
    Dbz,
    /// Uncalibrated power relative to one squared stored I/Q unit.
    RelativeIqPowerDb,
    Dbm,
    MetersPerSecond,
    /// Velocity as an analyst reads it off a warning.
    Knots,
    Decibels,
    Dimensionless,
    Degrees,
    DegreesPerKilometer,
    Meters,
    /// Echo tops as an analyst reads them off a sounding.
    Kilofeet,
    KilogramsPerSquareMeter,
    /// VIL density: the operational literature is written in g/m3.
    GramsPerCubicMeter,
    Millimeters,
    /// Hail size as it is reported to the public.
    Inches,
    Percent,
    /// Shear derivatives are quoted in thousandths; 0.005 1/s reads as 5.
    MilliPerSecond,
    JoulesPerMeterPerSecond,
}

impl DisplayUnit {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Dbz => "dBZ",
            Self::RelativeIqPowerDb => "dB re stored I/Q unit²",
            Self::Dbm => "dBm",
            Self::MetersPerSecond => "m/s",
            Self::Knots => "kt",
            Self::Decibels => "dB",
            Self::Dimensionless => "",
            Self::Degrees => "deg",
            Self::DegreesPerKilometer => "deg/km",
            Self::Meters => "m",
            Self::Kilofeet => "kft",
            Self::KilogramsPerSquareMeter => "kg/m2",
            Self::GramsPerCubicMeter => "g/m3",
            Self::Millimeters => "mm",
            Self::Inches => "in",
            Self::Percent => "%",
            Self::MilliPerSecond => "1e-3/s",
            Self::JoulesPerMeterPerSecond => "J/m/s",
        }
    }
}

/// A height measured from the radar's own antenna, in metres.
///
/// Separate from [`HeightMslM`] because the two are not interchangeable and the
/// mistake is invisible. KTLX sits at 370 m; comparing a 3000 m MSL freezing
/// level against a 3000 m above-radar echo top puts the melting layer 370 m in
/// the wrong place, which is enough to move a hail estimate a whole size class.
/// A bare `f32` lets that happen silently. A newtype does not.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
pub struct HeightArlM(pub f32);

/// A height measured from mean sea level, in metres.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
pub struct HeightMslM(pub f32);

impl HeightArlM {
    pub fn to_msl(self, radar_site_elevation_m: f32) -> HeightMslM {
        HeightMslM(self.0 + radar_site_elevation_m)
    }

    pub fn metres(self) -> f32 {
        self.0
    }
}

impl HeightMslM {
    /// The same height expressed from the antenna.
    ///
    /// Takes the site elevation rather than assuming sea level, because a radar
    /// on a mountain would otherwise report every echo top a kilometre too tall.
    pub fn to_arl(self, radar_site_elevation_m: f32) -> HeightArlM {
        HeightArlM(self.0 - radar_site_elevation_m)
    }

    pub fn metres(self) -> f32 {
        self.0
    }
}

/// One knot is exactly 1852 m per 3600 s, so one metre per second is
/// `3600 / 1852` knots. Written as the quotient of the two exact definitions
/// rather than as a rounded literal, so the constant cannot drift.
pub const METERS_PER_SECOND_TO_KNOTS: f64 = 3600.0 / 1852.0;

/// The international foot is exactly 0.3048 m, so a kilofoot is exactly 304.8 m.
pub const METERS_TO_KILOFEET: f64 = 1.0 / 304.8;

/// The inch is exactly 25.4 mm.
pub const MILLIMETERS_TO_INCHES: f64 = 1.0 / 25.4;

/// VIL density is stored in kg/m3 and read in g/m3.
pub const KILOGRAMS_PER_CUBIC_METER_TO_GRAMS: f64 = 1000.0;

/// Shear derivatives are stored in 1/s and read in 1e-3/s.
pub const PER_SECOND_TO_MILLI_PER_SECOND: f64 = 1000.0;

/// The conversion from an engine value to a display value.
///
/// Every unit this application needs is affine, so one transform covers all of
/// them and there is exactly one place a conversion can be wrong. Both fields
/// are `f64`: a tick ladder built in `f32` visibly wobbles on labels like
/// `39.37`, and the arithmetic is not hot.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AffineTransform {
    pub scale: f64,
    pub offset: f64,
}

impl AffineTransform {
    /// Display units equal engine units. The common case, and the safe default.
    pub const IDENTITY: Self = Self {
        scale: 1.0,
        offset: 0.0,
    };

    /// A pure scaling, which covers every conversion in this application.
    /// Offsets exist for a future temperature scale, not for present use.
    pub const fn scaled(scale: f64) -> Self {
        Self { scale, offset: 0.0 }
    }

    pub const fn new(scale: f64, offset: f64) -> Self {
        Self { scale, offset }
    }

    pub fn to_display(self, engine_value: f32) -> f64 {
        f64::from(engine_value) * self.scale + self.offset
    }

    /// The inverse, used to place a display-unit tick back on an engine-unit
    /// bar. Never used to feed a colour table.
    pub fn to_engine(self, display_value: f64) -> f32 {
        ((display_value - self.offset) / self.scale) as f32
    }

    /// A transform with a zero, infinite, or NaN scale cannot be inverted, so a
    /// legend built from it would silently place every tick at the same place.
    /// The registry test asserts no built-in domain carries one.
    pub fn is_invertible(self) -> bool {
        self.scale.is_finite() && self.scale != 0.0 && self.offset.is_finite()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn twelve_thousand_metres_displays_as_thirty_nine_point_three_seven_kilofeet() {
        let transform = AffineTransform::scaled(METERS_TO_KILOFEET);
        let display = transform.to_display(12_000.0);
        assert!(
            (display - 39.370_078_740_157_48).abs() < 1e-12,
            "12 000 m should read 39.37 kft, got {display}"
        );
    }

    #[test]
    fn an_echo_top_round_trips_back_to_the_stored_metres() {
        let transform = AffineTransform::scaled(METERS_TO_KILOFEET);
        let engine = transform.to_engine(transform.to_display(12_000.0));
        assert_eq!(engine, 12_000.0);
    }

    #[test]
    fn fifty_metres_per_second_displays_as_ninety_seven_point_two_knots() {
        let transform = AffineTransform::scaled(METERS_PER_SECOND_TO_KNOTS);
        let display = transform.to_display(50.0);
        assert!(
            (display - 97.192_224_622_030_24).abs() < 1e-12,
            "50 m/s should read 97.19 kt, got {display}"
        );
    }

    // The tolerances below are not slack hiding a unit error; they are the
    // width of an `f32`. `0.004_f32` widened to `f64` is 0.004000000189989805,
    // so a conversion that is exactly right still cannot land on 4.0. An `f32`
    // carries about seven significant digits, so 1e-6 relative is the tightest
    // honest bound. A factor-of-1000 mistake would miss by 1e+3.
    #[test]
    fn four_thousandths_of_a_kilogram_per_cubic_metre_displays_as_four_grams() {
        let transform = AffineTransform::scaled(KILOGRAMS_PER_CUBIC_METER_TO_GRAMS);
        let display = transform.to_display(0.004);
        assert!(
            (display - 4.0).abs() < 1e-6,
            "0.004 kg/m3 should read 4 g/m3, got {display}"
        );
    }

    #[test]
    fn twenty_five_point_four_millimetres_displays_as_one_inch() {
        let transform = AffineTransform::scaled(MILLIMETERS_TO_INCHES);
        let display = transform.to_display(25.4);
        assert!(
            (display - 1.0).abs() < 1e-6,
            "25.4 mm should read 1 in, got {display}"
        );
    }

    #[test]
    fn five_thousandths_per_second_displays_as_five() {
        let transform = AffineTransform::scaled(PER_SECOND_TO_MILLI_PER_SECOND);
        let display = transform.to_display(0.005);
        assert!(
            (display - 5.0).abs() < 1e-5,
            "0.005 1/s should read 5, got {display}"
        );
    }

    #[test]
    fn the_identity_transform_changes_nothing() {
        assert_eq!(AffineTransform::IDENTITY.to_display(37.5), 37.5);
        assert_eq!(AffineTransform::IDENTITY.to_engine(37.5), 37.5);
    }

    #[test]
    fn received_power_is_not_labelled_as_an_unreferenced_decibel_ratio() {
        assert_eq!(PhysicalUnit::Dbm.label(), "dBm");
        assert_eq!(DisplayUnit::Dbm.label(), "dBm");
        assert_ne!(PhysicalUnit::Dbm.label(), PhysicalUnit::Decibels.label());
    }

    #[test]
    fn a_zero_scale_transform_is_not_invertible() {
        assert!(!AffineTransform::scaled(0.0).is_invertible());
        assert!(AffineTransform::scaled(METERS_TO_KILOFEET).is_invertible());
    }

    #[test]
    fn relative_iq_power_is_not_labelled_as_an_absolute_decibel_quantity() {
        assert_eq!(PhysicalUnit::Decibels.label(), "dB");
        assert_eq!(DisplayUnit::Decibels.label(), "dB");
        assert_eq!(
            PhysicalUnit::RelativeIqPowerDb.label(),
            "dB re stored I/Q unit²"
        );
        assert_eq!(
            DisplayUnit::RelativeIqPowerDb.label(),
            "dB re stored I/Q unit²"
        );
    }

    /// KTLX's antenna sits near 370 m. A thermal level quoted from a sounding
    /// is above sea level; an echo top measured by the radar is above the
    /// antenna. Comparing them without this conversion misplaces the melting
    /// layer by the site elevation.
    #[test]
    fn a_height_above_the_radar_converts_to_sea_level_by_adding_the_site_elevation() {
        let top = HeightArlM(12_000.0);
        assert_eq!(top.to_msl(370.0), HeightMslM(12_370.0));
    }

    #[test]
    fn a_sea_level_height_converts_back_to_the_radar_frame() {
        let freezing_level = HeightMslM(3_370.0);
        assert_eq!(freezing_level.to_arl(370.0), HeightArlM(3_000.0));
    }

    #[test]
    fn converting_a_height_to_the_other_frame_and_back_returns_it_unchanged() {
        let original = HeightArlM(4_250.0);
        assert_eq!(original.to_msl(370.0).to_arl(370.0), original);
    }

    #[test]
    fn a_radar_below_sea_level_still_converts_in_the_right_direction() {
        // Not hypothetical: several operational radars sit below sea level.
        assert_eq!(HeightArlM(1_000.0).to_msl(-50.0), HeightMslM(950.0));
    }
}
