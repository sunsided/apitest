//! Load testing: reuse functional tests as load scenarios.
//!
//! Each virtual user (VU) gets its own `vu` scope (so `Scope::Vu` fixtures, e.g. a
//! per-user login, run once per VU) and runs the scenario's test in a loop. Every
//! iteration is a fresh `test` scope with the usual setup/teardown.
//!
//! ```mermaid
//! flowchart LR
//!     plan[LoadPlan] --> pre[make_shared + drift check]
//!     pre --> setup[suite fixtures + before_all]
//!     setup --> sc[run_scenario per Scenario]
//!     sc --> vus[VUs via Executor]
//!     vus --> it[iterations: run_test]
//!     it --> m[Metrics]
//!     m --> rep[ScenarioReport + thresholds]
//!     sc --> td[after_all + suite unwind]
//! ```

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use hdrhistogram::Histogram;
use serde::Serialize;

use crate::context::{Ctx, Scope, Shared};
use crate::contract::DriftReport;
use crate::runner::{RunConfig, check_drift, make_shared, run_test, setup_fixtures};
use crate::suite::{HookKind, Suite, TestCase};

/// How many distinct iteration error messages a scenario keeps as samples.
const MAX_SAMPLE_ERRORS: usize = 5;

/// How often the ramping executor re-evaluates its VU target.
const RAMP_TICK: Duration = Duration::from_millis(50);

/// Converts a count or a microsecond latency to `f64` for statistics.
#[expect(
    clippy::cast_precision_loss,
    reason = "request counts and microsecond latencies (histogram max: one hour) stay far below 2^52, so the conversion is exact"
)]
fn as_f64(n: u64) -> f64 {
    n as f64
}

/// Latency (in microseconds) of `d`, clamped to the histogram's lowest bound of 1 µs.
fn micros(d: Duration) -> u64 {
    u64::try_from(d.as_micros()).unwrap_or(u64::MAX).max(1)
}

/// Latency and error counters for one scenario.
///
/// Shared by every VU of the scenario (and by the HTTP layer through the scenario
/// context), so all counters are atomics or behind a mutex. Latencies are recorded
/// in microseconds and reported in milliseconds.
pub struct Metrics {
    /// Latency histogram of single HTTP requests, in microseconds (1 µs to 1 h, 3 significant digits).
    http: Mutex<Histogram<u64>>,
    /// Number of HTTP requests sent.
    http_count: AtomicU64,
    /// Number of HTTP requests that failed (transport error or a 5xx status).
    http_failed: AtomicU64,
    /// Latency histogram of whole test iterations, in microseconds.
    iter: Mutex<Histogram<u64>>,
    /// Number of iterations run (including failed VU setups).
    iter_count: AtomicU64,
    /// Number of iterations that failed or whose cleanup failed.
    iter_failed: AtomicU64,
    /// Up to [`MAX_SAMPLE_ERRORS`] distinct iteration error messages, in first-seen order.
    errors: Mutex<Vec<String>>,
}

impl Metrics {
    /// Empty counters with histograms tracking 1 µs to one hour at 3 significant digits.
    fn new() -> Self {
        let h = || Histogram::new_with_bounds(1, 3_600_000_000, 3).expect("histogram bounds");

        Metrics {
            http: Mutex::new(h()),
            http_count: AtomicU64::new(0),
            http_failed: AtomicU64::new(0),
            iter: Mutex::new(h()),
            iter_count: AtomicU64::new(0),
            iter_failed: AtomicU64::new(0),
            errors: Mutex::default(),
        }
    }

    /// Records one HTTP request that took `d`; `failed` marks transport errors and 5xx responses.
    ///
    /// Latencies above the histogram's one-hour bound are counted but not recorded.
    pub(crate) fn record_http(&self, d: Duration, failed: bool) {
        self.http_count.fetch_add(1, Ordering::Relaxed);
        if failed {
            self.http_failed.fetch_add(1, Ordering::Relaxed);
        }

        let _ = self.http.lock().unwrap().record(micros(d));
    }

