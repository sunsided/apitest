//! Functional runner: drift check, scoped setup/teardown, serial/parallel scheduling.
//!
//! ```mermaid
//! flowchart TD
//!     shared[make_shared] --> drift[check_drift]
//!     drift -->|breaking + check_drift| skip[skip all tests]
//!     drift --> suite[suite fixtures + before_all]
//!     suite --> vu[vu fixtures]
//!     vu --> sched[schedule: serial barriers, parallel tasks]
//!     sched --> td[vu unwind, after_all, suite unwind]
//!     suite -->|failed| td
//!     td --> lock[update lockfile]
//! ```

mod drift;
mod vars;

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow};
use tokio::sync::{Mutex as AsyncMutex, Semaphore};
use tokio::task::JoinSet;

use crate::context::{Ctx, Scope, Shared, guarded, guarded_value};
pub use drift::check_drift;
pub(crate) use drift::drift_failure;

use crate::contract::{Contract, DriftReport, DriftTarget, Lockfile};
use crate::report::{Outcome, StepReport, SuiteReport, TestReport};
use crate::suite::{Concurrency, HookKind, Suite, TestCase};

/// Run-wide settings.
#[derive(Debug, Clone)]
pub struct RunConfig {
    /// Prefix for relative request paths, e.g. `http://127.0.0.1:8080`.
    pub base_url: String,
    /// Named services (`hub`, `app`, `mail`, ...) → base URL. Contracts and actors
    /// bound to a service send their requests there.
    pub services: BTreeMap<String, String>,
    /// Variables visible through `cx.env(..)` and `${NAME}` (`--var`). They win
    /// over every other source; see [`RunConfig::lookup`] for the order.
    pub vars: BTreeMap<String, String>,
    /// Variables read from `.env` files ([`RunConfig::env_file`]). The process
    /// environment wins over them.
    pub env_file_vars: BTreeMap<String, String>,
    /// Defaults (`vars:` in `apitest.nova`), used when no other source has the name.
    pub var_defaults: BTreeMap<String, String>,
    /// Masks secrets and patterns in logs, errors and reports. Clones share it.
    pub redactor: crate::redact::Redactor,
    /// Timeout for a test body, unless the test sets its own (default: 30 s).
    pub test_timeout: Duration,
    /// Timeout for each fixture setup, hook and cleanup.
    pub hook_timeout: Duration,
    /// Validate every n-th contract call (1 = all). Load runs raise this to keep the client cheap.
    pub validate_every: u64,
    /// Path of `apitest.lock.nova`. `None` disables lockfile use entirely.
    pub lockfile: Option<PathBuf>,
    /// Merge this run's contract usage into the lockfile afterwards.
    pub update_lock: bool,
    /// Fail the suite before running tests if a compared spec broke the used surface.
    pub check_drift: bool,
    /// Named comparison specs (`dev`, `qa`, `prod`, `branch`, ...), each with a file
    /// or URL per contract, to diff the pins against instead of or besides the live
    /// spec. The names `live` and `all` are reserved.
    pub drift_targets: BTreeMap<String, DriftTarget>,
    /// Which targets the drift check compares against, in order: `live` (each
    /// contract's own live spec), names from [`RunConfig::drift_targets`], or `all`
    /// (live plus every named target). Empty means `["live"]`.
    pub drift_against: Vec<String>,
    /// Only run tests whose name contains one of these substrings (empty = all).
    pub only: Vec<String>,
    /// Upper bound for concurrently running `parallel` tests.
    pub max_parallel: usize,
    /// Suppress `cx.log` output.
    pub quiet: bool,
    /// Default `User-Agent` of every request. Overridable per suite
    /// ([`Suite::user_agent`]), scope (`cx.default_header`), actor and request.
    pub user_agent: String,
    /// Trace-context propagation (W3C `traceparent` by default).
    pub tracing: crate::trace::TracingConfig,
    /// How each service logs in (`Actor::login`, `Auth::Login`). Unlisted services use
    /// the default flow: operation `login`, `{username, password}`, token at `/token`.
    pub auth: BTreeMap<String, crate::auth::LoginFlow>,
    /// Named actors (`actor("hub_admin")`, `{ as: "hub_admin" }` in scripts).
    pub actors: BTreeMap<String, crate::auth::ActorConfig>,
    /// Directory for caching drift-check results across processes of one run
    /// (set automatically by the harness under nextest, where every test is a process).
    pub drift_cache: Option<PathBuf>,
}

