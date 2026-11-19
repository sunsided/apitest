//! Checks that `serial` and `resources` hold both in-process (cargo test) and
//! across processes (cargo nextest, one process per test).
//!
//! Every test marks itself active in a shared directory while it runs. A serial
//! test must never see another active test; two tests sharing a resource must
//! never see each other.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Result, ensure};

/// The marker directory shared by all tests of one run (per nextest run, else per process).
fn dir() -> PathBuf {
    let run =
        std::env::var("NEXTEST_RUN_ID").unwrap_or_else(|_| format!("pid-{}", std::process::id()));
    let d = std::env::temp_dir().join(format!("apitest-locktest-{run}"));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Names of the tests that are currently running, read from their markers.
fn active() -> Vec<String> {
    std::fs::read_dir(dir())
        .unwrap()
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .collect()
}

/// Marks the test `name` as running for about 300 ms and checks the other running
/// tests with `check` every 50 ms; the marker is removed even when a check fails.
async fn hold(name: &str, check: impl Fn(&[String]) -> Result<()>) -> Result<()> {
    let marker = dir().join(name);
    std::fs::write(&marker, b"")?;
    let result = async {
        for _ in 0..6 {
            let others: Vec<String> = active().into_iter().filter(|n| n != name).collect();
            check(&others)?;
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Ok(())
    }
    .await;
    std::fs::remove_file(&marker)?;
    result
}

/// A parallel test never overlaps with a serial one.
#[apitest::test(parallel)]
async fn parallel_a() -> Result<()> {
    hold("parallel_a", |o| {
        ensure!(
            !o.iter().any(|n| n.starts_with("serial")),
            "serial test overlapped: {o:?}"
        );
        Ok(())
    })
    .await
}

/// A second parallel test, so parallel tests have someone to overlap with.
#[apitest::test(parallel)]
async fn parallel_b() -> Result<()> {
    hold("parallel_b", |o| {
        ensure!(
            !o.iter().any(|n| n.starts_with("serial")),
            "serial test overlapped: {o:?}"
        );
        Ok(())
    })
    .await
}

/// A serial test never sees any other test running.
#[apitest::test(serial)]
async fn serial_alone() -> Result<()> {
    hold("serial_alone", |o| {
        ensure!(o.is_empty(), "serial test saw other tests running: {o:?}");
        Ok(())
    })
    .await
}

/// Tests sharing the `db` resource never run at the same time.
#[apitest::test(parallel, resources = ["db"])]
async fn db_writer_1() -> Result<()> {
    hold("db_writer_1", |o| {
        ensure!(
            !o.iter().any(|n| n.starts_with("db_writer")),
            "resource overlap: {o:?}"
        );
        Ok(())
    })
    .await
}

/// The other half of the `db` resource pair.
#[apitest::test(parallel, resources = ["db"])]
async fn db_writer_2() -> Result<()> {
    hold("db_writer_2", |o| {
        ensure!(
            !o.iter().any(|n| n.starts_with("db_writer")),
            "resource overlap: {o:?}"
        );
        Ok(())
    })
    .await
}

/// Runs the registered tests with up to eight in parallel; no service is contacted.
fn main() {
    apitest::harness::main(|| async {
        let mut cfg = apitest::RunConfig::new("http://127.0.0.1:9").quiet(true);
        cfg.max_parallel = 8;
        apitest::harness::Plan::new(cfg).registered()
    })
}