    /// Records one iteration that took `d`; `err` is its error message, if it failed.
    ///
    /// Keeps the error as a sample if fewer than [`MAX_SAMPLE_ERRORS`] distinct ones
    /// were seen so far.
    fn record_iter(&self, d: Duration, err: Option<String>) {
        self.iter_count.fetch_add(1, Ordering::Relaxed);
        if let Some(e) = err {
            self.iter_failed.fetch_add(1, Ordering::Relaxed);
            let mut errs = self.errors.lock().unwrap();
            if errs.len() < MAX_SAMPLE_ERRORS && !errs.contains(&e) {
                errs.push(e);
            }
        }

        let _ = self.iter.lock().unwrap().record(micros(d));
    }

    /// HTTP and iteration statistics, with rates computed over the wall-clock time `wall`.
    fn summary(&self, wall: Duration) -> (Stats, Stats) {
        let http = Stats::from_histogram(
            &self.http.lock().unwrap(),
            self.http_count.load(Ordering::Relaxed),
            self.http_failed.load(Ordering::Relaxed),
            wall,
        );
        let iterations = Stats::from_histogram(
            &self.iter.lock().unwrap(),
            self.iter_count.load(Ordering::Relaxed),
            self.iter_failed.load(Ordering::Relaxed),
            wall,
        );

        (http, iterations)
    }

    /// A copy of the sampled iteration error messages.
    fn sample_errors(&self) -> Vec<String> {
        self.errors.lock().unwrap().clone()
    }
}

/// Summary statistics of one metric group (HTTP requests or iterations).
///
/// All latencies are in milliseconds, derived from a microsecond histogram.
#[derive(Debug, Clone, Serialize)]
pub struct Stats {
    /// Number of recorded events.
    pub count: u64,
    /// Number of failed events (`failed <= count`).
    pub failed: u64,
    /// `failed / count` as a fraction in `0.0..=1.0`; `0.0` when nothing was recorded.
    pub error_rate: f64,
    /// Events per second of scenario wall-clock time.
    pub rate_per_s: f64,
    /// Mean latency in milliseconds.
    pub mean_ms: f64,
    /// Median latency in milliseconds.
    pub p50_ms: f64,
    /// 90th percentile latency in milliseconds.
    pub p90_ms: f64,
    /// 95th percentile latency in milliseconds.
    pub p95_ms: f64,
    /// 99th percentile latency in milliseconds.
    pub p99_ms: f64,
    /// Highest recorded latency in milliseconds.
    pub max_ms: f64,
}

impl Stats {
    /// Builds statistics from a microsecond histogram, `count` events of which
    /// `failed` failed, observed over the wall-clock time `wall`.
    fn from_histogram(h: &Histogram<u64>, count: u64, failed: u64, wall: Duration) -> Self {
        let ms = |micros: u64| as_f64(micros) / 1000.0;

        Stats {
            count,
            failed,
            error_rate: if count == 0 {
                0.0
            } else {
                as_f64(failed) / as_f64(count)
            },
            rate_per_s: as_f64(count) / wall.as_secs_f64().max(1e-9),
            mean_ms: h.mean() / 1000.0,
            p50_ms: ms(h.value_at_quantile(0.50)),
            p90_ms: ms(h.value_at_quantile(0.90)),
            p95_ms: ms(h.value_at_quantile(0.95)),
            p99_ms: ms(h.value_at_quantile(0.99)),
            max_ms: ms(h.max()),
        }
    }

    /// Looks up a metric by its threshold name (`p95`, `error_rate`, `rps`, ...).
    ///
    /// Latencies come back in milliseconds, rates per second, counts as plain numbers.
    /// `None` for unknown names.
    fn metric(&self, name: &str) -> Option<f64> {
        Some(match name {
            "count" => as_f64(self.count),
            "failed" => as_f64(self.failed),
            "error_rate" => self.error_rate,
            "rate" | "rps" => self.rate_per_s,
            "mean" | "avg" => self.mean_ms,
            "p50" | "med" => self.p50_ms,
            "p90" => self.p90_ms,
            "p95" => self.p95_ms,
            "p99" => self.p99_ms,
            "max" => self.max_ms,
            _ => return None,
        })
    }
}

/// One ramp stage: move linearly to `target` VUs over `duration`.
#[derive(Debug, Clone, Copy)]
pub struct Stage {
    /// Time the ramp from the previous stage's target to `target` takes.
    pub duration: Duration,
    /// VU count reached at the end of the stage.
    pub target: usize,
}

