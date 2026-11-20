//! Shared plumbing for the scripting frontends (Rhai, JS), the Gherkin frontend's
//! loader dispatch and the Python bindings.
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
    /// The async context this facade forwards to; scripts share its scope and values.
    pub cx: Ctx,
    /// Handle of the runtime that owns `cx`, captured at construction so blocking
    /// calls from worker threads can drive futures on it.
    rt: Handle,
}

impl SyncCtx {
    /// Wraps `cx` for use from a blocking thread.
    ///
    /// # Panics
    ///
    /// Panics when called outside a tokio runtime, because it captures the
    /// current runtime handle.
    #[must_use]
    pub fn new(cx: Ctx) -> Self {
        SyncCtx {
            cx,
            rt: Handle::current(),
        }
    }

    /// Drives `f` to completion on the captured runtime, blocking this thread.
    ///
    /// # Panics
    ///
    /// Panics when called from an async task of the runtime itself; use it only
    /// from `spawn_blocking` or a dedicated worker thread.
    pub fn block_on<F: std::future::Future>(&self, f: F) -> F::Output {
        self.rt.block_on(f)
    }

    /// Looks `key` up through the scope chain; a missing key reads as JSON `null`.
    #[must_use]
    pub fn get(&self, key: &str) -> Value {
        self.cx.get(key).unwrap_or(Value::Null)
    }

    /// Stores `value` under `key` in the current scope.
    ///
    /// # Errors
    ///
    /// Fails when the value cannot be represented as JSON.
    pub fn set(&self, key: &str, value: Value) -> Result<()> {
        self.cx.set(key, value)
    }

    /// Reads a `--var` override or, failing that, a process environment variable.
    ///
    /// # Errors
    ///
    /// Fails when the variable is set in neither place.
    pub fn env(&self, key: &str) -> Result<String> {
        self.cx.env(key)
    }

    /// Like [`SyncCtx::env`], but yields `None` for an unset variable.
    #[must_use]
    pub fn env_opt(&self, key: &str) -> Option<String> {
        self.cx.env_opt(key)
    }

    /// Adds a default header to every request from the current scope and its children.
    pub fn header(&self, name: &str, value: &str) {
        self.cx.default_header(name, value);
    }

    /// Appends `msg` to the current test's log.
    pub fn log(&self, msg: &str) {
        self.cx.log(msg);
    }

    /// Virtual-user index under load; `Some(0)` in functional runs.
    #[must_use]
    pub fn vu(&self) -> Option<usize> {
        self.cx.vu()
    }

    /// The current test's trace context as a value (`trace_id`, `sampled`, ...).
    #[must_use]
    pub fn trace(&self) -> Value {
        serde_json::to_value(self.cx.trace()).unwrap_or(Value::Null)
    }

    /// `Authorization: Bearer <token>` for the rest of the current scope.
    pub fn bearer(&self, token: &str) {
        self.cx.bearer(token);
    }

    /// HTTP Basic credentials for the rest of the current scope.
    pub fn basic(&self, username: &str, password: &str) {
        self.cx.basic(username, password);
    }

    /// Scope auth from a value (`{ type: "login", service, username, password }`, ...).
    ///
    /// # Errors
    ///
    /// Fails when `auth` does not deserialize into an [`crate::auth::Auth`].
    pub fn auth(&self, auth: Value) -> Result<()> {
        self.cx
            .auth(serde_json::from_value(auth).map_err(|e| anyhow::anyhow!("invalid auth: {e}"))?);
        Ok(())
    }

    /// Raw request; returns the response as a value (`status`, `headers`, `body`, `text`, ...).
    ///
    /// A `null` `opts` means "no options".
    ///
    /// # Errors
    ///
    /// Fails on invalid options, transport errors and failed response checks.
    pub fn http(&self, method: &str, path: &str, opts: Value) -> Result<Value> {
        let opts = if opts.is_null() {
            Value::Object(serde_json::Map::default())
        } else {
            opts
        };

        Ok(self
            .block_on(self.cx.request_value(method, path, opts))?
            .to_value())
    }

    /// Contract call by operationId; returns the response as a value.
    ///
    /// A `null` `opts` means "no options".
    ///
    /// # Errors
    ///
    /// Fails on an unknown contract or operation, invalid options, transport
    /// errors and responses that break the contract.
    pub fn call(&self, contract: &str, op: &str, opts: Value) -> Result<Value> {
        let opts = if opts.is_null() {
            Value::Object(serde_json::Map::default())
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
    /// Cucumber tag expression for `.feature` files (`@smoke and not @slow`).
    /// Other frontends ignore it.
    pub tags: Option<String>,
}

impl Default for LoadOptions {
    fn default() -> Self {
        LoadOptions {
            js_workers: 1,
            tags: None,
        }
    }
}

/// Loads suites from a script file, picking the frontend by extension.
///
/// `.rhai` goes to the Rhai frontend, `.js`/`.mjs` to the JS frontend and
/// `.feature` to the Gherkin frontend, each only when its feature is enabled.
///
/// # Errors
///
/// Fails when the file has no extension, no enabled frontend handles it, or the
/// frontend fails to read or evaluate the script.
#[cfg_attr(
    not(any(feature = "js", feature = "cucumber")),
    expect(
        unused_variables,
        reason = "`opts` only configures the JS and Gherkin frontends"
    )
)]
pub fn load_suites(path: &Path, opts: &LoadOptions) -> Result<Vec<Suite>> {
    match path.extension().and_then(|e| e.to_str()) {
        #[cfg(feature = "rhai")]
        Some("rhai") => rhai::load(path),
        #[cfg(feature = "js")]
        Some("js" | "mjs") => js::load(path, opts.js_workers),
        #[cfg(feature = "cucumber")]
        Some("feature") => crate::cucumber::load(
            path,
            &crate::cucumber::FeatureOptions {
                tags: opts.tags.clone(),
            },
        ),
        Some(ext) => bail!(
            "no frontend for '.{ext}' files (enabled: {})",
            enabled_frontends()
        ),
        None => bail!("cannot tell the script language of {}", path.display()),
    }
}

/// Human-readable list of the script extensions compiled into this build.
fn enabled_frontends() -> String {
    let on: Vec<&str> = [
        (cfg!(feature = "rhai"), ".rhai"),
        (cfg!(feature = "js"), ".js"),
        (cfg!(feature = "cucumber"), ".feature"),
    ]
    .into_iter()
    .filter_map(|(on, ext)| on.then_some(ext))
    .collect();
    if on.is_empty() {
        "none".into()
    } else {
        on.join(", ")
    }
}

/// Resolves a path named in a script against the script's directory `base`.
///
/// Absolute paths are returned unchanged.
#[cfg(any(feature = "rhai", feature = "js", feature = "cucumber"))]
pub(crate) fn resolve_relative(base: &Path, p: &str) -> std::path::PathBuf {
    let path = Path::new(p);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

/// Applies language-neutral test options `{ parallel, serial, resources, timeout_ms, tags }`.
///
/// A non-object `v` is ignored. `timeoutMs` and `resource` are accepted as aliases.
///
/// # Errors
///
/// Fails on an unknown key, a `resources` value that is neither a string nor a
/// list, or a `timeout_ms` that is not a non-negative integer.
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
