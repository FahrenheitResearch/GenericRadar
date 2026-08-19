//! The one catalog of products this application can draw.
//!
//! Before this existed, a product's name lived in one `match`, its units in
//! another, its colour family in a third, and its cut choice in a fourth. Every
//! new product meant finding all four, and a product whose legend said one
//! thing while its colour table sampled another was a normal Tuesday. The
//! registry makes each of those a field on one descriptor, so a product either
//! has an answer or does not compile.
//!
//! The catalog is built once through `LazyLock`. It is read on the render
//! thread and on the update thread every frame; rebuilding it per frame would
//! allocate eleven strings for nothing.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use radar_core::{MomentType, ProductId};

use crate::availability::AvailabilityRule;
use crate::domain::{DisplayDomain, PlausibleRange, TickHint, ValueRange};
use crate::provenance::{
    AMBURN_WOLF_1997, AlgorithmMetadata, AlgorithmStatus, BERGEN_ALBERS_1988, FOOTE_ET_AL_2005,
    GREENE_CLARK_1972, LAKSHMANAN_ET_AL_2013, NEXRAD_LEVEL_II_ICD, WALDVOGEL_ET_AL_1979,
    WITT_ET_AL_1998,
};
use crate::units::{
    AffineTransform, DisplayUnit, KILOGRAMS_PER_CUBIC_METER_TO_GRAMS, METERS_PER_SECOND_TO_KNOTS,
    METERS_TO_KILOFEET, MILLIMETERS_TO_INCHES, PhysicalUnit,
};

/// Where a product sits in the picker.
///
/// Order matters: it is the order an analyst tabs through, so it is declared
/// once here rather than reconstructed by whichever widget draws last.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ProductGroup {
    Base,
    VelocityAnalysis,
    DualPolarization,
    VolumeReflectivity,
    Hail,
    Kinematics,
    Experimental,
}

impl ProductGroup {
    pub const ALL: [Self; 7] = [
        Self::Base,
        Self::VelocityAnalysis,
        Self::DualPolarization,
        Self::VolumeReflectivity,
        Self::Hail,
        Self::Kinematics,
        Self::Experimental,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Base => "Base",
            Self::VelocityAnalysis => "Velocity Analysis",
            Self::DualPolarization => "Dual Polarization",
            Self::VolumeReflectivity => "Volume Reflectivity",
            Self::Hail => "Hail",
            Self::Kinematics => "Kinematics",
            Self::Experimental => "Experimental",
        }
    }
}

/// A field computed from a whole volume rather than from one sweep.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DerivedVolumeId {
    /// Greatest reflectivity anywhere in the column.
    CompositeReflectivity,
    /// Height of the 18 dBZ top, above radar level.
    EchoTop18,
    /// Vertically integrated liquid.
    Vil,
    /// VIL divided by echo-top height.
    VilDensity,
    /// Maximum expected size of hail.
    Mesh,
    /// Probability of hail.
    ProbabilityOfHail,
    /// Probability of severe hail.
    ProbabilityOfSevereHail,
}

impl DerivedVolumeId {
    /// Whether this product needs a thermal environment before it means
    /// anything. Hail products do; reflectivity and liquid water do not.
    pub const fn needs_hail_environment(self) -> bool {
        matches!(
            self,
            Self::Mesh | Self::ProbabilityOfHail | Self::ProbabilityOfSevereHail
        )
    }
}

/// How a product's values are produced.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProductComputation {
    /// Read straight out of a cut's moment grid.
    BaseMoment(MomentType),
    /// Base velocity with the Nyquist folds undone.
    DealiasedVelocity,
    /// Velocity with the storm's own motion subtracted.
    StormRelativeVelocity { dealiased: bool },
    /// Integrated over every tilt that sampled a column.
    DerivedVolume(DerivedVolumeId),
}

impl ProductComputation {
    /// The base moment this computation ultimately reads.
    pub fn source_moment(&self) -> MomentType {
        match self {
            Self::BaseMoment(moment) => moment.clone(),
            Self::DealiasedVelocity | Self::StormRelativeVelocity { .. } => MomentType::Velocity,
            // Every volume product in this build is built from reflectivity.
            Self::DerivedVolume(_) => MomentType::Reflectivity,
        }
    }

    /// The volume product this computes, if it is one.
    pub const fn derived_volume(&self) -> Option<DerivedVolumeId> {
        match self {
            Self::DerivedVolume(id) => Some(*id),
            _ => None,
        }
    }

    pub fn uses_dealiased_velocity(&self) -> bool {
        matches!(
            self,
            Self::DealiasedVelocity | Self::StormRelativeVelocity { dealiased: true }
        )
    }

    pub fn is_storm_relative(&self) -> bool {
        matches!(self, Self::StormRelativeVelocity { .. })
    }
}

/// Which leg of a split cut a product wants.
///
/// NEXRAD splits its lowest tilts into a long-range surveillance scan and a
/// short-range Doppler scan at nearly the same elevation. Choosing by cut index
/// picks whichever the file happens to list first, which is how reflectivity
/// ends up drawn from the Doppler leg and folds to purple past about 150 km.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CutSelectionPolicy {
    /// Prefer the leg whose moment is sampled unfolded to the greatest range:
    /// the surveillance leg. Reflectivity and the dual-pol moments ride it.
    LongestUnfoldedRange,
    /// Prefer the leg that carries velocity, breaking ties on the higher
    /// Nyquist: the Doppler leg.
    VelocityLeg,
}