/// How a scenario schedules its virtual users.
#[derive(Debug, Clone)]
pub enum Executor {
    /// `vus` VUs loop for `duration`.
    ConstantVus {
        /// Number of concurrent VUs.
        vus: usize,
        /// How long the VUs keep starting new iterations.
        duration: Duration,
    },
    /// `vus` VUs share a total of `iterations`.
    Iterations {
        /// Number of concurrent VUs.
        vus: usize,
        /// Total iterations across all VUs.
        iterations: u64,
    },
    /// VU count follows the stages, starting from 0.
    RampingVus {
        /// Stages in execution order.
        stages: Vec<Stage>,
    },
}

impl Executor {
    /// Highest number of VUs this executor runs at once.
    fn max_vus(&self) -> usize {
        match self {
            Executor::ConstantVus { vus, .. } | Executor::Iterations { vus, .. } => *vus,
            Executor::RampingVus { stages } => stages.iter().map(|s| s.target).max().unwrap_or(0),
        }
    }
}

/// `metric op value`, e.g. `http.p95 < 300ms`, `iterations.error_rate <= 0.01`, `http.rps > 50`.
///
/// Time values are compared in milliseconds, percentages as fractions (`2%` is `0.02`).
#[derive(Debug, Clone, Serialize)]
pub struct Threshold {
    /// The expression as written, used in reports.
    pub expr: String,
    /// Normalized metric group: `http` or `iterations`.
    group: String,
    /// Metric name inside the group, as understood by [`Stats::metric`].
    metric: String,
    /// One of `<`, `<=`, `>`, `>=`, `==`.
    op: String,
    /// Right-hand side, normalized by [`parse_quantity`].
    value: f64,
}

impl Threshold {
    /// Parses a threshold expression such as `http.p95 < 300ms`.
    ///
    /// The group may be written `http`/`http_req` or `iter`/`iteration`/`iterations`.
    /// The metric name is only checked when the threshold is evaluated.
    ///
    /// # Errors
    ///
    /// Fails if the expression has no comparison operator, the left side is not
    /// `group.metric`, the group is unknown, or the right side is not a valid quantity
    /// (see [`parse_quantity`]).
    pub fn parse(expr: &str) -> Result<Threshold> {
        let (idx, op) = find_operator(expr)?;
        let (lhs, rhs) = (expr[..idx].trim(), expr[idx + op.len()..].trim());
        let (group, metric) = lhs.split_once('.').ok_or_else(|| {
            anyhow!("threshold metric '{lhs}' must look like http.p95 or iterations.error_rate")
        })?;
        let group = match group {
            "http" | "http_req" => "http",
            "iter" | "iteration" | "iterations" => "iterations",
            g => bail!("unknown metric group '{g}' (http | iterations)"),
        };
        let value = parse_quantity(rhs)?;

        Ok(Threshold {
            expr: expr.to_string(),
            group: group.into(),
            metric: metric.into(),
            op: op.into(),
            value,
        })
    }

    /// Evaluates the threshold; returns the actual metric value and whether it passed.
    ///
    /// `==` compares within `f64::EPSILON`.
    fn eval(&self, http: &Stats, iter: &Stats) -> Result<(f64, bool)> {
        let stats = if self.group == "http" { http } else { iter };
        let actual = stats.metric(&self.metric).ok_or_else(|| {
            anyhow!(
                "unknown metric '{}' in threshold '{}'",
                self.metric,
                self.expr
            )
        })?;
        let ok = match self.op.as_str() {
            "<" => actual < self.value,
            "<=" => actual <= self.value,
            ">" => actual > self.value,
            ">=" => actual >= self.value,
            _ => (actual - self.value).abs() < f64::EPSILON,
        };

        Ok((actual, ok))
    }
}

/// Finds the leftmost comparison operator in `expr`, preferring `<=` over `<` at the
/// same position. Returns its byte index and the operator.
fn find_operator(expr: &str) -> Result<(usize, &'static str)> {
    let ops = ["<=", ">=", "<", ">", "=="];

    ops.iter()
        .filter_map(|op| expr.find(op).map(|i| (i, *op)))
        .min_by_key(|(i, op)| (*i, std::cmp::Reverse(op.len())))
        .ok_or_else(|| anyhow!("threshold '{expr}' has no comparison operator"))
}

