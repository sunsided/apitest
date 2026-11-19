//! Python bindings for apitest (`apitest._native`).
//!
//! Python callbacks run on tokio's blocking pool and take the GIL only while
//! Python code runs; HTTP calls release it, so parallel tests and load VUs
//! overlap on I/O. Values cross the boundary as JSON.

#![forbid(unsafe_code)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyTraceback};

use apitest::contract::{Compat, Contract};
use apitest::load::{Executor, LoadPlan, Scenario, Stage, Threshold, parse_duration, run_load};
use apitest::report::junit_xml;
use apitest::script::{SyncCtx, blocking_fn};
use apitest::suite::{CtxFn, HookKind, Suite, TestOpts};
use apitest::{RunConfig, Scope, run_suite};
use serde_json::Value;

fn rt_err(e: anyhow::Error) -> PyErr {
    PyRuntimeError::new_err(format!("{e:#}"))
}

fn to_json(py: Python<'_>, v: &Bound<'_, PyAny>) -> PyResult<Value> {
    if v.is_none() {
        return Ok(Value::Null);
    }
    let s: String = py.import("json")?.call_method1("dumps", (v,))?.extract()?;
    serde_json::from_str(&s).map_err(|e| PyValueError::new_err(e.to_string()))
}

fn from_json<'py>(py: Python<'py>, v: &Value) -> PyResult<Bound<'py, PyAny>> {
    py.import("json")?.call_method1("loads", (v.to_string(),))
}

fn py_to_anyhow(py: Python<'_>, e: PyErr) -> anyhow::Error {
    let tb = e
        .traceback(py)
        .and_then(|tb: Bound<'_, PyTraceback>| tb.format().ok())
        .unwrap_or_default();
    if tb.is_empty() {
        anyhow::anyhow!("{e}")
    } else {
        anyhow::anyhow!("{e}\n{}", tb.trim_end())
    }
}

/// Wraps a Python callable `f(cx) -> value` into a core callback.
fn py_value_fn(f: Py<PyAny>) -> CtxFn<Value> {
    let f = Arc::new(f);
    blocking_fn(move |sync: SyncCtx| {
        Python::attach(|py| {
            let cx = Py::new(py, PyCtx { sync })?;
            let out = f.call1(py, (cx,))?;
            to_json(py, out.bind(py)).map_err(|e| {
                PyValueError::new_err(format!("fixture values must be JSON-serializable: {e}"))
            })
        })
        .map_err(|e| Python::attach(|py| py_to_anyhow(py, e)))
    })
}

/// Wraps a Python callable `f(cx)` whose return value is ignored (tests, hooks, cleanups).
fn py_unit_fn(f: Py<PyAny>) -> CtxFn<()> {
    let f = Arc::new(f);
    blocking_fn(move |sync: SyncCtx| {
        Python::attach(|py| {
            let cx = Py::new(py, PyCtx { sync })?;
            f.call1(py, (cx,)).map(|_| ())
        })
        .map_err(|e| Python::attach(|py| py_to_anyhow(py, e)))
    })
}

/// Response of `cx.http(...)` / `cx.call(...)`.
#[pyclass(name = "Response", frozen)]
struct PyResponse {
    raw: Value,
}

#[pymethods]
impl PyResponse {
    #[getter]
    fn status(&self) -> u16 {
        self.raw["status"].as_u64().unwrap_or(0) as u16
    }
    #[getter]
    fn text(&self) -> String {
        self.raw["text"].as_str().unwrap_or_default().to_string()
    }
    #[getter]
    fn elapsed_ms(&self) -> f64 {
        self.raw["elapsed_ms"].as_f64().unwrap_or(0.0)
    }
    #[getter]
    fn headers<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        from_json(py, &self.raw["headers"])
    }
    /// Parsed JSON body (or the text if the body was not JSON).
    #[getter]
    fn body<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        from_json(py, &self.raw["body"])
    }
    fn json<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        self.body(py)
    }
    /// The request as sent: `{"method", "url", "headers"}` (incl. traceparent, User-Agent).
    #[getter]
    fn request<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        from_json(py, &self.raw["request"])
    }
    /// The span this request was sent as (`trace_id`, `span_id`, `sampled`), or None.
    #[getter]
    fn trace<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        from_json(py, &self.raw["trace"])
    }
    /// Raises unless the status matches; returns the response for chaining.
    fn expect(slf: Py<Self>, py: Python<'_>, status: u16) -> PyResult<Py<Self>> {
        let me = slf.get();
        if me.status() != status {
            let text: String = me.text().chars().take(300).collect();
            return Err(PyAssertionError(format!(
                "expected status {status}, got {}: {text}",
                me.status()
            )));
        }
        let _ = py;
        Ok(slf)
    }
    fn __repr__(&self) -> String {
        format!("<Response {}>", self.status())
    }
}

