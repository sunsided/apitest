//! Functional runner: drift check, scoped setup/teardown, serial/parallel scheduling.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow};
use tokio::sync::{Mutex as AsyncMutex, Semaphore};
use tokio::task::JoinSet;

use crate::context::{Ctx, Scope, Shared, guarded, guarded_value};
use crate::contract::{Contract, DriftReport, Finding, LiveSpec, Lockfile, Severity, Spec};
use crate::report::{Outcome, SuiteReport, TestReport};
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
            tracing: Default::default(),
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
    pub fn user_agent(mut self, ua: impl Into<String>) -> Self {
        self.user_agent = ua.into();
        self
    }

    /// How `service` turns username + password into a token.
    pub fn login_flow(mut self, service: impl Into<String>, flow: crate::auth::LoginFlow) -> Self {
        self.auth.insert(service.into(), flow);
        self
    }

    /// A named actor, e.g. `.actor("hub_admin", "hub", Auth::login("hub", "${HUB_ADMIN_EMAIL}", "${HUB_ADMIN_PASSWORD}"))`.
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

    pub fn new(base_url: impl Into<String>) -> Self {
        RunConfig {
            base_url: base_url.into(),
            ..Default::default()
        }
    }

    /// Registers a named service, e.g. `.service("hub", hub_server.base_url())`.
    pub fn service(mut self, name: impl Into<String>, base_url: impl Into<String>) -> Self {
        self.services.insert(name.into(), base_url.into());
        self
    }

    /// Base URL for a service name. `None`, and any name while no services are
    /// configured at all (single-service setups), use `base_url`.
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
    pub fn contract_url(&self, c: &Contract) -> Result<&str> {
        match &c.service {
            Some(s) => self.service_url(Some(s)),
            None => Ok(self
                .services
                .get(&c.name)
                .map(String::as_str)
                .unwrap_or(&self.base_url)),
        }
    }

    pub fn var(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.vars.insert(k.into(), v.into());
        self
    }

    pub fn lockfile(mut self, path: impl Into<PathBuf>) -> Self {
        self.lockfile = Some(path.into());
        self
    }

    #[must_use]
    pub fn update_lock(mut self, yes: bool) -> Self {
        self.update_lock = yes;
        self
    }

    #[must_use]
    pub fn quiet(mut self, yes: bool) -> Self {
        self.quiet = yes;
        self
    }
}

pub(crate) fn make_shared(suite: &Suite, cfg: &RunConfig) -> Result<Arc<Shared>> {
    // The User-Agent is set per request (it is overridable at several levels).
    let http = reqwest::Client::builder()
        .build()
        .context("building HTTP client")?;
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
        validate_counter: Default::default(),
    }))
}

/// Fetches live specs and diffs them against the pinned ones.
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
        let mut notes = vec![];
        let used = match locked {
            Some(l) if l.sha256 == c.pinned.sha256 => Some(l.used.as_slice()),
            Some(l) => {
                notes.push(Finding {
                    severity: Severity::Info,
                    operation: "-".into(),
                    location: "lockfile".into(),
                    message: format!(
                        "pinned spec changed since the lockfile was written (lock: v{}, now: v{}); checking all operations",
                        l.version, c.pinned.version
                    ),
                });
                None
            }
            None => None,
        };

        let cache_file = cfg.drift_cache.as_ref().map(|dir| {
            dir.join(format!(
                "drift-{}.json",
                drift_cache_key(c, live, used, cfg)
            ))
        });
        if let Some(cached) = cache_file
            .as_ref()
            .and_then(|f| std::fs::read(f).ok())
            .and_then(|b| serde_json::from_slice::<DriftReport>(&b).ok())
        {
            out.push(cached);
            continue;
        }

        let (spec, source) = match live {
            LiveSpec::Document(doc) => (
                Spec::from_value(doc.clone(), String::new())?,
                "in-process document".to_string(),
            ),
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
                let ext = if url.ends_with(".yaml") || url.ends_with(".yml") {
                    Some("yaml")
                } else {
                    None
                };
                (Spec::from_bytes(&bytes, ext)?, url)
            }
        };
        let mut report = c.diff(&spec, used, &source);
        report.findings.splice(0..0, notes);
        if let Some(file) = &cache_file {
            // Write-then-rename so concurrent processes never read a partial file.
            let _ = std::fs::create_dir_all(file.parent().unwrap_or(std::path::Path::new(".")));
            let tmp = file.with_extension(format!("tmp{}", std::process::id()));
            if std::fs::write(&tmp, serde_json::to_vec(&report)?).is_ok() {
                let _ = std::fs::rename(&tmp, file);
            }
        }
        out.push(report);
    }
    Ok(out)
}

fn live_url(u: &str, base: &str) -> String {
    if u.starts_with("http://") || u.starts_with("https://") {
        u.to_string()
    } else {
        format!("{}{}", base.trim_end_matches('/'), u)
    }
}

/// Everything that influences a drift result: contract, pin, live source, used surface, mode.
fn drift_cache_key(
    c: &Contract,
    live: &LiveSpec,
    used: Option<&[crate::contract::OpUsage]>,
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
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Runs fixtures of `scope` in declaration order, registering their teardowns.
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

/// Executes one test inside a fresh test scope. Returns the body result and cleanup errors.
pub(crate) async fn run_test(
    suite: &Suite,
    parent: &Ctx,
    test: &TestCase,
) -> (Result<()>, Vec<String>, Vec<crate::report::StepReport>) {
    let cx = parent.child(Scope::Test, test.name.clone());
    let cfg = cx.config().clone();
    let mut teardown_errors = vec![];

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
    if (setup_attempted || test.opts.setup.is_none())
        && let Some(td) = &test.opts.teardown
        && fixtures_ok
        && let Err(e) = guarded(&cx, td(cx.clone()), cfg.hook_timeout).await
    {
        teardown_errors.push(format!("test teardown: {e:#}"));
    }
    if fixtures_ok {
        teardown_errors.extend(run_hooks(suite, &cx, HookKind::AfterEach).await);
    }
    teardown_errors.extend(cx.unwind().await);
    (result, teardown_errors, cx.take_steps())
}

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
            Err(e) => Outcome::Failed {
                message: format!("{e:#}"),
            },
        },
        duration_ms: started.elapsed().as_secs_f64() * 1000.0,
        teardown_errors,
    }
}