/// Parses `300ms`, `1.5s`, `2%`, `0.01`, `50`. Times are returned in ms.
///
/// Suffixes: `ms` (milliseconds), `s` (seconds), `m` (minutes), `%` (percent, returned
/// as a fraction). Without a suffix the number is returned unchanged.
///
/// # Errors
///
/// Fails if the part before the suffix is not a valid `f64`.
pub fn parse_quantity(s: &str) -> Result<f64> {
    let s = s.trim();
    let num = |t: &str| {
        t.trim()
            .parse::<f64>()
            .map_err(|_| anyhow!("invalid number '{t}'"))
    };

    Ok(if let Some(v) = s.strip_suffix("ms") {
        num(v)?
    } else if let Some(v) = s.strip_suffix('s') {
        num(v)? * 1000.0
    } else if let Some(v) = s.strip_suffix('m') {
        num(v)? * 60_000.0
    } else if let Some(v) = s.strip_suffix('%') {
        num(v)? / 100.0
    } else {
        num(s)?
    })
}

/// Parses `30s`, `500ms`, `2m` into a duration. A bare number counts as milliseconds.
///
/// # Errors
///
/// Fails if `s` is not a valid quantity (see [`parse_quantity`]), is a
/// percentage, or is negative, not finite, or too large for a [`Duration`].
pub fn parse_duration(s: &str) -> Result<Duration> {
    if s.trim().ends_with('%') {
        bail!("invalid duration '{s}': a percentage is not a time (expected e.g. 500ms, 30s, 2m)");
    }

    Duration::try_from_secs_f64(parse_quantity(s)? / 1000.0)
        .map_err(|_| anyhow!("invalid duration '{s}' (expected e.g. 500ms, 30s, 2m)"))
}

/// One load scenario: a functional test driven by an executor, plus pass criteria.
#[derive(Debug, Clone)]
pub struct Scenario {
    /// Name shown in reports; defaults to the test name.
    pub name: String,
    /// Name of the suite test every iteration runs.
    pub test: String,
    /// How VUs are scheduled.
    pub executor: Executor,
    /// Pass criteria specific to this scenario (in addition to [`LoadPlan::thresholds`]).
    pub thresholds: Vec<Threshold>,
}

impl Scenario {
    /// A scenario named after `test`, without thresholds.
    #[must_use]
    pub fn new(test: &str, executor: Executor) -> Self {
        Scenario {
            name: test.to_string(),
            test: test.to_string(),
            executor,
            thresholds: vec![],
        }
    }

    /// Adds a threshold, e.g. `.threshold("http.p95 < 300ms")?`.
    ///
    /// # Errors
    ///
    /// Fails if `expr` is not a valid threshold (see [`Threshold::parse`]).
    pub fn threshold(mut self, expr: &str) -> Result<Self> {
        self.thresholds.push(Threshold::parse(expr)?);
        Ok(self)
    }
}

/// A suite plus the scenarios to run against it.
pub struct LoadPlan {
    /// Suite providing the tests, fixtures and hooks.
    pub suite: Suite,
    /// Scenarios, run one after another in this order.
    pub scenarios: Vec<Scenario>,
    /// Thresholds applied to every scenario.
    pub thresholds: Vec<Threshold>,
}

impl LoadPlan {
    /// A plan without scenarios or thresholds.
    #[must_use]
    pub fn new(suite: Suite) -> Self {
        LoadPlan {
            suite,
            scenarios: vec![],
            thresholds: vec![],
        }
    }

    /// Appends a scenario.
    #[must_use]
    pub fn scenario(mut self, s: Scenario) -> Self {
        self.scenarios.push(s);
        self
    }

    /// Adds a threshold that applies to every scenario.
    ///
    /// # Errors
    ///
    /// Fails if `expr` is not a valid threshold (see [`Threshold::parse`]).
    pub fn threshold(mut self, expr: &str) -> Result<Self> {
        self.thresholds.push(Threshold::parse(expr)?);
        Ok(self)
    }

    /// Largest VU count across scenarios (size JS worker pools with this).
    ///
    /// `1` for a plan without scenarios.
    #[must_use]
    pub fn max_vus(&self) -> usize {
        self.scenarios
            .iter()
            .map(|s| s.executor.max_vus())
            .max()
            .unwrap_or(1)
    }
}

/// Outcome of one threshold after a scenario finished.
#[derive(Debug, Clone, Serialize)]
pub struct ThresholdResult {
    /// The expression as written.
    pub expr: String,
    /// Measured value (ms for latencies); `NaN` if the metric name was unknown.
    pub actual: f64,
    /// Whether the threshold held. Always `false` for unknown metrics.
    pub passed: bool,
}

