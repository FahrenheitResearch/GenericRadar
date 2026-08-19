//! Compiles `src/xsection.rs` (and its submodules) as a test crate and runs
//! its unit tests TODAY, before the one-line `mod xsection;` wiring lands in
//! `main.rs`. The module deliberately reaches no `crate::` sibling — its
//! whole surface arrives through `XSectionInput` — which is what makes this
//! include valid. Once `main.rs` declares the module, these same tests run as
//! ordinary unit tests of the binary and this harness can be deleted.
//!
//! `dead_code` is allowed because the harness exercises the module's tests,
//! not its full public surface — the application is the caller of the rest.
#[allow(dead_code)]
#[path = "../src/xsection.rs"]
mod xsection;
