//! Integration tests that run on the standard libtest harness, in one test binary.
//!
//! Modules that need optional features are compiled only when those features are on, so the
//! property tests still run in lean builds.

#[cfg(all(feature = "axum", feature = "rhai", feature = "js"))]
mod headers;
mod props;
#[cfg(all(feature = "axum", feature = "load", feature = "rhai", feature = "js"))]
mod scenarios;