/// Results of one scenario.
#[derive(Debug, Clone, Serialize)]
pub struct ScenarioReport {
    /// Scenario name.
    pub name: String,
    /// Name of the test the scenario ran.
    pub test: String,
    /// Wall-clock duration in milliseconds, including VU setup and teardown.
    pub duration_ms: f64,
    /// Statistics of single HTTP requests.
    pub http: Stats,
    /// Statistics of whole test iterations.
    pub iterations: Stats,
    /// Up to five distinct iteration error messages.
    pub sample_errors: Vec<String>,
    /// Results of the plan-wide thresholds followed by the scenario's own.
    pub thresholds: Vec<ThresholdResult>,
}

/// Results of a whole load run.
#[derive(Debug, Clone, Serialize)]
pub struct LoadReport {
    /// Suite name.
    pub suite: String,
    /// Drift-check results, one per contract and selected drift target it has a spec for.
    pub drift: Vec<DriftReport>,
    /// Why the run stopped before or between scenarios, if it did.
    pub setup_error: Option<String>,
    /// Failures of cleanups, VU tasks and `after_all` hooks.
    pub teardown_errors: Vec<String>,
    /// One report per scenario that ran, in plan order.
    pub scenarios: Vec<ScenarioReport>,
}

impl LoadReport {
    /// Masks secrets and redaction patterns in every text of the report.
    /// [`run_load`] does this before it returns.
    pub fn redact(&mut self, r: &crate::redact::Redactor) {
        r.redact_in_place(&mut self.suite);
        self.drift.iter_mut().for_each(|d| d.redact(r));
        if let Some(e) = &mut self.setup_error {
            r.redact_in_place(e);
        }

        self.teardown_errors
            .iter_mut()
            .for_each(|e| r.redact_in_place(e));
        for s in &mut self.scenarios {
            r.redact_in_place(&mut s.name);
            r.redact_in_place(&mut s.test);
            s.sample_errors
                .iter_mut()
                .for_each(|e| r.redact_in_place(e));
            s.thresholds
                .iter_mut()
                .for_each(|t| r.redact_in_place(&mut t.expr));
        }
    }

    /// `true` if setup and teardown succeeded and every threshold held.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.setup_error.is_none()
            && self.teardown_errors.is_empty()
            && self
                .scenarios
                .iter()
                .all(|s| s.thresholds.iter().all(|t| t.passed))
    }

    /// Human-readable summary: drift, per-scenario statistics, thresholds, verdict.
    #[must_use]
    pub fn render(&self) -> String {
        use std::fmt::Write;

        let mut s = String::new();
        let _ = writeln!(s, "load {}", self.suite);
        for d in &self.drift {
            let verdict = if d.is_breaking() {
                "BREAKING"
            } else {
                "compatible"
            };
            let _ = writeln!(
                s,
                "  contract {}: pinned v{} vs {} v{} → {verdict}",
                d.contract, d.pinned_version, d.target, d.live_version,
            );
        }

        if let Some(e) = &self.setup_error {
            let _ = writeln!(s, "  ✗ setup failed: {e}");
        }

        for sc in &self.scenarios {
            render_scenario(&mut s, sc);
        }

        for e in &self.teardown_errors {
            let _ = writeln!(s, "  ✗ teardown: {e}");
        }

        let _ = writeln!(s, "  {}", if self.passed() { "PASSED" } else { "FAILED" });
        s
    }
}

/// Appends a scenario's header, statistics, error samples and thresholds to `s`.
fn render_scenario(s: &mut String, sc: &ScenarioReport) {
    use std::fmt::Write;

    let _ = writeln!(
        s,
        "  scenario {} (test '{}', {:.1}s)",
        sc.name,
        sc.test,
        sc.duration_ms / 1000.0
    );
    for (label, st) in [("http", &sc.http), ("iterations", &sc.iterations)] {
        let _ = writeln!(
            s,
            "    {label:<10} n={:<7} failed={:<5} rate={:>8.1}/s  avg={:>7.2}ms p50={:>7.2}ms p90={:>7.2}ms p95={:>7.2}ms p99={:>7.2}ms max={:>7.2}ms",
            st.count,
            st.failed,
            st.rate_per_s,
            st.mean_ms,
            st.p50_ms,
            st.p90_ms,
            st.p95_ms,
            st.p99_ms,
            st.max_ms
        );
    }

    for e in &sc.sample_errors {
        let _ = writeln!(s, "    error sample: {e}");
    }

    for t in &sc.thresholds {
        let _ = writeln!(
            s,
            "    {} {}  (actual {:.3})",
            if t.passed { "✓" } else { "✗" },
            t.expr,
            t.actual
        );
    }
}

