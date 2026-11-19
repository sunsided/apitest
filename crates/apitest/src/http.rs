//! HTTP layer: language-neutral request options, responses, and contract-aware sending.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use anyhow::{Context as _, Result, anyhow, bail};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::context::Ctx;
use crate::contract::Contract;

/// Request options. This is the shape every frontend passes in
/// (a Rhai map, a JS object, a Python dict all deserialize into it).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RequestOpts {
    /// Values for `{placeholders}` in the path template.
    pub path: Map<String, Value>,
    pub query: Map<String, Value>,
    pub headers: Map<String, Value>,
    pub json: Option<Value>,
    pub form: Option<Map<String, Value>>,
    pub body: Option<String>,
    /// Shorthand for an `Authorization: Bearer ...` header.
    pub bearer: Option<String>,
    /// Named service for raw requests (see `RunConfig::services`).
    pub service: Option<String>,
    /// `false` suppresses trace headers for this request, `true` forces them even
    /// if tracing is disabled. Default: the run's / suite's tracing setting.
    pub trace: Option<bool>,
    /// Credentials for this request (`{ type: "basic", username, password }`, ...).
    pub auth: Option<crate::auth::Auth>,
    /// Send as a named actor from the configuration (its service, headers and auth).
    #[serde(rename = "as")]
    pub as_actor: Option<String>,
}

/// What was actually sent, as seen on the wire (after all defaults, auth and tracing).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SentRequest {
    pub method: String,
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
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    /// Parsed JSON body, or the body as a string if it was not JSON (`null` when empty).
    pub body: Value,
    pub text: String,
    pub elapsed_ms: f64,
    /// `contract:operationId` for contract calls.
    pub operation: Option<String>,
    /// The request as sent (method, URL, all headers incl. trace and User-Agent).
    pub request: SentRequest,
    /// The span this request was sent as (`None` if tracing was off for it).
    pub trace: Option<crate::trace::SpanContext>,
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
    pub fn expect(self, status: u16) -> Result<Self> {
        if self.status != status {
            bail!(
                "expected status {status}, got {}{}: {}",
                self.status,
                self.operation
                    .as_deref()
                    .map(|o| format!(" from {o}"))
                    .unwrap_or_default(),
                truncate(&self.text, 300)
            );
        }
        Ok(self)
    }

    /// A value from the body by JSON pointer (`"/id"`, `"/items/0/code"`; `"id"` works
    /// too), deserialized into `T`. `""` is the whole body.
    ///
    /// ```ignore
    /// let id: String = resp.get("/id")?;
    /// ```
    pub fn get<T: DeserializeOwned>(&self, pointer: &str) -> Result<T> {
        let ptr = if pointer.is_empty() || pointer.starts_with('/') {
            pointer.to_string()
        } else {
            format!("/{pointer}")
        };
        let what = self
            .operation
            .as_deref()
            .map(|o| format!(" of {o}"))
            .unwrap_or_default();
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
    pub fn expect_json(self, expected: &Value) -> Result<Self> {
        crate::dsl::json_includes(&self.body, expected).with_context(|| {
            format!(
                "response{} does not match",
                self.operation
                    .as_deref()
                    .map(|o| format!(" of {o}"))
                    .unwrap_or_default()
            )
        })?;
        Ok(self)
    }

    /// Deserializes the JSON body.
    pub fn json<T: DeserializeOwned>(&self) -> Result<T> {
        serde_json::from_value(self.body.clone()).context("response body has unexpected shape")
    }

    /// JSON pointer into the body, e.g. `/items/0/id`.
    #[must_use]
    pub fn pointer(&self, ptr: &str) -> Option<&Value> {
        self.body.pointer(ptr)
    }

    /// The response as a plain value (what scripts receive).
    #[must_use]
    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).expect("response is serializable")
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

enum Target {
    Raw {
        method: String,
        path: String,
    },
    Op {
        contract: Option<String>,
        op: String,
    },
}

