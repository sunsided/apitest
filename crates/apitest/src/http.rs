//! HTTP layer: language-neutral request options, responses, and contract-aware sending.
//!
//! A [`Request`] is resolved and sent in these steps:
//!
//! ```mermaid
//! flowchart LR
//!     A[named actor<br/>`as`] --> B[target:<br/>raw path or operation]
//!     B --> C[effective auth]
//!     C --> D[Ctx::send:<br/>validate, headers, trace]
//!     D --> E{401 with<br/>dynamic auth?}
//!     E -- yes --> F[re-acquire,<br/>send once more] --> G
//!     E -- no --> G[expect / expect_json]
//!     G --> H[register rollbacks]
//! ```

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use anyhow::{Context as _, Result, anyhow, bail};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::auth::Auth;
use crate::context::{Ctx, Scope};
use crate::contract::Contract;
use crate::trace::SpanContext;

/// Request options. This is the shape every frontend passes in
/// (a Rhai map, a JS object, a Python dict all deserialize into it).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RequestOpts {
    /// Values for `{placeholders}` in the path template.
    pub path: Map<String, Value>,
    /// Query string parameters. Strings are sent as is, other values as JSON text.
    pub query: Map<String, Value>,
    /// Request headers, rendered like query values. They replace run, scope and
    /// actor defaults of the same name (case-insensitively).
    pub headers: Map<String, Value>,
    /// JSON body. Wins over `form` and `body`.
    pub json: Option<Value>,
    /// `application/x-www-form-urlencoded` body, used when `json` is `None`.
    pub form: Option<Map<String, Value>>,
    /// Raw body text, used when neither `json` nor `form` is set.
    pub body: Option<String>,
    /// Shorthand for an `Authorization: Bearer ...` header.
    pub bearer: Option<String>,
    /// Named service for raw requests (see `RunConfig::services`).
    pub service: Option<String>,
    /// `false` suppresses trace headers for this request, `true` forces them even
    /// if tracing is disabled. Default: the run's / suite's tracing setting.
    pub trace: Option<bool>,
    /// Credentials for this request (`{ type: "basic", username, password }`, ...).
    pub auth: Option<Auth>,
    /// Send as a named actor from the configuration (its service, headers and auth).
    #[serde(rename = "as")]
    pub as_actor: Option<String>,
}

/// What was actually sent, as seen on the wire (after all defaults, auth and tracing).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SentRequest {
    /// HTTP method as sent (raw requests are uppercased, contract calls use the spec's method).
    pub method: String,
    /// Absolute URL with path parameters filled in; the query string is not included.
    pub url: String,
    /// Header names as sent (original case); look up with [`SentRequest::header`].
    pub headers: BTreeMap<String, String>,
}

impl SentRequest {
    /// Case-insensitive header lookup.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// A fully received response.
#[derive(Debug, Clone, Serialize)]
pub struct Response {
    /// HTTP status code.
    pub status: u16,
    /// Response headers with lowercase names; values that are not valid UTF-8 are empty.
    pub headers: BTreeMap<String, String>,
    /// Parsed JSON body, or the body as a string if it was not JSON (`null` when empty).
    pub body: Value,
    /// The body as received, before JSON parsing.
    pub text: String,
    /// Time from sending the request until the whole body arrived, in milliseconds.
    pub elapsed_ms: f64,
    /// `contract:operationId` for contract calls.
    pub operation: Option<String>,
    /// The request as sent (method, URL, all headers incl. trace and User-Agent).
    pub request: SentRequest,
    /// The span this request was sent as (`None` if tracing was off for it).
    pub trace: Option<SpanContext>,
}

impl Response {
    /// Case-insensitive response header lookup.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// Fails unless the status matches.
    ///
    /// # Errors
    ///
    /// Fails when the status differs from `status`; the error names the operation
    /// and shows the first 300 bytes of the body.
    pub fn expect(self, status: u16) -> Result<Self> {
        if self.status != status {
            bail!(
                "expected status {status}, got {}{}: {}",
                self.status,
                self.operation_label(" from "),
                truncate(&self.text, 300)
            );
        }

        Ok(self)
    }

