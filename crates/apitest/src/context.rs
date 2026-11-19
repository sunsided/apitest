//! Scope chain, fixture values, deferred teardown and the test-facing [`Ctx`].

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::str::FromStr;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::contract::Contract;
use crate::runner::RunConfig;
use crate::suite::BoxFut;

/// Lifetime of a fixture value or a deferred cleanup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    /// The whole run (all suites).
    Run,
    /// One suite. Set up once, shared by all tests and all virtual users.
    Suite,
    /// One virtual user. In functional runs there is exactly one VU.
    Vu,
    /// One test execution (one iteration under load).
    Test,
}

impl FromStr for Scope {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "run" => Scope::Run,
            "suite" => Scope::Suite,
            "vu" => Scope::Vu,
            "test" | "iteration" => Scope::Test,
            other => bail!("unknown scope '{other}' (expected suite, vu or test)"),
        })
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Scope::Run => "run",
            Scope::Suite => "suite",
            Scope::Vu => "vu",
            Scope::Test => "test",
        })
    }
}

/// A cleanup action, run when its scope ends.
pub type DeferFn = Box<dyn FnOnce(Ctx) -> BoxFut<()> + Send>;

struct Deferred {
    label: String,
    f: DeferFn,
}

pub(crate) struct Frame {
    scope: Scope,
    name: String,
    vu: Option<usize>,
    parent: Option<Arc<Frame>>,
    values: Mutex<HashMap<String, Value>>,
    headers: Mutex<Vec<(String, String)>>,
    teardown: Mutex<Vec<Deferred>>,
    steps: Mutex<Vec<crate::report::StepReport>>,
    trace: Mutex<Option<crate::trace::TraceContext>>,
    auth: Mutex<Option<crate::auth::Auth>>,
    #[cfg(feature = "load")]
    metrics: Option<Arc<crate::load::Metrics>>,
}

/// State shared by every context of one run.
pub(crate) struct Shared {
    pub(crate) http: reqwest::Client,
    pub(crate) config: RunConfig,
    pub(crate) contracts: HashMap<String, Arc<Contract>>,
    pub(crate) validate_counter: AtomicU64,
    /// The run's parent trace (from `TRACEPARENT` or config), resolved once.
    pub(crate) trace_parent: Option<(crate::trace::Traceparent, Option<String>)>,
}

/// The handle tests, hooks and fixtures receive.
///
/// Cheap to clone. Each context points at one frame of the scope chain; values
/// and default headers are looked up from the innermost frame outwards.
#[derive(Clone)]
pub struct Ctx {
    pub(crate) shared: Arc<Shared>,
    frame: Arc<Frame>,
}

impl Ctx {
    pub(crate) fn root(shared: Arc<Shared>) -> Self {
        Ctx {
            shared,
            frame: Arc::new(Frame {
                scope: Scope::Run,
                name: "run".into(),
                vu: None,
                parent: None,
                values: Mutex::default(),
                headers: Mutex::default(),
                teardown: Mutex::default(),
                steps: Mutex::default(),
                trace: Mutex::default(),
                auth: Mutex::default(),
                #[cfg(feature = "load")]
                metrics: None,
            }),
        }
    }

    /// Opens a child scope.
    pub(crate) fn child(&self, scope: Scope, name: impl Into<String>) -> Self {
        self.child_frame(scope, name.into(), None)
    }

    /// Opens a virtual-user scope.
    pub(crate) fn child_vu(&self, vu: usize) -> Self {
        self.child_frame(Scope::Vu, format!("vu-{vu}"), Some(vu))
    }

    /// Opens a scope whose HTTP traffic and iterations are recorded into `metrics`.
    #[cfg(feature = "load")]
    pub(crate) fn child_metrics(&self, name: &str, metrics: Arc<crate::load::Metrics>) -> Self {
        let mut cx = self.child_frame(Scope::Suite, name.to_string(), None);
        Arc::get_mut(&mut cx.frame).expect("fresh frame").metrics = Some(metrics);
        cx
    }