#[expect(non_snake_case)]
fn PyAssertionError(msg: String) -> PyErr {
    pyo3::exceptions::PyAssertionError::new_err(msg)
}

/// The context passed to fixtures, hooks and tests.
#[pyclass(name = "Ctx", frozen)]
struct PyCtx {
    sync: SyncCtx,
}

fn opts_value(
    py: Python<'_>,
    opts: Option<&Bound<'_, PyDict>>,
    kwargs: Option<&Bound<'_, PyDict>>,
) -> PyResult<Value> {
    let mut v = match opts {
        Some(o) => to_json(py, o.as_any())?,
        None => Value::Object(Default::default()),
    };
    if let (Some(k), Value::Object(map)) = (kwargs, &mut v)
        && let Value::Object(extra) = to_json(py, k.as_any())?
    {
        map.extend(extra);
    }
    Ok(v)
}

#[pymethods]
impl PyCtx {
    /// A fixture or value from this scope or a parent scope (None if missing).
    fn get<'py>(&self, py: Python<'py>, key: &str) -> PyResult<Bound<'py, PyAny>> {
        from_json(py, &self.sync.get(key))
    }
    fn set(&self, py: Python<'_>, key: &str, value: &Bound<'_, PyAny>) -> PyResult<()> {
        self.sync.set(key, to_json(py, value)?).map_err(rt_err)
    }
    #[pyo3(signature = (key, default=None))]
    fn env(&self, key: &str, default: Option<String>) -> PyResult<String> {
        match (self.sync.env_opt(key), default) {
            (Some(v), _) => Ok(v),
            (None, Some(d)) => Ok(d),
            (None, None) => self.sync.env(key).map_err(rt_err),
        }
    }
    /// Default header for every request from this scope and below.
    fn header(&self, name: &str, value: &str) {
        self.sync.header(name, value);
    }
    fn log(&self, msg: &str) {
        self.sync.log(msg);
    }
    fn vu(&self) -> Option<usize> {
        self.sync.vu()
    }
    /// The current test's trace context (`trace_id`, `sampled`, ...).
    fn trace<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        from_json(py, &self.sync.trace())
    }
    /// `Authorization: Bearer <token>` for the rest of this scope.
    fn bearer(&self, token: &str) {
        self.sync.bearer(token);
    }
    fn basic(&self, username: &str, password: &str) {
        self.sync.basic(username, password);
    }
    /// Any credentials for this scope, e.g. `{"type": "login", "service": "hub", "username": ..., "password": ...}`.
    fn auth(&self, py: Python<'_>, auth: &Bound<'_, PyAny>) -> PyResult<()> {
        self.sync.auth(to_json(py, auth)?).map_err(rt_err)
    }
    #[getter]
    fn path(&self) -> String {
        self.sync.cx.path()
    }
    /// Raw request. Options: path, query, headers, json, form, body, bearer.
    #[pyo3(signature = (method, path, opts=None, **kwargs))]
    fn http(
        &self,
        py: Python<'_>,
        method: &str,
        path: &str,
        opts: Option<&Bound<'_, PyDict>>,
        kwargs: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<PyResponse> {
        let o = opts_value(py, opts, kwargs)?;
        let sync = self.sync.clone();
        let (m, p) = (method.to_string(), path.to_string());
        let raw = py.detach(move || sync.http(&m, &p, o)).map_err(rt_err)?;
        Ok(PyResponse { raw })
    }
    /// Contract call by operationId, validated against the pinned spec.
    #[pyo3(signature = (contract, operation, opts=None, **kwargs))]
    fn call(
        &self,
        py: Python<'_>,
        contract: &str,
        operation: &str,
        opts: Option<&Bound<'_, PyDict>>,
        kwargs: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<PyResponse> {
        let o = opts_value(py, opts, kwargs)?;
        let sync = self.sync.clone();
        let (c, op) = (contract.to_string(), operation.to_string());
        let raw = py.detach(move || sync.call(&c, &op, o)).map_err(rt_err)?;
        Ok(PyResponse { raw })
    }
    /// Alias of `call`, matching the Rhai frontend.
    #[pyo3(signature = (contract, operation, opts=None, **kwargs))]
    fn op(
        &self,
        py: Python<'_>,
        contract: &str,
        operation: &str,
        opts: Option<&Bound<'_, PyDict>>,
        kwargs: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<PyResponse> {
        self.call(py, contract, operation, opts, kwargs)
    }
    /// Registers `f(cx)` to run when the current scope ends (LIFO), whatever happened.
    fn defer(&self, f: Py<PyAny>) {
        let call = py_unit_fn(f);
        self.sync
            .cx
            .defer_boxed("python defer", Box::new(move |c| call(c)));
    }
}

/// A suite under construction. Use the decorator API in `apitest` instead of this directly.
#[pyclass(name = "Suite", frozen)]
struct PySuite {
    inner: Mutex<Suite>,
}

fn parse_scope(s: &str) -> PyResult<Scope> {
    s.parse()
        .map_err(|e: anyhow::Error| PyValueError::new_err(e.to_string()))
}

fn report_dict<'py, T: serde::Serialize>(
    py: Python<'py>,
    r: &T,
    passed: bool,
) -> PyResult<Bound<'py, PyAny>> {
    let mut v = serde_json::to_value(r).map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
    if let Value::Object(m) = &mut v {
        m.insert("passed".into(), Value::Bool(passed));
    }
    from_json(py, &v)
}

