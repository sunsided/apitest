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
//! In `tests/api.rs` (serve your app in-process with [`TestServer`](crate::TestServer)
//! and hand it to [`Plan::keep`] so it lives until the run ends):
//!
//! ```no_run
//! use apitest::prelude::*;
//!
//! fn main() {
//!     apitest::harness::main(|| async {
//!         let cfg = RunConfig::new("http://127.0.0.1:8080");
//!         let suite = Suite::new("health").test("responds", |cx| async move {
//!             cx.request("GET", "/health").send().await?.expect(200)?;
//!             Ok(())
//!         });
//!
//!         Ok(apitest::harness::Plan::new(cfg).suite(suite))
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
use std::fmt::Write as _;
use std::future::Future;

use anyhow::Result;
use libtest_mimic::{Arguments, Failed, Trial};
use tokio::runtime::Runtime;

use crate::contract::DriftReport;
use crate::report::{Outcome, SuiteReport, TestReport, junit_xml};
use crate::runner::{RunConfig, run_suite};
use crate::suite::Suite;

/// What to run, plus resources (servers, temp dirs, ...) to keep alive meanwhile.
pub struct Plan {
    /// Settings for every suite of the plan.
    pub config: RunConfig,
    /// Suites, run one after another in this order.
    pub suites: Vec<Suite>,
    /// Values dropped only after all suites and `finally` checks finished.
    keep: Vec<Box<dyn Any + Send>>,
    /// Named checks run after all suites, in registration order.
    finally: Vec<(String, Finally)>,
}

/// A deferred async check registered with [`Plan::finally`].
type Finally =
    Box<dyn FnOnce() -> std::pin::Pin<Box<dyn Future<Output = Result<()>> + Send>> + Send>;

impl Plan {
    /// An empty plan using `config` for every suite.
    #[must_use]
    pub fn new(config: RunConfig) -> Self {
        Plan {
            config,
            suites: vec![],
            keep: vec![],
            finally: vec![],
        }
    }

    /// Appends a suite.
    #[must_use]
    pub fn suite(mut self, s: Suite) -> Self {
        self.suites.push(s);
        self
    }

    /// Appends several suites.
    #[must_use]
    pub fn suites(mut self, s: impl IntoIterator<Item = Suite>) -> Self {
        self.suites.extend(s);
        self
    }

    /// Adds every suite registered with `#[apitest::test]`, `#[apitest::fixture]`
    /// and `#[apitest::suite]` in this test binary.
    ///
    /// # Errors
    ///
    /// Fails if two `#[apitest::suite]` declarations share a suite key, or a suite
    /// builder returns an error.
    #[cfg(feature = "macros")]
    pub fn registered(mut self) -> Result<Self> {
        self.suites.extend(crate::registry::collect()?);
        Ok(self)
    }

    /// A check that runs after all suites (and all their teardowns) finished, e.g.
    /// "the services contain no leftover test data". Reported as `apitest::[finally] <name>`.
    ///
    /// Skipped under nextest, where each process only runs part of the suites.
    #[must_use]
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
    #[must_use]
    pub fn keep(mut self, v: impl Any + Send) -> Self {
        self.keep.push(Box::new(v));
        self
    }
}

/// Whether the libtest filter and `--skip` arguments select the test `name`.
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
///
/// Builds a multi-threaded tokio runtime, awaits `setup` for the [`Plan`], runs the
/// selected tests and exits the process with libtest's exit code. A failing `setup`
/// is reported as the single test `apitest::[harness setup]`.
///
/// # Panics
///
/// Panics if the tokio runtime cannot be built.
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

    let mut plan = match rt.block_on(setup()) {
        Ok(p) => p,
        Err(e) => {
            let msg = format!("{e:#}");
            let trial = Trial::test("apitest::[harness setup]", move || Err(Failed::from(msg)));
            libtest_mimic::run(&args, vec![trial]).exit();
        }
    };

    if args.list {
        libtest_mimic::run(&args, listed_trials(&plan.suites)).exit();
    }

    let nextest_run = std::env::var("NEXTEST_RUN_ID").ok();
    let suites = filtered_suites(&plan.suites, &args);

    // Under nextest, hold cross-process locks for the whole process run.
    let _locks = match &nextest_run {
        Some(run_id) if !suites.is_empty() => nextest_locks(run_id, &suites, &mut plan.config),
        _ => None,
    };

    // Under nextest every process runs only part of the suite, so "after
    // everything" checks are meaningless there.
    let run_finally = nextest_run.is_none() && !suites.is_empty();
    let finally = std::mem::take(&mut plan.finally);
    let (reports, finally_results) = execute(&rt, &suites, &plan.config, finally, run_finally);
    drop(plan.keep);
    rt.shutdown_background();

    write_junit_if_requested(&reports);
    let trials = if nextest_run.is_some() {
        folded_trials(reports)
    } else {
        standard_trials(reports, finally_results)
    };

    libtest_mimic::run(&args, trials).exit()
}

