//! The canonical catalog of radar products: what each one means, what unit it
//! is stored in, what unit it is read in, what it needs before it can be drawn,
//! and which paper it implements.
//!
//! This crate is the single source of truth for product semantics. It knows
//! nothing about rendering, colour tables, or the UI: `render2d` implements the
//! numbers, `color_tables` owns the palettes, and `workstation_app` composes
//! them. A product's meaning is declared here exactly once, so a legend, a
//! probe, and a colour lookup cannot come to different conclusions about it.
//!
//! The rule that holds it together: a value is stored, colorized, integrated,
//! and range-checked in **engine units**, and converted to **display units**
//! only at a formatting boundary. See [`units`] for why that is a type-level
//! distinction rather than a convention.

pub mod availability;
pub mod capabilities;
pub mod cut_selection;
pub mod domain;
pub mod environment;
pub mod provenance;
pub mod registry;
pub mod stats;
pub mod ticks;
pub mod units;

pub use availability::{
    AvailabilityQualifier, AvailabilityRule, ProductAvailability, UnavailableReason,
};
pub use capabilities::{
    CutCapabilities, CutIdentity, CutLeg, NominalElevationGroup, VolumeCapabilities,
};
pub use cut_selection::CutChoice;
pub use domain::{DisplayDomain, PlausibilityDisposition, PlausibleRange, TickHint, ValueRange};
pub use environment::{HailEnvironment, HailEnvironmentError, HailEnvironmentProvenance};
pub use provenance::{AlgorithmMetadata, AlgorithmStatus, LiteratureCitation};
pub use registry::{
    CutSelectionPolicy, PaletteKey, ProductComputation, ProductDescriptor, ProductGroup,
    ProductRegistry, ProductVisibility,
};
pub use stats::{CellState, FieldStats, PlausibilityReport, PlausibilityViolation, summarize};
pub use units::{AffineTransform, DisplayUnit, HeightArlM, HeightMslM, PhysicalUnit};
