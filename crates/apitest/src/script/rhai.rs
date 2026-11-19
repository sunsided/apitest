//! Rhai frontend.
//!
//! Contract calls are `cx.op(contract, operationId, opts)` here (`x.call(..)` is
//! reserved by Rhai for calling function pointers).
//!
//! ```rhai
//! let s = suite("billing");
//! s.contract("billing", "../specs/billing-1.0.yaml", #{ live_spec: "/openapi.json" });
//!
//! s.fixture("token", "suite", |cx| {
//!     let r = cx.http("POST", "/auth/token", #{ form: #{ user: cx.env("API_USER"), pass: cx.env("API_PASS") } });
//!     r.expect(200).body.access_token
//! }, |cx| {
//!     cx.http("POST", "/auth/revoke", #{ bearer: cx.get("token") });
//! });
//!
//! s.test("create invoice", #{ parallel: true }, |cx| {
//!     let inv = cx.op("billing", "createInvoice", #{ json: #{ amount: 4200, currency: "EUR" } }).expect(201).body;
//!     cx.defer(|cx| cx.op("billing", "deleteInvoice", #{ path: #{ id: inv.id } }).expect(204));
//! });
//!
//! s   // the script evaluates to a suite (or an array of suites)
//! ```

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow, bail};
use rhai::{AST, Array, Dynamic, Engine, EvalAltResult, FnPtr, Map};
use serde_json::Value;

use super::{SyncCtx, blocking_fn, resolve_relative};
use crate::context::Scope;
use crate::contract::{Compat, Contract};
use crate::suite::{HookKind, Suite, TestOpts};

/// Result type of native functions registered with Rhai.
type RResult<T> = std::result::Result<T, Box<EvalAltResult>>;

/// Engine plus compiled script, shared by every callback of the loaded suites.
///
/// Callbacks are function pointers into `ast`, so both must outlive the suites;
/// [`convert`] hands an `Arc` to [`Suite::keep_alive`] for that.
struct Runtime {
    /// Engine with the apitest API registered (see [`build_engine`]).
    engine: Engine,
    /// The compiled suite file; holds the closures the suites call back into.
    ast: AST,
}

/// Compile-time proof that the runtime and `cx` can cross onto the blocking pool.
const _: () = {
    /// Accepts only `Send + Sync` types.
    const fn is_send_sync<T: Send + Sync>() {}
    is_send_sync::<Runtime>();
    is_send_sync::<RCx>();
};

/// The `cx` object passed to Rhai callbacks.
#[derive(Clone)]
struct RCx {
    /// Blocking context of the running test, fixture or hook.
    sync: SyncCtx,
    /// Runtime used to call closures handed to `cx.defer(..)`.
    rt: Arc<Runtime>,
}

/// A test registered with `s.test(..)`, before conversion.
#[derive(Clone, Default)]
struct RTestDef {
    /// Test name as shown in reports.
    name: String,
    /// Test body; always `Some` for tests registered from scripts.
    body: Option<FnPtr>,
    /// Raw options map, including the `setup`/`teardown` closures.
    opts: Map,
}

/// Suite definition built by the script.
#[derive(Clone, Default)]
struct RSuite {
    /// Suite name as shown in reports.
    name: String,
    /// `(name, spec path relative to the script, options)` per pinned contract.
    contracts: Vec<(String, String, Map)>,
    /// `(name, scope, setup, teardown)` per fixture.
    fixtures: Vec<(String, String, FnPtr, Option<FnPtr>)>,
    /// Lifecycle hooks in registration order.
    hooks: Vec<(HookKind, FnPtr)>,
    /// Tests in registration order.
    tests: Vec<RTestDef>,
}

/// Turns any displayable error into a Rhai runtime error, keeping the cause chain.
#[expect(
    clippy::unnecessary_box_returns,
    reason = "Rhai native functions must fail with Box<EvalAltResult>; boxing here keeps `map_err(err)` usable"
)]
fn err<E: std::fmt::Display>(e: E) -> Box<EvalAltResult> {
    format!("{e:#}").into()
}

/// Converts a JSON value into a Rhai value.
fn to_dyn(v: &Value) -> RResult<Dynamic> {
    rhai::serde::to_dynamic(v)
}

/// Converts a Rhai value into JSON; `()` becomes `null`.
fn from_dyn(d: &Dynamic) -> RResult<Value> {
    if d.is_unit() {
        return Ok(Value::Null);
    }

    rhai::serde::from_dynamic(d)
}

