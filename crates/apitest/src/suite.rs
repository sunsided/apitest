//! Suite definition: contracts, fixtures, hooks and tests.

use std::any::Any;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::Result;
use serde::Serialize;
use serde_json::Value;

use crate::context::{Ctx, Scope};
use crate::contract::Contract;

/// A boxed, sendable future returning `Result<T>`.
pub type BoxFut<T> = Pin<Box<dyn Future<Output = Result<T>> + Send>>;

/// A type-erased async callback receiving a [`Ctx`].
pub type CtxFn<T> = Arc<dyn Fn(Ctx) -> BoxFut<T> + Send + Sync>;

fn value_fn<T, F, Fut>(f: F) -> CtxFn<Value>
where
    T: Serialize,
    F: Fn(Ctx) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<T>> + Send + 'static,
{
    Arc::new(move |cx| {
        let fut = f(cx);
        Box::pin(async move { Ok(serde_json::to_value(fut.await?)?) })
    })
}

/// Wraps an async closure into a [`CtxFn`].
pub fn ctx_fn<T, F, Fut>(f: F) -> CtxFn<T>
where
    F: Fn(Ctx) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<T>> + Send + 'static,
{
    Arc::new(move |cx| Box::pin(f(cx)))
}

/// A named value produced by a setup function at the start of a scope.
///
/// `teardown` (if any) is registered as a cleanup of the same scope right after
/// `setup` succeeded, so it runs exactly when the value goes out of scope. It can
/// read the value via `cx.get(name)`.
#[derive(Clone)]
pub struct Fixture {
    pub name: String,
    pub scope: Scope,
    pub setup: CtxFn<Value>,
    pub teardown: Option<CtxFn<()>>,
}

/// How a test may overlap with other tests of the same suite.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Concurrency {
    /// Runs alone: waits for running tests to finish, and nothing starts until it is done.
    #[default]
    Serial,
    /// May run concurrently with other parallel tests (bounded by `RunConfig::max_parallel`
    /// and by shared [`TestOpts::resources`]).
    Parallel,
}

/// Per-test options.
#[derive(Clone, Default)]
pub struct TestOpts {
    pub concurrency: Concurrency,
    /// Named locks. Parallel tests that share a resource name never overlap.
    pub resources: Vec<String>,
    /// Overrides `RunConfig::test_timeout`.
    pub timeout: Option<std::time::Duration>,
    /// Runs before this test's body (after `before_each`), inside the test scope.
    pub setup: Option<CtxFn<()>>,
    /// Runs after this test's body (before `after_each`), even if the body failed.
    pub teardown: Option<CtxFn<()>>,
    pub tags: Vec<String>,
}

impl TestOpts {
    pub fn parallel() -> Self {
        TestOpts {
            concurrency: Concurrency::Parallel,
            ..Default::default()
        }
    }
    pub fn serial() -> Self {
        TestOpts::default()
    }
    pub fn resource(mut self, name: impl Into<String>) -> Self {
        self.resources.push(name.into());
        self
    }
    pub fn timeout(mut self, t: std::time::Duration) -> Self {
        self.timeout = Some(t);
        self
    }
    pub fn tag(mut self, t: impl Into<String>) -> Self {
        self.tags.push(t.into());
        self
    }
    pub fn setup<F, Fut>(mut self, f: F) -> Self
    where
        F: Fn(Ctx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        self.setup = Some(ctx_fn(f));
        self
    }
    pub fn teardown<F, Fut>(mut self, f: F) -> Self
    where
        F: Fn(Ctx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        self.teardown = Some(ctx_fn(f));
        self
    }
}

/// One test case.
#[derive(Clone)]
pub struct TestCase {
    pub name: String,
    /// Stable id used by the libtest harness (e.g. `billing::create_invoice` for
    /// macro-registered tests). Defaults to `suite::name`.
    pub id: Option<String>,
    pub opts: TestOpts,
    pub body: CtxFn<()>,
}

