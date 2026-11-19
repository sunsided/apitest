//! Integration tests that run on the standard libtest harness, in one test binary.
//!
//! Modules that need optional features are compiled only when those features are on, so the
//! property tests still run in lean builds.

#[cfg(all(feature = "axum", feature = "cucumber"))]
mod cucumber;
/// The billing service under test, shared by the modules that run suites against it.
#[cfg(any(
    all(feature = "axum", feature = "cucumber"),
    all(feature = "axum", feature = "load", feature = "rhai", feature = "js")
))]
#[path = "../support/demo_server.rs"]
mod demo;
#[cfg(all(feature = "axum", feature = "diagnostics"))]
mod diagnostics;
#[cfg(all(feature = "axum", feature = "rhai", feature = "js"))]
mod headers;
mod props;
#[cfg(all(feature = "axum", feature = "load", feature = "rhai", feature = "js"))]
mod scenarios;