/// Whether an analyst can pick a product.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProductVisibility {
    Normal,
    /// A diagnostic another product is built from. Not offered on its own.
    Internal,
    /// Offered only when experimental products are switched on, and badged
    /// wherever it appears.
    Experimental,
}

/// An opaque handle to a colour table.
///
/// A plain string so `product_engine` need not depend on `color_tables`, nor
/// `color_tables` on `product_engine`. The composition layer resolves it.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PaletteKey(pub &'static str);

impl PaletteKey {
    pub const fn as_str(self) -> &'static str {
        self.0
    }
}

/// Everything this application knows about one product.
#[derive(Clone, Debug, PartialEq)]
pub struct ProductDescriptor {
    pub id: ProductId,
    /// Alternate spellings accepted when importing or searching. Never written
    /// out; the canonical id is what gets saved.
    pub aliases: &'static [&'static str],
    pub short_name: &'static str,
    pub display_name: &'static str,
    pub group: ProductGroup,
    pub computation: ProductComputation,
    pub domain: DisplayDomain,
    pub availability: AvailabilityRule,
    pub cut_policy: CutSelectionPolicy,
    pub default_palette: PaletteKey,
    pub algorithm: AlgorithmMetadata,
    pub visibility: ProductVisibility,
}

impl ProductDescriptor {
    /// Whether this product is offered given the experimental-products setting.
    pub fn is_selectable(&self, show_experimental: bool) -> bool {
        match self.visibility {
            ProductVisibility::Normal => true,
            ProductVisibility::Internal => false,
            ProductVisibility::Experimental => show_experimental,
        }
    }
}

/// The immutable catalog.
pub struct ProductRegistry {
    descriptors: Vec<ProductDescriptor>,
    /// Uppercased id and alias to index. Built once so lookup does not scan.
    lookup: BTreeMap<String, usize>,
}

impl ProductRegistry {
    /// The built-in catalog, built once per process.
    pub fn builtin() -> &'static Self {
        static REGISTRY: LazyLock<ProductRegistry> =
            LazyLock::new(|| ProductRegistry::from_descriptors(builtin_descriptors()));
        &REGISTRY
    }

    fn from_descriptors(descriptors: Vec<ProductDescriptor>) -> Self {
        let mut lookup = BTreeMap::new();
        for (index, descriptor) in descriptors.iter().enumerate() {
            let previous = lookup.insert(descriptor.id.0.to_ascii_uppercase(), index);
            assert!(
                previous.is_none(),
                "duplicate product id {}",
                descriptor.id.0
            );
            for alias in descriptor.aliases {
                let previous = lookup.insert(alias.to_ascii_uppercase(), index);
                assert!(previous.is_none(), "alias {alias} is claimed twice");
            }
        }
        Self {
            descriptors,
            lookup,
        }
    }

    /// Look up by canonical id or by any alias, ignoring case and surrounding
    /// space. A saved workspace holds the canonical id; a human types anything.
    pub fn get(&self, id_or_alias: &str) -> Option<&ProductDescriptor> {
        let key = id_or_alias.trim().to_ascii_uppercase();
        self.lookup.get(&key).map(|index| &self.descriptors[*index])
    }

    pub fn get_id(&self, id: &ProductId) -> Option<&ProductDescriptor> {
        self.get(&id.0)
    }

    /// Every descriptor, including internal ones, in canonical display order.
    pub fn all(&self) -> &[ProductDescriptor] {
        &self.descriptors
    }

    /// The products an analyst may pick, in canonical display order.
    pub fn selectable_products(
        &self,
        show_experimental: bool,
    ) -> impl Iterator<Item = &ProductDescriptor> {
        self.descriptors
            .iter()
            .filter(move |descriptor| descriptor.is_selectable(show_experimental))
    }

    /// The selectable products of one group, in canonical display order.
    pub fn group_products(
        &self,
        group: ProductGroup,
        show_experimental: bool,
    ) -> impl Iterator<Item = &ProductDescriptor> {
        self.selectable_products(show_experimental)
            .filter(move |descriptor| descriptor.group == group)
    }
}

/// The velocity plausibility bounds, shared by every velocity-dimension
/// product. Wide on purpose: they exist to catch a decode or unit fault, not to
/// judge whether a wind is realistic.
const VELOCITY_PLAUSIBLE: PlausibleRange = PlausibleRange::new(-100.0, 100.0, -160.0, 160.0);

fn velocity_domain(declared: ValueRange) -> DisplayDomain {
    DisplayDomain {
        engine_unit: PhysicalUnit::MetersPerSecond,
        display_unit: DisplayUnit::Knots,
        display_from_engine: AffineTransform::scaled(METERS_PER_SECOND_TO_KNOTS),
        declared_engine_range: declared,
        plausible: VELOCITY_PLAUSIBLE,
        tick_hint: TickHint::DEFAULT,
        decimals: 0,
    }
}

/// The domain 8-bit NEXRAD velocity encodes is -63.5 to 63.0 m/s. Declared a
/// touch wider so a legitimate extreme is not clipped by the legend.
const RAW_VELOCITY_RANGE: ValueRange = ValueRange::new(-64.0, 64.0);