/// Lifecycle hooks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HookKind {
    BeforeAll,
    AfterAll,
    BeforeEach,
    AfterEach,
}

impl std::str::FromStr for HookKind {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        Ok(match s {
            "before_all" | "beforeAll" => HookKind::BeforeAll,
            "after_all" | "afterAll" => HookKind::AfterAll,
            "before_each" | "beforeEach" => HookKind::BeforeEach,
            "after_each" | "afterEach" => HookKind::AfterEach,
            other => anyhow::bail!("unknown hook kind '{other}'"),
        })
    }
}

/// A suite: the unit of setup/teardown, contract binding and reporting.
///
/// Execution order (functional run):
///
/// ```text
/// contract drift check
/// suite fixtures (setup) → before_all / setup
///   vu fixtures
///     per test: test fixtures → before_each → test setup → body
///               → test teardown → after_each → test cleanups + fixture teardowns (LIFO)
///   vu cleanups
/// after_all / teardown → suite cleanups + fixture teardowns (LIFO)
/// ```
///
/// Teardown steps always run, even if the corresponding setup step, the body or
/// an earlier teardown failed. Serial tests run alone; parallel tests overlap.
#[derive(Clone)]
pub struct Suite {
    pub name: String,
    pub contracts: Vec<Arc<Contract>>,
    pub fixtures: Vec<Fixture>,
    pub hooks: Vec<(HookKind, CtxFn<()>)>,
    pub tests: Vec<TestCase>,
    /// Headers for every request of this suite (e.g. a suite-specific `User-Agent`).
    pub headers: Vec<(String, String)>,
    /// Overrides the run's tracing configuration for this suite.
    pub tracing: Option<crate::trace::TracingConfig>,
    /// Keeps frontend resources alive (e.g. the JS worker pool) for as long as the suite exists.
    pub(crate) keepalive: Vec<Arc<dyn Any + Send + Sync>>,
}

impl Suite {
    pub fn new(name: impl Into<String>) -> Self {
        Suite {
            name: name.into(),
            contracts: vec![],
            fixtures: vec![],
            hooks: vec![],
            tests: vec![],
            headers: vec![],
            tracing: None,
            keepalive: vec![],
        }
    }

    /// `User-Agent` for every request of this suite (overrides the run default).
    pub fn user_agent(self, ua: impl Into<String>) -> Self {
        self.header("User-Agent", ua)
    }

    /// A header for every request of this suite.
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Tracing settings for this suite only.
    pub fn tracing(mut self, t: crate::trace::TracingConfig) -> Self {
        self.tracing = Some(t);
        self
    }

    /// No trace headers from this suite.
    pub fn no_tracing(self) -> Self {
        self.tracing(crate::trace::TracingConfig::disabled())
    }

    /// Binds a pinned contract to this suite. Tests reach it via `cx.call("<name>", "<operationId>")`.
    pub fn contract(mut self, contract: Contract) -> Self {
        self.add_contract(contract);
        self
    }

    /// Loads and binds a pinned contract from a spec file.
    pub fn pinned(self, name: &str, spec: impl AsRef<Path>) -> Result<Self> {
        Ok(self.contract(Contract::pinned(name, spec)?))
    }

