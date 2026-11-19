//! JavaScript frontend (QuickJS via rquickjs).
//!
//! Scripts are synchronous like k6 scripts: `cx.http(...)` blocks the script
//! and returns the response. Each worker thread owns an independent QuickJS
//! runtime that evaluated the whole script ("init code runs per worker"). Under
//! load there is one worker per VU, so VUs run truly in parallel.
//!
//! ```js
//! const s = suite("billing");
//! s.contract("billing", "../specs/billing-1.0.yaml", { liveSpec: "/openapi.json" });
//! s.fixture("token", "suite",
//!   (cx) => cx.http("POST", "/auth/token", { form: { user: cx.env("API_USER"), pass: cx.env("API_PASS") } })
//!             .expect(200).body.access_token,
//!   (cx) => cx.http("POST", "/auth/revoke", { bearer: cx.get("token") }));
//! s.test("create invoice", { parallel: true }, (cx) => {
//!   const inv = cx.call("billing", "createInvoice", { json: { amount: 4200, currency: "EUR" } }).expect(201).body;
//!   cx.defer(() => cx.call("billing", "deleteInvoice", { path: { id: inv.id } }).expect(204));
//! });
//! ```

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Weak};

use anyhow::{Context as _, Result, anyhow};
use rquickjs::{Context, Ctx, Exception, Function, Object, Runtime};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::oneshot;

use super::{SyncCtx, apply_test_opts, resolve_relative};
use crate::context::Scope;
use crate::contract::{Compat, Contract};
use crate::suite::{BoxFut, CtxFn, HookKind, Suite, TestOpts};

const PRELUDE: &str = r#"
"use strict";
const __fns = [];
const __dyn = new Map();
let __nextDyn = 1e9;
const __suites = [];
function __reg(f, what) {
  if (typeof f !== "function") throw new TypeError(`${what}: expected a function, got ${typeof f}`);
  __fns.push(f);
  return __fns.length - 1;
}
globalThis.suite = function (name) {
  const def = { name, contracts: [], fixtures: [], hooks: [], tests: [] };
  __suites.push(def);
  const hook = (kind) => (f) => { def.hooks.push({ kind, id: __reg(f, kind) }); return api; };
  const api = {
    contract(name, path, opts = {}) {
      def.contracts.push({ name, path, live_spec: opts.liveSpec ?? opts.live_spec ?? null, mode: opts.mode ?? null });
      return api;
    },
    fixture(name, scope, setup, teardown) {
      def.fixtures.push({ name, scope, setup: __reg(setup, `fixture ${name}`),
        teardown: teardown === undefined ? null : __reg(teardown, `fixture ${name} teardown`) });
      return api;
    },
    test(name, a, b) {
      const [opts, body] = typeof a === "function" ? [{}, a] : [a ?? {}, b];
      const { setup, teardown, ...rest } = opts;
      def.tests.push({ name, id: __reg(body, `test ${name}`), opts: rest,
        setup: setup === undefined ? null : __reg(setup, `test ${name} setup`),
        teardown: teardown === undefined ? null : __reg(teardown, `test ${name} teardown`) });
      return api;
    },
    setup: hook("before_all"), teardown: hook("after_all"),
    beforeAll: hook("before_all"), afterAll: hook("after_all"),
    beforeEach: hook("before_each"), afterEach: hook("after_each"),
  };
  return api;
};
function __resp(r) {
  r.expect = function (status) {
    if (r.status !== status) throw new Error(`expected status ${status}, got ${r.status}: ${String(r.text).slice(0, 300)}`);
    return r;
  };
  return r;
}
function __cx(n) {
  return {
    get: (k) => JSON.parse(n.get(k)),
    set: (k, v) => n.set(k, JSON.stringify(v === undefined ? null : v)),
    env: (k, dflt) => (dflt === undefined ? n.env(k) : n.envOr(k, String(dflt))),
    header: (k, v) => n.header(k, String(v)),
    log: (...a) => n.log(a.map((x) => (typeof x === "string" ? x : JSON.stringify(x))).join(" ")),
    vu: () => n.vu(),
    trace: () => JSON.parse(n.trace()),
    bearer: (t) => n.bearer(String(t)),
    basic: (u, p) => n.basic(String(u), String(p)),
    auth: (a) => n.auth(JSON.stringify(a)),
    http: (m, p, o) => __resp(JSON.parse(n.http(m, p, JSON.stringify(o ?? {})))),
    call: (c, op, o) => __resp(JSON.parse(n.call(c, op, JSON.stringify(o ?? {})))),
    op: (c, op, o) => __resp(JSON.parse(n.call(c, op, JSON.stringify(o ?? {})))),
    defer: (f) => { if (typeof f !== "function") throw new TypeError("defer: expected a function"); const id = __nextDyn++; __dyn.set(id, f); n.defer(id); },
  };
}
globalThis.__invoke = function (id, n) {
  let f;
  if (id >= 1e9) { f = __dyn.get(id); __dyn.delete(id); } else { f = __fns[id]; }
  const r = f(__cx(n));
  return JSON.stringify(r === undefined ? null : r);
};
globalThis.assert = (c, m) => { if (!c) throw new Error(m ?? "assertion failed"); };
globalThis.assertEq = (a, b, m) => {
  const x = JSON.stringify(a), y = JSON.stringify(b);
  if (x !== y) throw new Error(`${m ?? "assertEq failed"}: ${x} !== ${y}`);
};
globalThis.console = { log: (...a) => __print(a.map((x) => (typeof x === "string" ? x : JSON.stringify(x))).join(" ")) };
"#;