    /// A value from the body by JSON pointer (`"/id"`, `"/items/0/code"`; `"id"` works
    /// too), deserialized into `T`. `""` is the whole body.
    ///
    /// ```no_run
    /// # fn demo(resp: apitest::Response) -> anyhow::Result<()> {
    /// let id: String = resp.get("/id")?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Fails when the pointer does not exist in the body, or when the value there
    /// does not deserialize into `T`.
    pub fn get<T: DeserializeOwned>(&self, pointer: &str) -> Result<T> {
        let ptr = if pointer.is_empty() || pointer.starts_with('/') {
            pointer.to_string()
        } else {
            format!("/{pointer}")
        };
        let what = self.operation_label(" of ");

        let v = self.body.pointer(&ptr).ok_or_else(|| {
            anyhow!(
                "response{what} has no '{ptr}': {}",
                truncate(&self.text, 300)
            )
        })?;

        serde_json::from_value(v.clone()).map_err(|e| {
            anyhow!("'{ptr}' in response{what} has unexpected shape: {e} (value: {v})")
        })
    }

    /// Fails unless the body contains `expected` (partial match, see [`crate::dsl::json_includes`]).
    ///
    /// # Errors
    ///
    /// Fails on the first mismatch, naming its path and the operation.
    pub fn expect_json(self, expected: &Value) -> Result<Self> {
        crate::dsl::json_includes(&self.body, expected)
            .with_context(|| format!("response{} does not match", self.operation_label(" of ")))?;
        Ok(self)
    }

    /// Deserializes the JSON body.
    ///
    /// # Errors
    ///
    /// Fails when the body does not deserialize into `T`.
    pub fn json<T: DeserializeOwned>(&self) -> Result<T> {
        serde_json::from_value(self.body.clone()).context("response body has unexpected shape")
    }

    /// JSON pointer into the body, e.g. `/items/0/id`.
    #[must_use]
    pub fn pointer(&self, ptr: &str) -> Option<&Value> {
        self.body.pointer(ptr)
    }

    /// The response as a plain value (what scripts receive).
    ///
    /// # Panics
    ///
    /// Never in practice: every field serializes to JSON infallibly.
    #[must_use]
    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).expect("response is serializable")
    }

    /// `"{prefix}contract:op"` for contract calls, empty for raw requests.
    fn operation_label(&self, prefix: &str) -> String {
        self.operation
            .as_deref()
            .map(|o| format!("{prefix}{o}"))
            .unwrap_or_default()
    }
}

/// `resp["id"]` is the top-level field `id` of the JSON body (`Value::Null` if absent),
/// like indexing a `serde_json::Value`.
impl std::ops::Index<&str> for Response {
    type Output = Value;
    fn index(&self, key: &str) -> &Value {
        static NULL: Value = Value::Null;
        self.body.get(key).unwrap_or(&NULL)
    }
}

/// What a [`Request`] addresses, before it is resolved against the run configuration.
enum Target {
    /// A method and a path (or absolute URL) on a service.
    Raw {
        /// HTTP method, uppercase.
        method: String,
        /// Path template relative to the service base URL, or an absolute URL.
        path: String,
    },
    /// An operation of a pinned contract; method and path come from the spec.
    Op {
        /// Contract name; `None` when the actor had neither a contract nor a
        /// service, which fails when the request is sent.
        contract: Option<String>,
        /// The operationId.
        op: String,
    },
}

/// A [`Target`] resolved against the run configuration: everything [`Ctx::send`] needs.
struct Endpoint {
    /// HTTP method.
    method: String,
    /// Path template (or absolute URL) with `{placeholders}` still unfilled.
    path: String,
    /// Contract and operationId for contract calls, used for validation and coverage.
    op: Option<(Arc<Contract>, String)>,
    /// Base URL the path is appended to.
    base: String,
}