    fn child_frame(&self, scope: Scope, name: String, vu: Option<usize>) -> Self {
        Ctx {
            shared: self.shared.clone(),
            frame: Arc::new(Frame {
                scope,
                name,
                vu: vu.or(self.frame.vu),
                parent: Some(self.frame.clone()),
                values: Mutex::default(),
                headers: Mutex::default(),
                teardown: Mutex::default(),
                steps: Mutex::default(),
                trace: Mutex::default(),
                auth: Mutex::default(),
                #[cfg(feature = "load")]
                metrics: self.frame.metrics.clone(),
            }),
        }
    }

    /// The scope this context belongs to.
    pub fn scope(&self) -> Scope {
        self.frame.scope
    }

    /// Name of the current frame (suite name, `vu-3`, test name, ...).
    pub fn name(&self) -> &str {
        &self.frame.name
    }

    /// Path like `billing/vu-0/create invoice`.
    pub fn path(&self) -> String {
        let mut parts = vec![];
        let mut f = Some(&self.frame);
        while let Some(frame) = f {
            if frame.scope != Scope::Run {
                parts.push(frame.name.as_str());
            }
            f = frame.parent.as_ref();
        }
        parts.reverse();
        parts.join("/")
    }

    /// Virtual user index, if running inside a VU (always `Some(0)` in functional runs).
    pub fn vu(&self) -> Option<usize> {
        self.frame.vu
    }

    #[cfg(feature = "load")]
    pub(crate) fn metrics(&self) -> Option<&Arc<crate::load::Metrics>> {
        self.frame.metrics.as_ref()
    }

    /// The run configuration.
    pub fn config(&self) -> &RunConfig {
        &self.shared.config
    }

    /// The trace requests from this scope are part of (see [`crate::trace`]).
    ///
    /// With the default [`crate::trace::TraceScope::Test`] this is the same for every
    /// request of one test, and different between tests. With `TraceScope::Request`
    /// every request starts its own trace; use `resp.trace` there.
    pub fn trace(&self) -> crate::trace::TraceContext {
        use crate::trace::{TraceContext, TraceScope};
        let cfg = &self.shared.config.tracing;
        if cfg.scope == TraceScope::Request {
            return TraceContext::fresh(cfg, self.shared.trace_parent.as_ref());
        }
        let want = match cfg.scope {
            TraceScope::Run => Scope::Run,
            TraceScope::Suite => Scope::Suite,
            _ => Scope::Test,
        };
        // The nearest frame of the wanted scope; else the nearest suite; else the root.
        let mut chain = vec![];
        let mut f = Some(&self.frame);
        while let Some(frame) = f {
            chain.push(frame);
            f = frame.parent.as_ref();
        }
        let target = chain
            .iter()
            .find(|fr| fr.scope == want)
            .or_else(|| chain.iter().find(|fr| fr.scope == Scope::Suite))
            .or(chain.last())
            .expect("frame chain is never empty");
        target
            .trace
            .lock()
            .unwrap()
            .get_or_insert_with(|| TraceContext::fresh(cfg, self.shared.trace_parent.as_ref()))
            .clone()
    }

    /// Authenticates every request from this scope and its children that has no
    /// auth of its own (actors and explicit headers win).
    pub fn auth(&self, auth: crate::auth::Auth) {
        *self.frame.auth.lock().unwrap() = Some(auth);
    }

    /// `Authorization: Bearer <token>` for this scope.
    pub fn bearer(&self, token: impl Into<String>) {
        self.auth(crate::auth::Auth::bearer(token));
    }

    /// HTTP Basic auth for this scope.
    pub fn basic(&self, username: impl Into<String>, password: impl Into<String>) {
        self.auth(crate::auth::Auth::basic(username, password));
    }