impl Default for RunConfig {
    fn default() -> Self {
        RunConfig {
            base_url: "http://127.0.0.1:8080".into(),
            services: BTreeMap::new(),
            vars: BTreeMap::new(),
            env_file_vars: BTreeMap::new(),
            var_defaults: BTreeMap::new(),
            redactor: crate::redact::Redactor::default(),
            test_timeout: Duration::from_secs(30),
            hook_timeout: Duration::from_secs(30),
            validate_every: 1,
            lockfile: None,
            update_lock: false,
            check_drift: true,
            drift_targets: BTreeMap::new(),
            drift_against: vec![],
            only: vec![],
            max_parallel: std::thread::available_parallelism().map_or(4, std::num::NonZero::get),
            quiet: false,
            drift_cache: None,
            user_agent: default_user_agent(),
            tracing: crate::trace::TracingConfig::default(),
            auth: BTreeMap::new(),
            actors: BTreeMap::new(),
        }
    }
}

/// `apitest/<version>`.
#[must_use]
pub fn default_user_agent() -> String {
    concat!("apitest/", env!("CARGO_PKG_VERSION")).to_string()
}

impl RunConfig {
    /// Overrides the default `User-Agent` for the whole run.
    #[must_use]
    pub fn user_agent(mut self, ua: impl Into<String>) -> Self {
        self.user_agent = ua.into();
        self
    }

    /// How `service` turns username + password into a token.
    #[must_use]
    pub fn login_flow(mut self, service: impl Into<String>, flow: crate::auth::LoginFlow) -> Self {
        self.auth.insert(service.into(), flow);
        self
    }

    /// A named actor, e.g. `.actor("hub_admin", "hub", Auth::login("hub", "${HUB_ADMIN_EMAIL}", "${HUB_ADMIN_PASSWORD}"))`.
    #[must_use]
    pub fn actor(
        mut self,
        name: impl Into<String>,
        service: &str,
        auth: crate::auth::Auth,
    ) -> Self {
        self.actors.insert(
            name.into(),
            crate::auth::ActorConfig {
                service: Some(service.into()),
                auth,
                ..Default::default()
            },
        );
        self
    }

    /// Replaces the tracing configuration (e.g. `TracingConfig::disabled()`).
    #[must_use]
    pub fn tracing(mut self, t: crate::trace::TracingConfig) -> Self {
        self.tracing = t;
        self
    }

    /// Defaults for everything except the base URL of relative request paths.
    #[must_use]
    pub fn new(base_url: impl Into<String>) -> Self {
        RunConfig {
            base_url: base_url.into(),
            ..Default::default()
        }
    }

    /// Registers a named service, e.g. `.service("hub", hub_server.base_url())`.
    #[must_use]
    pub fn service(mut self, name: impl Into<String>, base_url: impl Into<String>) -> Self {
        self.services.insert(name.into(), base_url.into());
        self
    }

    /// Base URL for a service name. `None`, and any name while no services are
    /// configured at all (single-service setups), use `base_url`.
    ///
    /// # Errors
    ///
    /// Fails if services are configured but `service` names none of them.
    pub fn service_url(&self, service: Option<&str>) -> Result<&str> {
        match service {
            None => Ok(&self.base_url),
            Some(_) if self.services.is_empty() => Ok(&self.base_url),
            Some(name) => self.services.get(name).map(String::as_str).ok_or_else(|| {
                let known: Vec<&str> = self.services.keys().map(String::as_str).collect();
                anyhow!(
                    "unknown service '{name}' (configured: {})",
                    if known.is_empty() {
                        "none".into()
                    } else {
                        known.join(", ")
                    }
                )
            }),
        }
    }