impl Target {
    /// Looks up method, path and base URL; `service` applies to raw requests only.
    fn resolve(self, cx: &Ctx, service: Option<&str>) -> Result<Endpoint> {
        match self {
            Target::Raw { method, path } => Ok(Endpoint {
                method,
                path,
                op: None,
                base: cx.config().service_url(service)?.to_string(),
            }),
            Target::Op { contract, op } => {
                let name = contract.ok_or_else(|| {
                    anyhow!("cannot call '{op}': this actor has no service/contract")
                })?;
                let c = cx.contract(&name)?;
                let o = c.operation(&op)?;

                Ok(Endpoint {
                    method: o.method.clone(),
                    path: o.path.clone(),
                    base: cx.config().contract_url(&c)?.to_string(),
                    op: Some((c.clone(), op)),
                })
            }
        }
    }
}

/// A deferred cleanup builder: turns the response into the request that undoes it.
type Rollback<'a> = Box<dyn FnOnce(&Response) -> Result<Request<'static>> + Send + 'a>;

/// A request builder. Await it directly (`req.await?`) or call `.send().await`.
///
/// Requests built from an [`crate::dsl::Actor`] resolve their context, contract and
/// service when they are sent, so they can be created anywhere and also used as
/// cleanups ([`crate::dsl::undo`]).
#[must_use = "requests do nothing until awaited"]
pub struct Request<'a> {
    /// Context to send in; `None` means the running one ([`crate::context::current`]).
    cx: Option<Ctx>,
    /// What to call.
    target: Target,
    /// Path, query, headers, body and per-request settings.
    opts: RequestOpts,
    /// Credentials set on the builder; win over `opts.auth`.
    auth: Option<Auth>,
    /// Status the response must have, checked after sending.
    expect: Option<u16>,
    /// Partial JSON the response body must contain, checked after sending.
    expect_json: Option<Value>,
    /// Cleanups registered in order once the response passed all checks.
    rollbacks: Vec<Rollback<'a>>,
}

/// Values a `.rollback(..)` closure may return.
pub trait IntoRequest {
    /// The cleanup request.
    ///
    /// # Errors
    ///
    /// Passes on the error of a closure that could not build its request.
    fn into_request(self) -> Result<Request<'static>>;
}

impl IntoRequest for Request<'static> {
    fn into_request(self) -> Result<Request<'static>> {
        Ok(self)
    }
}

impl IntoRequest for Result<Request<'static>> {
    fn into_request(self) -> Result<Request<'static>> {
        self
    }
}

impl Request<'static> {
    /// A raw request; `method` is uppercased.
    pub(crate) fn raw(cx: Option<Ctx>, method: &str, path: &str) -> Self {
        Request::new(
            cx,
            Target::Raw {
                method: method.to_ascii_uppercase(),
                path: path.to_string(),
            },
        )
    }

    /// A contract call; the contract is looked up only when the request is sent.
    pub(crate) fn op(cx: Option<Ctx>, contract: Option<String>, op: &str) -> Self {
        Request::new(
            cx,
            Target::Op {
                contract,
                op: op.to_string(),
            },
        )
    }

    /// A request to `target` with default options and no checks.
    fn new(cx: Option<Ctx>, target: Target) -> Self {
        Request {
            cx,
            target,
            opts: RequestOpts::default(),
            auth: None,
            expect: None,
            expect_json: None,
            rollbacks: vec![],
        }
    }
}

impl<'a> Request<'a> {
    /// Fills `{name}` in the path template.
    pub fn path(self, name: &str, value: impl Serialize) -> Self {
        self.path_param(name, value)
    }

    /// Fails the request (when awaited) unless the status matches.
    pub fn expect(mut self, status: u16) -> Self {
        self.expect = Some(status);
        self
    }