struct Job {
    id: u64,
    sync: SyncCtx,
    reply: oneshot::Sender<Result<Value>>,
}

/// A pool of QuickJS runtimes that all evaluated the same script.
struct Pool {
    senders: Vec<mpsc::Sender<Job>>,
    in_flight: Vec<AtomicUsize>,
}

impl Pool {
    /// Picks the least busy worker (ties → the VU's "home" worker).
    fn pick(&self, vu: Option<usize>) -> usize {
        let n = self.senders.len();
        let home = vu.unwrap_or(0) % n;
        (0..n)
            .map(|k| (home + k) % n)
            .min_by_key(|&w| self.in_flight[w].load(Ordering::Relaxed))
            .unwrap_or(0)
    }
}

/// Async dispatch of function `id` to a worker (fixed, or picked per call).
fn dispatch(pool: &Arc<Pool>, id: u64, fixed: Option<usize>) -> CtxFn<Value> {
    let pool = pool.clone();
    Arc::new(move |cx| -> BoxFut<Value> {
        let pool = pool.clone();
        Box::pin(async move {
            let w = fixed.unwrap_or_else(|| pool.pick(cx.vu()));
            let (tx, rx) = oneshot::channel();
            pool.in_flight[w].fetch_add(1, Ordering::Relaxed);
            let sent = pool.senders[w].send(Job {
                id,
                sync: SyncCtx::new(cx),
                reply: tx,
            });
            let result = match sent {
                Ok(()) => rx
                    .await
                    .map_err(|_| anyhow!("JS worker {w} stopped"))
                    .and_then(|r| r),
                Err(_) => Err(anyhow!("JS worker {w} is gone")),
            };
            pool.in_flight[w].fetch_sub(1, Ordering::Relaxed);
            result
        })
    })
}

fn unit(f: CtxFn<Value>) -> CtxFn<()> {
    Arc::new(move |cx| -> BoxFut<()> {
        let fut = f(cx);
        Box::pin(async move { fut.await.map(|_| ()) })
    })
}

fn js_error(ctx: &Ctx<'_>, e: rquickjs::Error) -> anyhow::Error {
    if matches!(e, rquickjs::Error::Exception) {
        let v = ctx.catch();
        if let Some(ex) = v.as_exception() {
            let msg = ex.message().unwrap_or_default();
            let stack = ex.stack().unwrap_or_default();
            return if stack.trim().is_empty() {
                anyhow!("{msg}")
            } else {
                anyhow!("{msg}\n{}", stack.trim_end())
            };
        }
        let s: Option<String> = ctx
            .json_stringify(v)
            .ok()
            .flatten()
            .and_then(|s| s.to_string().ok());
        return anyhow!("uncaught: {}", s.unwrap_or_else(|| "<value>".into()));
    }
    anyhow!("{e}")
}

