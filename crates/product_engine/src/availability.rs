//! Whether a product can be drawn from a given volume, and what an analyst
//! should be told about it if it can.
//!
//! Availability is not a boolean. "Cannot draw MESH" and "drew MESH from an
//! assumed freezing level" are different claims, and collapsing them into one
//! greyed-out menu item throws away the only information that would let an
//! analyst decide whether to trust the picture. So an unavailable product
//! carries the reason it is unavailable, and an available one carries the
//! qualifiers that limit it.

use radar_core::MomentType;

/// Why a product cannot be produced from this volume.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UnavailableReason {
    /// The volume has no cut carrying the moment this product reads.
    MissingMoment(MomentType),
    /// The moment exists but no cut carrying it is usable.
    NoUsableCut,
    /// A vertical product needs more than one distinct elevation.
    InsufficientUniqueElevations,
    /// Velocity is present but no usable Nyquist accompanies it, so it cannot
    /// be unfolded and must not be presented as if it had been.
    NoDealiasedVelocity,
    /// Geographic products need the site's latitude and longitude.
    MissingRadarLocation,
    /// Anything comparing a height against a thermal level needs the site's
    /// elevation, because the two are in different reference frames without it.
    MissingRadarElevation,
    MissingHailEnvironment,
    InvalidHailEnvironment,
    /// The algorithm's primary source has not been read, so no number is safe
    /// to print. This is a correct outcome, not a defect.
    AlgorithmPendingPrimaryVerification,
    /// The product exists but experimental products are switched off.
    ExperimentalProductsHidden,
    /// A growing volume does not yet contain the cut this product needs.
    PartialVolumeHasNoRequiredCut,
    /// A saved workspace named a product this build does not have. The name is
    /// kept so the status line can say which one.
    UnknownProduct(String),
}

impl UnavailableReason {
    /// Text for a pane header or a disabled menu entry. Short, and specific
    /// enough that an analyst knows whether waiting will help.
    pub fn label(&self) -> String {
        match self {
            Self::MissingMoment(moment) => format!("no {moment} in this volume"),
            Self::NoUsableCut => "no usable cut".to_owned(),
            Self::InsufficientUniqueElevations => "needs more than one elevation".to_owned(),
            Self::NoDealiasedVelocity => "velocity has no usable Nyquist".to_owned(),
            Self::MissingRadarLocation => "radar location unknown".to_owned(),
            Self::MissingRadarElevation => "radar elevation unknown".to_owned(),
            Self::MissingHailEnvironment => "no hail environment set".to_owned(),
            Self::InvalidHailEnvironment => "hail environment is not ordered".to_owned(),
            Self::AlgorithmPendingPrimaryVerification => "primary equation not verified".to_owned(),
            Self::ExperimentalProductsHidden => "experimental products hidden".to_owned(),
            Self::PartialVolumeHasNoRequiredCut => "waiting for the required cut".to_owned(),
            Self::UnknownProduct(id) => format!("unknown product {id}"),
        }
    }
}

/// A limitation on a product that was produced.
///
/// Every one of these changes what the numbers mean. They reach the pane
/// header, the legend, and the probe, because a MESH computed from a guessed
/// freezing level and one computed from a measured sounding are different
/// claims that must not look identical on screen.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum AvailabilityQualifier {
    /// Built from a first-look snapshot.
    Preview,
    /// Built from a volume that is still growing.
    Partial,
    /// The echo continued above the highest beam, so the value is a floor.
    Topped,
    /// The vertical integration could not span the whole column.
    VerticallyTruncated,
    /// The thermal levels are the built-in fallback, not a measurement.
    AssumedEnvironment,
    UserEnteredEnvironment,
    MeasuredEnvironment,
    Experimental,
    /// A value crossed a soft plausibility bound.
    PlausibilityWarning,
}

impl AvailabilityQualifier {
    /// The short all-caps badge drawn on the pane.
    pub const fn badge(self) -> &'static str {
        match self {
            Self::Preview => "PREVIEW",
            Self::Partial => "PARTIAL",
            Self::Topped => "TOPPED",
            Self::VerticallyTruncated => "TRUNCATED",
            Self::AssumedEnvironment => "ASSUMED ENV",
            Self::UserEnteredEnvironment => "USER ENV",
            Self::MeasuredEnvironment => "MEASURED ENV",
            Self::Experimental => "EXPERIMENTAL",
            Self::PlausibilityWarning => "IMPLAUSIBLE",
        }
    }
}

/// The result of asking whether a product can be drawn.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProductAvailability {
    Available {
        qualifiers: Vec<AvailabilityQualifier>,
    },
    Unavailable(UnavailableReason),
}

impl ProductAvailability {
    /// Available with nothing to warn about.
    pub fn available() -> Self {
        Self::Available {
            qualifiers: Vec::new(),
        }
    }

    pub fn available_with(qualifiers: Vec<AvailabilityQualifier>) -> Self {
        Self::Available { qualifiers }
    }

    pub fn is_available(&self) -> bool {
        matches!(self, Self::Available { .. })
    }

    pub fn qualifiers(&self) -> &[AvailabilityQualifier] {
        match self {
            Self::Available { qualifiers } => qualifiers,
            Self::Unavailable(_) => &[],
        }
    }

    pub fn unavailable_reason(&self) -> Option<&UnavailableReason> {
        match self {
            Self::Available { .. } => None,
            Self::Unavailable(reason) => Some(reason),
        }
    }
}

/// What a product needs from a volume before it can be drawn.
///
/// Kept separate from how the product is computed so the picker can grey an
/// entry out without building anything. The registry test pins each rule
/// against its product's computation so the two cannot drift apart.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AvailabilityRule {
    /// One base moment read straight from a cut.
    RequiresMoment(MomentType),
    /// Base velocity plus a usable Nyquist, so the fold can be undone.
    RequiresDealiasedVelocity,
}

impl AvailabilityRule {
    /// The moment this rule ultimately reads. Every velocity-derived product
    /// answers `Velocity`.
    pub fn required_moment(&self) -> MomentType {
        match self {
            Self::RequiresMoment(moment) => moment.clone(),
            Self::RequiresDealiasedVelocity => MomentType::Velocity,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_moment_names_the_moment_it_wanted() {
        let reason = UnavailableReason::MissingMoment(MomentType::DifferentialReflectivity);
        assert_eq!(reason.label(), "no ZDR in this volume");
    }

    #[test]
    fn an_unknown_saved_product_keeps_its_name_for_the_status_line() {
        let reason = UnavailableReason::UnknownProduct("MESH".to_owned());
        assert_eq!(reason.label(), "unknown product MESH");
    }

    #[test]
    fn an_available_product_with_no_qualifiers_reports_none() {
        let availability = ProductAvailability::available();
        assert!(availability.is_available());
        assert!(availability.qualifiers().is_empty());
        assert_eq!(availability.unavailable_reason(), None);
    }

    #[test]
    fn an_unavailable_product_reports_no_qualifiers_and_a_reason() {
        let availability = ProductAvailability::Unavailable(UnavailableReason::NoUsableCut);
        assert!(!availability.is_available());
        assert!(availability.qualifiers().is_empty());
        assert_eq!(
            availability.unavailable_reason(),
            Some(&UnavailableReason::NoUsableCut)
        );
    }

    #[test]
    fn every_velocity_rule_reads_the_velocity_moment() {
        assert_eq!(
            AvailabilityRule::RequiresDealiasedVelocity.required_moment(),
            MomentType::Velocity
        );
    }
}
