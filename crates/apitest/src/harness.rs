//! `cargo test` / nextest integration via libtest-mimic.
//!
//! In `Cargo.toml`:
//!
//! ```toml
//! [[test]]
//! name = "api"
//! harness = false
//! ```
//!
//! In `tests/api.rs`:
//!
//! ```ignore
//! fn main() {
//!     apitest::harness::main(|| async {
//!         let server = apitest::TestServer::axum(my_app::router()).await?;
//!         let cfg = apitest::RunConfig::new(server.base_url());
//!         Ok(apitest::harness::Plan::new(cfg).suite(my_suite()?).keep(server))
//!     })
//! }
//! ```
//!
//! Each test shows up under its id: `suite::test` for builder-style tests, the
//! module path (`billing::create_invoice`) for `#[apitest::test]` functions, so
//! rust-analyzer's "Run test" lands on exactly that test. Contract drift and suite
//! setup/teardown failures show up as `suite::[contract] name`, `suite::[setup]`
//! and `suite::[teardown]`. Filters (`cargo test create`) are applied *before*
//! running, so suite fixtures still run exactly once. Set `APITEST_JUNIT=path`
//! to also write a `JUnit` XML report.
//!
//! ## nextest
//!
//! nextest runs every test in its own process (`--exact <id>`). The harness
//! detects this (`NEXTEST_RUN_ID`) and adapts:
//!
//! * suite and VU fixtures run per process (that's inherent to process-per-test);
//! * `serial` tests take an exclusive file lock shared by all apitest processes
//!   of the run, `parallel` tests a shared one, and every `resource` an exclusive
//!   one, so the concurrency rules still hold across processes;
//! * drift-check results are cached for the run, so the live spec is fetched and
//!   diffed once, not once per test;
//! * suite-level failures (drift, setup, teardown) are reported on the test
//!   itself, because nextest only runs the tests it listed.

use std::any::Any;
use std::future::Future;

use anyhow::Result;
use libtest_mimic::{Arguments, Failed, Trial};

use crate::report::{Outcome, SuiteReport, junit_xml};
use crate::runner::{RunConfig, run_suite};
use crate::suite::Suite;

/// What to run, plus resources (servers, temp dirs, ...) to keep alive meanwhile.
pub struct Plan {
    pub config: RunConfig,
    pub suites: Vec<Suite>,
    keep: Vec<Box<dyn Any + Send>>,
    finally: Vec<(String, Finally)>,
}

type Finally =
    Box<dyn FnOnce() -> std::pin::Pin<Box<dyn Future<Output = Result<()>> + Send>> + Send>;

impl Plan {
    #[must_use]
    pub fn new(config: RunConfig) -> Self {
        Plan {
            config,
            suites: vec![],
            keep: vec![],
            finally: vec![],
        }
    }

    #[must_use]
    pub fn suite(mut self, s: Suite) -> Self {
        self.suites.push(s);
        self
    }

    pub fn suites(mut self, s: impl IntoIterator<Item = Suite>) -> Self {
        self.suites.extend(s);
        self
    }

    /// Adds every suite registered with `#[apitest::test]`, `#[apitest::fixture]`
    /// and `#[apitest::suite]` in this test binary.
    #[cfg(feature = "macros")]
    pub fn registered(mut self) -> Result<Self> {
        self.suites.extend(crate::registry::collect()?);
        Ok(self)
    }

    /// A check that runs after all suites (and all their teardowns) finished, e.g.
    /// "the services contain no leftover test data". Reported as `apitest::[finally] <name>`.
    pub fn finally<F, Fut>(mut self, name: &str, f: F) -> Self
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        self.finally
            .push((name.to_string(), Box::new(move || Box::pin(f()))));
        self
    }

    /// Keeps a value (e.g. a [`crate::TestServer`]) alive until all suites finished.
    pub fn keep(mut self, v: impl Any + Send) -> Self {
        self.keep.push(Box::new(v));
        self
    }
}

fn selected(args: &Arguments, name: &str) -> bool {
    let matches = |pat: &str| {
        if args.exact {
            name == pat
        } else {
            name.contains(pat)
        }
    };
    if args.skip.iter().any(|s| matches(s)) {
        return false;
    }
    match &args.filter {
        Some(f) => matches(f),
        None => true,
    }
}