    /// Fails the request (when awaited) unless the body contains `expected` (partial match).
    pub fn expect_json(mut self, expected: Value) -> Self {
        self.expect_json = Some(expected);
        self
    }

    /// After a successful response, registers the request returned by `f` as a cleanup
    /// of the current scope (LIFO, runs even if the test fails later).
    ///
    /// ```no_run
    /// # use apitest::prelude::*;
    /// # async fn demo(admin: Actor, user: Value) -> Result<()> {
    /// admin.call("createUser").json(&user).expect(201)
    ///     .rollback(|u| admin.call("deleteUser").path("id", &u["id"]).expect(204))
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn rollback<'b, F, R>(self, f: F) -> Request<'b>
    where
        'a: 'b,
        F: FnOnce(&Response) -> R + Send + 'b,
        R: IntoRequest,
    {
        let mut rollbacks: Vec<Rollback<'b>> = self
            .rollbacks
            .into_iter()
            .map(|r| r as Rollback<'b>)
            .collect();
        rollbacks.push(Box::new(move |resp| f(resp).into_request()));

        Request {
            cx: self.cx,
            target: self.target,
            opts: self.opts,
            auth: self.auth,
            expect: self.expect,
            expect_json: self.expect_json,
            rollbacks,
        }
    }

    /// Credentials for this request (overrides the actor's and the scope's).
    pub fn auth(mut self, auth: Auth) -> Self {
        self.auth = Some(auth);
        self
    }

    /// HTTP Basic auth for this request.
    pub fn basic(self, username: impl Into<String>, password: impl Into<String>) -> Self {
        self.auth(Auth::basic(username, password))
    }

    /// No trace headers on this request.
    pub fn no_trace(mut self) -> Self {
        self.opts.trace = Some(false);
        self
    }

    /// Forces (`true`) or suppresses (`false`) trace headers on this request.
    pub fn trace(mut self, on: bool) -> Self {
        self.opts.trace = Some(on);
        self
    }

    /// `User-Agent` for this request only.
    pub fn user_agent(self, ua: impl Into<String>) -> Self {
        self.header("User-Agent", ua)
    }

    /// Sends to a named service (raw requests; contract calls use the contract's service).
    pub fn service(mut self, name: &str) -> Self {
        self.opts.service = Some(name.to_string());
        self
    }

    /// Pins the context the request is sent in (instead of the running one).
    pub(crate) fn with_ctx(mut self, cx: Ctx) -> Self {
        self.cx = Some(cx);
        self
    }

    /// Short description for logs and cleanup labels, e.g. `hub:deleteUser {id=7}`.
    #[must_use]
    pub fn describe(&self) -> String {
        let target = match &self.target {
            Target::Raw { method, path } => format!("{method} {path}"),
            Target::Op { contract, op } => format!("{}:{op}", contract.as_deref().unwrap_or("?")),
        };
        if self.opts.path.is_empty() {
            return target;
        }

        let p: Vec<String> = self
            .opts
            .path
            .iter()
            .map(|(k, v)| format!("{k}={}", scalar(v)))
            .collect();

        format!("{target} {{{}}}", p.join(", "))
    }

    /// Fills `{name}` in the path template; the value is percent-encoded as one segment.
    pub fn path_param(mut self, name: &str, value: impl Serialize) -> Self {
        self.opts.path.insert(name.into(), to_value(value));
        self
    }

    /// Adds a query string parameter (replaces an earlier one of the same name).
    pub fn query(mut self, name: &str, value: impl Serialize) -> Self {
        self.opts.query.insert(name.into(), to_value(value));
        self
    }

    /// Sets a header (replaces run, scope and actor defaults of the same name).
    pub fn header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.opts
            .headers
            .insert(name.into(), Value::String(value.into()));
        self
    }

    /// `Authorization: Bearer <token>`; disables any configured auth for this request.
    pub fn bearer(mut self, token: impl Into<String>) -> Self {
        self.opts.bearer = Some(token.into());
        self
    }