/// Builds an engine with the suite, `cx` and assertion APIs registered.
///
/// `print` and `debug` go to stderr, prefixed with `[rhai]`, with secrets
/// masked (see [`crate::redact::console`]).
fn build_engine() -> Engine {
    let mut engine = Engine::new();
    engine.set_max_expr_depths(128, 64);
    engine.on_print(|s| eprintln!("{}", crate::redact::console(&format!("  [rhai] {s}"))));
    engine.on_debug(|s, _source, pos| {
        eprintln!(
            "{}",
            crate::redact::console(&format!("  [rhai debug {pos}] {s}"))
        );
    });

    register_suite_api(&mut engine);
    register_hook_api(&mut engine);
    register_ctx_state_api(&mut engine);
    register_ctx_request_api(&mut engine);
    register_assertion_api(&mut engine);
    engine
}

/// Registers `suite(..)` and the `contract`, `fixture` and `test` builders.
fn register_suite_api(engine: &mut Engine) {
    engine
        .register_type_with_name::<RSuite>("Suite")
        .register_fn("suite", |name: &str| RSuite {
            name: name.into(),
            ..Default::default()
        })
        .register_fn("contract", |s: &mut RSuite, name: &str, path: &str| {
            s.contracts.push((name.into(), path.into(), Map::new()));
        })
        .register_fn(
            "contract",
            |s: &mut RSuite, name: &str, path: &str, opts: Map| {
                s.contracts.push((name.into(), path.into(), opts));
            },
        )
        .register_fn(
            "fixture",
            |s: &mut RSuite, name: &str, scope: &str, f: FnPtr| {
                s.fixtures.push((name.into(), scope.into(), f, None));
            },
        )
        .register_fn(
            "fixture",
            |s: &mut RSuite, name: &str, scope: &str, f: FnPtr, td: FnPtr| {
                s.fixtures.push((name.into(), scope.into(), f, Some(td)));
            },
        )
        .register_fn("test", |s: &mut RSuite, name: &str, f: FnPtr| {
            s.tests.push(RTestDef {
                name: name.into(),
                body: Some(f),
                opts: Map::new(),
            });
        })
        .register_fn("test", |s: &mut RSuite, name: &str, opts: Map, f: FnPtr| {
            s.tests.push(RTestDef {
                name: name.into(),
                body: Some(f),
                opts,
            });
        });
}

/// Registers the suite lifecycle hooks; `setup`/`teardown` alias `before_all`/`after_all`.
fn register_hook_api(engine: &mut Engine) {
    engine
        .register_fn("setup", |s: &mut RSuite, f: FnPtr| {
            s.hooks.push((HookKind::BeforeAll, f));
        })
        .register_fn("teardown", |s: &mut RSuite, f: FnPtr| {
            s.hooks.push((HookKind::AfterAll, f));
        })
        .register_fn("before_all", |s: &mut RSuite, f: FnPtr| {
            s.hooks.push((HookKind::BeforeAll, f));
        })
        .register_fn("after_all", |s: &mut RSuite, f: FnPtr| {
            s.hooks.push((HookKind::AfterAll, f));
        })
        .register_fn("before_each", |s: &mut RSuite, f: FnPtr| {
            s.hooks.push((HookKind::BeforeEach, f));
        })
        .register_fn("after_each", |s: &mut RSuite, f: FnPtr| {
            s.hooks.push((HookKind::AfterEach, f));
        });
}

/// Registers the `Ctx` type and its scope, variable, logging and auth methods.
fn register_ctx_state_api(engine: &mut Engine) {
    engine
        .register_type_with_name::<RCx>("Ctx")
        .register_fn("get", |cx: &mut RCx, k: &str| to_dyn(&cx.sync.get(k)))
        .register_fn("set", |cx: &mut RCx, k: &str, v: Dynamic| -> RResult<()> {
            cx.sync.set(k, from_dyn(&v)?).map_err(err)
        })
        .register_fn("env", |cx: &mut RCx, k: &str| -> RResult<String> {
            cx.sync.env(k).map_err(err)
        })
        .register_fn("env", |cx: &mut RCx, k: &str, default: &str| -> String {
            cx.sync.env_opt(k).unwrap_or_else(|| default.to_string())
        })
        .register_fn("header", |cx: &mut RCx, k: &str, v: &str| {
            cx.sync.header(k, v);
        })
        .register_fn("log", |cx: &mut RCx, m: Dynamic| {
            cx.sync.log(&m.to_string());
        })
        .register_fn("trace", |cx: &mut RCx| to_dyn(&cx.sync.trace()))
        .register_fn("bearer", |cx: &mut RCx, t: &str| cx.sync.bearer(t))
        .register_fn("basic", |cx: &mut RCx, u: &str, p: &str| {
            cx.sync.basic(u, p);
        })
        .register_fn("auth", |cx: &mut RCx, a: Map| -> RResult<()> {
            cx.sync.auth(from_dyn(&Dynamic::from_map(a))?).map_err(err)
        })
        .register_fn("vu", |cx: &mut RCx| {
            cx.sync
                .vu()
                .map_or(-1, |v| i64::try_from(v).unwrap_or(i64::MAX))
        });
}