    /// The nearest scope auth, if any.
    pub fn scope_auth(&self) -> Option<crate::auth::Auth> {
        let mut f = Some(&self.frame);
        while let Some(frame) = f {
            if let Some(a) = frame.auth.lock().unwrap().clone() {
                return Some(a);
            }
            f = frame.parent.as_ref();
        }
        None
    }

    /// Name of the nearest frame of `scope` (suite name, test name, ...).
    pub fn name_of(&self, scope: Scope) -> Option<String> {
        let mut f = Some(&self.frame);
        while let Some(frame) = f {
            if frame.scope == scope {
                return Some(frame.name.clone());
            }
            f = frame.parent.as_ref();
        }
        None
    }

    /// Looks up a value (usually a fixture) in this scope or any parent scope.
    pub fn get(&self, key: &str) -> Option<Value> {
        let mut f = Some(&self.frame);
        while let Some(frame) = f {
            if let Some(v) = frame.values.lock().unwrap().get(key) {
                return Some(v.clone());
            }
            f = frame.parent.as_ref();
        }
        None
    }

    /// Typed variant of [`Ctx::get`]; errors if the value is missing.
    pub fn get_as<T: DeserializeOwned>(&self, key: &str) -> Result<T> {
        let v = self
            .get(key)
            .ok_or_else(|| anyhow!("no value '{key}' visible from {}", self.path()))?;
        serde_json::from_value(v).map_err(|e| anyhow!("value '{key}' has unexpected shape: {e}"))
    }

    /// Stores a value in the *current* scope.
    pub fn set(&self, key: impl Into<String>, value: impl Serialize) -> Result<()> {
        let v = serde_json::to_value(value)?;
        self.frame.values.lock().unwrap().insert(key.into(), v);
        Ok(())
    }

    /// Reads a variable: `--var` / [`RunConfig::vars`] first, then the process environment.
    pub fn env(&self, key: &str) -> Result<String> {
        self.env_opt(key).ok_or_else(|| {
            anyhow!("variable '{key}' is not set (pass --var {key}=... or export it)")
        })
    }

    /// Like [`Ctx::env`] but optional.
    pub fn env_opt(&self, key: &str) -> Option<String> {
        self.shared
            .config
            .vars
            .get(key)
            .cloned()
            .or_else(|| std::env::var(key).ok())
    }

    /// Adds a header to every request made from this scope and its children.
    pub fn default_header(&self, name: impl Into<String>, value: impl Into<String>) {
        let (name, value) = (name.into(), value.into());
        let mut h = self.frame.headers.lock().unwrap();
        h.retain(|(n, _)| !n.eq_ignore_ascii_case(&name));
        h.push((name, value));
    }

    /// All default headers visible from this scope (inner scopes win).
    pub(crate) fn default_headers(&self) -> Vec<(String, String)> {
        let mut chain = vec![];
        let mut f = Some(&self.frame);
        while let Some(frame) = f {
            chain.push(frame.clone());
            f = frame.parent.as_ref();
        }
        let mut out: Vec<(String, String)> = vec![];
        for frame in chain.iter().rev() {
            for (n, v) in frame.headers.lock().unwrap().iter() {
                out.retain(|(o, _)| !o.eq_ignore_ascii_case(n));
                out.push((n.clone(), v.clone()));
            }
        }
        out
    }

