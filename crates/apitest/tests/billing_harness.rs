//! The billing suites (Rust, Rhai, JS) as regular `cargo test` tests.
//!
//! cargo test -p apitest --features full --test `billing_harness`
//! cargo test -p apitest --features full --test `billing_harness` -- "create and fetch"
//! `APITEST_JUNIT=target/apitest/junit.xml` cargo test -p apitest --features full --test `billing_harness`

/// The billing service under test.
#[path = "support/demo_server.rs"]
mod demo;
/// The billing suite written in Rust, plus the paths to its script versions.
#[path = "support/rust_suite.rs"]
mod rust_suite;

use apitest::harness::{Plan, main as harness_main};
use apitest::script::{LoadOptions, load_suites};
use apitest::{RunConfig, TestServer};

/// Runs the Rust, Rhai and JS billing suites against one in-process 1.0 service.
fn main() {
    harness_main(|| async {
        // The service under test runs in-process, but is reached over real TCP.
        let server = TestServer::axum(demo::app(demo::Variant::V1_0)).await?;
        let cfg = RunConfig::new(server.base_url())
            .var("API_USER", "demo")
            .var("API_PASS", "secret")
            .quiet(true);
        let scripts = rust_suite::suites_dir();
        Ok(Plan::new(cfg)
            .suite(rust_suite::billing_suite()?)
            .suites(load_suites(
                &scripts.join("billing.rhai"),
                &LoadOptions::default(),
            )?)
            .suites(load_suites(
                &scripts.join("billing.js"),
                &LoadOptions {
                    js_workers: 2,
                    ..Default::default()
                },
            )?)
            .keep(server))
    })
}
