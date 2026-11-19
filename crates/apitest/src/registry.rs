//! Registrations made by `#[apitest::test]`, `#[apitest::fixture]` and
//! `#[apitest::suite]` (feature `macros`), collected into [`Suite`]s.
//!
//! The registrations live in `linkme` distributed slices, so they are gathered
//! at link time without life-before-main tricks. Items are grouped by suite key:
//! the module path relative to the crate root (`billing`, `billing::admin`), or
//! the crate name for items in the root module, unless `suite = "..."` is given.
//! Within a suite, fixtures and tests keep source order (file, line).

use std::collections::BTreeMap;

use anyhow::{Result, anyhow};
use serde_json::Value;

use crate::context::{Ctx, Scope};
use crate::suite::{BoxFut, Suite, TestOpts};

/// One `#[apitest::test]` function, as emitted by the macro.
#[doc(hidden)]
pub struct TestDef {
    /// `module_path!()` of the test, e.g. `my_crate::billing`.
    pub module: &'static str,
    /// Name of the annotated function.
    pub fn_name: &'static str,
    /// Display name from `name = "..."`; defaults to [`TestDef::fn_name`].
    pub name: Option<&'static str>,
    /// Explicit suite key from `suite = "..."`; defaults to the module path.
    pub suite: Option<&'static str>,
    /// Source file, for ordering tests within a suite.
    pub file: &'static str,
    /// Source line, for ordering tests within a suite.
    pub line: u32,
    /// Builds the test's options (concurrency, resource locks, timeout, tags).
    pub opts: fn() -> TestOpts,
    /// Runs the test body.
    pub body: fn(Ctx) -> BoxFut<()>,
}

/// One `#[apitest::fixture]` function, as emitted by the macro.
#[doc(hidden)]
pub struct FixtureDef {
    /// `module_path!()` of the fixture.
    pub module: &'static str,
    /// Fixture name tests request it by.
    pub name: &'static str,
    /// Explicit suite key from `suite = "..."`; defaults to the module path.
    pub suite: Option<&'static str>,
    /// Scope name (`run`, `suite`, `vu`, `test`), parsed into a [`Scope`].
    pub scope: &'static str,
    /// Source file, for ordering fixtures within a suite.
    pub file: &'static str,
    /// Source line, for ordering fixtures within a suite.
    pub line: u32,
    /// Produces the fixture value.
    pub setup: fn(Ctx) -> BoxFut<Value>,
    /// Cleans up when the fixture's scope ends, if the fixture has a teardown.
    pub teardown: Option<fn(Ctx) -> BoxFut<()>>,
}

/// One `#[apitest::suite]` function, as emitted by the macro.
#[doc(hidden)]
pub struct SuiteDef {
    /// `module_path!()` of the suite function.
    pub module: &'static str,
    /// Explicit suite key from `suite = "..."`; defaults to the module path.
    pub suite: Option<&'static str>,
    /// Builds the base suite (contracts, config) that registered tests and
    /// fixtures with the same key are added to.
    pub build: fn() -> Result<Suite>,
}

/// Every `#[apitest::test]` linked into this binary.
#[linkme::distributed_slice]
#[doc(hidden)]
pub static TESTS: [TestDef];

/// Every `#[apitest::fixture]` linked into this binary.
#[linkme::distributed_slice]
#[doc(hidden)]
pub static FIXTURES: [FixtureDef];

/// Every `#[apitest::suite]` linked into this binary.
#[linkme::distributed_slice]
#[doc(hidden)]
pub static SUITES: [SuiteDef];

/// `crate::billing::admin` → `billing::admin`; `crate` → `crate`.
fn relative_module(module: &str) -> &str {
    module.split_once("::").map_or(module, |(_, rest)| rest)
}

/// The suite an item belongs to: `explicit` if given, else its module path
/// relative to the crate root.
fn suite_key(module: &str, explicit: Option<&str>) -> String {
    explicit.map_or_else(|| relative_module(module).to_string(), str::to_string)
}

/// The harness id of a registered test, matching what rust-analyzer passes to
/// `cargo test -- <id> --exact`.
fn test_id(module: &str, fn_name: &str) -> String {
    match module.split_once("::") {
        Some((_, rest)) => format!("{rest}::{fn_name}"),
        None => fn_name.to_string(),
    }
}

/// Builds one [`Suite`] per suite key from all registrations in this binary,
/// sorted by key.
///
/// # Errors
///
/// Fails when two `#[apitest::suite]` functions share a key, when a suite
/// builder fails, or when a fixture names an unknown scope.
pub fn collect() -> Result<Vec<Suite>> {
    let mut suites: BTreeMap<String, Suite> = BTreeMap::new();

    for def in SUITES {
        let key = suite_key(def.module, def.suite);
        if suites.contains_key(&key) {
            return Err(anyhow!(
                "suite '{key}' is declared twice with #[apitest::suite]"
            ));
        }

        let suite = (def.build)().map_err(|e| anyhow!("building suite '{key}': {e:#}"))?;
        suites.insert(key, suite);
    }

    let mut fixtures: Vec<&FixtureDef> = FIXTURES.iter().collect();
    fixtures.sort_by_key(|f| (f.file, f.line));
    for f in fixtures {
        let key = suite_key(f.module, f.suite);
        let scope: Scope = f.scope.parse()?;
        let setup = f.setup;
        let teardown = f.teardown;
        suites
            .entry(key.clone())
            .or_insert_with(|| Suite::new(&key))
            .add_fixture(
                f.name,
                scope,
                std::sync::Arc::new(move |cx| setup(cx)),
                teardown
                    .map(|t| -> crate::suite::CtxFn<()> { std::sync::Arc::new(move |cx| t(cx)) }),
            );
    }

    let mut tests: Vec<&TestDef> = TESTS.iter().collect();
    tests.sort_by_key(|t| (t.file, t.line));
    for t in tests {
        let key = suite_key(t.module, t.suite);
        let body = t.body;
        suites
            .entry(key.clone())
            .or_insert_with(|| Suite::new(&key))
            .add_test_with_id(
                &test_id(t.module, t.fn_name),
                t.name.unwrap_or(t.fn_name),
                (t.opts)(),
                std::sync::Arc::new(move |cx| body(cx)),
            );
    }

    Ok(suites.into_values().collect())
}
