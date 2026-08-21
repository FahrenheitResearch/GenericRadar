//! The pane's `Copy` handle to a product, and its adapter to the registry.
//!
//! A pane stores which product it shows, and it does so many times per frame,
//! so the handle stays a small `Copy` enum. Everything the handle *means* —
//! its name, units, colour domain, plausible range, which moment it reads,
//! which leg of a split cut it wants — belongs to `product_engine` and is
//! looked up, never restated here. Before that split existed, a product's name
//! lived in one match and its units in another, and the two drifted.
//!
//! This adapter is temporary. Once pane state stores `ProductId` directly,
//! the fixed handle and its catalog-sized match can go away.

use product_engine::registry::ProductDescriptor;
use product_engine::{DisplayDomain, ProductRegistry};
use radar_core::{MomentType, ProductId, RadarVolume};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DisplayProduct {
    #[default]
    Reflectivity,
    RelativePower,
    Velocity,
    DealiasedVelocity,
    StormRelativeVelocity,
    DealiasedStormRelativeVelocity,
    SpectrumWidth,
    DifferentialReflectivity,
    CorrelationCoefficient,
    DifferentialPhase,
    SpecificDifferentialPhase,
    DowReceivedPowerH1,
    DowReceivedPowerH2,
    DowReceivedPowerHMerged,
    DowReceivedPowerV1,
    DowReceivedPowerV2,
    DowReceivedPowerVMerged,
    DowReflectivityH1,
    DowReflectivityH2,
    DowReflectivityHMerged,
    DowReflectivityV1,
    DowReflectivityV2,
    DowReflectivityVMerged,
    CompositeReflectivity,
    EchoTop18,
    Vil,
    VilDensity,
    Mesh,
    ProbabilityOfHail,
    ProbabilityOfSevereHail,
}

impl DisplayProduct {
    pub const ALL: [Self; 30] = [
        Self::Reflectivity,
        Self::RelativePower,
        Self::Velocity,
        Self::DealiasedVelocity,
        Self::StormRelativeVelocity,
        Self::DealiasedStormRelativeVelocity,
        Self::SpectrumWidth,
        Self::DifferentialReflectivity,
        Self::CorrelationCoefficient,
        Self::DifferentialPhase,
        Self::SpecificDifferentialPhase,
        Self::DowReceivedPowerH1,
        Self::DowReceivedPowerH2,
        Self::DowReceivedPowerHMerged,
        Self::DowReceivedPowerV1,
        Self::DowReceivedPowerV2,
        Self::DowReceivedPowerVMerged,
        Self::DowReflectivityH1,
        Self::DowReflectivityH2,
        Self::DowReflectivityHMerged,
        Self::DowReflectivityV1,
        Self::DowReflectivityV2,
        Self::DowReflectivityVMerged,
        Self::CompositeReflectivity,
        Self::EchoTop18,
        Self::Vil,
        Self::VilDensity,
        Self::Mesh,
        Self::ProbabilityOfHail,
        Self::ProbabilityOfSevereHail,
    ];