/// Unfolding adds whole Nyquist intervals, so a dealiased or storm-relative
/// value legitimately leaves the encoded domain.
const UNFOLDED_VELOCITY_RANGE: ValueRange = ValueRange::new(-100.0, 100.0);

fn builtin_descriptors() -> Vec<ProductDescriptor> {
    vec![
        ProductDescriptor {
            id: ProductId("REF".to_owned()),
            aliases: &["reflectivity", "base_reflectivity"],
            short_name: "REF",
            display_name: "Reflectivity",
            group: ProductGroup::Base,
            computation: ProductComputation::BaseMoment(MomentType::Reflectivity),
            domain: DisplayDomain {
                engine_unit: PhysicalUnit::Dbz,
                display_unit: DisplayUnit::Dbz,
                display_from_engine: AffineTransform::IDENTITY,
                // The 8-bit encoding runs from -32.0 dBZ to 94.5 dBZ.
                declared_engine_range: ValueRange::new(-32.0, 94.5),
                plausible: PlausibleRange::new(-35.0, 85.0, -40.0, 100.0),
                tick_hint: TickHint::DEFAULT,
                decimals: 1,
            },
            availability: AvailabilityRule::RequiresMoment(MomentType::Reflectivity),
            cut_policy: CutSelectionPolicy::LongestUnfoldedRange,
            default_palette: PaletteKey("ref"),
            algorithm: AlgorithmMetadata::new(
                "base.ref",
                1,
                AlgorithmStatus::OperationalDefinition,
                &[NEXRAD_LEVEL_II_ICD],
            ),
            visibility: ProductVisibility::Normal,
        },
        ProductDescriptor {
            id: ProductId("VEL".to_owned()),
            aliases: &["velocity", "base_velocity"],
            short_name: "VEL",
            display_name: "Base Velocity",
            group: ProductGroup::VelocityAnalysis,
            computation: ProductComputation::BaseMoment(MomentType::Velocity),
            domain: velocity_domain(RAW_VELOCITY_RANGE),
            availability: AvailabilityRule::RequiresMoment(MomentType::Velocity),
            cut_policy: CutSelectionPolicy::VelocityLeg,
            default_palette: PaletteKey("vel"),
            algorithm: AlgorithmMetadata::new(
                "base.vel",
                1,
                AlgorithmStatus::OperationalDefinition,
                &[NEXRAD_LEVEL_II_ICD],
            ),
            visibility: ProductVisibility::Normal,
        },
        ProductDescriptor {
            id: ProductId("DVEL".to_owned()),
            aliases: &["dealiased_velocity", "unfolded_velocity"],
            short_name: "DVEL",
            display_name: "Dealiased Velocity",
            group: ProductGroup::VelocityAnalysis,
            computation: ProductComputation::DealiasedVelocity,
            domain: velocity_domain(UNFOLDED_VELOCITY_RANGE),
            availability: AvailabilityRule::RequiresDealiasedVelocity,
            cut_policy: CutSelectionPolicy::VelocityLeg,
            default_palette: PaletteKey("vel"),
            algorithm: AlgorithmMetadata::new(
                "velocity.dealias.radial_walk",
                1,
                // The radial walk is Bergen and Albers' continuity method; the
                // azimuthal consensus and spike suppression around it are this
                // application's, so the pair is an adaptation, not the paper.
                AlgorithmStatus::LiteratureAdaptation,
                &[BERGEN_ALBERS_1988],
            ),
            visibility: ProductVisibility::Normal,
        },
        ProductDescriptor {
            id: ProductId("SRV".to_owned()),
            aliases: &["storm_relative_velocity"],
            short_name: "SRV",
            display_name: "Storm-Relative Velocity",
            group: ProductGroup::VelocityAnalysis,
            computation: ProductComputation::StormRelativeVelocity { dealiased: false },
            domain: velocity_domain(RAW_VELOCITY_RANGE),
            availability: AvailabilityRule::RequiresMoment(MomentType::Velocity),
            cut_policy: CutSelectionPolicy::VelocityLeg,
            default_palette: PaletteKey("vel"),
            algorithm: AlgorithmMetadata::new(
                "velocity.storm_relative",
                1,
                AlgorithmStatus::OperationalDefinition,
                &[NEXRAD_LEVEL_II_ICD],
            ),
            visibility: ProductVisibility::Normal,
        },
        ProductDescriptor {
            id: ProductId("DSRV".to_owned()),
            aliases: &["dealiased_storm_relative_velocity"],
            short_name: "DSRV",
            display_name: "Dealiased SRV",
            group: ProductGroup::VelocityAnalysis,
            computation: ProductComputation::StormRelativeVelocity { dealiased: true },
            domain: velocity_domain(UNFOLDED_VELOCITY_RANGE),
            availability: AvailabilityRule::RequiresDealiasedVelocity,
            cut_policy: CutSelectionPolicy::VelocityLeg,
            default_palette: PaletteKey("vel"),
            algorithm: AlgorithmMetadata::new(
                "velocity.dealias.radial_walk.storm_relative",
                1,
                AlgorithmStatus::LiteratureAdaptation,
                &[BERGEN_ALBERS_1988],
            ),
            visibility: ProductVisibility::Normal,
        },
        ProductDescriptor {
            id: ProductId("SW".to_owned()),
            aliases: &["spectrum_width"],
            short_name: "SW",
            display_name: "Spectrum Width",
            group: ProductGroup::VelocityAnalysis,
            computation: ProductComputation::BaseMoment(MomentType::SpectrumWidth),
            domain: DisplayDomain {
                engine_unit: PhysicalUnit::MetersPerSecond,
                // Held in m/s rather than converted to knots with the other
                // velocity products: every operational spectrum-width threshold
                // in the literature is quoted in m/s, and relabelling the axis
                // would make an analyst misread them.
                display_unit: DisplayUnit::MetersPerSecond,
                display_from_engine: AffineTransform::IDENTITY,
                declared_engine_range: ValueRange::new(0.0, 30.0),
                plausible: PlausibleRange::new(0.0, 50.0, 0.0, 80.0),
                tick_hint: TickHint::DEFAULT,
                decimals: 1,
            },
            availability: AvailabilityRule::RequiresMoment(MomentType::SpectrumWidth),
            // Spectrum width is collected on the Doppler leg, beside velocity.
            cut_policy: CutSelectionPolicy::VelocityLeg,
            default_palette: PaletteKey("sw"),
            algorithm: AlgorithmMetadata::new(
                "base.sw",
                1,
                AlgorithmStatus::OperationalDefinition,
                &[NEXRAD_LEVEL_II_ICD],
            ),
            visibility: ProductVisibility::Normal,
        },
        ProductDescriptor {
            id: ProductId("ZDR".to_owned()),
            aliases: &["differential_reflectivity"],
            short_name: "ZDR",
            display_name: "Differential Reflectivity",
            group: ProductGroup::DualPolarization,
            computation: ProductComputation::BaseMoment(MomentType::DifferentialReflectivity),
            domain: DisplayDomain {
                engine_unit: PhysicalUnit::Decibels,
                display_unit: DisplayUnit::Decibels,
                display_from_engine: AffineTransform::IDENTITY,
                // The full decoded domain, not the +/-8 dB most palettes ink.
                // Clipping the declared range instead would hide the large
                // positive tail that marks hail shafts and biological targets.
                declared_engine_range: ValueRange::new(-13.0, 20.0),
                plausible: PlausibleRange::new(-8.0, 8.0, -13.0, 20.0),
                tick_hint: TickHint::DEFAULT,
                decimals: 2,
            },
            availability: AvailabilityRule::RequiresMoment(MomentType::DifferentialReflectivity),
            // The dual-pol moments are collected on the surveillance leg with
            // reflectivity, so they want the same long-range cut it does.
            cut_policy: CutSelectionPolicy::LongestUnfoldedRange,
            default_palette: PaletteKey("zdr"),
            algorithm: AlgorithmMetadata::new(
                "base.zdr",
                1,
                AlgorithmStatus::OperationalDefinition,
                &[NEXRAD_LEVEL_II_ICD],
            ),
            visibility: ProductVisibility::Normal,
        },
        ProductDescriptor {
            id: ProductId("RHO".to_owned()),
            aliases: &["correlation_coefficient", "rhohv", "cc"],
            short_name: "RHO",
            display_name: "Correlation Coefficient",
            group: ProductGroup::DualPolarization,
            computation: ProductComputation::BaseMoment(MomentType::CorrelationCoefficient),
            domain: DisplayDomain {
                engine_unit: PhysicalUnit::Dimensionless,
                display_unit: DisplayUnit::Dimensionless,
                display_from_engine: AffineTransform::IDENTITY,
                // The DECODED endpoints of the field, not round numbers.
                // Level II carries rho-HV as `(raw + 60.5) / 300` over raw
                // 2..255, so the extreme codes are 62.5/300 and 315.5/300.
                // Written as those quotients so they are bit-identical to what
                // the decoder produces: a range that stopped at a tidy 1.05
                // left the saturation code ABOVE the last stop, and on five
                // real volumes 4 to 10 percent of all gates clamped there and
                // were drawn in the brightest colour on the scope. Those gates
                // are not measurements - their median reflectivity is -2.5 to
                // +7.5 dBZ - so that was noise outshining weather.
                declared_engine_range: ValueRange::new(62.5 / 300.0, 315.5 / 300.0),
                // Above 1.0 is not physical, but the encoding leaves headroom
                // there and real files use it, so it warns rather than rejects.
                plausible: PlausibleRange::new(0.0, 1.0, 0.0, 315.5 / 300.0),
                tick_hint: TickHint::intervals(5),
                decimals: 3,
            },
            availability: AvailabilityRule::RequiresMoment(MomentType::CorrelationCoefficient),
            cut_policy: CutSelectionPolicy::LongestUnfoldedRange,
            default_palette: PaletteKey("rho"),
            algorithm: AlgorithmMetadata::new(
                "base.rho",
                1,
                AlgorithmStatus::OperationalDefinition,
                &[NEXRAD_LEVEL_II_ICD],
            ),
            visibility: ProductVisibility::Normal,
        },
        ProductDescriptor {
            id: ProductId("PHI".to_owned()),
            aliases: &["differential_phase", "phidp"],
            short_name: "PHI",
            display_name: "Differential Phase",
            group: ProductGroup::DualPolarization,
            computation: ProductComputation::BaseMoment(MomentType::DifferentialPhase),
            domain: DisplayDomain {
                engine_unit: PhysicalUnit::Degrees,
                display_unit: DisplayUnit::Degrees,
                display_from_engine: AffineTransform::IDENTITY,
                declared_engine_range: ValueRange::new(0.0, 360.0),
                plausible: PlausibleRange::new(-180.0, 360.0, -360.0, 720.0),
                tick_hint: TickHint::DEFAULT,
                decimals: 1,
            },
            availability: AvailabilityRule::RequiresMoment(MomentType::DifferentialPhase),
            cut_policy: CutSelectionPolicy::LongestUnfoldedRange,
            default_palette: PaletteKey("phi"),
            algorithm: AlgorithmMetadata::new(
                "base.phi",
                1,
                AlgorithmStatus::OperationalDefinition,
                &[NEXRAD_LEVEL_II_ICD],
            ),
            visibility: ProductVisibility::Normal,
        },
        ProductDescriptor {
            id: ProductId("KDP".to_owned()),
            aliases: &["specific_differential_phase", "kdp_deg_km"],
            short_name: "KDP",
            display_name: "Specific Differential Phase",
            group: ProductGroup::DualPolarization,
            computation: ProductComputation::BaseMoment(MomentType::SpecificDifferentialPhase),
            domain: DisplayDomain {
                engine_unit: PhysicalUnit::DegreesPerKilometer,
                display_unit: DisplayUnit::DegreesPerKilometer,
                display_from_engine: AffineTransform::IDENTITY,
                declared_engine_range: ValueRange::new(-2.0, 7.0),
                plausible: PlausibleRange::new(-10.0, 20.0, -20.0, 30.0),
                tick_hint: TickHint::DEFAULT,
                decimals: 2,
            },
            availability: AvailabilityRule::RequiresMoment(MomentType::SpecificDifferentialPhase),
            cut_policy: CutSelectionPolicy::LongestUnfoldedRange,
            default_palette: PaletteKey("kdp"),
            algorithm: AlgorithmMetadata::new(
                "base.kdp",
                1,
                AlgorithmStatus::OperationalDefinition,
                &[NEXRAD_LEVEL_II_ICD],
            ),
            visibility: ProductVisibility::Normal,
        },
        ProductDescriptor {
            id: ProductId("CREF".to_owned()),
            aliases: &[],
            short_name: "CREF",
            display_name: "Composite Reflectivity",
            group: ProductGroup::VolumeReflectivity,
            computation: ProductComputation::DerivedVolume(DerivedVolumeId::CompositeReflectivity),
            domain: DisplayDomain {
                engine_unit: PhysicalUnit::Dbz,
                display_unit: DisplayUnit::Dbz,
                display_from_engine: AffineTransform::IDENTITY,
                declared_engine_range: ValueRange::new(-32.0, 94.5),
                plausible: PlausibleRange::new(-35.0, 85.0, -40.0, 100.0),
                tick_hint: TickHint::DEFAULT,
                decimals: 1,
            },
            availability: AvailabilityRule::RequiresMoment(MomentType::Reflectivity),
            cut_policy: CutSelectionPolicy::LongestUnfoldedRange,
            default_palette: PaletteKey("ref"),
            algorithm: AlgorithmMetadata::new(
                "derived.cref",
                1,
                AlgorithmStatus::OperationalDefinition,
                &[NEXRAD_LEVEL_II_ICD],
            ),
            visibility: ProductVisibility::Normal,
        },
        ProductDescriptor {
            id: ProductId("ET18".to_owned()),
            aliases: &[],
            short_name: "ET18",
            display_name: "Echo Tops (18 dBZ)",
            group: ProductGroup::VolumeReflectivity,
            computation: ProductComputation::DerivedVolume(DerivedVolumeId::EchoTop18),
            domain: DisplayDomain {
                engine_unit: PhysicalUnit::Meters,
                display_unit: DisplayUnit::Kilofeet,
                display_from_engine: AffineTransform::scaled(METERS_TO_KILOFEET),
                declared_engine_range: ValueRange::new(0.0, 21000.0),
                plausible: PlausibleRange::new(0.0, 22000.0, 0.0, 30000.0),
                tick_hint: TickHint::DEFAULT,
                decimals: 1,
            },
            availability: AvailabilityRule::RequiresMoment(MomentType::Reflectivity),
            cut_policy: CutSelectionPolicy::LongestUnfoldedRange,
            default_palette: PaletteKey("et"),
            algorithm: AlgorithmMetadata::new(
                "derived.echo_top.18dbz",
                1,
                AlgorithmStatus::LiteratureAdaptation,
                &[LAKSHMANAN_ET_AL_2013],
            ),
            visibility: ProductVisibility::Normal,
        },
        ProductDescriptor {
            id: ProductId("VIL".to_owned()),
            aliases: &[],
            short_name: "VIL",
            display_name: "Vertically Integrated Liquid",
            group: ProductGroup::VolumeReflectivity,
            computation: ProductComputation::DerivedVolume(DerivedVolumeId::Vil),
            domain: DisplayDomain {
                engine_unit: PhysicalUnit::KilogramsPerSquareMeter,
                display_unit: DisplayUnit::KilogramsPerSquareMeter,
                display_from_engine: AffineTransform::IDENTITY,
                declared_engine_range: ValueRange::new(0.0, 80.0),
                plausible: PlausibleRange::new(0.0, 150.0, 0.0, 250.0),
                tick_hint: TickHint::DEFAULT,
                decimals: 1,
            },
            availability: AvailabilityRule::RequiresMoment(MomentType::Reflectivity),
            cut_policy: CutSelectionPolicy::LongestUnfoldedRange,
            default_palette: PaletteKey("vil"),
            algorithm: AlgorithmMetadata::new(
                "derived.vil",
                1,
                AlgorithmStatus::LiteratureAdaptation,
                &[GREENE_CLARK_1972, AMBURN_WOLF_1997],
            ),
            visibility: ProductVisibility::Normal,
        },
        ProductDescriptor {
            id: ProductId("VILD".to_owned()),
            aliases: &[],
            short_name: "VILD",
            display_name: "VIL Density",
            group: ProductGroup::VolumeReflectivity,
            computation: ProductComputation::DerivedVolume(DerivedVolumeId::VilDensity),
            domain: DisplayDomain {
                engine_unit: PhysicalUnit::KilogramsPerCubicMeter,
                display_unit: DisplayUnit::GramsPerCubicMeter,
                display_from_engine: AffineTransform::scaled(KILOGRAMS_PER_CUBIC_METER_TO_GRAMS),
                declared_engine_range: ValueRange::new(0.0, 0.008),
                plausible: PlausibleRange::new(0.0, 0.012, 0.0, 0.025),
                tick_hint: TickHint::DEFAULT,
                decimals: 1,
            },
            availability: AvailabilityRule::RequiresMoment(MomentType::Reflectivity),
            cut_policy: CutSelectionPolicy::LongestUnfoldedRange,
            default_palette: PaletteKey("vild"),
            algorithm: AlgorithmMetadata::new(
                "derived.vil_density",
                1,
                AlgorithmStatus::LiteratureAdaptation,
                &[AMBURN_WOLF_1997],
            ),
            visibility: ProductVisibility::Normal,
        },
        ProductDescriptor {
            id: ProductId("MESH".to_owned()),
            aliases: &[],
            short_name: "MESH",
            display_name: "Max Estimated Hail Size",
            group: ProductGroup::Hail,
            computation: ProductComputation::DerivedVolume(DerivedVolumeId::Mesh),
            domain: DisplayDomain {
                engine_unit: PhysicalUnit::Millimeters,
                display_unit: DisplayUnit::Inches,
                display_from_engine: AffineTransform::scaled(MILLIMETERS_TO_INCHES),
                declared_engine_range: ValueRange::new(0.0, 100.0),
                plausible: PlausibleRange::new(0.0, 250.0, 0.0, 400.0),
                tick_hint: TickHint::DEFAULT,
                decimals: 2,
            },
            availability: AvailabilityRule::RequiresMoment(MomentType::Reflectivity),
            cut_policy: CutSelectionPolicy::LongestUnfoldedRange,
            default_palette: PaletteKey("mesh"),
            algorithm: AlgorithmMetadata::new(
                "derived.mesh.witt1998",
                1,
                AlgorithmStatus::LiteratureAdaptation,
                &[WITT_ET_AL_1998],
            ),
            visibility: ProductVisibility::Normal,
        },
        ProductDescriptor {
            id: ProductId("POH".to_owned()),
            aliases: &[],
            short_name: "POH",
            display_name: "Probability of Hail",
            group: ProductGroup::Hail,
            computation: ProductComputation::DerivedVolume(DerivedVolumeId::ProbabilityOfHail),
            domain: DisplayDomain {
                engine_unit: PhysicalUnit::Percent,
                display_unit: DisplayUnit::Percent,
                display_from_engine: AffineTransform::IDENTITY,
                declared_engine_range: ValueRange::new(0.0, 100.0),
                plausible: PlausibleRange::new(0.0, 100.0, 0.0, 100.0),
                tick_hint: TickHint::DEFAULT,
                decimals: 0,
            },
            availability: AvailabilityRule::RequiresMoment(MomentType::Reflectivity),
            cut_policy: CutSelectionPolicy::LongestUnfoldedRange,
            default_palette: PaletteKey("poh"),
            algorithm: AlgorithmMetadata::new(
                "derived.poh.foote2005",
                1,
                AlgorithmStatus::LiteratureAdaptation,
                &[WALDVOGEL_ET_AL_1979, FOOTE_ET_AL_2005],
            ),
            visibility: ProductVisibility::Normal,
        },
        ProductDescriptor {
            id: ProductId("POSH".to_owned()),
            aliases: &[],
            short_name: "POSH",
            display_name: "Probability of Severe Hail",
            group: ProductGroup::Hail,
            computation: ProductComputation::DerivedVolume(
                DerivedVolumeId::ProbabilityOfSevereHail,
            ),
            domain: DisplayDomain {
                engine_unit: PhysicalUnit::Percent,
                display_unit: DisplayUnit::Percent,
                display_from_engine: AffineTransform::IDENTITY,
                declared_engine_range: ValueRange::new(0.0, 100.0),
                plausible: PlausibleRange::new(0.0, 100.0, 0.0, 100.0),
                tick_hint: TickHint::DEFAULT,
                decimals: 0,
            },
            availability: AvailabilityRule::RequiresMoment(MomentType::Reflectivity),
            cut_policy: CutSelectionPolicy::LongestUnfoldedRange,
            default_palette: PaletteKey("posh"),
            algorithm: AlgorithmMetadata::new(
                "derived.posh.witt1998",
                1,
                AlgorithmStatus::LiteratureAdaptation,
                &[WITT_ET_AL_1998],
            ),
            visibility: ProductVisibility::Normal,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::PlausibilityDisposition;

    /// The ids a saved workspace already holds. Changing one of these silently
    /// retargets every workspace an analyst has saved, so the list is pinned.
    const CANONICAL_IDS: [&str; 17] = [
        "REF", "VEL", "DVEL", "SRV", "DSRV", "SW", "ZDR", "RHO", "PHI", "KDP", "CREF", "ET18",
        "VIL", "VILD", "MESH", "POH", "POSH",
    ];

    /// The ten a saved workspace may already hold, in their original order.
    /// These must stay at the front: appending is safe, reordering is not.
    const ORIGINAL_IDS: [&str; 10] = [
        "REF", "VEL", "DVEL", "SRV", "DSRV", "SW", "ZDR", "RHO", "PHI", "KDP",
    ];

    #[test]
    fn the_existing_uppercase_product_ids_remain_canonical() {
        let registry = ProductRegistry::builtin();
        let ids: Vec<&str> = registry
            .all()
            .iter()
            .map(|descriptor| descriptor.id.0.as_str())
            .collect();
        assert_eq!(ids, CANONICAL_IDS);
        assert_eq!(
            &ids[..ORIGINAL_IDS.len()],
            &ORIGINAL_IDS,
            "the products a saved workspace already names must keep their order"
        );
    }

    #[test]
    fn every_canonical_id_resolves_to_itself() {
        let registry = ProductRegistry::builtin();
        for id in CANONICAL_IDS {
            let descriptor = registry.get(id).expect("canonical id must resolve");
            assert_eq!(descriptor.id.0, id);
        }
    }

    #[test]
    fn lookup_ignores_case_and_surrounding_space() {
        let registry = ProductRegistry::builtin();
        assert_eq!(registry.get("  dvel ").map(|d| d.short_name), Some("DVEL"));
        assert_eq!(
            registry
                .get("Correlation_Coefficient")
                .map(|d| d.short_name),
            Some("RHO")
        );
    }

    #[test]
    fn an_unknown_product_resolves_to_nothing() {
        // Deliberately a plausible radar product this build does not have, so
        // the test keeps meaning something as the catalog grows.
        assert!(ProductRegistry::builtin().get("AZSHR").is_none());
        assert!(ProductRegistry::builtin().get("not_a_product").is_none());
    }

    #[test]
    fn every_descriptor_reads_the_moment_its_availability_rule_demands() {
        // The rule greys the picker out and the computation does the work. If
        // they ever disagree, a product is offered that cannot be drawn.
        for descriptor in ProductRegistry::builtin().all() {
            assert_eq!(
                descriptor.availability.required_moment(),
                descriptor.computation.source_moment(),
                "{} promises one moment and reads another",
                descriptor.id.0
            );
        }
    }

    #[test]
    fn every_domain_can_be_converted_back_to_engine_units() {
        for descriptor in ProductRegistry::builtin().all() {
            assert!(
                descriptor.domain.display_from_engine.is_invertible(),
                "{} has a transform a legend cannot invert",
                descriptor.id.0
            );
        }
    }

    #[test]
    fn every_plausible_range_puts_its_soft_bounds_inside_its_hard_bounds() {
        for descriptor in ProductRegistry::builtin().all() {
            assert!(
                descriptor.domain.plausible.is_well_ordered(),
                "{} has a plausible range that cannot classify anything",
                descriptor.id.0
            );
        }
    }

    #[test]
    fn every_declared_range_is_a_non_empty_forward_interval() {
        for descriptor in ProductRegistry::builtin().all() {
            let range = descriptor.domain.declared_engine_range;
            assert!(
                range.span() > 0.0,
                "{} declares an empty domain, so its legend would be a point",
                descriptor.id.0
            );
        }
    }

    #[test]
    fn the_four_velocity_products_share_one_source_moment() {
        let registry = ProductRegistry::builtin();
        for id in ["VEL", "DVEL", "SRV", "DSRV"] {
            let descriptor = registry.get(id).expect("velocity product must exist");
            assert_eq!(
                descriptor.computation.source_moment(),
                MomentType::Velocity,
                "{id} must read base velocity"
            );
        }
    }

    #[test]
    fn only_the_dealiased_products_unfold_velocity() {
        let registry = ProductRegistry::builtin();
        let unfolds = |id: &str| {
            registry
                .get(id)
                .expect("product must exist")
                .computation
                .uses_dealiased_velocity()
        };
        assert!(unfolds("DVEL"));
        assert!(unfolds("DSRV"));
        assert!(!unfolds("VEL"));
        assert!(!unfolds("SRV"));
        assert!(!unfolds("REF"));
    }

    #[test]
    fn only_the_storm_relative_products_subtract_storm_motion() {
        let registry = ProductRegistry::builtin();
        let storm_relative = |id: &str| {
            registry
                .get(id)
                .expect("product must exist")
                .computation
                .is_storm_relative()
        };
        assert!(storm_relative("SRV"));
        assert!(storm_relative("DSRV"));
        assert!(!storm_relative("VEL"));
        assert!(!storm_relative("DVEL"));
    }

    #[test]
    fn reflectivity_and_dual_pol_want_the_surveillance_leg_of_a_split_cut() {
        let registry = ProductRegistry::builtin();
        for id in ["REF", "ZDR", "RHO", "PHI", "KDP"] {
            assert_eq!(
                registry.get(id).expect("product must exist").cut_policy,
                CutSelectionPolicy::LongestUnfoldedRange,
                "{id} is collected on the surveillance leg"
            );
        }
    }

    #[test]
    fn velocity_and_spectrum_width_want_the_doppler_leg_of_a_split_cut() {
        let registry = ProductRegistry::builtin();
        for id in ["VEL", "DVEL", "SRV", "DSRV", "SW"] {
            assert_eq!(
                registry.get(id).expect("product must exist").cut_policy,
                CutSelectionPolicy::VelocityLeg,
                "{id} is collected on the Doppler leg"
            );
        }
    }

    #[test]
    fn reflectivity_stays_in_dbz_from_storage_to_readout() {
        let registry = ProductRegistry::builtin();
        let domain = registry.get("REF").expect("REF must exist").domain;
        assert_eq!(domain.engine_unit, PhysicalUnit::Dbz);
        assert_eq!(domain.display_unit, DisplayUnit::Dbz);
        assert_eq!(domain.format_display(52.5), "52.5 dBZ");
    }

    #[test]
    fn velocity_is_stored_in_metres_per_second_and_read_in_knots() {
        let registry = ProductRegistry::builtin();
        let domain = registry.get("DVEL").expect("DVEL must exist").domain;
        assert_eq!(domain.engine_unit, PhysicalUnit::MetersPerSecond);
        assert_eq!(domain.display_unit, DisplayUnit::Knots);
        // 30 m/s is 58.3 kt; rounded to whole knots for a readout.
        assert_eq!(domain.format_display(30.0), "58 kt");
    }

    #[test]
    fn spectrum_width_is_read_in_metres_per_second_not_knots() {
        let domain = ProductRegistry::builtin()
            .get("SW")
            .expect("SW must exist")
            .domain;
        assert_eq!(domain.display_unit, DisplayUnit::MetersPerSecond);
        assert_eq!(domain.format_display(4.0), "4.0 m/s");
    }

    #[test]
    fn correlation_coefficient_reads_three_decimals_with_no_unit() {
        let domain = ProductRegistry::builtin()
            .get("RHO")
            .expect("RHO must exist")
            .domain;
        // 0.98 rather than a value that rounds at the fourth decimal: the
        // nearest f32 to 0.9725 is 0.97249996, which formats to 0.972 and would
        // make this test a coin flip about float representation, not decimals.
        assert_eq!(domain.format_display(0.98), "0.980");
    }

    #[test]
    fn differential_reflectivity_keeps_its_full_decoded_domain() {
        // A ZDR field clipped to the +/-8 dB most palettes ink would erase the
        // large positive values that mark a hail shaft.
        let domain = ProductRegistry::builtin()
            .get("ZDR")
            .expect("ZDR must exist")
            .domain;
        assert_eq!(domain.declared_engine_range, ValueRange::new(-13.0, 20.0));
        assert_eq!(domain.classify(12.0), PlausibilityDisposition::Warn);
        assert_eq!(domain.classify(6.0), PlausibilityDisposition::Pass);
        assert_eq!(domain.classify(25.0), PlausibilityDisposition::Reject);
    }

    #[test]
    fn every_product_is_selectable_because_none_are_experimental_yet() {
        let registry = ProductRegistry::builtin();
        let count = CANONICAL_IDS.len();
        assert_eq!(registry.selectable_products(false).count(), count);
        assert_eq!(registry.selectable_products(true).count(), count);
    }

    #[test]
    fn the_groups_partition_the_catalog_in_declaration_order() {
        let registry = ProductRegistry::builtin();
        let grouped: Vec<&str> = ProductGroup::ALL
            .iter()
            .flat_map(|group| registry.group_products(*group, true))
            .map(|descriptor| descriptor.short_name)
            .collect();
        assert_eq!(
            grouped, CANONICAL_IDS,
            "grouping must not reorder the products an analyst tabs through"
        );
    }

    #[test]
    fn every_algorithm_that_may_run_carries_at_least_one_citation() {
        for descriptor in ProductRegistry::builtin().all() {
            assert!(
                !descriptor.algorithm.citations.is_empty(),
                "{} produces numbers with no stated source",
                descriptor.id.0
            );
        }
    }

    #[test]
    fn the_dealiased_products_are_marked_as_an_adaptation_not_the_paper() {
        let registry = ProductRegistry::builtin();
        for id in ["DVEL", "DSRV"] {
            assert_eq!(
                registry
                    .get(id)
                    .expect("product must exist")
                    .algorithm
                    .status,
                AlgorithmStatus::LiteratureAdaptation
            );
        }
    }
}