/// Registers `cx.http`, `cx.op` and `cx.defer`.
fn register_ctx_request_api(engine: &mut Engine) {
    engine
        .register_fn(
            "http",
            |cx: &mut RCx, m: &str, p: &str| -> RResult<Dynamic> {
                to_dyn(&cx.sync.http(m, p, Value::Null).map_err(err)?)
            },
        )
        .register_fn(
            "http",
            |cx: &mut RCx, m: &str, p: &str, o: Map| -> RResult<Dynamic> {
                let opts = from_dyn(&Dynamic::from_map(o))?;
                to_dyn(&cx.sync.http(m, p, opts).map_err(err)?)
            },
        )
        .register_fn(
            "op",
            |cx: &mut RCx, c: &str, op: &str| -> RResult<Dynamic> {
                to_dyn(&cx.sync.call(c, op, Value::Null).map_err(err)?)
            },
        )
        .register_fn(
            "op",
            |cx: &mut RCx, c: &str, op: &str, o: Map| -> RResult<Dynamic> {
                let opts = from_dyn(&Dynamic::from_map(o))?;
                to_dyn(&cx.sync.call(c, op, opts).map_err(err)?)
            },
        )
        .register_fn("defer", |cx: &mut RCx, f: FnPtr| {
            let rt = cx.rt.clone();
            let call = blocking_fn(move |sync| invoke_unit(&rt, &f, sync));
            cx.sync
                .cx
                .defer_boxed("rhai defer", Box::new(move |c| call(c)));
        });
}

/// Registers `response.expect(status)`, `assert`, `assert_eq` and `fail`.
fn register_assertion_api(engine: &mut Engine) {
    engine
        .register_fn("expect", |r: &mut Map, status: i64| -> RResult<Map> {
            let got = r.get("status").and_then(|s| s.as_int().ok()).unwrap_or(-1);
            if got != status {
                let text = r
                    .get("text")
                    .map(std::string::ToString::to_string)
                    .unwrap_or_default();
                return Err(format!(
                    "expected status {status}, got {got}: {}",
                    truncate(&text, 300)
                )
                .into());
            }

            Ok(r.clone())
        })
        .register_fn("assert", |c: bool| -> RResult<()> {
            if c {
                Ok(())
            } else {
                Err("assertion failed".into())
            }
        })
        .register_fn("assert", |c: bool, msg: &str| -> RResult<()> {
            if c {
                Ok(())
            } else {
                Err(format!("assertion failed: {msg}").into())
            }
        })
        .register_fn("assert_eq", |a: Dynamic, b: Dynamic| -> RResult<()> {
            let (x, y) = (from_dyn(&a)?, from_dyn(&b)?);
            if x == y {
                Ok(())
            } else {
                Err(format!("assert_eq failed: {x} != {y}").into())
            }
        })
        .register_fn("fail", |msg: &str| -> RResult<()> {
            Err(msg.to_string().into())
        });
}

/// The first `n` characters of `s` (not bytes, so multi-byte text never splits).
fn truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Calls a fixture setup closure with a fresh `cx` and returns its value as JSON.
fn invoke(rt: &Arc<Runtime>, f: &FnPtr, sync: SyncCtx) -> Result<Value> {
    let cx = RCx {
        sync,
        rt: rt.clone(),
    };
    let out: Dynamic = f
        .call(&rt.engine, &rt.ast, (cx,))
        .map_err(|e| anyhow!("{e}"))?;

    from_dyn(&out).map_err(|e| anyhow!("callback returned a value that is not JSON-like: {e}"))
}

/// Loads a `.rhai` file and returns the suite(s) it evaluates to.
///
/// Relative contract paths resolve against the file's directory.
///
/// # Errors
///
/// Fails when the file cannot be read, plus every error of [`load_str`].
pub fn load(path: &Path) -> Result<Vec<Suite>> {
    let src =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    load_str(&src, path.parent().unwrap_or(Path::new(".")))
}

