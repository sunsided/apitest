//! Python bindings for apitest (`apitest._native`).
//!
//! Python callbacks run on tokio's blocking pool and take the GIL only while
//! Python code runs; HTTP calls release it, so parallel tests and load VUs
//! overlap on I/O. Values cross the boundary as JSON.

#![forbid(unsafe_code)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use pyo3::exceptions::{PyAssertionError, PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyTraceback};

use apitest::contract::{Compat, Contract};
use apitest::load::{Executor, LoadPlan, Scenario, Stage, Threshold, parse_duration, run_load};
use apitest::report::junit_xml;
use apitest::script::{SyncCtx, blocking_fn};
use apitest::suite::{CtxFn, HookKind, Suite, TestOpts};
use apitest::{RunConfig, Scope, run_suite};
use serde_json::Value;

/// Raises a core error as a Python `RuntimeError`, keeping its cause chain.
fn rt_err(e: &anyhow::Error) -> PyErr {
    PyRuntimeError::new_err(format!("{e:#}"))
}

/// Converts a Python object to JSON via `json.dumps`; `None` becomes `null`.
fn to_json(py: Python<'_>, v: &Bound<'_, PyAny>) -> PyResult<Value> {
    if v.is_none() {
        return Ok(Value::Null);
    }

    let s: String = py.import("json")?.call_method1("dumps", (v,))?.extract()?;
    serde_json::from_str(&s).map_err(|e| PyValueError::new_err(e.to_string()))
}

/// Converts JSON to a Python object via `json.loads`.
fn from_json<'py>(py: Python<'py>, v: &Value) -> PyResult<Bound<'py, PyAny>> {
    py.import("json")?.call_method1("loads", (v.to_string(),))
}

/// Converts a Python exception into a core error, appending the formatted traceback.
fn py_to_anyhow(py: Python<'_>, e: &PyErr) -> anyhow::Error {
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
        .map_err(|e| Python::attach(|py| py_to_anyhow(py, &e)))
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
        .map_err(|e| Python::attach(|py| py_to_anyhow(py, &e)))
    })
}

/// Response of `cx.http(...)` / `cx.call(...)`.
#[pyclass(name = "Response", frozen)]
struct PyResponse {
    /// The core response as a JSON object (`status`, `headers`, `body`, `text`,
    /// `elapsed_ms`, `request`, `trace`).
    raw: Value,
}

#[pymethods]
impl PyResponse {
    /// HTTP status code (0 if the response carried none).
    #[getter]
    fn status(&self) -> u16 {
        self.raw["status"]
            .as_u64()
            .and_then(|s| u16::try_from(s).ok())
            .unwrap_or(0)
    }

    /// The body as text, regardless of content type.
    #[getter]
    fn text(&self) -> String {
        self.raw["text"].as_str().unwrap_or_default().to_string()
    }

    /// Time from sending the request to receiving the full body, in milliseconds.
    #[getter]
    fn elapsed_ms(&self) -> f64 {
        self.raw["elapsed_ms"].as_f64().unwrap_or(0.0)
    }

    /// Response headers as a dict.
    #[getter]
    fn headers<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        from_json(py, &self.raw["headers"])
    }

    /// Parsed JSON body (or the text if the body was not JSON).
    #[getter]
    fn body<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        from_json(py, &self.raw["body"])
    }

    /// Same as the `body` property, as a method for `requests`-style code.
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
    ///
    /// The `AssertionError` message includes the first 300 characters of the body.
    fn expect(slf: Py<Self>, status: u16) -> PyResult<Py<Self>> {
        let me = slf.get();
        if me.status() != status {
            let text: String = me.text().chars().take(300).collect();
            return Err(PyAssertionError::new_err(format!(
                "expected status {status}, got {}: {text}",
                me.status()
            )));
        }

        Ok(slf)
    }

    /// `<Response 200>`.
    fn __repr__(&self) -> String {
        format!("<Response {}>", self.status())
    }
}

/// The context passed to fixtures, hooks and tests.
#[pyclass(name = "Ctx", frozen)]
struct PyCtx {
    /// Blocking context of the running test, fixture or hook.
    sync: SyncCtx,
}