/// Runs a load plan. Contract validation is sampled (see [`RunConfig::validate_every`]).
///
/// Never fails: problems end up in [`LoadReport::setup_error`] and
/// [`LoadReport::teardown_errors`]. `after_all` hooks and suite cleanups run whenever
/// the suite scope was created, even if setup failed. Secrets and redaction
/// patterns ([`RunConfig::redactor`]) are masked in the report.
pub async fn run_load(plan: &LoadPlan, cfg: &RunConfig) -> LoadReport {
    let mut report = run_load_unmasked(plan, cfg).await;
    report.redact(&cfg.redactor);
    report
}

/// [`run_load`] before masking.
async fn run_load_unmasked(plan: &LoadPlan, cfg: &RunConfig) -> LoadReport {
    let suite = Arc::new(plan.suite.clone());
    let mut report = LoadReport {
        suite: suite.name.clone(),
        drift: vec![],
        setup_error: None,
        teardown_errors: vec![],
        scenarios: vec![],
    };
    let shared = match preflight(&suite, cfg, &mut report.drift).await {
        Ok(shared) => shared,
        Err(e) => {
            report.setup_error = Some(e);
            return report;
        }
    };

    let scx = Ctx::root(shared).child(Scope::Suite, suite.name.clone());
    for (n, v) in &suite.headers {
        scx.default_header(n.clone(), v.clone());
    }

    match suite_setup(&suite, &scx, cfg).await {
        Err(e) => report.setup_error = Some(e),
        Ok(()) => run_scenarios(plan, &suite, &scx, &mut report).await,
    }

    for h in suite.hooks(HookKind::AfterAll) {
        if let Err(e) = crate::context::guarded(&scx, h(scx.clone()), cfg.hook_timeout).await {
            report.teardown_errors.push(format!("after_all: {e:#}"));
        }
    }

    report.teardown_errors.extend(scx.unwind().await);
    report
}

/// Builds the shared run state and checks contract drift, storing the drift results.
///
/// Returns the setup error message if the client cannot be built, the drift check
/// fails, or (with [`RunConfig::check_drift`]) a contract drifted incompatibly.
async fn preflight(
    suite: &Suite,
    cfg: &RunConfig,
    drift: &mut Vec<DriftReport>,
) -> Result<Arc<Shared>, String> {
    let shared = make_shared(suite, cfg).map_err(|e| format!("{e:#}"))?;
    *drift = check_drift(&suite.contracts, cfg)
        .await
        .map_err(|e| format!("contract check: {e:#}"))?;
    if cfg.check_drift
        && let Some(error) = crate::runner::drift_failure(drift)
    {
        return Err(error);
    }

    Ok(shared)
}

/// Runs suite fixtures, then `before_all` hooks until the first failure.
async fn suite_setup(suite: &Suite, scx: &Ctx, cfg: &RunConfig) -> Result<(), String> {
    setup_fixtures(suite, scx, Scope::Suite)
        .await
        .map_err(|e| format!("{e:#}"))?;
    for h in suite.hooks(HookKind::BeforeAll) {
        if let Err(e) = crate::context::guarded(scx, h(scx.clone()), cfg.hook_timeout).await {
            return Err(format!("before_all: {e:#}"));
        }
    }

    Ok(())
}

/// Runs the plan's scenarios in order; stops at the first scenario naming an unknown test.
async fn run_scenarios(plan: &LoadPlan, suite: &Arc<Suite>, scx: &Ctx, report: &mut LoadReport) {
    for scenario in &plan.scenarios {
        let Some(idx) = suite.tests.iter().position(|t| t.name == scenario.test) else {
            report.setup_error = Some(format!(
                "scenario '{}': no test named '{}'",
                scenario.name, scenario.test
            ));
            break;
        };
        let thresholds: Vec<&Threshold> =
            plan.thresholds.iter().chain(&scenario.thresholds).collect();
        let (sr, errs) = run_scenario(suite, scx, scenario, idx, &thresholds).await;
        report.teardown_errors.extend(errs);
        report.scenarios.push(sr);
    }
}

