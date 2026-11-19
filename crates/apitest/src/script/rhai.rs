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
use crate::suite::{Concurrency, HookKind, Suite, TestOpts};

type RResult<T> = std::result::Result<T, Box<EvalAltResult>>;

struct Runtime {
    engine: Engine,
    ast: AST,
}

/// The `cx` object passed to Rhai callbacks.
#[derive(Clone)]
struct RCx {
    sync: SyncCtx,
    rt: Arc<Runtime>,
}

#[derive(Clone, Default)]
struct RTestDef {
    name: String,
    body: Option<FnPtr>,
    opts: Map,
}

/// Suite definition built by the script.
#[derive(Clone, Default)]
struct RSuite {
    name: String,
    contracts: Vec<(String, String, Map)>,
    fixtures: Vec<(String, String, FnPtr, Option<FnPtr>)>,
    hooks: Vec<(HookKind, FnPtr)>,
    tests: Vec<RTestDef>,
}

fn err<E: std::fmt::Display>(e: E) -> Box<EvalAltResult> {
    format!("{e:#}").into()
}

fn to_dyn(v: &Value) -> RResult<Dynamic> {
    rhai::serde::to_dynamic(v)
}

fn from_dyn(d: &Dynamic) -> RResult<Value> {
    if d.is_unit() {
        return Ok(Value::Null);
    }
    rhai::serde::from_dynamic(d)
}

