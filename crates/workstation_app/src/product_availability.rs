//! What the current volume can and cannot draw.
//!
//! Split out of `product_picker` because it answers a different question. The
//! picker is a widget; this is a measurement - it turns a
//! `product_engine::VolumeCapabilities` into a per-product verdict, and it is
//! the only place in the workspace that does. It carries no egui.
//!
//! A product the volume cannot show is greyed WITH ITS REASON rather than
//! hidden. Hiding it makes the application look broken when the real answer is
//! that this radar sent no dual-pol on this sweep.

use product_engine::{
    AvailabilityRule, CutCapabilities, ProductAvailability, ProductDescriptor, UnavailableReason,
    VolumeCapabilities,
};

use crate::product::DisplayProduct;

/// One row of the list.
pub struct ProductEntry<'a> {
    pub product: DisplayProduct,
    pub descriptor: &'static ProductDescriptor,
    pub availability: &'a ProductAvailability,
}

impl ProductEntry<'_> {
    pub fn is_available(&self) -> bool {
        self.availability.is_available()
    }

    /// Why this product cannot be drawn, phrased for a row.
    pub fn unavailable_label(&self) -> Option<String> {
        self.availability
            .unavailable_reason()
            .map(UnavailableReason::label)
    }
}

/// What the volume on screen can draw, one answer per product.
///
/// Built once when the volume changes and read every frame the picker is open.
/// A caller that knows something this cannot see - a hail environment that has
/// not been entered - overrides one entry with
/// [`ProductAvailabilityIndex::set`] rather than teaching this file about it.
#[derive(Clone, Debug)]
pub struct ProductAvailabilityIndex {
    /// Parallel to `DisplayProduct::ALL`.
    entries: Vec<ProductAvailability>,
}

impl ProductAvailabilityIndex {
    /// Nothing is known to be missing. This is the honest answer before a
    /// volume has loaded: greying the whole catalog out at startup would say
    /// the products are impossible when they are merely unmeasured.
    pub fn unrestricted() -> Self {
        Self {
            entries: vec![ProductAvailability::available(); DisplayProduct::ALL.len()],
        }
    }

    pub fn from_capabilities(capabilities: &VolumeCapabilities) -> Self {
        Self {
            entries: DisplayProduct::ALL
                .iter()
                .map(|product| availability_in(product.descriptor(), capabilities))
                .collect(),
        }
    }

    pub fn from_optional_capabilities(capabilities: Option<&VolumeCapabilities>) -> Self {
        capabilities.map_or_else(Self::unrestricted, Self::from_capabilities)
    }

    /// Override one product's availability.
    ///
    /// The documented escape hatch for a fact the volume cannot state - a hail
    /// environment nobody has entered, an algorithm awaiting verification - so
    /// that this module does not have to learn about them. Nothing overrides
    /// anything today, which is why the compiler calls it unused.
    #[allow(dead_code)]
    pub fn set(&mut self, product: DisplayProduct, availability: ProductAvailability) {
        if let Some(slot) = product_index(product) {
            self.entries[slot] = availability;
        }
    }

    pub fn get(&self, product: DisplayProduct) -> &ProductAvailability {
        static AVAILABLE: ProductAvailability = ProductAvailability::Available {
            qualifiers: Vec::new(),
        };
        product_index(product)
            .and_then(|slot| self.entries.get(slot))
            .unwrap_or(&AVAILABLE)
    }
}

impl Default for ProductAvailabilityIndex {
    fn default() -> Self {
        Self::unrestricted()
    }
}

fn product_index(product: DisplayProduct) -> Option<usize> {
    DisplayProduct::ALL
        .iter()
        .position(|candidate| *candidate == product)
}

/// What a volume's measured capabilities say about one product.
///
/// The first question comes from the descriptor's own `AvailabilityRule`, so a
/// product that gains a requirement in the registry greys out here without
/// this file changing. Two conditions the rule cannot state are added on top:
///
/// * Unfolding needs a Nyquist velocity to unfold against. A volume can carry
///   velocity on a sweep that reports none, and presenting a "dealiased" field
///   that was never unfolded is worse than refusing it.
/// * A vertical integration needs more than one commanded tilt. On a single
///   sweep, VIL and echo tops are a picture of one beam pretending to be a
///   column - which is exactly what a volume still being built looks like.
pub(crate) fn availability_in(
    descriptor: &ProductDescriptor,
    capabilities: &VolumeCapabilities,
) -> ProductAvailability {
    // Asked before the volume is asked anything: `AlgorithmStatus` declares
    // that a product whose primary source has not been read may not produce a
    // number at all, and no amount of data changes that. Nothing in this build
    // carries the status today; the registry is where it would arrive, and it
    // must not arrive as a selectable product.
    if !descriptor.algorithm.status.may_produce_values() {
        return ProductAvailability::Unavailable(
            UnavailableReason::AlgorithmPendingPrimaryVerification,
        );
    }
    let moment = descriptor.availability.required_moment();
    if !capabilities.has_moment(&moment) {
        return ProductAvailability::Unavailable(UnavailableReason::MissingMoment(moment));
    }
    if descriptor.availability == AvailabilityRule::RequiresDealiasedVelocity
        && !capabilities.cuts.iter().any(unfoldable)
    {
        return ProductAvailability::Unavailable(UnavailableReason::NoDealiasedVelocity);
    }
    if descriptor.computation.derived_volume().is_some() && capabilities.groups.len() < 2 {
        return ProductAvailability::Unavailable(UnavailableReason::InsufficientUniqueElevations);
    }
    ProductAvailability::available()
}

/// A sweep carrying velocity and a Nyquist to unfold it against.
fn unfoldable(cut: &CutCapabilities) -> bool {
    cut.has_velocity()
        && cut
            .representative_nyquist_mps
            .is_some_and(|nyquist| nyquist.is_finite() && nyquist > 0.0)
}