    /// Adds a fixture: `setup` runs when the scope starts and its return value is
    /// visible to everything inside that scope via `cx.get(name)`. Register the
    /// matching cleanup with `cx.defer(...)` inside `setup`.
    pub fn fixture<T, F, Fut>(mut self, name: &str, scope: Scope, setup: F) -> Self
    where
        T: Serialize,
        F: Fn(Ctx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<T>> + Send + 'static,
    {
        self.add_fixture(name, scope, value_fn(setup), None);
        self
    }

    /// Like [`Suite::fixture`] with an explicit teardown. The teardown runs when the
    /// fixture's scope ends (LIFO with other cleanups) and only if `setup` succeeded.
    pub fn fixture_with_teardown<T, F, Fut, G, GFut>(
        mut self,
        name: &str,
        scope: Scope,
        setup: F,
        teardown: G,
    ) -> Self
    where
        T: Serialize,
        F: Fn(Ctx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<T>> + Send + 'static,
        G: Fn(Ctx) -> GFut + Send + Sync + 'static,
        GFut: Future<Output = Result<()>> + Send + 'static,
    {
        self.add_fixture(name, scope, value_fn(setup), Some(ctx_fn(teardown)));
        self
    }

    /// Suite setup (alias for `before_all`).
    pub fn setup<F, Fut>(self, f: F) -> Self
    where
        F: Fn(Ctx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        self.before_all(f)
    }

    /// Suite teardown (alias for `after_all`). Runs even if setup or tests failed.
    pub fn teardown<F, Fut>(self, f: F) -> Self
    where
        F: Fn(Ctx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        self.after_all(f)
    }

    pub fn before_all<F, Fut>(mut self, f: F) -> Self
    where
        F: Fn(Ctx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        self.add_hook(HookKind::BeforeAll, ctx_fn(f));
        self
    }

    pub fn after_all<F, Fut>(mut self, f: F) -> Self
    where
        F: Fn(Ctx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        self.add_hook(HookKind::AfterAll, ctx_fn(f));
        self
    }

    pub fn before_each<F, Fut>(mut self, f: F) -> Self
    where
        F: Fn(Ctx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        self.add_hook(HookKind::BeforeEach, ctx_fn(f));
        self
    }

    pub fn after_each<F, Fut>(mut self, f: F) -> Self
    where
        F: Fn(Ctx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        self.add_hook(HookKind::AfterEach, ctx_fn(f));
        self
    }

    pub fn test<F, Fut>(mut self, name: &str, body: F) -> Self
    where
        F: Fn(Ctx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        self.add_test(name, TestOpts::default(), ctx_fn(body));
        self
    }

    /// A test with options (concurrency, resources, timeout, per-test setup/teardown).
    pub fn test_with<F, Fut>(mut self, name: &str, opts: TestOpts, body: F) -> Self
    where
        F: Fn(Ctx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        self.add_test(name, opts, ctx_fn(body));
        self
    }

    // --- type-erased variants, used by the scripting frontends ---

    pub fn add_contract(&mut self, contract: Contract) {
        self.contracts.push(Arc::new(contract));
    }

    pub fn add_fixture(
        &mut self,
        name: &str,
        scope: Scope,
        setup: CtxFn<Value>,
        teardown: Option<CtxFn<()>>,
    ) {
        self.fixtures.push(Fixture {
            name: name.into(),
            scope,
            setup,
            teardown,
        });
    }

    pub fn add_hook(&mut self, kind: HookKind, f: CtxFn<()>) {
        self.hooks.push((kind, f));
    }

    pub fn add_test(&mut self, name: &str, opts: TestOpts, body: CtxFn<()>) {
        self.tests.push(TestCase {
            name: name.into(),
            id: None,
            opts,
            body,
        });
    }

    /// Like [`Suite::add_test`] with an explicit harness id.
    pub fn add_test_with_id(&mut self, id: &str, name: &str, opts: TestOpts, body: CtxFn<()>) {
        self.add_test(name, opts, body);
        self.tests.last_mut().expect("just pushed").id = Some(id.into());
    }

    /// The id the harness reports this test under.
    pub fn test_id(&self, t: &TestCase) -> String {
        t.id.clone()
            .unwrap_or_else(|| format!("{}::{}", self.name, t.name))
    }

    pub fn keep_alive(&mut self, resource: Arc<dyn Any + Send + Sync>) {
        self.keepalive.push(resource);
    }

    pub(crate) fn hooks(&self, kind: HookKind) -> impl Iterator<Item = &CtxFn<()>> {
        self.hooks
            .iter()
            .filter(move |(k, _)| *k == kind)
            .map(|(_, f)| f)
    }

    pub(crate) fn fixtures_for(&self, scope: Scope) -> impl Iterator<Item = &Fixture> {
        self.fixtures.iter().filter(move |f| f.scope == scope)
    }
}