fn runtime() -> PyResult<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| PyRuntimeError::new_err(e.to_string()))
}

#[expect(clippy::too_many_arguments)]
fn make_config(
    py: Python<'_>,
    base_url: String,
    vars: Option<&Bound<'_, PyDict>>,
    lockfile: Option<String>,
    update_lock: bool,
    check_drift: bool,
    max_parallel: Option<usize>,
    quiet: bool,
) -> PyResult<RunConfig> {
    let mut cfg = RunConfig::new(base_url)
        .quiet(quiet)
        .update_lock(update_lock);
    cfg.check_drift = check_drift;
    if let Some(l) = lockfile {
        cfg = cfg.lockfile(l);
    }
    if let Some(n) = max_parallel {
        cfg.max_parallel = n;
    }
    if let Some(v) = vars
        && let Value::Object(m) = to_json(py, v.as_any())?
    {
        for (k, v) in m {
            cfg.vars
                .insert(k, v.as_str().map_or_else(|| v.to_string(), str::to_string));
        }
    }
    Ok(cfg)
}

#[pymethods]
impl PySuite {
    #[new]
    fn new(name: &str) -> Self {
        PySuite {
            inner: Mutex::new(Suite::new(name)),
        }
    }

    #[getter]
    fn name(&self) -> String {
        self.inner.lock().unwrap().name.clone()
    }

    #[getter]
    fn tests(&self) -> Vec<String> {
        self.inner
            .lock()
            .unwrap()
            .tests
            .iter()
            .map(|t| t.name.clone())
            .collect()
    }

    #[pyo3(signature = (name, path, live_spec=None, mode=None))]
    fn contract(
        &self,
        name: &str,
        path: &str,
        live_spec: Option<String>,
        mode: Option<String>,
    ) -> PyResult<()> {
        let mut c = Contract::pinned(name, path).map_err(rt_err)?;
        if let Some(u) = live_spec {
            c = c.live_spec(u);
        }
        if let Some(m) = mode {
            c = c.mode(m.parse::<Compat>().map_err(rt_err)?);
        }
        self.inner.lock().unwrap().add_contract(c);
        Ok(())
    }

    #[pyo3(signature = (name, scope, setup, teardown=None))]
    fn add_fixture(
        &self,
        name: &str,
        scope: &str,
        setup: Py<PyAny>,
        teardown: Option<Py<PyAny>>,
    ) -> PyResult<()> {
        let scope = parse_scope(scope)?;
        self.inner.lock().unwrap().add_fixture(
            name,
            scope,
            py_value_fn(setup),
            teardown.map(py_unit_fn),
        );
        Ok(())
    }

    /// kind: `before_all` | `after_all` | `before_each` | `after_each`
    fn add_hook(&self, kind: &str, f: Py<PyAny>) -> PyResult<()> {
        let kind: HookKind = kind.parse().map_err(rt_err)?;
        self.inner.lock().unwrap().add_hook(kind, py_unit_fn(f));
        Ok(())
    }

