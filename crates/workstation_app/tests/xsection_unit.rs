//! Compiles `src/xsection.rs` (and its submodules) as a focused test crate and
//! runs its unit tests independently of the rest of the application.
//!
//! The module reaches exactly one `crate::` sibling — `units`, which is a leaf
//! with no siblings of its own — and everything else about its surface arrives
//! through `XSectionInput`. That is what keeps this include a two-module
//! harness rather than a second copy of the application.
//!
//! `dead_code` is allowed because the harness exercises the module's tests,
//! not its full public surface — the application is the caller of the rest.
#[allow(dead_code)]
#[path = "../src/units.rs"]
mod units;

#[allow(dead_code)]
#[path = "../src/xsection.rs"]
mod xsection;
