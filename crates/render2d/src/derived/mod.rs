//! Volume-derived and sweep-derived radar fields.
//!
//! Everything here is camera-independent. A field is a function of a radar
//! volume, a product, and a configuration - never of where the analyst happens
//! to be looking. That is what lets a pan reuse a field, four panes share one
//! allocation, and a palette change repaint without recomputing anything.
//!
//! The numerical cores are deliberately pure functions over small slices, so
//! each can be pinned against a hand-computed column or neighbourhood without
//! a radar volume, and so a wrong constant shows up as a failing arithmetic
//! test rather than as a plausible-looking picture.

pub mod compute;
pub mod field;
pub mod grid;
pub mod hail;
pub mod profile;
pub mod reflectivity;
pub mod sampling;
pub mod velocity_gradients;
pub mod vil;
pub mod wind;
