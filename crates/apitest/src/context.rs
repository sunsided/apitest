//! Scope chain, fixture values, deferred teardown and the test-facing [`Ctx`].
//!
//! Every context points at one frame; frames link to their parent, forming a
//! chain from the innermost scope up to the run:
//!
//! ```mermaid
//! flowchart BT
//!     T[test frame] --> V[vu frame] --> S[suite frame] --> R[run frame]
//! ```
//!
//! Lookups (values, default headers, scope auth) walk this chain from the inside
//! out. Each frame owns its cleanups, which run LIFO when its scope ends.
//!
//! All per-frame state sits behind `std::sync::Mutex`es that are only held for
//! short, non-panicking critical sections. Methods documented with `# Panics`
//! panic only if such a mutex was poisoned anyway.

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

/// A registered cleanup and the label it is reported under if it fails.
struct Deferred {
    /// Shown in cleanup error messages, e.g. `undo hub:deleteUser {id=7}`.
    label: String,
    /// The cleanup itself.
    f: DeferFn,
}

/// One link of the scope chain: what a single scope owns.
pub(crate) struct Frame {
    /// Which kind of scope this frame is.
    scope: Scope,
    /// Suite name, `vu-<n>`, test name, or `run` for the root.
    name: String,
    /// Virtual user index, inherited from the parent unless this is a VU frame.
    vu: Option<usize>,
    /// The enclosing frame; `None` only for the run frame.
    parent: Option<Arc<Frame>>,
    /// Fixture values and [`Ctx::set`] values stored in this scope.
    values: Mutex<HashMap<String, Value>>,
    /// Default headers of this scope, unique by name (case-insensitively).
    headers: Mutex<Vec<(String, String)>>,
    /// Cleanups in registration order; popped from the end (LIFO).
    teardown: Mutex<Vec<Deferred>>,
    /// Steps recorded by [`crate::dsl::step`]; only filled on test and run frames.
    steps: Mutex<Vec<crate::report::StepReport>>,
    /// The trace of this scope, created on first use.
    trace: Mutex<Option<crate::trace::TraceContext>>,
    /// Scope-wide credentials set with [`Ctx::auth`].
    auth: Mutex<Option<crate::auth::Auth>>,
    /// Load-test metrics sink, inherited by child frames.
    #[cfg(feature = "load")]
    metrics: Option<Arc<crate::load::Metrics>>,
}

/// State shared by every context of one run.
pub(crate) struct Shared {
    /// HTTP client for all requests of the run (connection pooling).
    pub(crate) http: reqwest::Client,
    /// The run configuration.
    pub(crate) config: RunConfig,
    /// Contracts by name.
    pub(crate) contracts: HashMap<String, Arc<Contract>>,
    /// Counts contract calls to sample validation (`RunConfig::validate_every`).
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
    /// Run-wide state, the same for every context of the run.
    pub(crate) shared: Arc<Shared>,
    /// The innermost frame this context sees.
    frame: Arc<Frame>,
}

impl Ctx {
    /// The run-scope context every other context descends from.
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

    /// A context on a new frame below this one; `vu` overrides the inherited VU index.
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
    #[must_use]
    pub fn scope(&self) -> Scope {
        self.frame.scope
    }

    /// Name of the current frame (suite name, `vu-3`, test name, ...).
    #[must_use]
    pub fn name(&self) -> &str {
        &self.frame.name
    }

    /// Path like `billing/vu-0/create invoice`.
    #[must_use]
    pub fn path(&self) -> String {
        let mut parts: Vec<&str> = self
            .frames()
            .filter(|frame| frame.scope != Scope::Run)
            .map(|frame| frame.name.as_str())
            .collect();
        parts.reverse();
        parts.join("/")
    }

    /// This context's frame followed by all its ancestors, ending at the run frame.
    fn frames(&self) -> impl Iterator<Item = &Arc<Frame>> {
        std::iter::successors(Some(&self.frame), |frame| frame.parent.as_ref())
    }

    /// Virtual user index, if running inside a VU (always `Some(0)` in functional runs).
    #[must_use]
    pub fn vu(&self) -> Option<usize> {
        self.frame.vu
    }

    /// The load-test metrics sink of this scope, if it runs under a load plan.
    #[cfg(feature = "load")]
    pub(crate) fn metrics(&self) -> Option<&Arc<crate::load::Metrics>> {
        self.frame.metrics.as_ref()
    }

    /// The run configuration.
    #[must_use]
    pub fn config(&self) -> &RunConfig {
        &self.shared.config
    }

    /// The trace requests from this scope are part of (see [`crate::trace`]).
    ///
    /// With the default [`crate::trace::TraceScope::Test`] this is the same for every
    /// request of one test, and different between tests. With `TraceScope::Request`
    /// every request starts its own trace; use `resp.trace` there.
    ///
    /// # Panics
    ///
    /// Panics if the frame's trace mutex is poisoned.
    #[must_use]
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
        let chain: Vec<&Arc<Frame>> = self.frames().collect();
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
    ///
    /// # Panics
    ///
    /// Panics if the frame's auth mutex is poisoned.
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
    ///
    /// # Panics
    ///
    /// Panics if a frame's auth mutex is poisoned.
    #[must_use]
    pub fn scope_auth(&self) -> Option<crate::auth::Auth> {
        self.frames()
            .find_map(|frame| frame.auth.lock().unwrap().clone())
    }