fn install_natives<'js>(
    ctx: &Ctx<'js>,
    n: &Object<'js>,
    sync: &SyncCtx,
    pool: &Weak<Pool>,
    worker: usize,
) -> rquickjs::Result<()> {
    fn throw<'js>(ctx: &Ctx<'js>, e: anyhow::Error) -> rquickjs::Error {
        Exception::throw_message(ctx, &format!("{e:#}"))
    }
    let s = sync.clone();
    n.set(
        "get",
        Function::new(ctx.clone(), move |k: String| -> String {
            s.get(&k).to_string()
        })?,
    )?;
    let s = sync.clone();
    n.set(
        "set",
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, k: String, v: String| -> rquickjs::Result<()> {
                let v: Value = serde_json::from_str(&v).map_err(|e| throw(&ctx, e.into()))?;
                s.set(&k, v).map_err(|e| throw(&ctx, e))
            },
        )?,
    )?;
    let s = sync.clone();
    n.set(
        "env",
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, k: String| -> rquickjs::Result<String> {
                s.env(&k).map_err(|e| throw(&ctx, e))
            },
        )?,
    )?;
    let s = sync.clone();
    n.set(
        "envOr",
        Function::new(ctx.clone(), move |k: String, d: String| -> String {
            s.env_opt(&k).unwrap_or(d)
        })?,
    )?;
    let s = sync.clone();
    n.set(
        "header",
        Function::new(ctx.clone(), move |k: String, v: String| s.header(&k, &v))?,
    )?;
    let s = sync.clone();
    n.set(
        "log",
        Function::new(ctx.clone(), move |m: String| s.log(&m))?,
    )?;
    let s = sync.clone();
    n.set(
        "vu",
        Function::new(ctx.clone(), move || -> i32 {
            s.vu().map(|v| v as i32).unwrap_or(-1)
        })?,
    )?;
    let s = sync.clone();
    n.set(
        "trace",
        Function::new(ctx.clone(), move || -> String { s.trace().to_string() })?,
    )?;
    let s = sync.clone();
    n.set(
        "bearer",
        Function::new(ctx.clone(), move |t: String| s.bearer(&t))?,
    )?;
    let s = sync.clone();
    n.set(
        "basic",
        Function::new(ctx.clone(), move |u: String, p: String| s.basic(&u, &p))?,
    )?;
    let s = sync.clone();
    n.set(
        "auth",
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, a: String| -> rquickjs::Result<()> {
                let v: Value = serde_json::from_str(&a).map_err(|e| throw(&ctx, e.into()))?;
                s.auth(v).map_err(|e| throw(&ctx, e))
            },
        )?,
    )?;
    let s = sync.clone();
    n.set(
        "http",
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, m: String, p: String, o: String| -> rquickjs::Result<String> {
                let o: Value = serde_json::from_str(&o).map_err(|e| throw(&ctx, e.into()))?;
                s.http(&m, &p, o)
                    .map(|r| r.to_string())
                    .map_err(|e| throw(&ctx, e))
            },
        )?,
    )?;
    let s = sync.clone();
    n.set(
        "call",
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, c: String, op: String, o: String| -> rquickjs::Result<String> {
                let o: Value = serde_json::from_str(&o).map_err(|e| throw(&ctx, e.into()))?;
                s.call(&c, &op, o)
                    .map(|r| r.to_string())
                    .map_err(|e| throw(&ctx, e))
            },
        )?,
    )?;
    let s = sync.clone();
    let weak = pool.clone();
    n.set(
        "defer",
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, id: f64| -> rquickjs::Result<()> {
                let pool = weak
                    .upgrade()
                    .ok_or_else(|| throw(&ctx, anyhow!("JS pool is shutting down")))?;
                // The closure lives in *this* runtime, so the cleanup must run on this worker.
                let f = unit(dispatch(&pool, id as u64, Some(worker)));
                s.cx.defer_boxed("js defer", Box::new(move |c| f(c)));
                Ok(())
            },
        )?,
    )?;
    Ok(())
}

fn worker_main(
    idx: usize,
    src: String,
    pool: Weak<Pool>,
    rx: mpsc::Receiver<Job>,
    init: mpsc::Sender<Result<Option<String>>>,
) {
    let setup = || -> Result<(Runtime, Context)> {
        let rt = Runtime::new()?;
        let ctx = Context::full(&rt)?;
        Ok((rt, ctx))
    };
    let (_rt, context) = match setup() {
        Ok(v) => v,
        Err(e) => {
            let _ = init.send(Err(e));
            return;
        }
    };
    let defs = context.with(|ctx| -> Result<String> {
        ctx.globals()
            .set(
                "__print",
                Function::new(ctx.clone(), |m: String| eprintln!("  [js] {m}"))?,
            )
            .map_err(|e| js_error(&ctx, e))?;
        ctx.eval::<(), _>(PRELUDE)
            .map_err(|e| js_error(&ctx, e))
            .context("prelude")?;
        ctx.eval::<(), _>(src.as_bytes().to_vec())
            .map_err(|e| js_error(&ctx, e))?;
        ctx.eval::<String, _>("JSON.stringify(__suites)")
            .map_err(|e| js_error(&ctx, e))
    });
    let ok = defs.is_ok();
    let _ = init.send(defs.map(|d| (idx == 0).then_some(d)));
    if !ok {
        return;
    }
    while let Ok(job) = rx.recv() {
        let result = context.with(|ctx| -> Result<Value> {
            let n = Object::new(ctx.clone()).map_err(|e| js_error(&ctx, e))?;
            install_natives(&ctx, &n, &job.sync, &pool, idx).map_err(|e| js_error(&ctx, e))?;
            let invoke: Function = ctx
                .globals()
                .get("__invoke")
                .map_err(|e| js_error(&ctx, e))?;
            let out: String = invoke
                .call((job.id as f64, n))
                .map_err(|e| js_error(&ctx, e))?;
            serde_json::from_str(&out).context("JS callback returned a non-JSON value")
        });
        let _ = job.reply.send(result);
    }
}

