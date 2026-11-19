//! Shared plumbing for the scripting frontends (Rhai, JS) and the Python bindings.
//!
//! Script code is synchronous. [`SyncCtx`] wraps a [`Ctx`] and blocks on the async
//! core, so it must be used from a non-runtime thread (`spawn_blocking` or a
//! dedicated worker thread). Every value crossing the boundary is JSON.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Result, bail};
use serde_json::Value;
use tokio::runtime::Handle;

use crate::context::Ctx;
use crate::suite::{BoxFut, CtxFn, Suite};

#[cfg(feature = "js")]
pub mod js;
#[cfg(feature = "rhai")]
pub mod rhai;

/// Blocking facade over [`Ctx`] for script engines.
#[derive(Clone)]
pub struct SyncCtx {
    pub cx: Ctx,
    rt: Handle,
}

impl SyncCtx {
    /// Must be called from inside the tokio runtime (it captures the current handle).
    pub fn new(cx: Ctx) -> Self {
        SyncCtx {
            cx,
            rt: Handle::current(),
        }
    }

    pub fn block_on<F: std::future::Future>(&self, f: F) -> F::Output {
        self.rt.block_on(f)
    }

    pub fn get(&self, key: &str) -> Value {
        self.cx.get(key).unwrap_or(Value::Null)
    }

    pub fn set(&self, key: &str, value: Value) -> Result<()> {
        self.cx.set(key, value)
    }

    pub fn env(&self, key: &str) -> Result<String> {
        self.cx.env(key)
    }

    pub fn env_opt(&self, key: &str) -> Option<String> {
        self.cx.env_opt(key)
    }

    pub fn header(&self, name: &str, value: &str) {
        self.cx.default_header(name, value)
    }

    pub fn log(&self, msg: &str) {
        self.cx.log(msg)
    }

    pub fn vu(&self) -> Option<usize> {
        self.cx.vu()
    }

    /// The current test's trace context as a value (`trace_id`, `sampled`, ...).
    pub fn trace(&self) -> Value {
        serde_json::to_value(self.cx.trace()).unwrap_or(Value::Null)
    }

    /// `Authorization: Bearer <token>` for the rest of the current scope.
    pub fn bearer(&self, token: &str) {
        self.cx.bearer(token)
    }

    pub fn basic(&self, username: &str, password: &str) {
        self.cx.basic(username, password)
    }

    /// Scope auth from a value (`{ type: "login", service, username, password }`, ...).
    pub fn auth(&self, auth: Value) -> Result<()> {
        self.cx
            .auth(serde_json::from_value(auth).map_err(|e| anyhow::anyhow!("invalid auth: {e}"))?);
        Ok(())
    }

    /// Raw request; returns the response as a value (`status`, `headers`, `body`, `text`, ...).
    pub fn http(&self, method: &str, path: &str, opts: Value) -> Result<Value> {
        let opts = if opts.is_null() {
            Value::Object(Default::default())
        } else {
            opts
        };
        Ok(self
            .block_on(self.cx.request_value(method, path, opts))?
            .to_value())
    }

    /// Contract call by operationId; returns the response as a value.
    pub fn call(&self, contract: &str, op: &str, opts: Value) -> Result<Value> {
        let opts = if opts.is_null() {
            Value::Object(Default::default())
        } else {
            opts
        };
        Ok(self
            .block_on(self.cx.call_value(contract, op, opts))?
            .to_value())
    }
}

/// Wraps a blocking callback into an async [`CtxFn`] that runs on the blocking pool.
pub fn blocking_fn<T, F>(f: F) -> CtxFn<T>
where
    T: Send + 'static,
    F: Fn(SyncCtx) -> Result<T> + Send + Sync + 'static,
{
    let f = Arc::new(f);
    Arc::new(move |cx: Ctx| -> BoxFut<T> {
        let f = f.clone();
        Box::pin(async move {
            let sync = SyncCtx::new(cx);
            tokio::task::spawn_blocking(move || f(sync)).await?
        })
    })
}

/// Options for loading script suites.
#[derive(Debug, Clone)]
pub struct LoadOptions {
    /// Number of independent JS runtimes (one per VU under load).
    pub js_workers: usize,
}

impl Default for LoadOptions {
    fn default() -> Self {
        LoadOptions { js_workers: 1 }
    }
}

/// Loads suites from a script file, picking the frontend by extension.
#[allow(unused_variables)]
pub fn load_suites(path: &Path, opts: &LoadOptions) -> Result<Vec<Suite>> {
    match path.extension().and_then(|e| e.to_str()) {
        #[cfg(feature = "rhai")]
        Some("rhai") => rhai::load(path),
        #[cfg(feature = "js")]
        Some("js" | "mjs") => js::load(path, opts.js_workers),
        Some(ext) => bail!(
            "no frontend for '.{ext}' files (enabled: {})",
            enabled_frontends()
        ),
        None => bail!("cannot tell the script language of {}", path.display()),
    }
}

fn enabled_frontends() -> &'static str {
    match (cfg!(feature = "rhai"), cfg!(feature = "js")) {
        (true, true) => ".rhai, .js",
        (true, false) => ".rhai",
        (false, true) => ".js",
        (false, false) => "none",
    }
}

/// Helpers shared by frontends for reading suite definitions.
#[cfg(any(feature = "rhai", feature = "js"))]
pub(crate) fn resolve_relative(base: &Path, p: &str) -> std::path::PathBuf {
    let path = Path::new(p);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

/// Applies language-neutral test options `{ parallel, serial, resources, timeout_ms, tags }`.
#[cfg(any(feature = "rhai", feature = "js"))]
pub(crate) fn apply_test_opts(opts: &mut crate::suite::TestOpts, v: &Value) -> Result<()> {
    use crate::suite::Concurrency;
    let Some(o) = v.as_object() else {
        return Ok(());
    };
    for (k, val) in o {
        match k.as_str() {
            "parallel" => {
                if val.as_bool().unwrap_or(false) {
                    opts.concurrency = Concurrency::Parallel;
                }
            }
            "serial" => {
                if val.as_bool().unwrap_or(false) {
                    opts.concurrency = Concurrency::Serial;
                }
            }
            "resources" | "resource" => match val {
                Value::String(s) => opts.resources.push(s.clone()),
                Value::Array(a) => opts
                    .resources
                    .extend(a.iter().filter_map(|v| v.as_str().map(str::to_string))),
                _ => bail!("`resources` must be a string or a list of strings"),
            },
            "timeout_ms" | "timeoutMs" => {
                let ms = val
                    .as_u64()
                    .ok_or_else(|| anyhow::anyhow!("`timeout_ms` must be a positive integer"))?;
                opts.timeout = Some(std::time::Duration::from_millis(ms));
            }
            "tags" => opts.tags.extend(
                val.as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|v| v.as_str().map(str::to_string)),
            ),
            other => bail!(
                "unknown test option '{other}' (parallel, serial, resources, timeout_ms, tags, setup, teardown)"
            ),
        }
    }
    Ok(())
}