    /// JSON body. A value that fails to serialize is sent as `null`.
    pub fn json(mut self, body: impl Serialize) -> Self {
        self.opts.json = Some(to_value(body));
        self
    }

    /// Form body (`application/x-www-form-urlencoded`), used unless a JSON body is set.
    pub fn form(mut self, fields: &[(&str, &str)]) -> Self {
        self.opts.form = Some(
            fields
                .iter()
                .map(|(k, v)| ((*k).to_string(), Value::String((*v).to_string())))
                .collect(),
        );
        self
    }

    /// Replaces all options at once (the frontends' entry point). Headers already
    /// on the request (e.g. from an actor) are kept unless `opts` sets them too.
    pub fn opts(mut self, opts: RequestOpts) -> Self {
        let mut headers = std::mem::take(&mut self.opts.headers);
        headers.extend(opts.headers.clone());
        self.opts = RequestOpts { headers, ..opts };
        self
    }

    /// Sends the request, applies `expect` / `expect_json`, and registers rollbacks.
    ///
    /// # Errors
    ///
    /// Fails when there is no context to send in, the named actor, service,
    /// contract or operation is unknown, credentials cannot be resolved, a path
    /// parameter is unfilled, the request or contract validation fails, the
    /// response does not meet `expect` / `expect_json`, or a rollback cannot be built.
    pub async fn send(self) -> Result<Response> {
        let Request {
            cx,
            target,
            mut opts,
            auth,
            expect,
            expect_json,
            rollbacks,
        } = self;
        let cx = match cx {
            Some(cx) => cx,
            None => crate::context::current()?,
        };

        let mut auth = auth.or(opts.auth.take());
        if let Some(name) = opts.as_actor.take() {
            apply_named_actor(&cx, &name, &mut opts, &mut auth)?;
        }

        let endpoint = target.resolve(&cx, opts.service.as_deref())?;
        let auth = effective_auth(&cx, auth, &opts);
        let mut resp = send_authenticated(&cx, &endpoint, opts, auth.as_ref()).await?;

        if let Some(status) = expect {
            resp = resp.expect(status)?;
        }

        if let Some(expected) = &expect_json {
            resp = resp.expect_json(expected)?;
        }

        for rb in rollbacks {
            cx.undo(rb(&resp).context("building rollback request")?);
        }

        Ok(resp)
    }
}

impl<'a> std::future::IntoFuture for Request<'a> {
    type Output = Result<Response>;
    type IntoFuture =
        std::pin::Pin<Box<dyn std::future::Future<Output = Result<Response>> + Send + 'a>>;
    fn into_future(self) -> Self::IntoFuture {
        Box::pin(self.send())
    }
}

/// Lets a named actor (scripts: `{ as: "hub_admin" }`) contribute service,
/// headers and auth where the request has none of its own.
fn apply_named_actor(
    cx: &Ctx,
    name: &str,
    opts: &mut RequestOpts,
    auth: &mut Option<Auth>,
) -> Result<()> {
    let a = crate::dsl::Actor::from_config_in(cx, name)?;
    for (k, v) in a.headers {
        if !opts.headers.keys().any(|h| h.eq_ignore_ascii_case(&k)) {
            opts.headers.insert(k, Value::String(v));
        }
    }

    if opts.service.is_none() {
        opts.service = a.service;
    }

    if auth.is_none() && a.auth != Auth::None {
        *auth = Some(a.auth);
    }

    Ok(())
}

/// The credentials to apply: the request's own, else the scope's. `None` when
/// there are none, or when the request sets `Authorization` / a bearer itself
/// (explicit headers always win over configured auth).
fn effective_auth(cx: &Ctx, auth: Option<Auth>, opts: &RequestOpts) -> Option<Auth> {
    let manual = opts.bearer.is_some()
        || opts
            .headers
            .keys()
            .any(|h| h.eq_ignore_ascii_case("authorization"));
    if manual {
        return None;
    }

    auth.or_else(|| cx.scope_auth())
        .filter(|a| *a != Auth::None)
}

