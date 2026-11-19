//! Checks that `serial` and `resources` hold both in-process (cargo test) and
//! across processes (cargo nextest, one process per test).
//!
//! Every test marks itself active in a shared directory while it runs. A serial
//! test must never see another active test; two tests sharing a resource must
//! never see each other.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Result, ensure};

fn dir() -> PathBuf {
    let run =
        std::env::var("NEXTEST_RUN_ID").unwrap_or_else(|_| format!("pid-{}", std::process::id()));
    let d = std::env::temp_dir().join(format!("apitest-locktest-{run}"));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn active() -> Vec<String> {
    std::fs::read_dir(dir())
        .unwrap()
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .collect()
}

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

#[apitest::test(serial)]
async fn serial_alone() -> Result<()> {
    hold("serial_alone", |o| {
        ensure!(o.is_empty(), "serial test saw other tests running: {o:?}");
        Ok(())
    })
    .await
}

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

fn main() {
    apitest::harness::main(|| async {
        let mut cfg = apitest::RunConfig::new("http://127.0.0.1:9").quiet(true);
        cfg.max_parallel = 8;
        apitest::harness::Plan::new(cfg).registered()
    })
}