    /// Base URL used for a contract: its explicit service, else a service named like
    /// the contract, else `base_url`.
    ///
    /// # Errors
    ///
    /// Fails if the contract names a service that is not configured (see
    /// [`RunConfig::service_url`]).
    pub fn contract_url(&self, c: &Contract) -> Result<&str> {
        match &c.service {
            Some(s) => self.service_url(Some(s)),
            None => Ok(self
                .services
                .get(&c.name)
                .map_or(self.base_url.as_str(), String::as_str)),
        }
    }

    /// An explicit variable, like `--var k=v`; it wins over every other source
    /// (see [`RunConfig::lookup`]).
    #[must_use]
    pub fn var(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.vars.insert(k.into(), v.into());
        self
    }

    /// Uses the lockfile at `path` for drift checks (and updates, see [`RunConfig::update_lock`]).
    #[must_use]
    pub fn lockfile(mut self, path: impl Into<PathBuf>) -> Self {
        self.lockfile = Some(path.into());
        self
    }

    /// Whether to merge this run's contract usage into the lockfile afterwards.
    #[must_use]
    pub fn update_lock(mut self, yes: bool) -> Self {
        self.update_lock = yes;
        self
    }

    /// Whether to suppress `cx.log` output.
    #[must_use]
    pub fn quiet(mut self, yes: bool) -> Self {
        self.quiet = yes;
        self
    }
}

/// Builds the state shared by every context of one run: HTTP client, effective
/// config (with the suite's tracing override applied), resolved trace parent and
/// the suite's contracts by name.
///
/// # Errors
///
/// Fails if the HTTP client cannot be built or the trace parent is malformed.
pub(crate) fn make_shared(suite: &Suite, cfg: &RunConfig) -> Result<Arc<Shared>> {
    // The User-Agent is set per request (it is overridable at several levels).
    let http = reqwest::Client::builder()
        .build()
        .context("building HTTP client")?;

    make_shared_with(suite, cfg, http)
}

/// [`make_shared`] with a ready-made HTTP client.
///
/// # Errors
///
/// Fails if a variable the configured actors use does not resolve (see
/// [`RunConfig::check_variables`]) or the trace parent is malformed.
fn make_shared_with(suite: &Suite, cfg: &RunConfig, http: reqwest::Client) -> Result<Arc<Shared>> {
    cfg.redactor.activate();
    cfg.check_variables()?;
    let mut config = cfg.clone();
    if let Some(t) = &suite.tracing {
        config.tracing = t.clone();
    }

    let trace_parent = config.tracing.resolved_parent()?;
    let contracts = suite
        .contracts
        .iter()
        .map(|c| (c.name.clone(), c.clone()))
        .collect();

    Ok(Arc::new(Shared {
        http,
        config,
        trace_parent,
        contracts,
        validate_counter: AtomicU64::default(),
    }))
}

impl Ctx {
    /// A run-scope context outside any suite: no contracts, no fixtures, nothing
    /// deferred.
    ///
    /// For one-off requests such as the CLI's `apitest call`. Requests sent
    /// through it still get the run's User-Agent, tracing, services, vars and
    /// named actors from `cfg`.
    ///
    /// Redirects are followed only within the origin (scheme, host and port) of
    /// the request, at most 10 times. A redirect to another origin is returned as
    /// the response instead: following it would send an actor's API-key or
    /// configured headers, which the HTTP client only strips for `Authorization`
    /// and cookies, to that host. That includes an `http` to `https` upgrade of
    /// the same host; configure the `https` URL instead.
    ///
    /// # Errors
    ///
    /// Fails if the HTTP client cannot be built or the configured trace parent is
    /// malformed.
    pub fn standalone(cfg: &RunConfig) -> Result<Ctx> {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::custom(same_origin_redirect))
            .build()
            .context("building HTTP client")?;

        Ok(Ctx::root(make_shared_with(
            &Suite::new("standalone"),
            cfg,
            http,
        )?))
    }
}

/// Redirect policy of [`Ctx::standalone`]; the decision is [`redirect_decision`].
fn same_origin_redirect(attempt: reqwest::redirect::Attempt<'_>) -> reqwest::redirect::Action {
    match redirect_decision(attempt.previous(), attempt.url()) {
        Ok(true) => attempt.follow(),
        Ok(false) => attempt.stop(),
        Err(e) => attempt.error(e),
    }
}