type Rollback<'a> = Box<dyn FnOnce(&Response) -> Result<Request<'static>> + Send + 'a>;

/// A request builder. Await it directly (`req.await?`) or call `.send().await`.
///
/// Requests built from an [`crate::dsl::Actor`] resolve their context, contract and
/// service when they are sent, so they can be created anywhere and also used as
/// cleanups ([`crate::dsl::undo`]).
#[must_use = "requests do nothing until awaited"]
pub struct Request<'a> {
    cx: Option<Ctx>,
    target: Target,
    opts: RequestOpts,
    auth: Option<crate::auth::Auth>,
    expect: Option<u16>,
    expect_json: Option<Value>,
    rollbacks: Vec<Rollback<'a>>,
}

/// Values a `.rollback(..)` closure may return.
pub trait IntoRequest {
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
    pub(crate) fn raw(cx: Option<Ctx>, method: &str, path: &str) -> Self {
        Request {
            cx,
            target: Target::Raw {
                method: method.to_ascii_uppercase(),
                path: path.to_string(),
            },
            opts: RequestOpts::default(),
            auth: None,
            expect: None,
            expect_json: None,
            rollbacks: vec![],
        }
    }

    pub(crate) fn op(cx: Option<Ctx>, contract: Option<String>, op: &str) -> Self {
        Request {
            cx,
            target: Target::Op {
                contract,
                op: op.to_string(),
            },
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
    /// ```ignore
    /// admin.call("createUser").json(&user).expect(201)
    ///     .rollback(|u| admin.call("deleteUser").path("id", &u["id"]).expect(204))
    ///     .await?;
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
    pub fn auth(mut self, auth: crate::auth::Auth) -> Self {
        self.auth = Some(auth);
        self
    }

    /// HTTP Basic auth for this request.
    pub fn basic(self, username: impl Into<String>, password: impl Into<String>) -> Self {
        self.auth(crate::auth::Auth::basic(username, password))
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
            target
        } else {
            let p: Vec<String> = self
                .opts
                .path
                .iter()
                .map(|(k, v)| format!("{k}={}", scalar(v)))
                .collect();
            format!("{target} {{{}}}", p.join(", "))
        }
    }

    pub fn path_param(mut self, name: &str, value: impl Serialize) -> Self {
        self.opts.path.insert(name.into(), to_value(value));
        self
    }
    pub fn query(mut self, name: &str, value: impl Serialize) -> Self {
        self.opts.query.insert(name.into(), to_value(value));
        self
    }
    pub fn header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.opts
            .headers
            .insert(name.into(), Value::String(value.into()));
        self
    }
    pub fn bearer(mut self, token: impl Into<String>) -> Self {
        self.opts.bearer = Some(token.into());
        self
    }
    pub fn json(mut self, body: impl Serialize) -> Self {
        self.opts.json = Some(to_value(body));
        self
    }
    pub fn form(mut self, fields: &[(&str, &str)]) -> Self {
        self.opts.form = Some(
            fields
                .iter()
                .map(|(k, v)| (k.to_string(), Value::String(v.to_string())))
                .collect(),
        );
        self
    }
    pub fn opts(mut self, opts: RequestOpts) -> Self {
        // Keep headers an actor attached unless the options override them.
        let mut headers = std::mem::take(&mut self.opts.headers);
        headers.extend(opts.headers.clone());
        self.opts = RequestOpts { headers, ..opts };
        self
    }

