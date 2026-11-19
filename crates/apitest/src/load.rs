//! Load testing: reuse functional tests as load scenarios.
//!
//! Each virtual user (VU) gets its own `vu` scope (so `Scope::Vu` fixtures, e.g. a
//! per-user login, run once per VU) and runs the scenario's test in a loop. Every
//! iteration is a fresh `test` scope with the usual setup/teardown.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use hdrhistogram::Histogram;
use serde::Serialize;

use crate::context::{Ctx, Scope};
use crate::contract::DriftReport;
use crate::runner::{RunConfig, check_drift, make_shared, run_test, setup_fixtures};
use crate::suite::{HookKind, Suite, TestCase};

/// Latency and error counters for one scenario.
pub struct Metrics {
    http: Mutex<Histogram<u64>>,
    http_count: AtomicU64,
    http_failed: AtomicU64,
    iter: Mutex<Histogram<u64>>,
    iter_count: AtomicU64,
    iter_failed: AtomicU64,
    errors: Mutex<Vec<String>>,
}

impl Metrics {
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

    pub(crate) fn record_http(&self, d: Duration, failed: bool) {
        self.http_count.fetch_add(1, Ordering::Relaxed);
        if failed {
            self.http_failed.fetch_add(1, Ordering::Relaxed);
        }
        let _ = self
            .http
            .lock()
            .unwrap()
            .record((d.as_micros() as u64).max(1));
    }

    fn record_iter(&self, d: Duration, err: Option<String>) {
        self.iter_count.fetch_add(1, Ordering::Relaxed);
        if let Some(e) = err {
            self.iter_failed.fetch_add(1, Ordering::Relaxed);
            let mut errs = self.errors.lock().unwrap();
            if errs.len() < 5 && !errs.contains(&e) {
                errs.push(e);
            }
        }
        let _ = self
            .iter
            .lock()
            .unwrap()
            .record((d.as_micros() as u64).max(1));
    }