/// Sends with the auth header applied. Cached logins / tokens can go stale
/// (revoked, server restarted), so a `401` with dynamic credentials re-acquires
/// them and sends exactly once more.
async fn send_authenticated(
    cx: &Ctx,
    ep: &Endpoint,
    mut opts: RequestOpts,
    auth: Option<&Auth>,
) -> Result<Response> {
    let mut retry_opts = None;
    if let Some(a) = auth
        && let Some((name, value)) = crate::auth::resolve(cx, a, false).await?
    {
        opts.headers.insert(name, Value::String(value));
        if a.is_dynamic() {
            retry_opts = Some(opts.clone());
        }
    }

    let resp = cx
        .send(&ep.method, &ep.path, ep.op.clone(), opts, &ep.base)
        .await?;
    if resp.status == 401
        && let (Some(a), Some(mut again)) = (auth, retry_opts)
        && let Some((name, value)) = crate::auth::resolve(cx, a, true).await?
    {
        again.headers.insert(name, Value::String(value));
        return cx
            .send(&ep.method, &ep.path, ep.op.clone(), again, &ep.base)
            .await;
    }

    Ok(resp)
}

/// Serializes a builder argument; values that cannot be represented become `null`.
fn to_value(v: impl Serialize) -> Value {
    serde_json::to_value(v).unwrap_or(Value::Null)
}

/// At most `n` bytes of `s` (cut back to a char boundary), with `…` if shortened.
pub(crate) fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_string();
    }

    let mut end = n;
    while !s.is_char_boundary(end) {
        end -= 1;
    }

    format!("{}…", &s[..end])
}