    pub async fn send(self) -> Result<Response> {
        let cx = match self.cx {
            Some(cx) => cx,
            None => crate::context::current()?,
        };
        let mut opts = self.opts;
        // A named actor (scripts: `{ as: "hub_admin" }`) contributes service,
        // headers and auth where the request has none of its own.
        let mut auth = self.auth.or(opts.auth.take());
        if let Some(name) = opts.as_actor.take() {
            let a = crate::dsl::Actor::from_config_in(&cx, &name)?;
            for (k, v) in a.headers {
                if !opts.headers.keys().any(|h| h.eq_ignore_ascii_case(&k)) {
                    opts.headers.insert(k, Value::String(v));
                }
            }
            if opts.service.is_none() {
                opts.service = a.service;
            }
            if auth.is_none() && a.auth != crate::auth::Auth::None {
                auth = Some(a.auth);
            }
        }
        let (method, path, op, base) = match self.target {
            Target::Raw { method, path } => {
                let base = cx
                    .config()
                    .service_url(opts.service.as_deref())?
                    .to_string();
                (method, path, None, base)
            }
            Target::Op { contract, op } => {
                let name = contract.ok_or_else(|| {
                    anyhow!("cannot call '{op}': this actor has no service/contract")
                })?;
                let c = cx.contract(&name)?;
                let o = c.operation(&op)?;
                let base = cx.config().contract_url(&c)?.to_string();
                (
                    o.method.clone(),
                    o.path.clone(),
                    Some((c.clone(), op)),
                    base,
                )
            }
        };
        let auth = auth
            .or_else(|| cx.scope_auth())
            .filter(|a| *a != crate::auth::Auth::None);
        // Explicit Authorization headers / bearer always win over any configured auth.
        let manual = opts.bearer.is_some()
            || opts
                .headers
                .keys()
                .any(|h| h.eq_ignore_ascii_case("authorization"));
        let auth = auth.filter(|_| !manual);

        let mut retry_opts = None;
        if let Some(a) = &auth
            && let Some((name, value)) = crate::auth::resolve(&cx, a, false).await?
        {
            opts.headers.insert(name, Value::String(value));
            if a.is_dynamic() {
                retry_opts = Some(opts.clone());
            }
        }
        let mut resp = cx.send(&method, &path, op.clone(), opts, &base).await?;
        // Cached logins/tokens can go stale (revoked, server restarted): re-acquire once.
        if resp.status == 401
            && let (Some(a), Some(mut again)) = (&auth, retry_opts)
            && let Some((name, value)) = crate::auth::resolve(&cx, a, true).await?
        {
            again.headers.insert(name, Value::String(value));
            resp = cx.send(&method, &path, op, again, &base).await?;
        }
        if let Some(status) = self.expect {
            resp = resp.expect(status)?;
        }
        if let Some(expected) = &self.expect_json {
            resp = resp.expect_json(expected)?;
        }
        for rb in self.rollbacks {
            let undo = rb(&resp).context("building rollback request")?;
            cx.undo(undo);
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

fn to_value(v: impl Serialize) -> Value {
    serde_json::to_value(v).unwrap_or(Value::Null)
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        let mut end = n;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &s[..end])
    }
}

fn scalar(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

impl Ctx {
    /// A raw (non-contract) request.
    pub fn request(&self, method: &str, path: &str) -> Request<'static> {
        Request::raw(Some(self.clone()), method, path)
    }

    /// A request to `operationId` of a pinned contract. Method and path come from
    /// the pinned spec; request and response are validated against it.
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
    pub async fn request_value(&self, method: &str, path: &str, opts: Value) -> Result<Response> {
        let opts: RequestOpts = serde_json::from_value(opts).context("invalid request options")?;
        // Through `Request` so auth, named actors and retries apply like everywhere else.
        Request::raw(Some(self.clone()), method, path)
            .opts(opts)
            .send()
            .await
    }

    /// Frontend entry point for contract calls (options as a value).
    pub async fn call_value(
        &self,
        contract: &str,
        operation: &str,
        opts: Value,
    ) -> Result<Response> {
        let opts: RequestOpts = serde_json::from_value(opts).context("invalid request options")?;
        self.call(contract, operation)?.opts(opts).send().await
    }

    pub(crate) async fn send(
        &self,
        method: &str,
        path: &str,
        op: Option<(Arc<Contract>, String)>,
        opts: RequestOpts,
        base_url: &str,
    ) -> Result<Response> {
        let cfg = &self.shared.config;

        // Contract validation is sampled under load: it costs CPU on the client.
        let validate = op.is_some() && {
            let every = cfg.validate_every.max(1);
            self.shared
                .validate_counter
                .fetch_add(1, Ordering::Relaxed)
                .is_multiple_of(every)
        };

        if let Some((c, id)) = &op {
            c.record_request(id, &opts);
            if validate && let Some(body) = &opts.json {
                c.validate_request(id, body)?;
            }
        }

        // Path template.
        let mut rendered = path.to_string();
        for (k, v) in &opts.path {
            rendered = rendered.replace(&format!("{{{k}}}"), &encode_segment(&scalar(v)));
        }
        if rendered.contains('{') {
            bail!("unfilled path parameter in '{rendered}' (pass it via `path`)");
        }
        let url = if rendered.starts_with("http://") || rendered.starts_with("https://") {
            rendered
        } else {
            format!("{}{}", base_url.trim_end_matches('/'), rendered)
        };

        let method_parsed = reqwest::Method::from_bytes(method.as_bytes())
            .map_err(|_| anyhow!("invalid HTTP method '{method}'"))?;
        // Headers, lowest precedence first; later entries replace earlier ones
        // case-insensitively: run User-Agent → scope defaults → request/actor → auth.
        let mut headers: Vec<(String, String)> =
            vec![("User-Agent".into(), cfg.user_agent.clone())];
        let put = |headers: &mut Vec<(String, String)>, n: &str, v: String| {
            headers.retain(|(k, _)| !k.eq_ignore_ascii_case(n));
            headers.push((n.to_string(), v));
        };
        for (n, v) in self.default_headers() {
            put(&mut headers, &n, v);
        }
        for (n, v) in &opts.headers {
            put(&mut headers, n, scalar(v));
        }
        if let Some(t) = &opts.bearer {
            put(&mut headers, "Authorization", format!("Bearer {t}"));
        }
        // Trace headers never replace headers that were set explicitly.
        let span = if opts.trace.unwrap_or(cfg.tracing.enabled) {
            let trace = self.trace();
            let span = trace.span();
            let mut baggage = cfg.tracing.baggage.clone();
            if cfg.tracing.test_baggage {
                if let Some(suite) = self.name_of(crate::context::Scope::Suite) {
                    baggage.insert("apitest.suite".into(), suite);
                }
                if let Some(test) = self.name_of(crate::context::Scope::Test) {
                    baggage.insert("apitest.test".into(), test);
                }
            }
            for (n, v) in crate::trace::headers(&span, &trace, &cfg.tracing, &baggage) {
                if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case(&n)) {
                    headers.push((n, v));
                }
            }
            Some(span)
        } else {
            None
        };
        let sent = SentRequest {
            method: method.to_string(),
            url: url.clone(),
            headers: headers.iter().cloned().collect(),
        };

        let mut req = self.shared.http.request(method_parsed, &url);
        for (n, v) in &headers {
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
            req = req.json(j);
        } else if let Some(f) = &opts.form {
            let f: Vec<(String, String)> = f.iter().map(|(k, v)| (k.clone(), scalar(v))).collect();
            req = req.form(&f);
        } else if let Some(b) = &opts.body {
            req = req.body(b.clone());
        }

        let started = Instant::now();
        let result = req.send().await;
        let result = match result {
            Ok(r) => {
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
                r.text().await.map(|t| (status, headers, t))
            }
            Err(e) => Err(e),
        };
        let elapsed = started.elapsed();

        #[cfg(feature = "load")]
        if let Some(m) = self.metrics() {
            let failed = match &result {
                Ok((status, _, _)) => *status >= 500,
                Err(_) => true,
            };
            m.record_http(elapsed, failed);
        }

        let (status, headers, text) = result.with_context(|| format!("{method} {url} failed"))?;
        let body = if text.is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&text).unwrap_or_else(|_| Value::String(text.clone()))
        };

        let response = Response {
            status,
            headers,
            body,
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
}

fn encode_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