/// One no-op trial per test, for `--list`.
fn listed_trials(suites: &[Suite]) -> Vec<Trial> {
    suites
        .iter()
        .flat_map(|s| {
            s.tests
                .iter()
                .map(move |t| Trial::test(s.test_id(t), || Ok(())))
        })
        .collect()
}

/// Copies of the suites holding only the selected tests; suites left empty are dropped.
fn filtered_suites(suites: &[Suite], args: &Arguments) -> Vec<Suite> {
    let mut out = vec![];
    for s in suites {
        let mut s = s.clone();
        let ids: Vec<String> = s.tests.iter().map(|t| s.test_id(t)).collect();
        let mut keep = ids.iter().map(|id| selected(args, id));
        s.tests.retain(|_| keep.next().unwrap_or(false));
        if !s.tests.is_empty() {
            out.push(s);
        }
    }

    out
}

/// Points the drift cache at the run's shared directory and takes the cross-process
/// locks the selected tests need.
///
/// `APITEST_NEXTEST_LOCKS=0` disables the locks. Lock failures are reported on
/// stderr and the run continues without them.
fn nextest_locks(run_id: &str, suites: &[Suite], config: &mut RunConfig) -> Option<xlock::Held> {
    let dir = std::env::temp_dir().join(format!("apitest-{run_id}"));
    config.drift_cache = Some(dir.join("drift"));
    if std::env::var("APITEST_NEXTEST_LOCKS").as_deref() == Ok("0") {
        eprintln!("apitest: cross-process locks disabled (APITEST_NEXTEST_LOCKS=0)");
        return None;
    }

    let tests = suites.iter().flat_map(|s| s.tests.iter());
    let exclusive = tests
        .clone()
        .any(|t| t.opts.concurrency == crate::suite::Concurrency::Serial);
    let resources: Vec<String> = tests
        .flat_map(|t| t.opts.resources.iter().cloned())
        .collect();

    xlock::acquire(&dir, exclusive, &resources)
        .inspect_err(|e| {
            eprintln!(
                "apitest: could not take nextest locks in {}: {e}",
                dir.display()
            );
        })
        .ok()
}

/// Runs the suites in order, then (if `run_finally`) the `finally` checks.
fn execute(
    rt: &Runtime,
    suites: &[Suite],
    config: &RunConfig,
    finally: Vec<(String, Finally)>,
    run_finally: bool,
) -> (Vec<SuiteReport>, Vec<(String, Result<()>)>) {
    rt.block_on(async {
        let mut reports = vec![];
        for s in suites {
            reports.push(run_suite(s, config).await);
        }

        let mut finally_results = vec![];
        if run_finally {
            for (name, f) in finally {
                finally_results.push((name, f().await));
            }
        }

        (reports, finally_results)
    })
}

/// Writes a `JUnit` report to `$APITEST_JUNIT`, if set. Failures go to stderr.
fn write_junit_if_requested(reports: &[SuiteReport]) {
    let Ok(path) = std::env::var("APITEST_JUNIT") else {
        return;
    };

    if let Some(dir) = std::path::Path::new(&path).parent() {
        let _ = std::fs::create_dir_all(dir);
    }

    if let Err(e) = std::fs::write(&path, junit_xml(reports)) {
        eprintln!("apitest: writing {path}: {e}");
    }
}

/// Trials for a regular `cargo test` run: suite-level pseudo-tests, one trial per
/// test, then the `finally` checks.
fn standard_trials(
    reports: Vec<SuiteReport>,
    finally_results: Vec<(String, Result<()>)>,
) -> Vec<Trial> {
    let mut trials = vec![];
    for r in reports {
        trials.extend(r.drift.iter().map(|d| contract_trial(&r.suite, d)));
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

        trials.extend(r.tests.into_iter().map(test_trial));
    }

    trials.extend(finally_results.into_iter().map(|(name, r)| {
        let r = r.map_err(|e| Failed::from(format!("{e:#}")));
        Trial::test(format!("apitest::[finally] {name}"), move || r)
    }));
    trials
}

/// The `suite::[contract] name` trial: fails with all findings if the drift is breaking.
fn contract_trial(suite: &str, d: &DriftReport) -> Trial {
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

    Trial::test(format!("{suite}::[contract] {}", d.contract), move || {
        if breaking {
            Err(Failed::from(msg))
        } else {
            Ok(())
        }
    })
}

/// The trial of one test: ignored if skipped, failed with steps, error and cleanup
/// errors otherwise.
fn test_trial(t: TestReport) -> Trial {
    if let Some(e) = test_failure(&t) {
        return Trial::test(t.id, move || Err(Failed::from(e)));
    }

    let skipped = matches!(t.outcome, Outcome::Skipped { .. });
    Trial::test(t.id, || Ok(())).with_ignored_flag(skipped)
}