/// Whether to follow a redirect to `next`: `Ok(true)` while it stays on the
/// origin (scheme, host and port) of the first request and at most 10 were
/// followed, `Ok(false)` to return the 3xx instead.
///
/// `previous` is every URL requested so far, the first request's URL first
/// (reqwest pushes the current URL before asking the policy). An `http` to
/// `https` upgrade on the same host is a different origin, so it is not
/// followed: reqwest drops `Authorization` on a scheme change but keeps an
/// API-key header, so a followed upgrade would authenticate some actors and
/// not others.
///
/// # Errors
///
/// Fails after 10 redirects, and when `previous` is empty, so a change in
/// reqwest can't silently disable redirects or the origin check.
fn redirect_decision(previous: &[reqwest::Url], next: &reqwest::Url) -> Result<bool, &'static str> {
    let Some(first) = previous.first() else {
        return Err("redirect without the URL of the original request");
    };

    if first.origin() != next.origin() {
        return Ok(false);
    }

    if previous.len() > 10 {
        return Err("too many redirects");
    }

    Ok(true)
}

/// Runs fixtures of `scope` in declaration order, registering their teardowns.
///
/// # Errors
///
/// Fails at the first fixture whose setup fails or times out, or whose value cannot
/// be stored in the scope. Teardowns of the fixtures set up before stay registered.
pub(crate) async fn setup_fixtures(suite: &Suite, cx: &Ctx, scope: Scope) -> Result<()> {
    let timeout = cx.config().hook_timeout;
    for f in suite.fixtures_for(scope) {
        let value = guarded_value(cx, (f.setup)(cx.clone()), timeout)
            .await
            .with_context(|| format!("fixture '{}' ({scope}) setup", f.name))?;
        cx.set(f.name.clone(), value)?;
        if let Some(td) = f.teardown.clone() {
            cx.defer_boxed(
                format!("fixture '{}' teardown", f.name),
                Box::new(move |cx| td(cx)),
            );
        }
    }

    Ok(())
}

/// Runs the suite's hooks of `kind` and returns their errors as `Kind: message`.
///
/// Setup hooks (`BeforeAll`, `BeforeEach`) stop at the first failure; teardown
/// hooks all run.
async fn run_hooks(suite: &Suite, cx: &Ctx, kind: HookKind) -> Vec<String> {
    let mut errors = vec![];
    for h in suite.hooks(kind) {
        if let Err(e) = guarded(cx, h(cx.clone()), cx.config().hook_timeout).await {
            errors.push(format!("{kind:?}: {e:#}"));
            if matches!(kind, HookKind::BeforeAll | HookKind::BeforeEach) {
                break; // a failed setup hook stops the remaining setup hooks
            }
        }
    }

    errors
}

/// Executes one test inside a fresh test scope. Returns the body result, cleanup
/// errors and the recorded steps.
///
/// The test's own teardown runs only if its fixtures succeeded and its setup was
/// attempted (or it has none); `after_each` hooks run whenever fixtures succeeded.
pub(crate) async fn run_test(
    suite: &Suite,
    parent: &Ctx,
    test: &TestCase,
) -> (Result<()>, Vec<String>, Vec<StepReport>) {
    let cx = parent.child(Scope::Test, test.name.clone());
    let cfg = cx.config().clone();

    let mut result = setup_fixtures(suite, &cx, Scope::Test).await;
    let fixtures_ok = result.is_ok();
    if fixtures_ok {
        let errs = run_hooks(suite, &cx, HookKind::BeforeEach).await;
        if !errs.is_empty() {
            result = Err(anyhow!(errs.join("; ")));
        }
    }

    let mut setup_attempted = false;
    if result.is_ok()
        && let Some(setup) = &test.opts.setup
    {
        setup_attempted = true;
        result = guarded(&cx, setup(cx.clone()), cfg.hook_timeout)
            .await
            .context("test setup");
    }

    if result.is_ok() {
        let timeout = test.opts.timeout.unwrap_or(cfg.test_timeout);
        result = guarded(&cx, (test.body)(cx.clone()), timeout).await;
    }

    let mut teardown_errors = vec![];
    if fixtures_ok {
        let run_teardown = setup_attempted || test.opts.setup.is_none();
        teardown_errors.extend(test_cleanup(suite, &cx, test, run_teardown, &cfg).await);
    }

    teardown_errors.extend(cx.unwind().await);
    (result, teardown_errors, cx.take_steps())
}