    /// Name of the nearest frame of `scope` (suite name, test name, ...).
    #[must_use]
    pub fn name_of(&self, scope: Scope) -> Option<String> {
        self.frames()
            .find(|frame| frame.scope == scope)
            .map(|frame| frame.name.clone())
    }

    /// Looks up a value (usually a fixture) in this scope or any parent scope.
    ///
    /// # Panics
    ///
    /// Panics if a frame's value mutex is poisoned.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<Value> {
        self.frames()
            .find_map(|frame| frame.values.lock().unwrap().get(key).cloned())
    }

    /// Typed variant of [`Ctx::get`]; errors if the value is missing.
    ///
    /// # Errors
    ///
    /// Fails when no visible scope holds `key`, or when its value does not
    /// deserialize into `T`.
    pub fn get_as<T: DeserializeOwned>(&self, key: &str) -> Result<T> {
        let v = self
            .get(key)
            .ok_or_else(|| anyhow!("no value '{key}' visible from {}", self.path()))?;
        serde_json::from_value(v).map_err(|e| anyhow!("value '{key}' has unexpected shape: {e}"))
    }

    /// Stores a value in the *current* scope.
    ///
    /// # Errors
    ///
    /// Fails when `value` cannot be serialized to JSON.
    ///
    /// # Panics
    ///
    /// Panics if the frame's value mutex is poisoned.
    pub fn set(&self, key: impl Into<String>, value: impl Serialize) -> Result<()> {
        let v = serde_json::to_value(value)?;
        self.frame.values.lock().unwrap().insert(key.into(), v);
        Ok(())
    }

    /// Reads a variable: `--var` / [`RunConfig::vars`] first, then the process environment.
    ///
    /// # Errors
    ///
    /// Fails when the variable is set in neither place.
    pub fn env(&self, key: &str) -> Result<String> {
        self.env_opt(key).ok_or_else(|| {
            anyhow!("variable '{key}' is not set (pass --var {key}=... or export it)")
        })
    }

    /// Like [`Ctx::env`] but optional.
    #[must_use]
    pub fn env_opt(&self, key: &str) -> Option<String> {
        self.shared
            .config
            .vars
            .get(key)
            .cloned()
            .or_else(|| std::env::var(key).ok())
    }

    /// Adds a header to every request made from this scope and its children.
    ///
    /// # Panics
    ///
    /// Panics if the frame's header mutex is poisoned.
    pub fn default_header(&self, name: impl Into<String>, value: impl Into<String>) {
        let (name, value) = (name.into(), value.into());
        let mut h = self.frame.headers.lock().unwrap();
        h.retain(|(n, _)| !n.eq_ignore_ascii_case(&name));
        h.push((name, value));
    }

    /// All default headers visible from this scope (inner scopes win).
    pub(crate) fn default_headers(&self) -> Vec<(String, String)> {
        let chain: Vec<&Arc<Frame>> = self.frames().collect();
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
    ///
    /// # Panics
    ///
    /// Panics if the frame's teardown mutex is poisoned.
    pub fn defer_boxed(&self, label: impl Into<String>, f: DeferFn) {
        self.frame.teardown.lock().unwrap().push(Deferred {
            label: label.into(),
            f,
        });
    }

    /// Looks up a contract registered on the suite.
    ///
    /// # Errors
    ///
    /// Fails when no contract of that name is registered.
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
    /// The context of the fixture, hook, test or cleanup running on this task.
    static CURRENT: Ctx;
}

/// The context of the fixture, hook, test or cleanup currently running on this task.
///
/// This is what lets [`crate::dsl::Actor`]s, [`crate::dsl::step`], [`crate::dsl::undo`]
/// etc. work without passing `cx` around. Tasks you `tokio::spawn` yourself do not
/// inherit it; pass `cx` explicitly there.
///
/// # Errors
///
/// Fails when called outside a fixture, hook, test or cleanup (or on a task
/// spawned from one).
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
    ///
    /// Outside a test (fixtures, suite hooks) the step lands on the run frame.
    pub(crate) fn record_step(&self, step: crate::report::StepReport) {
        if let Some(frame) = self
            .frames()
            .find(|frame| frame.scope == Scope::Test || frame.parent.is_none())
        {
            frame.steps.lock().unwrap().push(step);
        }
    }

    /// Removes and returns the steps recorded on this context's own frame.
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

/// [`guarded`] for futures that produce a value (fixture setups).
///
/// On timeout the task is aborted, so it stops at its next `.await`.
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
            Err(anyhow!("panicked: {}", panic_message(&*join.into_panic())))
        }
        Ok(Err(join)) => Err(anyhow!("task failed: {join}")),
        Ok(Ok(r)) => r,
    }
}

/// The message of a panic payload (`&str` or `String`), or a placeholder.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(std::string::ToString::to_string)
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "<non-string panic>".into())
}