#[derive(Deserialize)]
struct JsContract {
    name: String,
    path: String,
    live_spec: Option<String>,
    mode: Option<String>,
}

#[derive(Deserialize)]
struct JsFixture {
    name: String,
    scope: String,
    setup: u64,
    teardown: Option<u64>,
}

#[derive(Deserialize)]
struct JsHook {
    kind: String,
    id: u64,
}

#[derive(Deserialize)]
struct JsTest {
    name: String,
    id: u64,
    #[serde(default)]
    opts: Value,
    setup: Option<u64>,
    teardown: Option<u64>,
}

#[derive(Deserialize)]
struct JsSuite {
    name: String,
    contracts: Vec<JsContract>,
    fixtures: Vec<JsFixture>,
    hooks: Vec<JsHook>,
    tests: Vec<JsTest>,
}

/// Loads a `.js` suite file with `workers` independent runtimes.
pub fn load(path: &Path, workers: usize) -> Result<Vec<Suite>> {
    let src =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    load_str(&src, path.parent().unwrap_or(Path::new(".")), workers)
        .with_context(|| format!("loading {}", path.display()))
}

pub fn load_str(src: &str, base: &Path, workers: usize) -> Result<Vec<Suite>> {
    let workers = workers.max(1);
    let mut receivers = vec![];
    let mut senders = vec![];
    for _ in 0..workers {
        let (tx, rx) = mpsc::channel::<Job>();
        senders.push(tx);
        receivers.push(rx);
    }
    let pool = Arc::new(Pool {
        senders,
        in_flight: (0..workers).map(|_| AtomicUsize::new(0)).collect(),
    });
    let (init_tx, init_rx) = mpsc::channel();
    for (idx, rx) in receivers.into_iter().enumerate() {
        let (src, weak, init) = (src.to_string(), Arc::downgrade(&pool), init_tx.clone());
        std::thread::Builder::new()
            .name(format!("apitest-js-{idx}"))
            .spawn(move || worker_main(idx, src, weak, rx, init))?;
    }
    drop(init_tx);
    let mut defs = None;
    for _ in 0..workers {
        match init_rx.recv() {
            Ok(Ok(Some(d))) => defs = Some(d),
            Ok(Ok(None)) => {}
            Ok(Err(e)) => return Err(e),
            Err(_) => return Err(anyhow!("JS worker died during init")),
        }
    }
    let defs: Vec<JsSuite> =
        serde_json::from_str(&defs.ok_or_else(|| anyhow!("no suite definitions"))?)?;
    if defs.is_empty() {
        anyhow::bail!("script defined no suites (call `suite(\"name\")`)");
    }

    let mut suites = vec![];
    for d in defs {
        let mut s = Suite::new(&d.name);
        for c in d.contracts {
            let mut contract = Contract::pinned(&c.name, resolve_relative(base, &c.path))?;
            if let Some(u) = c.live_spec {
                contract = contract.live_spec(u);
            }
            if let Some(m) = c.mode {
                contract = contract.mode(m.parse::<Compat>()?);
            }
            s.add_contract(contract);
        }
        for f in d.fixtures {
            s.add_fixture(
                &f.name,
                f.scope.parse::<Scope>()?,
                dispatch(&pool, f.setup, None),
                f.teardown.map(|t| unit(dispatch(&pool, t, None))),
            );
        }
        for h in d.hooks {
            s.add_hook(
                h.kind.parse::<HookKind>()?,
                unit(dispatch(&pool, h.id, None)),
            );
        }
        for t in d.tests {
            let mut opts = TestOpts::default();
            apply_test_opts(&mut opts, &t.opts)
                .with_context(|| format!("test '{}' options", t.name))?;
            opts.setup = t.setup.map(|id| unit(dispatch(&pool, id, None)));
            opts.teardown = t.teardown.map(|id| unit(dispatch(&pool, id, None)));
            s.add_test(&t.name, opts, unit(dispatch(&pool, t.id, None)));
        }
        s.keep_alive(pool.clone());
        suites.push(s);
    }
    Ok(suites)
}