/// Runs a suite functionally and returns its report.
pub async fn run_suite(suite: &Suite, cfg: &RunConfig) -> SuiteReport {
    let started = Instant::now();
    let suite = Arc::new(suite.clone());
    let selected: Vec<usize> = (0..suite.tests.len())
        .filter(|&i| {
            cfg.only.is_empty()
                || cfg
                    .only
                    .iter()
                    .any(|f| suite.tests[i].name.contains(f.as_str()))
        })
        .collect();
    let mut report = SuiteReport {
        suite: suite.name.clone(),
        tests: vec![],
        setup_error: None,
        teardown_errors: vec![],
        drift: vec![],
        duration_ms: 0.0,
    };
    let skip_all = |report: &mut SuiteReport, reason: &str| {
        report.tests = selected
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
            .collect();
    };

    let shared = match make_shared(&suite, cfg) {
        Ok(s) => s,
        Err(e) => {
            report.setup_error = Some(format!("{e:#}"));
            skip_all(&mut report, "runner setup failed");
            return report;
        }
    };

    // 1. Contract drift.
    match check_drift(&suite.contracts, cfg).await {
        Ok(d) => report.drift = d,
        Err(e) => {
            report.setup_error = Some(format!("contract check: {e:#}"));
            skip_all(&mut report, "contract check failed");
            report.duration_ms = started.elapsed().as_secs_f64() * 1000.0;
            return report;
        }
    }
    if cfg.check_drift && report.drift.iter().any(DriftReport::is_breaking) {
        let names: Vec<_> = report
            .drift
            .iter()
            .filter(|d| d.is_breaking())
            .map(|d| d.contract.clone())
            .collect();
        report.setup_error = Some(format!(
            "live API is incompatible with pinned contract(s): {}",
            names.join(", ")
        ));
        skip_all(&mut report, "contract drift");
        report.duration_ms = started.elapsed().as_secs_f64() * 1000.0;
        return report;
    }

    // 2. Suite setup.
    let root = Ctx::root(shared);
    let scx = root.child(Scope::Suite, suite.name.clone());
    for (n, v) in &suite.headers {
        scx.default_header(n.clone(), v.clone());
    }
    let mut setup = setup_fixtures(&suite, &scx, Scope::Suite)
        .await
        .map_err(|e| format!("{e:#}"));
    if setup.is_ok() {
        let errs = run_hooks(&suite, &scx, HookKind::BeforeAll).await;
        if !errs.is_empty() {
            setup = Err(errs.join("; "));
        }
    }

    // 3. Tests.
    if let Err(e) = setup {
        report.setup_error = Some(e);
        skip_all(&mut report, "suite setup failed");
    } else {
        let vu = scx.child_vu(0);
        match setup_fixtures(&suite, &vu, Scope::Vu).await {
            Err(e) => {
                report.setup_error = Some(format!("{e:#}"));
                skip_all(&mut report, "vu setup failed");
            }
            Ok(()) => report.tests = schedule(&suite, &vu, &selected, cfg.max_parallel).await,
        }
        report.teardown_errors.extend(vu.unwind().await);
    }

    // 4. Suite teardown (always).
    report
        .teardown_errors
        .extend(run_hooks(&suite, &scx, HookKind::AfterAll).await);
    report.teardown_errors.extend(scx.unwind().await);

    // 5. Lockfile.
    if cfg.update_lock
        && let Some(path) = &cfg.lockfile
        && let Err(e) = update_lockfile(path, &suite.contracts)
    {
        report
            .teardown_errors
            .push(format!("updating {}: {e:#}", path.display()));
    }

    report.duration_ms = started.elapsed().as_secs_f64() * 1000.0;
    report
}

pub(crate) fn update_lockfile(path: &std::path::Path, contracts: &[Arc<Contract>]) -> Result<()> {
    if contracts.is_empty() {
        return Ok(());
    }
    let mut lock = Lockfile::load(path)?.unwrap_or_default();
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(std::path::Path::new("."));
    for c in contracts {
        lock.record(c, dir);
    }
    lock.save(path)
}

/// Serial tests act as barriers; parallel tests overlap, bounded by a semaphore and named resource locks.
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

    for &i in selected {
        let test = &suite.tests[i];
        match test.opts.concurrency {
            Concurrency::Serial => {
                drain(&mut running, &mut results).await;
                results.insert(i, run_one(suite.clone(), vu.clone(), i).await);
            }
            Concurrency::Parallel => {
                let mut res = test.opts.resources.clone();
                res.sort();
                res.dedup();
                let res_locks: Vec<_> = res
                    .iter()
                    .map(|r| {
                        locks
                            .entry(r.clone())
                            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
                            .clone()
                    })
                    .collect();
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

/// Runs several suites one after another.
pub async fn run_suites(suites: &[Suite], cfg: &RunConfig) -> Vec<SuiteReport> {
    let mut out = vec![];
    for s in suites {
        out.push(run_suite(s, cfg).await);
    }
    out
}