/// The libtest failure message of one test, `None` if it passed or was skipped.
///
/// Plain text on purpose: libtest, nextest and IDEs show and parse it. Terminal
/// rendering with miette is only for the CLI (`SuiteReport::render_terminal`).
fn test_failure(t: &TestReport) -> Option<String> {
    match &t.outcome {
        Outcome::Skipped { .. } => None,
        Outcome::Passed if t.teardown_errors.is_empty() => None,
        Outcome::Passed => Some(format!("cleanup failed:\n{}", t.teardown_errors.join("\n"))),
        Outcome::Failed { message, .. } => {
            let mut e = if t.steps.is_empty() {
                message.clone()
            } else {
                format!("steps:\n{}\n\n{message}", t.step_trail())
            };
            if !t.teardown_errors.is_empty() {
                let _ = write!(
                    e,
                    "\ncleanup also failed:\n{}",
                    t.teardown_errors.join("\n")
                );
            }

            Some(e)
        }
    }
}

/// One trial per test, with suite-level problems folded into each test's result.
fn folded_trials(reports: Vec<SuiteReport>) -> Vec<Trial> {
    let mut trials = vec![];
    for r in reports {
        let suite_problems = suite_problems(&r);
        for t in r.tests {
            trials.push(folded_test_trial(t, &suite_problems));
        }
    }

    trials
}

/// Suite-level problems (breaking drift, setup and teardown failures) as messages.
fn suite_problems(r: &SuiteReport) -> Vec<String> {
    let mut problems = vec![];
    for d in r.drift.iter().filter(|d| d.is_breaking()) {
        let findings: Vec<String> = d
            .findings
            .iter()
            .filter(|f| f.severity == crate::contract::Severity::Breaking)
            .map(|f| format!("  {} @ {}: {}", f.operation, f.location, f.message))
            .collect();
        problems.push(format!(
            "contract '{}': live v{} is incompatible with pinned v{}:\n{}",
            d.contract,
            d.live_version,
            d.pinned_version,
            findings.join("\n")
        ));
    }

    if let Some(e) = &r.setup_error {
        problems.push(format!("suite setup failed: {e}"));
    }

    for e in &r.teardown_errors {
        problems.push(format!("suite teardown failed: {e}"));
    }

    problems
}

/// The trial of one test under nextest: its own failure first, then the suite's
/// problems, then its cleanup errors. A skip only fails the trial when the suite
/// had no problems to explain it.
fn folded_test_trial(t: TestReport, suite_problems: &[String]) -> Trial {
    let mut problems = suite_problems.to_vec();
    let trail = t.step_trail();
    match t.outcome {
        Outcome::Failed { message, .. } if !t.steps.is_empty() => {
            problems.insert(0, format!("steps:\n{trail}\n\n{message}"));
        }
        Outcome::Failed { message, .. } => problems.insert(0, message),
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

    Trial::test(t.id, move || {
        if problems.is_empty() {
            Ok(())
        } else {
            Err(Failed::from(problems.join("\n")))
        }
    })
}

/// Cross-process locks for nextest's process-per-test model.
///
/// Advisory file locks in the run's temp directory: `serial.lock` (exclusive for
/// serial tests, shared otherwise) and one exclusive `resource-<name>.lock` per
/// resource, taken in sorted order so processes cannot deadlock.
mod xlock {
    use std::fs::{File, OpenOptions};
    use std::path::Path;

    /// Held locks; dropping the files releases them.
    pub struct Held {
        /// The locked files, only kept open for their locks.
        _files: Vec<File>,
    }

    /// Opens (creating if needed) a lock file without truncating it.
    fn open(path: &Path) -> std::io::Result<File> {
        OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)
    }

    /// Blocks until the global lock (exclusive if `exclusive`) and every resource lock
    /// are held. Resource names are sanitized to ASCII alphanumerics and `_`.
    ///
    /// # Errors
    ///
    /// Fails if `dir` or a lock file cannot be created or a lock cannot be taken.
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

        Ok(Held { _files: held })
    }
}

#[cfg(test)]
mod tests {
    use anyhow::{Context as _, anyhow};

    use super::test_failure;
    use crate::report::{Outcome, StepReport, TestReport};

    /// libtest messages keep the joined chain and carry no miette glyphs or colours.
    #[test]
    fn failure_messages_are_plain_text() {
        let e = Err::<(), _>(anyhow!("status 401"))
            .context("login as 'mallory' failed")
            .context("step 'sign in' failed")
            .unwrap_err();
        let t = TestReport {
            name: "t".into(),
            id: "s::t".into(),
            outcome: Outcome::failed(&e),
            duration_ms: 1.0,
            teardown_errors: vec!["defer 'x': gone".into()],
            steps: vec![StepReport {
                name: "sign in".into(),
                duration_ms: 1.0,
                error: Some("login as 'mallory' failed: status 401".into()),
            }],
        };

        let msg = test_failure(&t).unwrap();
        assert_eq!(
            msg,
            "steps:\n  ✗ sign in [1 ms]\n\n\
             step 'sign in' failed: login as 'mallory' failed: status 401\n\
             cleanup also failed:\ndefer 'x': gone"
        );
        for glyph in ["×", "─▶", "\x1b["] {
            assert!(!msg.contains(glyph), "{msg}");
        }
    }
}