    fn summary(&self, wall: Duration) -> (Stats, Stats) {
        let stats = |h: &Histogram<u64>, n: u64, failed: u64| Stats {
            count: n,
            failed,
            error_rate: if n == 0 {
                0.0
            } else {
                failed as f64 / n as f64
            },
            rate_per_s: n as f64 / wall.as_secs_f64().max(1e-9),
            mean_ms: h.mean() / 1000.0,
            p50_ms: h.value_at_quantile(0.50) as f64 / 1000.0,
            p90_ms: h.value_at_quantile(0.90) as f64 / 1000.0,
            p95_ms: h.value_at_quantile(0.95) as f64 / 1000.0,
            p99_ms: h.value_at_quantile(0.99) as f64 / 1000.0,
            max_ms: h.max() as f64 / 1000.0,
        };
        (
            stats(
                &self.http.lock().unwrap(),
                self.http_count.load(Ordering::Relaxed),
                self.http_failed.load(Ordering::Relaxed),
            ),
            stats(
                &self.iter.lock().unwrap(),
                self.iter_count.load(Ordering::Relaxed),
                self.iter_failed.load(Ordering::Relaxed),
            ),
        )
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Stats {
    pub count: u64,
    pub failed: u64,
    pub error_rate: f64,
    pub rate_per_s: f64,
    pub mean_ms: f64,
    pub p50_ms: f64,
    pub p90_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
}

impl Stats {
    fn metric(&self, name: &str) -> Option<f64> {
        Some(match name {
            "count" => self.count as f64,
            "failed" => self.failed as f64,
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
    pub duration: Duration,
    pub target: usize,
}

#[derive(Debug, Clone)]
pub enum Executor {
    /// `vus` VUs loop for `duration`.
    ConstantVus { vus: usize, duration: Duration },
    /// `vus` VUs share a total of `iterations`.
    Iterations { vus: usize, iterations: u64 },
    /// VU count follows the stages, starting from 0.
    RampingVus { stages: Vec<Stage> },
}

impl Executor {
    fn max_vus(&self) -> usize {
        match self {
            Executor::ConstantVus { vus, .. } | Executor::Iterations { vus, .. } => *vus,
            Executor::RampingVus { stages } => stages.iter().map(|s| s.target).max().unwrap_or(0),
        }
    }
}

/// `metric op value`, e.g. `http.p95 < 300ms`, `iterations.error_rate <= 0.01`, `http.rps > 50`.
#[derive(Debug, Clone, Serialize)]
pub struct Threshold {
    pub expr: String,
    group: String,
    metric: String,
    op: String,
    value: f64,
}

impl Threshold {
    pub fn parse(expr: &str) -> Result<Threshold> {
        let ops = ["<=", ">=", "<", ">", "=="];
        let (idx, op) = ops
            .iter()
            .filter_map(|op| expr.find(op).map(|i| (i, *op)))
            .min_by_key(|(i, op)| (*i, std::cmp::Reverse(op.len())))
            .ok_or_else(|| anyhow!("threshold '{expr}' has no comparison operator"))?;
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

/// Parses `300ms`, `1.5s`, `2%`, `0.01`, `50`. Times are returned in ms.
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

/// Parses `30s`, `500ms`, `2m` into a duration.
pub fn parse_duration(s: &str) -> Result<Duration> {
    Ok(Duration::from_secs_f64(parse_quantity(s)? / 1000.0))
}

#[derive(Debug, Clone)]
pub struct Scenario {
    pub name: String,
    pub test: String,
    pub executor: Executor,
    pub thresholds: Vec<Threshold>,
}

impl Scenario {
    pub fn new(test: &str, executor: Executor) -> Self {
        Scenario {
            name: test.to_string(),
            test: test.to_string(),
            executor,
            thresholds: vec![],
        }
    }

    pub fn threshold(mut self, expr: &str) -> Result<Self> {
        self.thresholds.push(Threshold::parse(expr)?);
        Ok(self)
    }
}

pub struct LoadPlan {
    pub suite: Suite,
    pub scenarios: Vec<Scenario>,
    /// Thresholds applied to every scenario.
    pub thresholds: Vec<Threshold>,
}

impl LoadPlan {
    pub fn new(suite: Suite) -> Self {
        LoadPlan {
            suite,
            scenarios: vec![],
            thresholds: vec![],
        }
    }

    pub fn scenario(mut self, s: Scenario) -> Self {
        self.scenarios.push(s);
        self
    }

    pub fn threshold(mut self, expr: &str) -> Result<Self> {
        self.thresholds.push(Threshold::parse(expr)?);
        Ok(self)
    }

    /// Largest VU count across scenarios (size JS worker pools with this).
    pub fn max_vus(&self) -> usize {
        self.scenarios
            .iter()
            .map(|s| s.executor.max_vus())
            .max()
            .unwrap_or(1)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ThresholdResult {
    pub expr: String,
    pub actual: f64,
    pub passed: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScenarioReport {
    pub name: String,
    pub test: String,
    pub duration_ms: f64,
    pub http: Stats,
    pub iterations: Stats,
    pub sample_errors: Vec<String>,
    pub thresholds: Vec<ThresholdResult>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LoadReport {
    pub suite: String,
    pub drift: Vec<DriftReport>,
    pub setup_error: Option<String>,
    pub teardown_errors: Vec<String>,
    pub scenarios: Vec<ScenarioReport>,
}

impl LoadReport {
    pub fn passed(&self) -> bool {
        self.setup_error.is_none()
            && self.teardown_errors.is_empty()
            && self
                .scenarios
                .iter()
                .all(|s| s.thresholds.iter().all(|t| t.passed))
    }

    pub fn render(&self) -> String {
        use std::fmt::Write;
        let mut s = String::new();
        let _ = writeln!(s, "load {}", self.suite);
        for d in &self.drift {
            let _ = writeln!(
                s,
                "  contract {}: pinned v{} vs live v{} → {}",
                d.contract,
                d.pinned_version,
                d.live_version,
                if d.is_breaking() {
                    "BREAKING"
                } else {
                    "compatible"
                }
            );
        }
        if let Some(e) = &self.setup_error {
            let _ = writeln!(s, "  ✗ setup failed: {e}");
        }
        for sc in &self.scenarios {
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
        for e in &self.teardown_errors {
            let _ = writeln!(s, "  ✗ teardown: {e}");
        }
        let _ = writeln!(s, "  {}", if self.passed() { "PASSED" } else { "FAILED" });
        s
    }
}

/// Runs a load plan. Contract validation is sampled (see [`RunConfig::validate_every`]).
pub async fn run_load(plan: &LoadPlan, cfg: &RunConfig) -> LoadReport {
    let suite = Arc::new(plan.suite.clone());
    let mut report = LoadReport {
        suite: suite.name.clone(),
        drift: vec![],
        setup_error: None,
        teardown_errors: vec![],
        scenarios: vec![],
    };
    let shared = match make_shared(&suite, cfg) {
        Ok(s) => s,
        Err(e) => {
            report.setup_error = Some(format!("{e:#}"));
            return report;
        }
    };
    match check_drift(&suite.contracts, cfg).await {
        Ok(d) => report.drift = d,
        Err(e) => {
            report.setup_error = Some(format!("contract check: {e:#}"));
            return report;
        }
    }
    if cfg.check_drift && report.drift.iter().any(DriftReport::is_breaking) {
        report.setup_error = Some("live API is incompatible with the pinned contract".into());
        return report;
    }

    let scx = Ctx::root(shared).child(Scope::Suite, suite.name.clone());
    for (n, v) in &suite.headers {
        scx.default_header(n.clone(), v.clone());
    }
    let mut setup = setup_fixtures(&suite, &scx, Scope::Suite)
        .await
        .map_err(|e| format!("{e:#}"));
    if setup.is_ok() {
        for h in suite.hooks(HookKind::BeforeAll) {
            if let Err(e) = crate::context::guarded(&scx, h(scx.clone()), cfg.hook_timeout).await {
                setup = Err(format!("before_all: {e:#}"));
                break;
            }
        }
    }
    match setup {
        Err(e) => report.setup_error = Some(e),
        Ok(()) => {
            for sc in &plan.scenarios {
                match suite.tests.iter().position(|t| t.name == sc.test) {
                    None => {
                        report.setup_error = Some(format!(
                            "scenario '{}': no test named '{}'",
                            sc.name, sc.test
                        ));
                        break;
                    }
                    Some(idx) => {
                        let thresholds: Vec<&Threshold> =
                            plan.thresholds.iter().chain(&sc.thresholds).collect();
                        let (sr, errs) = run_scenario(&suite, &scx, sc, idx, &thresholds).await;
                        report.teardown_errors.extend(errs);
                        report.scenarios.push(sr);
                    }
                }
            }
        }
    }
    for h in suite.hooks(HookKind::AfterAll) {
        if let Err(e) = crate::context::guarded(&scx, h(scx.clone()), cfg.hook_timeout).await {
            report.teardown_errors.push(format!("after_all: {e:#}"));
        }
    }
    report.teardown_errors.extend(scx.unwind().await);
    report
}

struct VuHandle {
    stop: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<Vec<String>>,
}

async fn run_scenario(
    suite: &Arc<Suite>,
    scx: &Ctx,
    sc: &Scenario,
    test_idx: usize,
    thresholds: &[&Threshold],
) -> (ScenarioReport, Vec<String>) {
    let metrics = Arc::new(Metrics::new());
    let sctx = scx.child_metrics(&sc.name, metrics.clone());
    let started = Instant::now();
    let budget = match &sc.executor {
        Executor::Iterations { iterations, .. } => Some(Arc::new(AtomicU64::new(*iterations))),
        _ => None,
    };
    let spawn = |id: usize, deadline: Option<Instant>| {
        let stop = Arc::new(AtomicBool::new(false));
        let task = tokio::spawn(vu_loop(
            suite.clone(),
            sctx.clone(),
            id,
            test_idx,
            stop.clone(),
            deadline,
            budget.clone(),
            metrics.clone(),
        ));
        VuHandle { stop, task }
    };

    let mut vus: Vec<VuHandle> = vec![];
    match &sc.executor {
        Executor::ConstantVus { vus: n, duration } => {
            let deadline = Instant::now() + *duration;
            vus = (0..*n).map(|i| spawn(i, Some(deadline))).collect();
        }
        Executor::Iterations { vus: n, .. } => {
            vus = (0..*n).map(|i| spawn(i, None)).collect();
        }
        Executor::RampingVus { stages } => {
            let mut from = 0usize;
            let mut next_id = 0usize;
            for st in stages {
                let stage_start = Instant::now();
                loop {
                    let frac = (stage_start.elapsed().as_secs_f64()
                        / st.duration.as_secs_f64().max(1e-9))
                    .min(1.0);
                    let target =
                        (from as f64 + (st.target as f64 - from as f64) * frac).round() as usize;
                    let active = vus
                        .iter()
                        .filter(|v| !v.stop.load(Ordering::Relaxed))
                        .count();
                    if active < target {
                        for _ in active..target {
                            vus.push(spawn(next_id, None));
                            next_id += 1;
                        }
                    } else if active > target {
                        for v in vus
                            .iter()
                            .rev()
                            .filter(|v| !v.stop.load(Ordering::Relaxed))
                            .take(active - target)
                        {
                            v.stop.store(true, Ordering::Relaxed);
                        }
                    }
                    if frac >= 1.0 {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                from = st.target;
            }
            for v in &vus {
                v.stop.store(true, Ordering::Relaxed);
            }
        }
    }
    let mut teardown_errors = vec![];
    for v in vus {
        match v.task.await {
            Ok(errs) => teardown_errors.extend(errs),
            Err(e) => teardown_errors.push(format!("vu task: {e}")),
        }
    }
    teardown_errors.extend(sctx.unwind().await);
    let wall = started.elapsed();
    let (http, iterations) = metrics.summary(wall);
    let thresholds = thresholds
        .iter()
        .map(|t| match t.eval(&http, &iterations) {
            Ok((actual, passed)) => ThresholdResult {
                expr: t.expr.clone(),
                actual,
                passed,
            },
            Err(_) => ThresholdResult {
                expr: t.expr.clone(),
                actual: f64::NAN,
                passed: false,
            },
        })
        .collect();
    (
        ScenarioReport {
            name: sc.name.clone(),
            test: sc.test.clone(),
            duration_ms: wall.as_secs_f64() * 1000.0,
            http,
            iterations,
            sample_errors: metrics.errors.lock().unwrap().clone(),
            thresholds,
        },
        teardown_errors,
    )
}

#[allow(clippy::too_many_arguments)]
async fn vu_loop(
    suite: Arc<Suite>,
    parent: Ctx,
    id: usize,
    test_idx: usize,
    stop: Arc<AtomicBool>,
    deadline: Option<Instant>,
    budget: Option<Arc<AtomicU64>>,
    metrics: Arc<Metrics>,
) -> Vec<String> {
    let vu = parent.child_vu(id);
    let mut errors = vec![];
    if let Err(e) = setup_fixtures(&suite, &vu, Scope::Vu).await {
        metrics.record_iter(Duration::ZERO, Some(format!("vu setup: {e:#}")));
        errors.extend(vu.unwind().await);
        return errors;
    }
    let test: &TestCase = &suite.tests[test_idx];
    loop {
        if stop.load(Ordering::Relaxed) || deadline.is_some_and(|d| Instant::now() >= d) {
            break;
        }
        if let Some(b) = &budget
            && b.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1))
                .is_err()
        {
            break;
        }
        let started = Instant::now();
        let (result, cleanup, _steps) = run_test(&suite, &vu, test).await;
        let err = match (result, cleanup.is_empty()) {
            (Err(e), _) => Some(format!("{e:#}")),
            (Ok(()), false) => Some(cleanup.join("; ")),
            _ => None,
        };
        metrics.record_iter(started.elapsed(), err);
    }
    errors.extend(vu.unwind().await);
    errors
}
