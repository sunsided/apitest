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

#[doc(hidden)]
pub struct TestDef {
    pub module: &'static str,
    pub fn_name: &'static str,
    pub name: Option<&'static str>,
    pub suite: Option<&'static str>,
    pub file: &'static str,
    pub line: u32,
    pub opts: fn() -> TestOpts,
    pub body: fn(Ctx) -> BoxFut<()>,
}

#[doc(hidden)]
pub struct FixtureDef {
    pub module: &'static str,
    pub name: &'static str,
    pub suite: Option<&'static str>,
    pub scope: &'static str,
    pub file: &'static str,
    pub line: u32,
    pub setup: fn(Ctx) -> BoxFut<Value>,
    pub teardown: Option<fn(Ctx) -> BoxFut<()>>,
}

#[doc(hidden)]
pub struct SuiteDef {
    pub module: &'static str,
    pub suite: Option<&'static str>,
    pub build: fn() -> Result<Suite>,
}

#[linkme::distributed_slice]
#[doc(hidden)]
pub static TESTS: [TestDef];

#[linkme::distributed_slice]
#[doc(hidden)]
pub static FIXTURES: [FixtureDef];

#[linkme::distributed_slice]
#[doc(hidden)]
pub static SUITES: [SuiteDef];

/// `crate::billing::admin` → `billing::admin`; `crate` → `crate`.
fn relative_module(module: &str) -> &str {
    module.split_once("::").map_or(module, |(_, rest)| rest)
}

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

/// Builds one [`Suite`] per suite key from all registrations in this binary.
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