fn build_engine() -> Engine {
    let mut engine = Engine::new();
    engine.set_max_expr_depths(128, 64);
    engine.on_print(|s| eprintln!("  [rhai] {s}"));

    // --- suite definition ---
    engine
        .register_type_with_name::<RSuite>("Suite")
        .register_fn("suite", |name: &str| RSuite {
            name: name.into(),
            ..Default::default()
        })
        .register_fn("contract", |s: &mut RSuite, name: &str, path: &str| {
            s.contracts.push((name.into(), path.into(), Map::new()))
        })
        .register_fn(
            "contract",
            |s: &mut RSuite, name: &str, path: &str, opts: Map| {
                s.contracts.push((name.into(), path.into(), opts))
            },
        )
        .register_fn(
            "fixture",
            |s: &mut RSuite, name: &str, scope: &str, f: FnPtr| {
                s.fixtures.push((name.into(), scope.into(), f, None))
            },
        )
        .register_fn(
            "fixture",
            |s: &mut RSuite, name: &str, scope: &str, f: FnPtr, td: FnPtr| {
                s.fixtures.push((name.into(), scope.into(), f, Some(td)))
            },
        )
        .register_fn("setup", |s: &mut RSuite, f: FnPtr| {
            s.hooks.push((HookKind::BeforeAll, f))
        })
        .register_fn("teardown", |s: &mut RSuite, f: FnPtr| {
            s.hooks.push((HookKind::AfterAll, f))
        })
        .register_fn("before_all", |s: &mut RSuite, f: FnPtr| {
            s.hooks.push((HookKind::BeforeAll, f))
        })
        .register_fn("after_all", |s: &mut RSuite, f: FnPtr| {
            s.hooks.push((HookKind::AfterAll, f))
        })
        .register_fn("before_each", |s: &mut RSuite, f: FnPtr| {
            s.hooks.push((HookKind::BeforeEach, f))
        })
        .register_fn("after_each", |s: &mut RSuite, f: FnPtr| {
            s.hooks.push((HookKind::AfterEach, f))
        })
        .register_fn("test", |s: &mut RSuite, name: &str, f: FnPtr| {
            s.tests.push(RTestDef {
                name: name.into(),
                body: Some(f),
                opts: Map::new(),
            })
        })
        .register_fn("test", |s: &mut RSuite, name: &str, opts: Map, f: FnPtr| {
            s.tests.push(RTestDef {
                name: name.into(),
                body: Some(f),
                opts,
            })
        });

    // --- cx ---
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
            cx.sync.header(k, v)
        })
        .register_fn("log", |cx: &mut RCx, m: Dynamic| {
            cx.sync.log(&m.to_string())
        })
        .register_fn("trace", |cx: &mut RCx| to_dyn(&cx.sync.trace()))
        .register_fn("bearer", |cx: &mut RCx, t: &str| cx.sync.bearer(t))
        .register_fn("basic", |cx: &mut RCx, u: &str, p: &str| {
            cx.sync.basic(u, p)
        })
        .register_fn("auth", |cx: &mut RCx, a: Map| -> RResult<()> {
            cx.sync.auth(from_dyn(&Dynamic::from_map(a))?).map_err(err)
        })
        .register_fn("vu", |cx: &mut RCx| {
            cx.sync.vu().map(|v| v as i64).unwrap_or(-1)
        })
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

    // --- responses & assertions ---
    engine
        .register_fn("expect", |r: &mut Map, status: i64| -> RResult<Map> {
            let got = r.get("status").and_then(|s| s.as_int().ok()).unwrap_or(-1);
            if got != status {
                let text = r.get("text").map(|t| t.to_string()).unwrap_or_default();
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
    engine
}

fn truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

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
pub fn load(path: &Path) -> Result<Vec<Suite>> {
    let src =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    load_str(&src, path.parent().unwrap_or(Path::new(".")))
}

/// Evaluates Rhai source; relative contract paths resolve against `base_dir`.
pub fn load_str(src: &str, base_dir: &Path) -> Result<Vec<Suite>> {
    let engine = build_engine();
    let ast = engine
        .compile(src)
        .map_err(|e| anyhow!("rhai compile error: {e}"))?;
    let out: Dynamic = engine
        .eval_ast(&ast)
        .map_err(|e| anyhow!("rhai error: {e}"))?;
    let defs: Vec<RSuite> = if out.is::<RSuite>() {
        vec![out.cast::<RSuite>()]
    } else if out.is_array() {
        out.cast::<Array>()
            .into_iter()
            .map(|d| {
                d.try_cast::<RSuite>()
                    .ok_or_else(|| anyhow!("array element is not a suite"))
            })
            .collect::<Result<_>>()?
    } else {
        bail!(
            "a .rhai suite file must evaluate to a suite (end the script with `s`) or an array of suites"
        );
    };
    let rt = Arc::new(Runtime { engine, ast });
    defs.into_iter()
        .map(|d| convert(d, &rt, base_dir))
        .collect()
}

fn invoke_unit(rt: &Arc<Runtime>, f: &FnPtr, sync: SyncCtx) -> Result<()> {
    let cx = RCx {
        sync,
        rt: rt.clone(),
    };
    f.call::<Dynamic>(&rt.engine, &rt.ast, (cx,))
        .map(|_| ())
        .map_err(|e| anyhow!("{e}"))
}

fn callback(rt: &Arc<Runtime>, f: FnPtr) -> crate::suite::CtxFn<()> {
    let rt = rt.clone();
    blocking_fn(move |sync| invoke_unit(&rt, &f, sync))
}

fn convert(d: RSuite, rt: &Arc<Runtime>, base: &Path) -> Result<Suite> {
    let mut s = Suite::new(&d.name);
    for (name, path, opts) in d.contracts {
        let mut c = Contract::pinned(&name, resolve_relative(base, &path))?;
        if let Some(u) = opts
            .get("live_spec")
            .and_then(|v| v.clone().into_string().ok())
        {
            c = c.live_spec(u);
        }
        if let Some(m) = opts.get("mode").and_then(|v| v.clone().into_string().ok()) {
            c = c.mode(m.parse::<Compat>()?);
        }
        s.add_contract(c);
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
        let mut opts = TestOpts::default();
        let mut o = t.opts;
        if let Some(f) = o.remove("setup").and_then(|v| v.try_cast::<FnPtr>()) {
            opts.setup = Some(callback(rt, f));
        }
        if let Some(f) = o.remove("teardown").and_then(|v| v.try_cast::<FnPtr>()) {
            opts.teardown = Some(callback(rt, f));
        }
        let rest = from_dyn(&Dynamic::from_map(o))
            .map_err(|e| anyhow!("test '{}' options: {e}", t.name))?;
        apply_opts(&mut opts, &rest).with_context(|| format!("test '{}' options", t.name))?;
        let body = t
            .body
            .ok_or_else(|| anyhow!("test '{}' has no body", t.name))?;
        s.add_test(&t.name, opts, callback(rt, body));
    }
    s.keep_alive(rt.clone());
    Ok(s)
}

/// Applies `{ parallel, resources, timeout_ms, tags }` to [`TestOpts`]. Shared with the JS frontend.
pub(crate) fn apply_opts(opts: &mut TestOpts, v: &Value) -> Result<()> {
    super::apply_test_opts(opts, v)
}

#[allow(dead_code)]
fn _assert_send_sync() {
    fn is<T: Send + Sync>() {}
    is::<Runtime>();
    is::<RCx>();
    let _ = Concurrency::Serial;
}
