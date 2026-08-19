//! The thermal environment the hail products need, and where its numbers came
//! from.
//!
//! Every hail estimate is a function of two heights: the freezing level and the
//! -20 C level. This application has no model or sounding store, so those
//! heights are either entered by an analyst, supplied by something external, or
//! guessed. A guess produces a number that looks exactly like a measurement,
//! which is why provenance travels with the values rather than beside them: a
//! MESH of 40 mm computed from a real sounding and one computed from a
//! continental average are different claims, and the pane must not render them
//! identically.
//!
//! Both heights are held **above radar level**. The radar measures heights from
//! its own antenna; a sounding quotes them from sea level. Converting once, at
//! the boundary, is the whole discipline here — see [`HailEnvironment::from_msl`].

use crate::units::{HeightArlM, HeightMslM};

/// The tallest thermal level worth accepting. Above this the input is a typo,
/// not an environment: the -20 C level does not reach 20 km in any atmosphere
/// this radar will see.
const MAX_THERMAL_HEIGHT_ARL_M: f32 = 20_000.0;

/// Where an environment's numbers came from.
#[derive(Clone, Debug, PartialEq)]
pub enum HailEnvironmentProvenance {
    /// An analyst typed heights already referenced to the antenna.
    UserEnteredArl,
    /// An analyst typed sea-level heights; the site elevation used to convert
    /// them is recorded so the conversion can be audited.
    UserEnteredMsl { radar_site_elevation_m: f32 },
    /// Supplied by something outside this application, labelled with its source.
    MeasuredExternal { label: String },
    /// The built-in guess. Never a measurement.
    ClimatologicalFallbackV1,
}

impl HailEnvironmentProvenance {
    /// The badge drawn on the pane, legend, and probe.
    pub fn badge(&self) -> &'static str {
        match self {
            Self::UserEnteredArl | Self::UserEnteredMsl { .. } => "USER ENV",
            Self::MeasuredExternal { .. } => "MEASURED ENV",
            Self::ClimatologicalFallbackV1 => "ASSUMED ENV",
        }
    }

    /// Whether these numbers were measured rather than assumed. Drives whether
    /// a hail product may be presented without a warning.
    pub fn is_assumed(&self) -> bool {
        matches!(self, Self::ClimatologicalFallbackV1)
    }
}

/// Why an environment was rejected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HailEnvironmentError {
    /// The freezing level is below the antenna. The column above the radar is
    /// then entirely above freezing, which the weighting function cannot mean.
    FreezingLevelBelowRadar,
    /// The -20 C level is at or below the freezing level. Temperature decreases
    /// with height, so this ordering is impossible and would make the thermal
    /// weighting divide by zero or run backwards.
    ThermalLevelsOutOfOrder,
    /// A level is above the top of any plausible troposphere.
    ImplausiblyHigh,
    /// A level is not a finite number.
    NotFinite,
}

impl HailEnvironmentError {
    pub const fn label(self) -> &'static str {
        match self {
            Self::FreezingLevelBelowRadar => "freezing level is below the radar",
            Self::ThermalLevelsOutOfOrder => "the -20 C level must be above the freezing level",
            Self::ImplausiblyHigh => "thermal level is above 20 km",
            Self::NotFinite => "thermal level is not a number",
        }
    }
}

/// The freezing and -20 C levels, above radar level, with their provenance.
#[derive(Clone, Debug, PartialEq)]
pub struct HailEnvironment {
    freezing_level: HeightArlM,
    minus_twenty_level: HeightArlM,
    provenance: HailEnvironmentProvenance,
    /// When the environment was valid, as an RFC3339 string, when known.
    valid_time: Option<String>,
}

impl HailEnvironment {
    /// Build from heights already referenced to the antenna.
    pub fn new(
        freezing_level: HeightArlM,
        minus_twenty_level: HeightArlM,
        provenance: HailEnvironmentProvenance,
    ) -> Result<Self, HailEnvironmentError> {
        validate(freezing_level, minus_twenty_level)?;
        Ok(Self {
            freezing_level,
            minus_twenty_level,
            provenance,
            valid_time: None,
        })
    }

    /// Build from sea-level heights, converting once, here.
    ///
    /// This is the only place an MSL height may enter. Everything downstream
    /// takes [`HeightArlM`], so a sounding height cannot reach an algorithm
    /// without passing through this conversion.
    pub fn from_msl(
        freezing_level: HeightMslM,
        minus_twenty_level: HeightMslM,
        radar_site_elevation_m: f32,
    ) -> Result<Self, HailEnvironmentError> {
        if !radar_site_elevation_m.is_finite() {
            return Err(HailEnvironmentError::NotFinite);
        }
        Self::new(
            freezing_level.to_arl(radar_site_elevation_m),
            minus_twenty_level.to_arl(radar_site_elevation_m),
            HailEnvironmentProvenance::UserEnteredMsl {
                radar_site_elevation_m,
            },
        )
    }

