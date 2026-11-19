//! apitest as a library: spin up the app in-process (real TCP), run suites
//! written in Rust, Rhai and JS against it, write a `JUnit` report.
//!
//! cargo run -p apitest --features full --example billing            # against the pinned version
//! cargo run -p apitest --features full --example billing -- 1.1     # compatible change
//! cargo run -p apitest --features full --example billing -- 2.0     # breaking change → fails before tests run

/// The billing service under test.
#[path = "../tests/support/demo_server.rs"]
mod demo;
/// The billing suite written in Rust, plus the paths to its script versions.
#[path = "../tests/support/rust_suite.rs"]
mod rust_suite;

use apitest::prelude::*;
use apitest::report::junit_xml;
use apitest::script::{LoadOptions, load_suites};

/// Runs all billing suites against the variant named by the first argument (default
/// `1.0`), prints each report and writes `target/apitest/junit.xml`; exits 1 on failure.
#[tokio::main]
async fn main() -> Result<()> {
    let variant: demo::Variant = std::env::args()
        .nth(1)
        .as_deref()
        .unwrap_or("1.0")
        .parse()
        .map_err(anyhow::Error::msg)?;
    let server = TestServer::axum(demo::app(variant)).await?;
    let cfg = RunConfig::new(server.base_url())
        .var("API_USER", "demo")
        .var("API_PASS", "secret");

    let mut suites = vec![rust_suite::billing_suite()?];
    suites.extend(load_suites(
        &rust_suite::suites_dir().join("billing.rhai"),
        &LoadOptions::default(),
    )?);
    suites.extend(load_suites(
        &rust_suite::suites_dir().join("billing.js"),
        &LoadOptions { js_workers: 2 },
    )?);

    let mut reports = vec![];
    for s in &suites {
        let r = run_suite(s, &cfg).await;
        print!("{}", r.render());
        reports.push(r);
    }
    let out =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/apitest/junit.xml");
    std::fs::create_dir_all(out.parent().unwrap())?;
    std::fs::write(&out, junit_xml(&reports))?;
    println!("JUnit report: {}", out.display());
    server.shutdown().await?;
    if reports.iter().all(apitest::SuiteReport::passed) {
        Ok(())
    } else {
        std::process::exit(1)
    }
}