/// Evaluates Rhai source; relative contract paths resolve against `base_dir`.
///
/// # Errors
///
/// Fails when the source does not compile, raises a runtime error, evaluates to
/// something other than a suite or an array of suites, or a suite is invalid
/// (unreadable contract, unknown fixture scope or contract mode, bad test options).
pub fn load_str(src: &str, base_dir: &Path) -> Result<Vec<Suite>> {
    let engine = build_engine();
    let ast = engine
        .compile(src)
        .map_err(|e| anyhow!("rhai compile error: {e}"))?;
    let out: Dynamic = engine
        .eval_ast(&ast)
        .map_err(|e| anyhow!("rhai error: {e}"))?;
    let defs = suite_defs(out)?;

    let rt = Arc::new(Runtime { engine, ast });
    defs.into_iter()
        .map(|d| convert(d, &rt, base_dir))
        .collect()
}

/// Unpacks the script's result: a single suite or an array of suites.
fn suite_defs(out: Dynamic) -> Result<Vec<RSuite>> {
    if out.is::<RSuite>() {
        return Ok(vec![out.cast::<RSuite>()]);
    }

    if !out.is_array() {
        bail!(
            "a .rhai suite file must evaluate to a suite (end the script with `s`) or an array of suites"
        );
    }

    out.cast::<Array>()
        .into_iter()
        .map(|d| {
            d.try_cast::<RSuite>()
                .ok_or_else(|| anyhow!("array element is not a suite"))
        })
        .collect()
}

/// Calls a closure with a fresh `cx`, discarding its return value.
fn invoke_unit(rt: &Arc<Runtime>, f: &FnPtr, sync: SyncCtx) -> Result<()> {
    let cx = RCx {
        sync,
        rt: rt.clone(),
    };

    f.call::<Dynamic>(&rt.engine, &rt.ast, (cx,))
        .map(|_| ())
        .map_err(|e| anyhow!("{e}"))
}

/// Wraps a closure as a unit-returning [`crate::suite::CtxFn`] on the blocking pool.
fn callback(rt: &Arc<Runtime>, f: FnPtr) -> crate::suite::CtxFn<()> {
    let rt = rt.clone();
    blocking_fn(move |sync| invoke_unit(&rt, &f, sync))
}

/// Turns a script-side suite definition into a runnable [`Suite`].
///
/// The suite keeps `rt` alive for as long as it exists.
fn convert(d: RSuite, rt: &Arc<Runtime>, base: &Path) -> Result<Suite> {
    let mut s = Suite::new(&d.name);
    for (name, path, opts) in d.contracts {
        s.add_contract(convert_contract(&name, &path, &opts, base)?);
    }

    for (name, scope, setup, teardown) in d.fixtures {
        let rt2 = rt.clone();
        let setup = blocking_fn(move |sync| invoke(&rt2, &setup, sync));
        s.add_fixture(
            &name,
            scope.parse::<Scope>()?,
            setup,
            teardown.map(|t| callback(rt, t)),
        );
    }

    for (kind, f) in d.hooks {
        s.add_hook(kind, callback(rt, f));
    }

    for t in d.tests {
        let opts = convert_test_opts(&t.name, t.opts, rt)?;
        let body = t
            .body
            .ok_or_else(|| anyhow!("test '{}' has no body", t.name))?;
        s.add_test(&t.name, opts, callback(rt, body));
    }

    s.keep_alive(rt.clone());
    Ok(s)
}

/// Pins a contract from `path` (relative to `base`) with the optional
/// `live_spec` URL and compatibility `mode` from `opts`.
fn convert_contract(name: &str, path: &str, opts: &Map, base: &Path) -> Result<Contract> {
    let mut c = Contract::pinned(name, resolve_relative(base, path))?;
    if let Some(u) = opts
        .get("live_spec")
        .and_then(|v| v.clone().into_string().ok())
    {
        c = c.live_spec(u);
    }

    if let Some(m) = opts.get("mode").and_then(|v| v.clone().into_string().ok()) {
        c = c.mode(m.parse::<Compat>()?);
    }

    Ok(c)
}

/// Builds [`TestOpts`] from the options map of test `name`: the `setup`/`teardown`
/// closures become callbacks, everything else goes through
/// [`super::apply_test_opts`].
fn convert_test_opts(name: &str, mut o: Map, rt: &Arc<Runtime>) -> Result<TestOpts> {
    let mut opts = TestOpts::default();
    if let Some(f) = o.remove("setup").and_then(rhai::Dynamic::try_cast::<FnPtr>) {
        opts.setup = Some(callback(rt, f));
    }

    if let Some(f) = o
        .remove("teardown")
        .and_then(rhai::Dynamic::try_cast::<FnPtr>)
    {
        opts.teardown = Some(callback(rt, f));
    }

    let rest =
        from_dyn(&Dynamic::from_map(o)).map_err(|e| anyhow!("test '{name}' options: {e}"))?;
    super::apply_test_opts(&mut opts, &rest).with_context(|| format!("test '{name}' options"))?;
    Ok(opts)
}