/// Merges the request options dict and keyword arguments into one JSON object;
/// keyword arguments win on conflicts.
fn opts_value(
    py: Python<'_>,
    opts: Option<&Bound<'_, PyDict>>,
    kwargs: Option<&Bound<'_, PyDict>>,
) -> PyResult<Value> {
    let mut v = match opts {
        Some(o) => to_json(py, o.as_any())?,
        None => Value::Object(serde_json::Map::default()),
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

    /// Stores a JSON-serializable value in the current scope.
    fn set(&self, py: Python<'_>, key: &str, value: &Bound<'_, PyAny>) -> PyResult<()> {
        self.sync
            .set(key, to_json(py, value)?)
            .map_err(|e| rt_err(&e))
    }

    /// A variable (`--var`, the environment, `.env` files, then `vars:`); raises if unset and
    /// no `default` is given.
    #[pyo3(signature = (key, default=None))]
    fn env(&self, key: &str, default: Option<String>) -> PyResult<String> {
        match (self.sync.env_opt(key), default) {
            (Some(v), _) => Ok(v),
            (None, Some(d)) => Ok(d),
            (None, None) => self.sync.env(key).map_err(|e| rt_err(&e)),
        }
    }

    /// Default header for every request from this scope and below.
    fn header(&self, name: &str, value: &str) {
        self.sync.header(name, value);
    }

    /// Appends a line to the current test's log.
    fn log(&self, msg: &str) {
        self.sync.log(msg);
    }

    /// Virtual-user index under load; 0 in functional runs.
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

    /// HTTP Basic credentials for the rest of this scope.
    fn basic(&self, username: &str, password: &str) {
        self.sync.basic(username, password);
    }

    /// Any credentials for this scope, e.g. `{"type": "login", "service": "hub", "username": ..., "password": ...}`.
    fn auth(&self, py: Python<'_>, auth: &Bound<'_, PyAny>) -> PyResult<()> {
        self.sync.auth(to_json(py, auth)?).map_err(|e| rt_err(&e))
    }

    /// Slash-separated path of the current scope (`suite/test`).
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
        let raw = py
            .detach(move || sync.http(&m, &p, o))
            .map_err(|e| rt_err(&e))?;

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
        let raw = py
            .detach(move || sync.call(&c, &op, o))
            .map_err(|e| rt_err(&e))?;

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
    /// The suite being built; cloned for each run so it can be run repeatedly.
    inner: Mutex<Suite>,
}

/// Parses a fixture scope name, raising `ValueError` on unknown names.
fn parse_scope(s: &str) -> PyResult<Scope> {
    s.parse()
        .map_err(|e: anyhow::Error| PyValueError::new_err(e.to_string()))
}

/// Serializes a report to a dict and adds the overall verdict as `passed`.
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

/// A fresh multi-threaded runtime for one `run`/`load` call.
fn runtime() -> PyResult<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| PyRuntimeError::new_err(e.to_string()))
}