/// A running VU task and its stop flag.
struct VuHandle {
    /// Set to ask the VU to finish its current iteration and exit.
    stop: Arc<AtomicBool>,
    /// The VU's task; resolves to its cleanup errors.
    task: tokio::task::JoinHandle<Vec<String>>,
}

impl VuHandle {
    /// `true` until the VU was asked to stop.
    fn is_active(&self) -> bool {
        !self.stop.load(Ordering::Relaxed)
    }

    /// Asks the VU to stop after its current iteration.
    fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Everything a VU of one scenario needs; cloned into each VU task.
#[derive(Clone)]
struct VuShared {
    /// The suite the test belongs to.
    suite: Arc<Suite>,
    /// Scenario scope; each VU creates its `vu` scope below it.
    scenario: Ctx,
    /// Index of the scenario's test in [`Suite::tests`].
    test_idx: usize,
    /// Remaining iterations shared by all VUs (`Iterations` executor only).
    budget: Option<Arc<AtomicU64>>,
    /// The scenario's counters.
    metrics: Arc<Metrics>,
}

impl VuShared {
    /// Starts VU number `id`; it stops by itself at `deadline`, if given.
    fn spawn(&self, id: usize, deadline: Option<Instant>) -> VuHandle {
        let stop = Arc::new(AtomicBool::new(false));
        let task = tokio::spawn(vu_loop(self.clone(), id, stop.clone(), deadline));

        VuHandle { stop, task }
    }
}

/// Runs one scenario and returns its report plus cleanup errors.
async fn run_scenario(
    suite: &Arc<Suite>,
    scx: &Ctx,
    sc: &Scenario,
    test_idx: usize,
    thresholds: &[&Threshold],
) -> (ScenarioReport, Vec<String>) {
    let metrics = Arc::new(Metrics::new());
    let scenario_cx = scx.child_metrics(&sc.name, metrics.clone());
    let started = Instant::now();
    let budget = match &sc.executor {
        Executor::Iterations { iterations, .. } => Some(Arc::new(AtomicU64::new(*iterations))),
        _ => None,
    };
    let shared = VuShared {
        suite: suite.clone(),
        scenario: scenario_cx.clone(),
        test_idx,
        budget,
        metrics: metrics.clone(),
    };

    let vus = start_vus(&sc.executor, &shared).await;
    let mut teardown_errors = join_vus(vus).await;
    teardown_errors.extend(scenario_cx.unwind().await);

    let wall = started.elapsed();
    let (http, iterations) = metrics.summary(wall);
    let thresholds = evaluate_thresholds(thresholds, &http, &iterations);
    let report = ScenarioReport {
        name: sc.name.clone(),
        test: sc.test.clone(),
        duration_ms: wall.as_secs_f64() * 1000.0,
        http,
        iterations,
        sample_errors: metrics.sample_errors(),
        thresholds,
    };

    (report, teardown_errors)
}

/// Starts the VUs as the executor prescribes.
///
/// For `RampingVus` this returns only after the last stage, with every VU asked to stop.
async fn start_vus(executor: &Executor, shared: &VuShared) -> Vec<VuHandle> {
    match executor {
        Executor::ConstantVus { vus, duration } => {
            let deadline = Instant::now() + *duration;
            (0..*vus).map(|i| shared.spawn(i, Some(deadline))).collect()
        }
        Executor::Iterations { vus, .. } => (0..*vus).map(|i| shared.spawn(i, None)).collect(),
        Executor::RampingVus { stages } => ramp(stages, shared).await,
    }
}

/// Drives the ramping executor through `stages`, then asks every VU to stop.
async fn ramp(stages: &[Stage], shared: &VuShared) -> Vec<VuHandle> {
    let mut vus = vec![];
    let mut next_id = 0usize;
    let mut from = 0usize;
    for stage in stages {
        ramp_stage(stage, from, &mut vus, &mut next_id, shared).await;
        from = stage.target;
    }

    for v in &vus {
        v.stop();
    }

    vus
}

/// Moves the active VU count linearly from `from` to `stage.target`, re-checking every
/// [`RAMP_TICK`] until the stage's duration elapsed.
async fn ramp_stage(
    stage: &Stage,
    from: usize,
    vus: &mut Vec<VuHandle>,
    next_id: &mut usize,
    shared: &VuShared,
) {
    let stage_start = Instant::now();
    loop {
        let frac =
            (stage_start.elapsed().as_secs_f64() / stage.duration.as_secs_f64().max(1e-9)).min(1.0);
        adjust_vus(vus, ramp_target(from, stage.target, frac), next_id, shared);
        if frac >= 1.0 {
            break;
        }

        tokio::time::sleep(RAMP_TICK).await;
    }
}

/// VU count at fraction `frac` (`0.0..=1.0`) of the way from `from` to `to`, rounded.
#[expect(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "VU counts are tiny and the interpolated value lies between `from` and `to`, so it is non-negative and fits in usize"
)]
fn ramp_target(from: usize, to: usize, frac: f64) -> usize {
    (from as f64 + (to as f64 - from as f64) * frac).round() as usize
}

