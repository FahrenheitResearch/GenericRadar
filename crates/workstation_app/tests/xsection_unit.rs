//! Compiles `src/xsection.rs` (and its submodules) as a test crate and runs
//! its unit tests TODAY, before the one-line `mod xsection;` wiring lands in
//! `main.rs`. Once `main.rs` declares the module, these same tests run as
//! ordinary unit tests of the binary and this harness can be deleted.
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