/// A value as it appears in a URL or header: strings unquoted, the rest as JSON text.
pub(crate) fn scalar(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Status, headers and body text of one received response.
type Received = (u16, BTreeMap<String, String>, String);

impl Ctx {
    /// A raw (non-contract) request.
    pub fn request(&self, method: &str, path: &str) -> Request<'static> {
        Request::raw(Some(self.clone()), method, path)
    }

    /// A request to `operationId` of a pinned contract. Method and path come from
    /// the pinned spec; request and response are validated against it.
    ///
    /// # Errors
    ///
    /// Fails when no contract `contract` is registered or it has no such operation.
    pub fn call(&self, contract: &str, operation: &str) -> Result<Request<'static>> {
        self.contract(contract)?.operation(operation)?;
        Ok(Request::op(
            Some(self.clone()),
            Some(contract.to_string()),
            operation,
        ))
    }

    /// Registers a prepared request as a cleanup of the current scope.
    pub fn undo(&self, req: Request<'static>) {
        let label = format!("undo {}", req.describe());
        self.defer_boxed(
            label,
            Box::new(move |cx| Box::pin(async move { req.with_ctx(cx).await.map(|_| ()) })),
        );
    }

    /// Frontend entry point for raw requests (options as a value).
    ///
    /// # Errors
    ///
    /// Fails when `opts` is not a valid [`RequestOpts`], and otherwise like [`Request::send`].
    pub async fn request_value(&self, method: &str, path: &str, opts: Value) -> Result<Response> {
        let opts: RequestOpts = serde_json::from_value(opts).context("invalid request options")?;
        // Through `Request` so auth, named actors and retries apply like everywhere else.
        Request::raw(Some(self.clone()), method, path)
            .opts(opts)
            .send()
            .await
    }

    /// Frontend entry point for contract calls (options as a value).
    ///
    /// # Errors
    ///
    /// Fails when `opts` is not a valid [`RequestOpts`], and otherwise like
    /// [`Ctx::call`] and [`Request::send`].
    pub async fn call_value(
        &self,
        contract: &str,
        operation: &str,
        opts: Value,
    ) -> Result<Response> {
        let opts: RequestOpts = serde_json::from_value(opts).context("invalid request options")?;
        self.call(contract, operation)?.opts(opts).send().await
    }

    /// Sends one HTTP request exactly as given (auth is already in `opts.headers`):
    /// validates against the contract (sampled), renders the URL, merges headers,
    /// adds trace headers, and records coverage and load metrics.
    pub(crate) async fn send(
        &self,
        method: &str,
        path: &str,
        op: Option<(Arc<Contract>, String)>,
        opts: RequestOpts,
        base_url: &str,
    ) -> Result<Response> {
        let validate = self.should_validate(op.is_some());
        if let Some((c, id)) = &op {
            c.record_request(id, &opts);
            if validate && let Some(body) = &opts.json {
                c.validate_request(id, body)?;
            }
        }

        let url = render_url(path, &opts.path, base_url)?;
        let method_parsed = reqwest::Method::from_bytes(method.as_bytes())
            .map_err(|_| anyhow!("invalid HTTP method '{method}'"))?;
        let mut headers = self.explicit_headers(&opts);
        let span = self.add_trace_headers(&mut headers, opts.trace);
        let sent = SentRequest {
            method: method.to_string(),
            url: url.clone(),
            headers: headers.iter().cloned().collect(),
        };

        let req = self.build_request(method_parsed, &url, &headers, &opts);
        let started = Instant::now();
        let result = fetch(req).await;
        let elapsed = started.elapsed();
        #[cfg(feature = "load")]
        self.record_metrics(&result, elapsed);

        let (status, headers, text) = result.with_context(|| format!("{method} {url} failed"))?;
        let response = Response {
            status,
            headers,
            body: parse_body(&text),
            text,
            elapsed_ms: elapsed.as_secs_f64() * 1000.0,
            operation: op.as_ref().map(|(c, id)| format!("{}:{id}", c.name)),
            request: sent,
            trace: span,
        };

        if let Some((c, id)) = &op {
            c.record_status(id, status);
            if validate {
                c.validate_response(id, status, &response.body)?;
            }
        }

        Ok(response)
    }

    /// Whether this contract call is validated. Validation is sampled (every
    /// `validate_every`-th call) because it costs client CPU under load.
    fn should_validate(&self, is_contract_call: bool) -> bool {
        if !is_contract_call {
            return false;
        }

        let every = self.shared.config.validate_every.max(1);
        self.shared
            .validate_counter
            .fetch_add(1, Ordering::Relaxed)
            .is_multiple_of(every)
    }

    /// Headers in precedence order, later ones replacing earlier ones
    /// case-insensitively: run `User-Agent` → scope defaults → request/actor → bearer.
    fn explicit_headers(&self, opts: &RequestOpts) -> Vec<(String, String)> {
        let mut headers = vec![("User-Agent".into(), self.shared.config.user_agent.clone())];
        for (n, v) in self.default_headers() {
            put_header(&mut headers, &n, v);
        }

        for (n, v) in &opts.headers {
            put_header(&mut headers, n, scalar(v));
        }

        if let Some(t) = &opts.bearer {
            put_header(&mut headers, "Authorization", format!("Bearer {t}"));
        }

        headers
    }

    /// Appends trace headers for a new span unless tracing is off for this request
    /// (`trace` overrides the configured default). Trace headers never replace
    /// headers that were set explicitly. Returns the span, if one was sent.
    fn add_trace_headers(
        &self,
        headers: &mut Vec<(String, String)>,
        trace: Option<bool>,
    ) -> Option<SpanContext> {
        let cfg = &self.shared.config.tracing;
        if !trace.unwrap_or(cfg.enabled) {
            return None;
        }

        let trace = self.trace();
        let span = trace.span();
        for (n, v) in crate::trace::headers(&span, &trace, cfg, &self.trace_baggage()) {
            if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case(&n)) {
                headers.push((n, v));
            }
        }

        Some(span)
    }

    /// Configured baggage plus, if enabled, the current suite and test names.
    fn trace_baggage(&self) -> BTreeMap<String, String> {
        let cfg = &self.shared.config.tracing;
        let mut baggage = cfg.baggage.clone();
        if !cfg.test_baggage {
            return baggage;
        }

        if let Some(suite) = self.name_of(Scope::Suite) {
            baggage.insert("apitest.suite".into(), suite);
        }

        if let Some(test) = self.name_of(Scope::Test) {
            baggage.insert("apitest.test".into(), test);
        }

        baggage
    }

    /// The `reqwest` request with headers, query and body (JSON, else form, else raw).
    fn build_request(
        &self,
        method: reqwest::Method,
        url: &str,
        headers: &[(String, String)],
        opts: &RequestOpts,
    ) -> reqwest::RequestBuilder {
        let mut req = self.shared.http.request(method, url);
        for (n, v) in headers {
            req = req.header(n.as_str(), v.as_str());
        }

        if !opts.query.is_empty() {
            let q: Vec<(String, String)> = opts
                .query
                .iter()
                .map(|(k, v)| (k.clone(), scalar(v)))
                .collect();
            req = req.query(&q);
        }

        if let Some(j) = &opts.json {
            req.json(j)
        } else if let Some(f) = &opts.form {
            let f: Vec<(String, String)> = f.iter().map(|(k, v)| (k.clone(), scalar(v))).collect();
            req.form(&f)
        } else if let Some(b) = &opts.body {
            req.body(b.clone())
        } else {
            req
        }
    }

    /// Feeds the load metrics of this scope: 5xx and transport errors count as failures.
    #[cfg(feature = "load")]
    fn record_metrics(&self, result: &reqwest::Result<Received>, elapsed: std::time::Duration) {
        if let Some(m) = self.metrics() {
            let failed = match result {
                Ok((status, _, _)) => *status >= 500,
                Err(_) => true,
            };
            m.record_http(elapsed, failed);
        }
    }
}