/// Runs the test's own teardown (if `run_teardown`), then the `after_each` hooks.
async fn test_cleanup(
    suite: &Suite,
    cx: &Ctx,
    test: &TestCase,
    run_teardown: bool,
    cfg: &RunConfig,
) -> Vec<String> {
    let mut errors = vec![];
    if run_teardown
        && let Some(td) = &test.opts.teardown
        && let Err(e) = guarded(cx, td(cx.clone()), cfg.hook_timeout).await
    {
        errors.push(format!("test teardown: {e:#}"));
    }

    errors.extend(run_hooks(suite, cx, HookKind::AfterEach).await);
    errors
}

/// Runs the test at `index` in the VU scope `vu` and turns the result into a report.
async fn run_one(suite: Arc<Suite>, vu: Ctx, index: usize) -> TestReport {
    let test = &suite.tests[index];
    let started = Instant::now();
    let (result, teardown_errors, steps) = run_test(&suite, &vu, test).await;

    TestReport {
        name: test.name.clone(),
        id: suite.test_id(test),
        steps,
        outcome: match result {
            Ok(()) => Outcome::Passed,
            Err(e) => Outcome::failed(&e),
        },
        duration_ms: elapsed_ms(started),
        teardown_errors,
    }
}

/// Milliseconds since `started`.
fn elapsed_ms(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0
}

/// Runs a suite functionally and returns its report, with secrets and
/// redaction patterns ([`RunConfig::redactor`]) masked.
///
/// Never fails: setup problems skip the selected tests and land in
/// [`SuiteReport::setup_error`]; teardown problems in [`SuiteReport::teardown_errors`].
/// Suite teardown runs whenever the suite scope was created. A variable the
/// configured actors use that does not resolve is a setup problem: no test runs.
pub async fn run_suite(suite: &Suite, cfg: &RunConfig) -> SuiteReport {
    let mut report = run_suite_unmasked(suite, cfg).await;
    report.redact(&cfg.redactor);
    report
}

/// [`run_suite`] before masking.
async fn run_suite_unmasked(suite: &Suite, cfg: &RunConfig) -> SuiteReport {
    let started = Instant::now();
    let suite = Arc::new(suite.clone());
    let selected = select_tests(&suite, cfg);
    let mut report = SuiteReport {
        suite: suite.name.clone(),
        tests: vec![],
        setup_error: None,
        setup_chain: vec![],
        teardown_errors: vec![],
        drift: vec![],
        duration_ms: 0.0,
    };

    let shared = match make_shared(&suite, cfg) {
        Ok(s) => s,
        Err(e) => {
            report.set_setup_error(&e);
            report.tests = skipped_reports(&suite, &selected, "runner setup failed");
            return report;
        }
    };

    if let Err((error, reason)) = drift_gate(&suite, cfg, &mut report.drift).await {
        report.set_setup_error(&error);
        report.tests = skipped_reports(&suite, &selected, reason);
        report.duration_ms = elapsed_ms(started);
        return report;
    }

    let root = Ctx::root(shared);
    let scx = root.child(Scope::Suite, suite.name.clone());
    for (n, v) in &suite.headers {
        scx.default_header(n.clone(), v.clone());
    }

    match suite_setup(&suite, &scx).await {
        Err(e) => {
            report.set_setup_error(&e);
            report.tests = skipped_reports(&suite, &selected, "suite setup failed");
        }
        Ok(()) => run_tests_in_vu(&suite, &scx, &selected, cfg, &mut report).await,
    }

    // Suite teardown (always).
    report
        .teardown_errors
        .extend(run_hooks(&suite, &scx, HookKind::AfterAll).await);
    report.teardown_errors.extend(scx.unwind().await);

    if cfg.update_lock
        && let Some(path) = &cfg.lockfile
        && let Err(e) = update_lockfile(path, &suite.contracts)
    {
        report
            .teardown_errors
            .push(format!("updating {}: {e:#}", path.display()));
    }

    report.duration_ms = elapsed_ms(started);
    report
}

