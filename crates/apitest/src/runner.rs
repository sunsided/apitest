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

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow};
use tokio::sync::{Mutex as AsyncMutex, Semaphore};
use tokio::task::JoinSet;

use crate::context::{Ctx, Scope, Shared, guarded, guarded_value};
use crate::contract::{
    Contract, DriftReport, Finding, LiveSpec, LockedContract, Lockfile, OpUsage, Severity, Spec,
};
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
    /// Variables visible through `cx.env(..)` (take precedence over the process environment).
    pub vars: BTreeMap<String, String>,
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
    /// Fail the suite before running tests if the live spec broke the used surface.
    pub check_drift: bool,
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
            test_timeout: Duration::from_secs(30),
            hook_timeout: Duration::from_secs(30),
            validate_every: 1,
            lockfile: None,
            update_lock: false,
            check_drift: true,
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

    /// Sets a variable visible through `cx.env(..)`, shadowing the process environment.
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
/// Fails if the trace parent is malformed.
fn make_shared_with(suite: &Suite, cfg: &RunConfig, http: reqwest::Client) -> Result<Arc<Shared>> {
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
    /// and cookies, to that host.
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

/// Redirect policy of [`Ctx::standalone`]: follow up to 10 redirects that stay
/// on the origin of the first request, stop at any other.
fn same_origin_redirect(attempt: reqwest::redirect::Attempt<'_>) -> reqwest::redirect::Action {
    let same_origin = attempt
        .previous()
        .first()
        .is_some_and(|first| first.origin() == attempt.url().origin());
    if !same_origin {
        return attempt.stop();
    }

    if attempt.previous().len() > 10 {
        return attempt.error("too many redirects");
    }

    attempt.follow()
}

/// Fetches live specs and diffs them against the pinned ones.
///
/// Contracts without a live spec are skipped. With a lockfile whose pin matches,
/// only the recorded used surface is checked; otherwise every operation. With
/// [`RunConfig::drift_cache`] set, results are read from and written to that
/// directory so concurrent processes of one run diff each contract only once.
///
/// # Errors
///
/// Fails if the lockfile cannot be read or parsed, a contract's service is unknown, the live
/// spec cannot be fetched or parsed, or a result cannot be serialized for the cache.
pub async fn check_drift(contracts: &[Arc<Contract>], cfg: &RunConfig) -> Result<Vec<DriftReport>> {
    let lock = match &cfg.lockfile {
        Some(p) => Lockfile::load(p)?,
        None => None,
    };
    let http = reqwest::Client::new();

    let mut out = vec![];
    for c in contracts {
        let Some(live) = &c.live_spec else { continue };
        let locked = lock.as_ref().and_then(|l| l.contract.get(&c.name));
        out.push(contract_drift(c, live, locked, cfg, &http).await?);
    }

    Ok(out)
}

/// Drift report of one contract, from the cache if possible.
async fn contract_drift(
    c: &Contract,
    live: &LiveSpec,
    locked: Option<&LockedContract>,
    cfg: &RunConfig,
    http: &reqwest::Client,
) -> Result<DriftReport> {
    let (used, notes) = used_surface(c, locked);
    let cache_file = cfg.drift_cache.as_ref().map(|dir| {
        dir.join(format!(
            "drift-{}.json",
            drift_cache_key(c, live, used, cfg)
        ))
    });
    if let Some(cached) = cache_file.as_deref().and_then(read_cached_drift) {
        return Ok(cached);
    }

    let (spec, source) = fetch_live_spec(c, live, cfg, http).await?;
    let mut report = c.diff(&spec, used, &source);
    report.findings.splice(0..0, notes);
    if let Some(file) = &cache_file {
        write_cached_drift(file, &report)?;
    }

    Ok(report)
}

/// The used surface to check, from the lockfile if its pin matches the contract's.
///
/// A stale lockfile entry yields `None` (check everything) plus an info finding
/// explaining why.
fn used_surface<'a>(
    c: &Contract,
    locked: Option<&'a LockedContract>,
) -> (Option<&'a [OpUsage]>, Vec<Finding>) {
    match locked {
        Some(l) if l.sha256 == c.pinned.sha256 => (Some(l.used.as_slice()), vec![]),
        Some(l) => {
            let note = Finding {
                severity: Severity::Info,
                operation: "-".into(),
                location: "lockfile".into(),
                message: format!(
                    "pinned spec changed since the lockfile was written (lock: v{}, now: v{}); checking all operations",
                    l.version, c.pinned.version
                ),
            };
            (None, vec![note])
        }
        None => (None, vec![]),
    }
}