    /// Registers a cleanup action for the current scope.
    ///
    /// Cleanups run in reverse registration order when the scope ends,
    /// whether it ended successfully or not.
    pub fn defer<F, Fut>(&self, label: impl Into<String>, f: F)
    where
        F: FnOnce(Ctx) -> Fut + Send + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        self.defer_boxed(label, Box::new(move |cx| Box::pin(f(cx))));
    }

    /// Type-erased [`Ctx::defer`], used by the scripting frontends.
    pub fn defer_boxed(&self, label: impl Into<String>, f: DeferFn) {
        self.frame.teardown.lock().unwrap().push(Deferred {
            label: label.into(),
            f,
        });
    }

    /// Looks up a contract registered on the suite.
    pub fn contract(&self, name: &str) -> Result<Arc<Contract>> {
        self.shared
            .contracts
            .get(name)
            .cloned()
            .ok_or_else(|| anyhow!("no contract named '{name}' is registered on this suite"))
    }

    /// Prints a log line tagged with the current scope path.
    pub fn log(&self, msg: impl fmt::Display) {
        if !self.shared.config.quiet {
            eprintln!("  [{}] {msg}", self.path());
        }
    }

    /// Runs all cleanups of this scope (LIFO). Returns one message per failed cleanup.
    pub(crate) async fn unwind(&self) -> Vec<String> {
        let timeout = self.shared.config.hook_timeout;
        let mut errors = vec![];
        loop {
            // Pop one at a time: cleanups may themselves register cleanups.
            let next = self.frame.teardown.lock().unwrap().pop();
            let Some(d) = next else { break };
            let cx = self.clone();
            if let Err(e) = guarded(&cx, (d.f)(cx.clone()), timeout).await {
                errors.push(format!("cleanup '{}' in {}: {e:#}", d.label, self.path()));
            }
        }
        errors
    }
}

tokio::task_local! {
    static CURRENT: Ctx;
}

/// The context of the fixture, hook, test or cleanup currently running on this task.
///
/// This is what lets [`crate::dsl::Actor`]s, [`crate::dsl::step`], [`crate::dsl::undo`]
/// etc. work without passing `cx` around. Tasks you `tokio::spawn` yourself do not
/// inherit it; pass `cx` explicitly there.
pub fn current() -> Result<Ctx> {
    CURRENT.try_with(Ctx::clone).map_err(|_| {
        anyhow!("no apitest context on this task (are you inside a fixture, hook, test or cleanup? tasks you spawn yourself need an explicit `cx`)")
    })
}

impl Ctx {
    /// Runs `fut` with this context installed as [`current()`].
    pub fn enter<F: Future>(&self, fut: F) -> tokio::task::futures::TaskLocalFuture<Ctx, F> {
        CURRENT.scope(self.clone(), fut)
    }

    /// Records a finished step on the innermost test scope.
    pub(crate) fn record_step(&self, step: crate::report::StepReport) {
        let mut f = Some(&self.frame);
        while let Some(frame) = f {
            if frame.scope == Scope::Test || frame.parent.is_none() {
                frame.steps.lock().unwrap().push(step);
                return;
            }
            f = frame.parent.as_ref();
        }
    }

    pub(crate) fn take_steps(&self) -> Vec<crate::report::StepReport> {
        std::mem::take(&mut *self.frame.steps.lock().unwrap())
    }
}

/// Runs a future on its own task with a timeout, turning panics into errors.
/// `cx` becomes the task's [`current()`] context.
pub(crate) async fn guarded<F>(cx: &Ctx, fut: F, timeout: Duration) -> Result<()>
where
    F: Future<Output = Result<()>> + Send + 'static,
{
    guarded_value(cx, fut, timeout).await
}

pub(crate) async fn guarded_value<T, F>(cx: &Ctx, fut: F, timeout: Duration) -> Result<T>
where
    T: Send + 'static,
    F: Future<Output = Result<T>> + Send + 'static,
{
    let handle = tokio::spawn(cx.enter(fut));
    let abort = handle.abort_handle();
    match tokio::time::timeout(timeout, handle).await {
        Err(_) => {
            abort.abort();
            Err(anyhow!("timed out after {timeout:?}"))
        }
        Ok(Err(join)) if join.is_panic() => {
            let payload = join.into_panic();
            let msg = payload
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "<non-string panic>".into());
            Err(anyhow!("panicked: {msg}"))
        }
        Ok(Err(join)) => Err(anyhow!("task failed: {join}")),
        Ok(Ok(r)) => r,
    }
}