/// Indices of the tests matching [`RunConfig::only`] (all tests if it is empty).
fn select_tests(suite: &Suite, cfg: &RunConfig) -> Vec<usize> {
    (0..suite.tests.len())
        .filter(|&i| {
            cfg.only.is_empty()
                || cfg
                    .only
                    .iter()
                    .any(|f| suite.tests[i].name.contains(f.as_str()))
        })
        .collect()
}

/// `Skipped` reports with `reason` for the selected tests.
fn skipped_reports(suite: &Suite, selected: &[usize], reason: &str) -> Vec<TestReport> {
    selected
        .iter()
        .map(|&i| TestReport {
            name: suite.tests[i].name.clone(),
            id: suite.test_id(&suite.tests[i]),
            steps: vec![],
            outcome: Outcome::Skipped {
                reason: reason.into(),
            },
            duration_ms: 0.0,
            teardown_errors: vec![],
        })
        .collect()
}

/// Checks contract drift, storing the results in `drift`.
///
/// Returns the setup error and the skip reason if the check failed or, with
/// [`RunConfig::check_drift`], a contract drifted incompatibly.
async fn drift_gate(
    suite: &Suite,
    cfg: &RunConfig,
    drift: &mut Vec<DriftReport>,
) -> Result<(), (anyhow::Error, &'static str)> {
    *drift = check_drift(&suite.contracts, cfg)
        .await
        .map_err(|e| (e.context("contract check"), "contract check failed"))?;
    if cfg.check_drift
        && let Some(error) = drift_failure(drift)
    {
        return Err((anyhow!(error), "contract drift"));
    }

    Ok(())
}

/// Runs suite fixtures, then the `before_all` hooks; returns the joined errors on failure.
async fn suite_setup(suite: &Suite, scx: &Ctx) -> Result<()> {
    setup_fixtures(suite, scx, Scope::Suite).await?;
    let errs = run_hooks(suite, scx, HookKind::BeforeAll).await;
    if !errs.is_empty() {
        return Err(anyhow!(errs.join("; ")));
    }

    Ok(())
}

/// Sets up the single functional VU scope, runs the selected tests in it and
/// unwinds it again, recording everything in `report`.
async fn run_tests_in_vu(
    suite: &Arc<Suite>,
    scx: &Ctx,
    selected: &[usize],
    cfg: &RunConfig,
    report: &mut SuiteReport,
) {
    let vu = scx.child_vu(0);
    match setup_fixtures(suite, &vu, Scope::Vu).await {
        Err(e) => {
            report.set_setup_error(&e);
            report.tests = skipped_reports(suite, selected, "vu setup failed");
        }
        Ok(()) => report.tests = schedule(suite, &vu, selected, cfg.max_parallel).await,
    }

    report.teardown_errors.extend(vu.unwind().await);
}

/// Merges the contracts' pins and used surface into the lockfile at `path`.
///
/// Spec paths are recorded relative to the lockfile's directory. Does nothing
/// without contracts.
///
/// # Errors
///
/// Fails if the existing lockfile cannot be read or parsed, or the result cannot
/// be written.
pub(crate) fn update_lockfile(path: &Path, contracts: &[Arc<Contract>]) -> Result<()> {
    if contracts.is_empty() {
        return Ok(());
    }

    let mut lock = Lockfile::load(path)?.unwrap_or_default();
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    for c in contracts {
        lock.record(c, dir);
    }

    lock.save(path)
}

/// Waits for all running parallel tests and stores their reports by test index.
///
/// Tasks that panicked are dropped from the results.
async fn drain(
    running: &mut JoinSet<(usize, TestReport)>,
    results: &mut BTreeMap<usize, TestReport>,
) {
    while let Some(r) = running.join_next().await {
        if let Ok((i, rep)) = r {
            results.insert(i, rep);
        }
    }
}