    /// The built-in guess: a 3 km freezing level and a 6 km -20 C level.
    ///
    /// A generic warm-season continental United States working assumption. It
    /// is not climatology for any particular place or day, and it is wrong by
    /// kilometres in a cold-season or tropical airmass. It exists so hail
    /// products can be looked at at all without a sounding, and it is badged
    /// `ASSUMED ENV` everywhere it reaches. It can be switched off entirely,
    /// in which case hail products report themselves unavailable — which is a
    /// better answer than a confident wrong number.
    pub fn climatological_fallback() -> Self {
        Self {
            freezing_level: HeightArlM(3_000.0),
            minus_twenty_level: HeightArlM(6_000.0),
            provenance: HailEnvironmentProvenance::ClimatologicalFallbackV1,
            valid_time: None,
        }
    }

    pub fn with_valid_time(mut self, valid_time: Option<String>) -> Self {
        self.valid_time = valid_time;
        self
    }

    pub fn freezing_level(&self) -> HeightArlM {
        self.freezing_level
    }

    pub fn minus_twenty_level(&self) -> HeightArlM {
        self.minus_twenty_level
    }

    pub fn provenance(&self) -> &HailEnvironmentProvenance {
        &self.provenance
    }

    pub fn valid_time(&self) -> Option<&str> {
        self.valid_time.as_deref()
    }

    /// The depth over which the thermal weight ramps from 0 to 1. Guaranteed
    /// positive by construction, so a weighting function may divide by it.
    pub fn thermal_depth_m(&self) -> f32 {
        self.minus_twenty_level.0 - self.freezing_level.0
    }

    /// A one-line summary for a pane header or an export.
    pub fn summary(&self) -> String {
        format!(
            "{} H0 {:.1} km / H-20 {:.1} km ARL",
            self.provenance.badge(),
            self.freezing_level.0 / 1000.0,
            self.minus_twenty_level.0 / 1000.0
        )
    }

    /// A stable fingerprint for a derived-field cache key.
    ///
    /// Two environments that differ only in provenance must produce different
    /// keys: switching from the fallback to a real sounding with coincidentally
    /// identical heights still changes what the badge must say, and a cached
    /// field carrying the old badge would be a lie.
    pub fn fingerprint(&self) -> u64 {
        let mut hash = 0xcbf2_9ce4_8422_2325_u64;
        let mut mix = |value: u64| {
            hash ^= value;
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        };
        mix(u64::from(self.freezing_level.0.to_bits()));
        mix(u64::from(self.minus_twenty_level.0.to_bits()));
        // Mix the provenance kind and its payload as two separate values, so a
        // site elevation can never collide with a discriminant.
        let (kind, payload) = match &self.provenance {
            HailEnvironmentProvenance::UserEnteredArl => (1, 0),
            HailEnvironmentProvenance::UserEnteredMsl {
                radar_site_elevation_m,
            } => (2, u64::from(radar_site_elevation_m.to_bits())),
            HailEnvironmentProvenance::MeasuredExternal { label } => (3, hash_bytes(label)),
            HailEnvironmentProvenance::ClimatologicalFallbackV1 => (4, 0),
        };
        mix(kind);
        mix(payload);
        mix(self.valid_time.as_deref().map_or(0, hash_bytes));
        hash
    }
}

fn hash_bytes(text: &str) -> u64 {
    text.bytes().fold(0_u64, |accumulated, byte| {
        accumulated.wrapping_mul(31).wrapping_add(u64::from(byte))
    })
}