    /// The canonical registry id. This is the one piece of product identity the
    /// handle keeps for itself, because it is what a saved workspace holds.
    pub const fn id(self) -> &'static str {
        match self {
            Self::Reflectivity => "REF",
            Self::RelativePower => "PWR_REL",
            Self::Velocity => "VEL",
            Self::DealiasedVelocity => "DVEL",
            Self::StormRelativeVelocity => "SRV",
            Self::DealiasedStormRelativeVelocity => "DSRV",
            Self::SpectrumWidth => "SW",
            Self::DifferentialReflectivity => "ZDR",
            Self::CorrelationCoefficient => "RHO",
            Self::DifferentialPhase => "PHI",
            Self::SpecificDifferentialPhase => "KDP",
            Self::DowReceivedPowerH1 => "DBMH1",
            Self::DowReceivedPowerH2 => "DBMH2",
            Self::DowReceivedPowerHMerged => "DBMHM",
            Self::DowReceivedPowerV1 => "DBMV1",
            Self::DowReceivedPowerV2 => "DBMV2",
            Self::DowReceivedPowerVMerged => "DBMVM",
            Self::DowReflectivityH1 => "DBZH1",
            Self::DowReflectivityH2 => "DBZH2",
            Self::DowReflectivityHMerged => "DBZHM",
            Self::DowReflectivityV1 => "DBZV1",
            Self::DowReflectivityV2 => "DBZV2",
            Self::DowReflectivityVMerged => "DBZVM",
            Self::CompositeReflectivity => "CREF",
            Self::EchoTop18 => "ET18",
            Self::Vil => "VIL",
            Self::VilDensity => "VILD",
            Self::Mesh => "MESH",
            Self::ProbabilityOfHail => "POH",
            Self::ProbabilityOfSevereHail => "POSH",
        }
    }

    /// The volume product this handle names, if it names one.
    pub fn derived_volume(self) -> Option<product_engine::registry::DerivedVolumeId> {
        self.descriptor().computation.derived_volume()
    }

    /// The registry entry behind this handle.
    ///
    /// Every variant is pinned to an entry by a test, so the lookup cannot miss
    /// at run time. A `LazyLock`-backed lookup over ten entries; not hot.
    pub fn descriptor(self) -> &'static ProductDescriptor {
        ProductRegistry::builtin()
            .get(self.id())
            .expect("every display product is pinned to a registry entry by test")
    }

    pub fn label(self) -> &'static str {
        self.descriptor().display_name
    }

    /// Units, colour domain, and plausible range.
    pub fn domain(self) -> DisplayDomain {
        self.descriptor().domain
    }

    pub fn product_id(self) -> ProductId {
        ProductId(self.id().to_owned())
    }

    /// The handle for a saved product id, or `None` when this build has no such
    /// product. The caller decides what to do about it; nothing here silently
    /// rewrites an analyst's saved workspace.
    pub fn try_from_product_id(id: &ProductId) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|product| product.id().eq_ignore_ascii_case(id.0.trim()))
    }

    /// The handle for a saved product id, falling back to reflectivity.
    ///
    /// The fallback is a stop-gap: an unknown id currently draws reflectivity
    /// without saying so. Surfacing it needs a pane header that can show
    /// `unavailable: unknown product`, which arrives with the legend and status
    /// work. Until then this at least draws something rather than panicking.
    pub fn from_product_id(id: &ProductId) -> Self {
        Self::try_from_product_id(id).unwrap_or_default()
    }

    pub fn source_moment(self) -> MomentType {
        self.descriptor().computation.source_moment()
    }

    pub fn uses_dealiased_velocity(self) -> bool {
        self.descriptor().computation.uses_dealiased_velocity()
    }

    pub fn is_storm_relative(self) -> bool {
        self.descriptor().computation.is_storm_relative()
    }

    pub fn is_available_in_cut(self, volume: &RadarVolume, cut_index: usize) -> bool {
        volume
            .cuts
            .get(cut_index)
            .is_some_and(|cut| cut.moments.contains_key(&self.source_moment()))
    }

    /// The lowest-indexed cut carrying this product's moment.
    ///
    /// Known defective, and left in place deliberately until split-cut-aware
    /// selection lands. NEXRAD splits its lowest tilts into a long-range
    /// surveillance scan and a short-range Doppler scan at nearly the same
    /// elevation, and both may carry reflectivity. Picking by index takes
    /// whichever the file lists first, so reflectivity is often drawn from the
    /// Doppler leg and folds to purple past about 150 km. The descriptor
    /// already records which leg each product wants in `cut_policy`; nothing
    /// reads it yet.
    pub fn first_available_cut(self, volume: &RadarVolume) -> Option<usize> {
        volume
            .cuts
            .iter()
            .position(|cut| cut.moments.contains_key(&self.source_moment()))
    }

    pub fn next_available_cut(
        self,
        volume: &RadarVolume,
        current: usize,
        delta: isize,
    ) -> Option<usize> {
        if volume.cuts.is_empty() {
            return None;
        }
        let current = current.min(volume.cuts.len() - 1);
        if delta == 0 {
            return self.is_available_in_cut(volume, current).then_some(current);
        }

        let mut index = current as isize;
        loop {
            index = index.saturating_add(delta);
            if index < 0 || index >= volume.cuts.len() as isize {
                return None;
            }
            let candidate = index as usize;
            if self.is_available_in_cut(volume, candidate) {
                return Some(candidate);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use product_engine::PhysicalUnit;

    #[test]
    fn product_ids_round_trip() {
        for product in DisplayProduct::ALL {
            assert_eq!(
                DisplayProduct::from_product_id(&product.product_id()),
                product
            );
        }
    }

    #[test]
    fn every_handle_resolves_to_a_registry_entry() {
        for product in DisplayProduct::ALL {
            assert_eq!(product.descriptor().id.0, product.id());
        }
    }

    #[test]
    fn the_handles_cover_every_selectable_registry_product() {
        // If the registry gains a product the panes cannot name, the picker
        // would silently omit it. That is the drift this pins.
        let selectable: Vec<&str> = ProductRegistry::builtin()
            .selectable_products(true)
            .map(|descriptor| descriptor.id.0.as_str())
            .collect();
        let handles: Vec<&str> = DisplayProduct::ALL
            .into_iter()
            .map(DisplayProduct::id)
            .collect();
        assert_eq!(handles, selectable);
    }

    #[test]
    fn velocity_variants_share_velocity_source() {
        assert_eq!(
            DisplayProduct::DealiasedStormRelativeVelocity.source_moment(),
            MomentType::Velocity
        );
    }

    #[test]
    fn relative_power_has_its_own_uncalibrated_source_moment() {
        assert_eq!(
            DisplayProduct::RelativePower.source_moment(),
            MomentType::RelativePower
        );
        assert_ne!(
            DisplayProduct::RelativePower.source_moment(),
            MomentType::Reflectivity
        );
    }

    #[test]
    fn an_unknown_saved_product_is_reported_rather_than_guessed() {
        let unknown = ProductId("AZSHR".to_owned());
        assert_eq!(DisplayProduct::try_from_product_id(&unknown), None);
    }

    #[test]
    fn a_saved_product_id_resolves_regardless_of_case_or_padding() {
        let saved = ProductId(" dvel ".to_owned());
        assert_eq!(
            DisplayProduct::try_from_product_id(&saved),
            Some(DisplayProduct::DealiasedVelocity)
        );
    }

    #[test]
    fn the_handle_reads_its_units_from_the_registry_not_from_itself() {
        assert_eq!(
            DisplayProduct::DealiasedVelocity.domain().engine_unit,
            PhysicalUnit::MetersPerSecond
        );
        assert_eq!(DisplayProduct::Reflectivity.label(), "Reflectivity");
    }
}