/// A run configuration for `base_url` with the `vars` dict applied.
///
/// Non-string variable values are stored as their JSON text.
fn base_config(
    py: Python<'_>,
    base_url: String,
    vars: Option<&Bound<'_, PyDict>>,
) -> PyResult<RunConfig> {
    let mut cfg = RunConfig::new(base_url);
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

/// Executor for `Suite.load`: `stages` wins over `iterations`, which wins over
/// a constant-VU `duration`.
fn load_executor(
    vus: usize,
    duration: &str,
    iterations: Option<u64>,
    stages: Option<Vec<(String, usize)>>,
) -> anyhow::Result<Executor> {
    if let Some(st) = stages {
        let stages = st
            .into_iter()
            .map(|(d, target)| {
                Ok(Stage {
                    duration: parse_duration(&d)?,
                    target,
                })
            })
            .collect::<anyhow::Result<_>>()?;
        return Ok(Executor::RampingVus { stages });
    }

    if let Some(n) = iterations {
        return Ok(Executor::Iterations { vus, iterations: n });
    }

    Ok(Executor::ConstantVus {
        vus,
        duration: parse_duration(duration)?,
    })
}

#[pymethods]
impl PySuite {
    /// An empty suite called `name`.
    #[new]
    fn new(name: &str) -> Self {
        PySuite {
            inner: Mutex::new(Suite::new(name)),
        }
    }

    /// The suite name.
    #[getter]
    fn name(&self) -> String {
        self.inner.lock().unwrap().name.clone()
    }

    /// Test names in registration order.
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

    /// Pins the spec at `path` as contract `name`; `live_spec` is compared
    /// against it before the run, under `mode` (`tolerant` or `strict`).
    #[pyo3(signature = (name, path, live_spec=None, mode=None))]
    fn contract(
        &self,
        name: &str,
        path: &str,
        live_spec: Option<String>,
        mode: Option<&str>,
    ) -> PyResult<()> {
        let mut c = Contract::pinned(name, path).map_err(|e| rt_err(&e))?;
        if let Some(u) = live_spec {
            c = c.live_spec(u);
        }

        if let Some(m) = mode {
            c = c.mode(m.parse::<Compat>().map_err(|e| rt_err(&e))?);
        }

        self.inner.lock().unwrap().add_contract(c);
        Ok(())
    }

    /// Registers fixture `name` in `scope` (`suite`, `test`, ...); `setup(cx)`
    /// returns its value and `teardown(cx)` runs when the scope ends.
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
        let kind: HookKind = kind.parse().map_err(|e| rt_err(&e))?;
        self.inner.lock().unwrap().add_hook(kind, py_unit_fn(f));
        Ok(())
    }

    /// Registers test `name` with `body(cx)` and its options.
    #[pyo3(signature = (name, body, parallel=false, resources=None, timeout_ms=None, setup=None, teardown=None, tags=None))]
    #[expect(
        clippy::too_many_arguments,
        reason = "mirrors the Python keyword arguments of Suite.add_test"
    )]
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
    #[expect(
        clippy::too_many_arguments,
        reason = "mirrors the Python keyword arguments of Suite.run"
    )]
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
        let mut cfg = base_config(py, base_url, vars)?
            .quiet(quiet)
            .update_lock(update_lock);
        cfg.check_drift = check_drift;
        if let Some(l) = lockfile {
            cfg = cfg.lockfile(l);
        }

        if let Some(n) = max_parallel {
            cfg.max_parallel = n;
        }

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
    #[pyo3(signature = (test, base_url, vus=10, duration="10s", iterations=None, stages=None, thresholds=None, vars=None, validate_every=100))]
    #[expect(
        clippy::too_many_arguments,
        reason = "mirrors the Python keyword arguments of Suite.load"
    )]
    fn load<'py>(
        &self,
        py: Python<'py>,
        test: &str,
        base_url: String,
        vus: usize,
        duration: &str,
        iterations: Option<u64>,
        stages: Option<Vec<(String, usize)>>,
        thresholds: Option<Vec<String>>,
        vars: Option<&Bound<'py, PyDict>>,
        validate_every: u64,
    ) -> PyResult<Bound<'py, PyAny>> {
        let mut cfg = base_config(py, base_url, vars)?.quiet(true);
        cfg.validate_every = validate_every;

        let executor = load_executor(vus, duration, iterations, stages).map_err(|e| rt_err(&e))?;
        let mut sc = Scenario::new(test, executor);
        for t in thresholds.unwrap_or_default() {
            sc.thresholds
                .push(Threshold::parse(&t).map_err(|e| rt_err(&e))?);
        }

        let plan = LoadPlan::new(self.inner.lock().unwrap().clone()).scenario(sc);
        let rt = runtime()?;
        let report = py.detach(move || rt.block_on(run_load(&plan, &cfg)));
        report_dict(py, &report, report.passed())
    }
}

/// The `apitest._native` extension module.
#[pymodule]
#[pyo3(name = "_native")]
fn native(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PySuite>()?;
    m.add_class::<PyCtx>()?;
    m.add_class::<PyResponse>()?;
    Ok(())
}