/// Replaces any header named `name` (case-insensitively) with `value`, appended last.
fn put_header(headers: &mut Vec<(String, String)>, name: &str, value: String) {
    headers.retain(|(k, _)| !k.eq_ignore_ascii_case(name));
    headers.push((name.to_string(), value));
}

/// Fills `{placeholders}` in `path` (percent-encoded) and prefixes `base_url`
/// unless the path already is an absolute `http(s)://` URL.
fn render_url(path: &str, params: &Map<String, Value>, base_url: &str) -> Result<String> {
    let mut rendered = path.to_string();
    for (k, v) in params {
        rendered = rendered.replace(&format!("{{{k}}}"), &encode_segment(&scalar(v)));
    }

    if rendered.contains('{') {
        bail!("unfilled path parameter in '{rendered}' (pass it via `path`)");
    }

    if rendered.starts_with("http://") || rendered.starts_with("https://") {
        Ok(rendered)
    } else {
        Ok(format!("{}{}", base_url.trim_end_matches('/'), rendered))
    }
}

/// Sends the request and reads the whole body.
async fn fetch(req: reqwest::RequestBuilder) -> reqwest::Result<Received> {
    let r = req.send().await?;
    let status = r.status().as_u16();
    let headers = r
        .headers()
        .iter()
        .map(|(k, v)| {
            (
                k.as_str().to_string(),
                v.to_str().unwrap_or_default().to_string(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let text = r.text().await?;

    Ok((status, headers, text))
}

/// The body as JSON, as a JSON string if it is not JSON, or `null` if empty.
fn parse_body(text: &str) -> Value {
    if text.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_string()))
    }
}

/// Percent-encodes everything except RFC 3986 unreserved characters, so the
/// value stays a single path segment.
fn encode_segment(s: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(char::from(b));
            }
            _ => {
                out.push('%');
                out.push(char::from(HEX[usize::from(b >> 4)]));
                out.push(char::from(HEX[usize::from(b & 0x0f)]));
            }
        }
    }

    out
}