fn validate(
    freezing_level: HeightArlM,
    minus_twenty_level: HeightArlM,
) -> Result<(), HailEnvironmentError> {
    if !freezing_level.0.is_finite() || !minus_twenty_level.0.is_finite() {
        return Err(HailEnvironmentError::NotFinite);
    }
    if freezing_level.0 < 0.0 {
        return Err(HailEnvironmentError::FreezingLevelBelowRadar);
    }
    if minus_twenty_level.0 <= freezing_level.0 {
        return Err(HailEnvironmentError::ThermalLevelsOutOfOrder);
    }
    if minus_twenty_level.0 > MAX_THERMAL_HEIGHT_ARL_M {
        return Err(HailEnvironmentError::ImplausiblyHigh);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fallback_is_three_and_six_kilometres_and_says_it_is_assumed() {
        let environment = HailEnvironment::climatological_fallback();
        assert_eq!(environment.freezing_level(), HeightArlM(3_000.0));
        assert_eq!(environment.minus_twenty_level(), HeightArlM(6_000.0));
        assert!(environment.provenance().is_assumed());
        assert_eq!(
            environment.summary(),
            "ASSUMED ENV H0 3.0 km / H-20 6.0 km ARL"
        );
    }

    #[test]
    fn a_sounding_in_sea_level_heights_is_converted_exactly_once() {
        // A 3370 m MSL freezing level over a 370 m antenna is 3000 m ARL.
        let environment =
            HailEnvironment::from_msl(HeightMslM(3_370.0), HeightMslM(6_370.0), 370.0)
                .expect("an ordered pair of levels is valid");
        assert_eq!(environment.freezing_level(), HeightArlM(3_000.0));
        assert_eq!(environment.minus_twenty_level(), HeightArlM(6_000.0));
        assert!(!environment.provenance().is_assumed());
        assert_eq!(environment.provenance().badge(), "USER ENV");
    }

    #[test]
    fn inverted_thermal_levels_are_refused() {
        // Temperature falls with height, so -20 C above 0 C is the only order
        // that exists. Accepting the reverse would divide by a negative depth.
        let error = HailEnvironment::new(
            HeightArlM(6_000.0),
            HeightArlM(3_000.0),
            HailEnvironmentProvenance::UserEnteredArl,
        )
        .expect_err("inverted levels must be refused");
        assert_eq!(error, HailEnvironmentError::ThermalLevelsOutOfOrder);
    }

    #[test]
    fn equal_thermal_levels_are_refused_because_the_weight_would_divide_by_zero() {
        let error = HailEnvironment::new(
            HeightArlM(4_000.0),
            HeightArlM(4_000.0),
            HailEnvironmentProvenance::UserEnteredArl,
        )
        .expect_err("a zero-depth ramp must be refused");
        assert_eq!(error, HailEnvironmentError::ThermalLevelsOutOfOrder);
    }

    #[test]
    fn a_freezing_level_below_the_antenna_is_refused() {
        let error = HailEnvironment::new(
            HeightArlM(-100.0),
            HeightArlM(3_000.0),
            HailEnvironmentProvenance::UserEnteredArl,
        )
        .expect_err("a sub-antenna freezing level must be refused");
        assert_eq!(error, HailEnvironmentError::FreezingLevelBelowRadar);
    }

    #[test]
    fn a_thermal_level_above_twenty_kilometres_is_refused() {
        let error = HailEnvironment::new(
            HeightArlM(3_000.0),
            HeightArlM(25_000.0),
            HailEnvironmentProvenance::UserEnteredArl,
        )
        .expect_err("a stratospheric -20 C level must be refused");
        assert_eq!(error, HailEnvironmentError::ImplausiblyHigh);
    }

    #[test]
    fn a_non_finite_level_is_refused() {
        let error = HailEnvironment::new(
            HeightArlM(f32::NAN),
            HeightArlM(6_000.0),
            HailEnvironmentProvenance::UserEnteredArl,
        )
        .expect_err("NaN must be refused");
        assert_eq!(error, HailEnvironmentError::NotFinite);
    }

    #[test]
    fn the_thermal_depth_is_the_ramp_the_weighting_function_divides_by() {
        assert_eq!(
            HailEnvironment::climatological_fallback().thermal_depth_m(),
            3_000.0
        );
    }

    #[test]
    fn an_assumed_and_a_measured_environment_with_equal_heights_have_different_keys() {
        // Otherwise a field computed under the guess would be served under the
        // measured badge, which is the exact lie provenance exists to prevent.
        let assumed = HailEnvironment::climatological_fallback();
        let measured = HailEnvironment::new(
            HeightArlM(3_000.0),
            HeightArlM(6_000.0),
            HailEnvironmentProvenance::MeasuredExternal {
                label: "KOUN 00Z".to_owned(),
            },
        )
        .expect("valid");
        assert_eq!(assumed.freezing_level(), measured.freezing_level());
        assert_ne!(assumed.fingerprint(), measured.fingerprint());
    }

    #[test]
    fn changing_a_thermal_level_changes_the_key() {
        let original = HailEnvironment::climatological_fallback();
        let warmer = HailEnvironment::new(
            HeightArlM(3_500.0),
            HeightArlM(6_000.0),
            HailEnvironmentProvenance::ClimatologicalFallbackV1,
        )
        .expect("valid");
        assert_ne!(original.fingerprint(), warmer.fingerprint());
    }

    #[test]
    fn changing_only_the_valid_time_changes_the_key() {
        let untimed = HailEnvironment::climatological_fallback();
        let timed = HailEnvironment::climatological_fallback()
            .with_valid_time(Some("2013-05-20T20:00:00Z".to_owned()));
        assert_ne!(untimed.fingerprint(), timed.fingerprint());
    }
}