/// Spawns new VUs or stops the most recently started active ones until exactly
/// `target` VUs are active.
fn adjust_vus(vus: &mut Vec<VuHandle>, target: usize, next_id: &mut usize, shared: &VuShared) {
    let active = vus.iter().filter(|v| v.is_active()).count();
    if active < target {
        for _ in active..target {
            vus.push(shared.spawn(*next_id, None));
            *next_id += 1;
        }
    } else if active > target {
        for v in vus
            .iter()
            .rev()
            .filter(|v| v.is_active())
            .take(active - target)
        {
            v.stop();
        }
    }
}

/// Waits for all VUs and collects their cleanup errors (and panics, as `vu task: ...`).
async fn join_vus(vus: Vec<VuHandle>) -> Vec<String> {
    let mut errors = vec![];
    for v in vus {
        match v.task.await {
            Ok(errs) => errors.extend(errs),
            Err(e) => errors.push(format!("vu task: {e}")),
        }
    }

    errors
}

/// Evaluates every threshold; unknown metrics fail with an `actual` of `NaN`.
fn evaluate_thresholds(
    thresholds: &[&Threshold],
    http: &Stats,
    iterations: &Stats,
) -> Vec<ThresholdResult> {
    thresholds
        .iter()
        .map(|t| {
            let (actual, passed) = t.eval(http, iterations).unwrap_or((f64::NAN, false));
            ThresholdResult {
                expr: t.expr.clone(),
                actual,
                passed,
            }
        })
        .collect()
}

/// Body of one VU: sets up `vu` fixtures, then runs iterations until stopped.
///
/// Stops when asked to, at `deadline`, or when the shared iteration budget runs out.
/// A failed VU setup is recorded as one failed iteration of zero duration. Returns
/// the VU scope's cleanup errors.
async fn vu_loop(
    shared: VuShared,
    id: usize,
    stop: Arc<AtomicBool>,
    deadline: Option<Instant>,
) -> Vec<String> {
    let vu = shared.scenario.child_vu(id);
    let mut errors = vec![];
    if let Err(e) = setup_fixtures(&shared.suite, &vu, Scope::Vu).await {
        shared
            .metrics
            .record_iter(Duration::ZERO, Some(format!("vu setup: {e:#}")));
        errors.extend(vu.unwind().await);
        return errors;
    }

    let test: &TestCase = &shared.suite.tests[shared.test_idx];
    while may_start_iteration(&stop, deadline, shared.budget.as_deref()) {
        let started = Instant::now();
        let (result, cleanup, _steps) = run_test(&shared.suite, &vu, test).await;
        let err = match (result, cleanup.is_empty()) {
            (Err(e), _) => Some(format!("{e:#}")),
            (Ok(()), false) => Some(cleanup.join("; ")),
            _ => None,
        };
        shared.metrics.record_iter(started.elapsed(), err);
    }

    errors.extend(vu.unwind().await);
    errors
}

/// Whether a VU may start another iteration; takes one unit from `budget` if so.
///
/// The budget is only touched when neither `stop` nor `deadline` ended the VU.
fn may_start_iteration(
    stop: &AtomicBool,
    deadline: Option<Instant>,
    budget: Option<&AtomicU64>,
) -> bool {
    if stop.load(Ordering::Relaxed) || deadline.is_some_and(|d| Instant::now() >= d) {
        return false;
    }

    budget.is_none_or(|b| {
        b.try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1))
            .is_ok()
    })
}