/// Entry point for a `harness = false` test target. Never returns.
pub fn main<F, Fut>(setup: F) -> !
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<Plan>>,
{
    let args = Arguments::from_args();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    let plan = match rt.block_on(setup()) {
        Ok(p) => p,
        Err(e) => {
            let msg = format!("{e:#}");
            let trial = Trial::test("apitest::[harness setup]", move || Err(Failed::from(msg)));
            libtest_mimic::run(&args, vec![trial]).exit();
        }
    };

    if args.list {
        let trials = plan
            .suites
            .iter()
            .flat_map(|s| {
                s.tests
                    .iter()
                    .map(move |t| Trial::test(s.test_id(t), || Ok(())))
            })
            .collect();
        libtest_mimic::run(&args, trials).exit();
    }

    let nextest_run = std::env::var("NEXTEST_RUN_ID").ok();
    let mut plan = plan;
    let mut suites = vec![];
    for s in &plan.suites {
        let mut s = s.clone();
        let ids: Vec<String> = s.tests.iter().map(|t| s.test_id(t)).collect();
        let mut keep = ids.iter().map(|id| selected(&args, id));
        s.tests.retain(|_| keep.next().unwrap_or(false));
        if !s.tests.is_empty() {
            suites.push(s);
        }
    }

    // Under nextest, hold cross-process locks for the whole process run.
    let _locks = match &nextest_run {
        Some(run_id) if !suites.is_empty() => {
            let dir = std::env::temp_dir().join(format!("apitest-{run_id}"));
            plan.config.drift_cache = Some(dir.join("drift"));
            if std::env::var("APITEST_NEXTEST_LOCKS").as_deref() == Ok("0") {
                eprintln!("apitest: cross-process locks disabled (APITEST_NEXTEST_LOCKS=0)");
                return_none_locks()
            } else {
                let tests = suites.iter().flat_map(|s| s.tests.iter());
                let exclusive = tests
                    .clone()
                    .any(|t| t.opts.concurrency == crate::suite::Concurrency::Serial);
                let resources: Vec<String> = tests
                    .flat_map(|t| t.opts.resources.iter().cloned())
                    .collect();
                match xlock::acquire(&dir, exclusive, &resources) {
                    Ok(l) => Some(l),
                    Err(e) => {
                        eprintln!(
                            "apitest: could not take nextest locks in {}: {e}",
                            dir.display()
                        );
                        None
                    }
                }
            }
        }
        _ => None,
    };

    let mut reports: Vec<SuiteReport> = vec![];
    let mut finally_results: Vec<(String, Result<()>)> = vec![];
    let finally = std::mem::take(&mut plan.finally);
    rt.block_on(async {
        for s in &suites {
            reports.push(run_suite(s, &plan.config).await);
        }
        // Under nextest every process runs only part of the suite, so "after
        // everything" checks are meaningless there.
        if nextest_run.is_none() && !suites.is_empty() {
            for (name, f) in finally {
                finally_results.push((name, f().await));
            }
        }
    });
    drop(plan.keep);
    rt.shutdown_background();

    if let Ok(path) = std::env::var("APITEST_JUNIT") {
        if let Some(dir) = std::path::Path::new(&path).parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(e) = std::fs::write(&path, junit_xml(&reports)) {
            eprintln!("apitest: writing {path}: {e}");
        }
    }

    if nextest_run.is_some() {
        libtest_mimic::run(&args, folded_trials(reports)).exit();
    }

    let mut finally_trials: Vec<Trial> = finally_results
        .into_iter()
        .map(|(name, r)| {
            let r = r.map_err(|e| Failed::from(format!("{e:#}")));
            Trial::test(format!("apitest::[finally] {name}"), move || r)
        })
        .collect();

    let mut trials = vec![];
    for r in reports {
        for d in &r.drift {
            let msg = d
                .findings
                .iter()
                .map(|f| {
                    format!(
                        "{:?} {} @ {}: {}",
                        f.severity, f.operation, f.location, f.message
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            let breaking = d.is_breaking();
            trials.push(Trial::test(
                format!("{}::[contract] {}", r.suite, d.contract),
                move || {
                    if breaking {
                        Err(Failed::from(msg))
                    } else {
                        Ok(())
                    }
                },
            ));
        }
        if let Some(e) = r.setup_error.clone() {
            trials.push(Trial::test(format!("{}::[setup]", r.suite), move || {
                Err(Failed::from(e))
            }));
        }
        if !r.teardown_errors.is_empty() {
            let e = r.teardown_errors.join("\n");
            trials.push(Trial::test(format!("{}::[teardown]", r.suite), move || {
                Err(Failed::from(e))
            }));
        }
        for t in r.tests {
            let name = t.id.clone();
            let trail = t.step_trail();
            let trial = match t.outcome {
                Outcome::Skipped { .. } => Trial::test(name, || Ok(())).with_ignored_flag(true),
                Outcome::Passed if t.teardown_errors.is_empty() => Trial::test(name, || Ok(())),
                Outcome::Passed => {
                    let e = format!("cleanup failed:\n{}", t.teardown_errors.join("\n"));
                    Trial::test(name, move || Err(Failed::from(e)))
                }
                Outcome::Failed { message } => {
                    let mut e = if t.steps.is_empty() {
                        message
                    } else {
                        format!("steps:\n{trail}\n\n{message}")
                    };
                    if !t.teardown_errors.is_empty() {
                        e.push_str(&format!(
                            "\ncleanup also failed:\n{}",
                            t.teardown_errors.join("\n")
                        ));
                    }
                    Trial::test(name, move || Err(Failed::from(e)))
                }
            };
            trials.push(trial);
        }
    }
    trials.append(&mut finally_trials);
    libtest_mimic::run(&args, trials).exit()
}

fn return_none_locks() -> Option<xlock::Held> {
    None
}

/// One trial per test, with suite-level problems folded into each test's result.
fn folded_trials(reports: Vec<SuiteReport>) -> Vec<Trial> {
    let mut trials = vec![];
    for r in reports {
        let mut suite_problems = vec![];
        for d in r.drift.iter().filter(|d| d.is_breaking()) {
            let findings: Vec<String> = d
                .findings
                .iter()
                .filter(|f| f.severity == crate::contract::Severity::Breaking)
                .map(|f| format!("  {} @ {}: {}", f.operation, f.location, f.message))
                .collect();
            suite_problems.push(format!(
                "contract '{}': live v{} is incompatible with pinned v{}:\n{}",
                d.contract,
                d.live_version,
                d.pinned_version,
                findings.join("\n")
            ));
        }
        if let Some(e) = &r.setup_error {
            suite_problems.push(format!("suite setup failed: {e}"));
        }
        for e in &r.teardown_errors {
            suite_problems.push(format!("suite teardown failed: {e}"));
        }
        for t in r.tests {
            let mut problems = suite_problems.clone();
            let trail = t.step_trail();
            match t.outcome {
                Outcome::Failed { message } if !t.steps.is_empty() => {
                    problems.insert(0, format!("steps:\n{trail}\n\n{message}"));
                }
                Outcome::Failed { message } => problems.insert(0, message),
                Outcome::Skipped { reason } if suite_problems.is_empty() => {
                    problems.push(format!("skipped: {reason}"));
                }
                _ => {}
            }
            problems.extend(
                t.teardown_errors
                    .iter()
                    .map(|e| format!("cleanup failed: {e}")),
            );
            trials.push(Trial::test(t.id, move || {
                if problems.is_empty() {
                    Ok(())
                } else {
                    Err(Failed::from(problems.join("\n")))
                }
            }));
        }
    }
    trials
}

/// Cross-process locks for nextest's process-per-test model.
mod xlock {
    use std::fs::{File, OpenOptions};
    use std::path::Path;

    /// Dropping the files releases the locks.
    pub struct Held(#[expect(dead_code)] Vec<File>);

    fn open(path: &Path) -> std::io::Result<File> {
        OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)
    }

    pub fn acquire(dir: &Path, exclusive: bool, resources: &[String]) -> std::io::Result<Held> {
        std::fs::create_dir_all(dir)?;
        let mut held = vec![];
        let global = open(&dir.join("serial.lock"))?;
        if exclusive {
            global.lock()?;
        } else {
            global.lock_shared()?;
        }
        held.push(global);
        let mut res: Vec<&String> = resources.iter().collect();
        res.sort();
        res.dedup();
        for r in res {
            let safe: String = r
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                .collect();
            let f = open(&dir.join(format!("resource-{safe}.lock")))?;
            f.lock()?;
            held.push(f);
        }
        Ok(Held(held))
    }
}