    #[pyo3(signature = (name, body, parallel=false, resources=None, timeout_ms=None, setup=None, teardown=None, tags=None))]
    #[expect(clippy::too_many_arguments)]
    fn add_test(
        &self,
        name: &str,
        body: Py<PyAny>,
        parallel: bool,
        resources: Option<Vec<String>>,
        timeout_ms: Option<u64>,
        setup: Option<Py<PyAny>>,
        teardown: Option<Py<PyAny>>,
        tags: Option<Vec<String>>,
    ) {
        let mut opts = if parallel {
            TestOpts::parallel()
        } else {
            TestOpts::serial()
        };
        opts.resources = resources.unwrap_or_default();
        opts.timeout = timeout_ms.map(Duration::from_millis);
        opts.setup = setup.map(py_unit_fn);
        opts.teardown = teardown.map(py_unit_fn);
        opts.tags = tags.unwrap_or_default();
        self.inner
            .lock()
            .unwrap()
            .add_test(name, opts, py_unit_fn(body));
    }

    /// Runs the suite and returns the report as a dict (`report["passed"]`).
    #[pyo3(signature = (base_url, vars=None, lockfile=None, update_lock=false, check_drift=true, only=None, max_parallel=None, junit=None, quiet=false))]
    #[expect(clippy::too_many_arguments)]
    fn run<'py>(
        &self,
        py: Python<'py>,
        base_url: String,
        vars: Option<&Bound<'py, PyDict>>,
        lockfile: Option<String>,
        update_lock: bool,
        check_drift: bool,
        only: Option<Vec<String>>,
        max_parallel: Option<usize>,
        junit: Option<String>,
        quiet: bool,
    ) -> PyResult<Bound<'py, PyAny>> {
        let mut cfg = make_config(
            py,
            base_url,
            vars,
            lockfile,
            update_lock,
            check_drift,
            max_parallel,
            quiet,
        )?;
        cfg.only = only.unwrap_or_default();
        let suite = self.inner.lock().unwrap().clone();
        let rt = runtime()?;
        let report = py.detach(move || rt.block_on(run_suite(&suite, &cfg)));
        if let Some(path) = junit {
            std::fs::write(&path, junit_xml(std::slice::from_ref(&report)))
                .map_err(|e| PyRuntimeError::new_err(format!("writing {path}: {e}")))?;
        }
        if !quiet {
            pyo3::types::PyModule::import(py, "sys")?
                .getattr("stderr")?
                .call_method1("write", (report.render(),))?;
        }
        report_dict(py, &report, report.passed())
    }

    /// Runs one test as a load scenario.
    /// Either `duration` ("10s"), `iterations` (shared budget) or `stages` ([("10s", 20), ("5s", 0)]).
    #[pyo3(signature = (test, base_url, vus=10, duration="10s".to_string(), iterations=None, stages=None, thresholds=None, vars=None, validate_every=100))]
    #[expect(clippy::too_many_arguments)]
    fn load<'py>(
        &self,
        py: Python<'py>,
        test: &str,
        base_url: String,
        vus: usize,
        duration: String,
        iterations: Option<u64>,
        stages: Option<Vec<(String, usize)>>,
        thresholds: Option<Vec<String>>,
        vars: Option<&Bound<'py, PyDict>>,
        validate_every: u64,
    ) -> PyResult<Bound<'py, PyAny>> {
        let mut cfg = make_config(py, base_url, vars, None, false, true, None, true)?;
        cfg.validate_every = validate_every;
        let executor = if let Some(st) = stages {
            Executor::RampingVus {
                stages: st
                    .into_iter()
                    .map(|(d, target)| {
                        Ok(Stage {
                            duration: parse_duration(&d)?,
                            target,
                        })
                    })
                    .collect::<anyhow::Result<_>>()
                    .map_err(rt_err)?,
            }
        } else if let Some(n) = iterations {
            Executor::Iterations { vus, iterations: n }
        } else {
            Executor::ConstantVus {
                vus,
                duration: parse_duration(&duration).map_err(rt_err)?,
            }
        };
        let mut sc = Scenario::new(test, executor);
        for t in thresholds.unwrap_or_default() {
            sc.thresholds.push(Threshold::parse(&t).map_err(rt_err)?);
        }
        let plan = LoadPlan::new(self.inner.lock().unwrap().clone()).scenario(sc);
        let rt = runtime()?;
        let report = py.detach(move || rt.block_on(run_load(&plan, &cfg)));
        report_dict(py, &report, report.passed())
    }
}

#[pymodule]
fn _native(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PySuite>()?;
    m.add_class::<PyCtx>()?;
    m.add_class::<PyResponse>()?;
    Ok(())
}