/// A cached drift report, if `file` exists and parses.
fn read_cached_drift(file: &Path) -> Option<DriftReport> {
    let bytes = std::fs::read(file).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Caches a drift report. I/O failures are ignored: the cache is an optimization.
///
/// # Errors
///
/// Fails only if the report cannot be serialized.
fn write_cached_drift(file: &Path, report: &DriftReport) -> Result<()> {
    // Write-then-rename so concurrent processes never read a partial file.
    let _ = std::fs::create_dir_all(file.parent().unwrap_or(Path::new(".")));
    let tmp = file.with_extension(format!("tmp{}", std::process::id()));
    if std::fs::write(&tmp, serde_json::to_vec(report)?).is_ok() {
        let _ = std::fs::rename(&tmp, file);
    }

    Ok(())
}

/// Loads a contract's live spec and describes where it came from.
///
/// # Errors
///
/// Fails if the contract's service is unknown, the request fails or returns an
/// error status, or the document is not a valid spec.
async fn fetch_live_spec(
    c: &Contract,
    live: &LiveSpec,
    cfg: &RunConfig,
    http: &reqwest::Client,
) -> Result<(Spec, String)> {
    match live {
        LiveSpec::Document(doc) => Ok((
            Spec::from_value(doc, String::new())?,
            "in-process document".to_string(),
        )),
        LiveSpec::Url(u) => {
            let url = live_url(u, cfg.contract_url(c)?);
            let resp = http
                .get(&url)
                .send()
                .await
                .and_then(reqwest::Response::error_for_status)
                .with_context(|| {
                    format!("fetching live spec of contract '{}' from {url}", c.name)
                })?;
            let bytes = resp.bytes().await?;
            let ext = is_yaml_url(&url).then_some("yaml");
            Ok((Spec::from_bytes(&bytes, ext)?, url))
        }
    }
}

/// `true` if `url` ends in `.yaml` or `.yml` (case-sensitive, like the server's path).
fn is_yaml_url(url: &str) -> bool {
    url.rsplit_once('.')
        .is_some_and(|(_, ext)| ext == "yaml" || ext == "yml")
}

/// Absolute URL of a live spec: `u` itself if absolute, else `u` appended to `base`.
fn live_url(u: &str, base: &str) -> String {
    if u.starts_with("http://") || u.starts_with("https://") {
        u.to_string()
    } else {
        format!("{}{}", base.trim_end_matches('/'), u)
    }
}

/// Everything that influences a drift result: contract, pin, live source, used surface, mode.
///
/// Returns the first 12 bytes of a SHA-256 over those inputs as 24 hex characters.
fn drift_cache_key(
    c: &Contract,
    live: &LiveSpec,
    used: Option<&[OpUsage]>,
    cfg: &RunConfig,
) -> String {
    use sha2::{Digest, Sha256};

    let mut h = Sha256::new();
    h.update(c.name.as_bytes());
    h.update(c.pinned.sha256.as_bytes());
    h.update(format!("{:?}", c.mode).as_bytes());
    match live {
        LiveSpec::Url(u) => {
            h.update(live_url(u, cfg.contract_url(c).unwrap_or(&cfg.base_url)).as_bytes());
        }
        LiveSpec::Document(d) => h.update(d.to_string().as_bytes()),
    }

    h.update(serde_json::to_vec(&used).unwrap_or_default());
    h.finalize()
        .iter()
        .take(12)
        .fold(String::with_capacity(24), |mut key, b| {
            let _ = write!(key, "{b:02x}");
            key
        })
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

/// Runs a suite functionally and returns its report.
///
/// Never fails: setup problems skip the selected tests and land in
/// [`SuiteReport::setup_error`]; teardown problems in [`SuiteReport::teardown_errors`].
/// Suite teardown runs whenever the suite scope was created.
pub async fn run_suite(suite: &Suite, cfg: &RunConfig) -> SuiteReport {
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
    if cfg.check_drift && drift.iter().any(DriftReport::is_breaking) {
        let names: Vec<_> = drift
            .iter()
            .filter(|d| d.is_breaking())
            .map(|d| d.contract.clone())
            .collect();
        let error = anyhow!(
            "live API is incompatible with pinned contract(s): {}",
            names.join(", ")
        );
        return Err((error, "contract drift"));
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