/// Serial tests act as barriers; parallel tests overlap, bounded by a semaphore and named resource locks.
///
/// Reports come back in selection order.
async fn schedule(
    suite: &Arc<Suite>,
    vu: &Ctx,
    selected: &[usize],
    max_parallel: usize,
) -> Vec<TestReport> {
    let sem = Arc::new(Semaphore::new(max_parallel.max(1)));
    let mut locks: HashMap<String, Arc<AsyncMutex<()>>> = HashMap::new();
    let mut results: BTreeMap<usize, TestReport> = BTreeMap::new();
    let mut running: JoinSet<(usize, TestReport)> = JoinSet::new();

    for &i in selected {
        let test = &suite.tests[i];
        match test.opts.concurrency {
            Concurrency::Serial => {
                drain(&mut running, &mut results).await;
                results.insert(i, run_one(suite.clone(), vu.clone(), i).await);
            }
            Concurrency::Parallel => {
                let res_locks = resource_locks(&mut locks, &test.opts.resources);
                let (sem, suite, vu) = (sem.clone(), suite.clone(), vu.clone());
                running.spawn(async move {
                    // Resources first (sorted → no lock-order deadlocks), then a concurrency slot.
                    let mut guards = vec![];
                    for l in res_locks {
                        guards.push(l.lock_owned().await);
                    }

                    let _permit = sem.acquire_owned().await.expect("semaphore closed");
                    (i, run_one(suite, vu, i).await)
                });
            }
        }
    }

    drain(&mut running, &mut results).await;
    results.into_values().collect()
}

/// The locks of `resources`, sorted and deduplicated, created in `locks` on first use.
fn resource_locks(
    locks: &mut HashMap<String, Arc<AsyncMutex<()>>>,
    resources: &[String],
) -> Vec<Arc<AsyncMutex<()>>> {
    let mut res = resources.to_vec();
    res.sort();
    res.dedup();

    res.iter()
        .map(|r| {
            locks
                .entry(r.clone())
                .or_insert_with(|| Arc::new(AsyncMutex::new(())))
                .clone()
        })
        .collect()
}

/// Runs several suites one after another.
pub async fn run_suites(suites: &[Suite], cfg: &RunConfig) -> Vec<SuiteReport> {
    let mut out = vec![];
    for s in suites {
        out.push(run_suite(s, cfg).await);
    }

    out
}

#[cfg(test)]
mod tests {
    use reqwest::Url;

    use super::redirect_decision;

    /// Parses a test URL.
    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn redirects_stay_on_the_first_origin() {
        let first = [url("http://hub.test/a")];
        assert_eq!(
            redirect_decision(&first, &url("http://hub.test/b")),
            Ok(true)
        );
        assert_eq!(
            redirect_decision(&first, &url("http://hub.test:80/b")),
            Ok(true)
        );
        assert_eq!(
            redirect_decision(&first, &url("http://hub.test:81/b")),
            Ok(false)
        );
        assert_eq!(
            redirect_decision(&first, &url("http://other.test/b")),
            Ok(false)
        );
        assert_eq!(
            redirect_decision(&first, &url("https://hub.test/b")),
            Ok(false),
            "no upgrade"
        );

        let https = [url("https://hub.test/a")];
        assert_eq!(
            redirect_decision(&https, &url("http://hub.test/b")),
            Ok(false),
            "no downgrade"
        );
    }

    #[test]
    fn the_origin_is_the_first_request_not_the_last_hop() {
        // Were the last hop the reference, a second hop could leave the origin.
        let chain = [url("http://hub.test/a"), url("http://hub.test/b")];
        assert_eq!(
            redirect_decision(&chain, &url("http://hub.test/c")),
            Ok(true)
        );
        assert_eq!(
            redirect_decision(&chain, &url("http://other.test/c")),
            Ok(false)
        );
    }

    #[test]
    fn ten_redirects_are_followed_not_eleven() {
        let mut chain = vec![url("http://hub.test/0")];
        for i in 1..=10 {
            assert_eq!(
                redirect_decision(&chain, &url(&format!("http://hub.test/{i}"))),
                Ok(true)
            );
            chain.push(url(&format!("http://hub.test/{i}")));
        }

        assert_eq!(
            redirect_decision(&chain, &url("http://hub.test/11")),
            Err("too many redirects")
        );
    }

    #[test]
    fn an_empty_history_is_an_error_not_a_silent_stop() {
        assert!(redirect_decision(&[], &url("http://hub.test/")).is_err());
    }
}
