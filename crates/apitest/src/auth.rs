//! Authentication as data.
//!
//! ```no_run
//! # use apitest::prelude::*;
//! # fn demo(cx: &Ctx, token: &str, key: &str, token_url: &str, id: &str, secret: &str) -> Result<()> {
//! let static_token = Actor::on("hub").bearer(token);
//! let basic = Actor::on("hub").basic("user", "pw");
//! let any_header = Actor::on("hub").api_key("X-Api-Key", key);
//! let lazy_login = Actor::on("hub").login("admin@hub.test", "secret"); // via the service's LoginFlow
//! let oauth = Actor::on("reports").client_credentials(ClientCredentials::new(token_url, id, secret));
//! let named = actor("hub_admin")?; // from apitest.nova / RunConfig
//! cx.bearer(token); // everything in this scope
//! # Ok(())
//! # }
//! ```
//!
//! Dynamic credentials (`login`, `client_credentials`) are resolved when a request
//! is sent, cached per service + identity for the run (one login, even with many
//! parallel requests), refreshed before they expire, and re-acquired once if the
//! service answers `401` (e.g. because a test revoked the token).

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::context::Ctx;
use crate::http::{Request, Response};

/// How an actor authenticates.
///
/// String fields may contain `${NAME}` placeholders; they are resolved from run
/// variables or the environment when a request is sent (see [`interpolate`]).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Auth {
    /// No credentials of its own: the request falls back to the scope's auth, or
    /// goes out unauthenticated.
    #[default]
    None,
    /// `Authorization: Bearer <token>`.
    Bearer {
        /// The token, without the `Bearer ` prefix.
        token: String,
    },
    /// `Authorization: Basic base64(username:password)`.
    Basic {
        /// User name; must not contain `:`.
        username: String,
        /// Password, sent base64-encoded (not encrypted).
        password: String,
    },
    /// Any header, e.g. `X-Api-Key: …`.
    Header {
        /// Header name, sent as given.
        name: String,
        /// Header value, sent as given.
        value: String,
    },
    /// Username/password exchanged for a token via the service's [`LoginFlow`].
    Login {
        /// Service to log in on; `None` means the actor's service.
        service: Option<String>,
        /// Substituted for `{username}` in the login body.
        username: String,
        /// Substituted for `{password}` in the login body.
        password: String,
    },
    /// OAuth 2.0 client-credentials grant.
    ClientCredentials(ClientCredentials),
}

impl Auth {
    /// `Authorization: Bearer <token>`.
    #[must_use]
    pub fn bearer(token: impl Into<String>) -> Auth {
        Auth::Bearer {
            token: token.into(),
        }
    }

    /// HTTP Basic auth.
    #[must_use]
    pub fn basic(username: impl Into<String>, password: impl Into<String>) -> Auth {
        Auth::Basic {
            username: username.into(),
            password: password.into(),
        }
    }

    /// The credential as an arbitrary header (`X-Api-Key`, ...).
    #[must_use]
    pub fn header(name: impl Into<String>, value: impl Into<String>) -> Auth {
        Auth::Header {
            name: name.into(),
            value: value.into(),
        }
    }

    /// A lazy login on `service` through its [`LoginFlow`].
    #[must_use]
    pub fn login(service: &str, username: impl Into<String>, password: impl Into<String>) -> Auth {
        Auth::Login {
            service: Some(service.into()),
            username: username.into(),
            password: password.into(),
        }
    }

    /// Every string field that may hold `${NAME}`: its path under `auth.`, its
    /// template, and whether it holds a secret (and must be masked in output).
    pub(crate) fn template_fields(&self) -> Vec<TemplateField<'_>> {
        match self {
            Auth::None => vec![],
            Auth::Bearer { token } => vec![TemplateField::new("auth.token", token, true)],
            Auth::Basic { username, password }
            | Auth::Login {
                username, password, ..
            } => vec![
                TemplateField::new("auth.username", username, false),
                TemplateField::new("auth.password", password, true),
            ],
            Auth::Header { value, .. } => vec![TemplateField::new("auth.value", value, true)],
            Auth::ClientCredentials(c) => c.template_fields(),
        }
    }

    /// Whether the header comes from a request to another endpoint (login or
    /// token), so it is cached and may need re-acquiring after a `401`.
    pub(crate) fn is_dynamic(&self) -> bool {
        matches!(self, Auth::Login { .. } | Auth::ClientCredentials(_))
    }

    /// Replaces `${NAME}` in all string fields with variables / environment values.
    ///
    /// Header names and the login service are taken literally.
    pub(crate) fn interpolated(&self, cx: &Ctx) -> Result<Auth> {
        let i = |s: &String| interpolate(s, cx);
        Ok(match self {
            Auth::None => Auth::None,
            Auth::Bearer { token } => Auth::Bearer { token: i(token)? },
            Auth::Basic { username, password } => Auth::Basic {
                username: i(username)?,
                password: i(password)?,
            },
            Auth::Header { name, value } => Auth::Header {
                name: name.clone(),
                value: i(value)?,
            },
            Auth::Login {
                service,
                username,
                password,
            } => Auth::Login {
                service: service.clone(),
                username: i(username)?,
                password: i(password)?,
            },
            Auth::ClientCredentials(c) => Auth::ClientCredentials(ClientCredentials {
                token_url: i(&c.token_url)?,
                client_id: i(&c.client_id)?,
                client_secret: i(&c.client_secret)?,
                scope: c.scope.as_ref().map(i).transpose()?,
                audience: c.audience.as_ref().map(i).transpose()?,
                basic_auth: c.basic_auth,
            }),
        })
    }
}

/// OAuth 2.0 client credentials (RFC 6749 §4.4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClientCredentials {
    /// Absolute URL of the token endpoint (not resolved against a service).
    pub token_url: String,
    /// OAuth client id.
    pub client_id: String,
    /// OAuth client secret. Cache keys contain only a hash of it.
    pub client_secret: String,
    /// Space-separated scopes to request; omitted from the request when `None`.
    #[serde(default)]
    pub scope: Option<String>,
    /// `audience` parameter some providers require; omitted when `None`.
    #[serde(default)]
    pub audience: Option<String>,
    /// Send the client credentials as HTTP Basic instead of form fields.
    #[serde(default)]
    pub basic_auth: bool,
}

impl ClientCredentials {
    /// Credentials sent as form fields, without scope or audience.
    #[must_use]
    pub fn new(
        token_url: impl Into<String>,
        client_id: impl Into<String>,
        client_secret: impl Into<String>,
    ) -> Self {
        ClientCredentials {
            token_url: token_url.into(),
            client_id: client_id.into(),
            client_secret: client_secret.into(),
            scope: None,
            audience: None,
            basic_auth: false,
        }
    }

    /// Requests these (space-separated) scopes.
    #[must_use]
    pub fn scope(mut self, scope: impl Into<String>) -> Self {
        self.scope = Some(scope.into());
        self
    }

    /// Requests a token for this audience.
    #[must_use]
    pub fn audience(mut self, audience: impl Into<String>) -> Self {
        self.audience = Some(audience.into());
        self
    }

    /// The string fields that may hold `${NAME}` (see [`Auth::template_fields`]).
    fn template_fields(&self) -> Vec<TemplateField<'_>> {
        let mut fields = vec![
            TemplateField::new("auth.token_url", &self.token_url, false),
            TemplateField::new("auth.client_id", &self.client_id, false),
            TemplateField::new("auth.client_secret", &self.client_secret, true),
        ];
        fields.extend(
            self.scope
                .as_ref()
                .map(|s| TemplateField::new("auth.scope", s, false)),
        );
        fields.extend(
            self.audience
                .as_ref()
                .map(|a| TemplateField::new("auth.audience", a, false)),
        );

        fields
    }
}

/// How a service turns username + password into a token (`auth:` in `apitest.nova`).
///
/// Default: `POST` the contract operation `login` with
/// `{"username": "{username}", "password": "{password}"}`, take the token from
/// `/token` (or `/access_token`), send it as `Authorization: Bearer <token>`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LoginFlow {
    /// operationId on the service's contract …
    pub operation: Option<String>,
    /// … or a raw path (`POST`) if there is no contract.
    pub path: Option<String>,
    /// Request body; `{username}` and `{password}` in strings are replaced.
    pub body: Value,
    /// Send the body as `application/x-www-form-urlencoded` instead of JSON.
    pub form: bool,
    /// JSON pointer to the token in the response; `/access_token` is the fallback.
    pub token: String,
    /// JSON pointer to the lifetime in seconds, if the service reports one.
    /// `/expires_in` is the fallback; without either the token is reused for the run.
    pub expires_in: Option<String>,
    /// Header to put the token in.
    pub header: String,
    /// Scheme written before the token (`Bearer`); `None` sends the raw token.
    pub scheme: Option<String>,
}

impl Default for LoginFlow {
    fn default() -> Self {
        LoginFlow {
            operation: Some("login".into()),
            path: None,
            body: json!({ "username": "{username}", "password": "{password}" }),
            form: false,
            token: "/token".into(),
            expires_in: None,
            header: "Authorization".into(),
            scheme: Some("Bearer".into()),
        }
    }
}

impl LoginFlow {
    /// Login through a contract operation.
    #[must_use]
    pub fn operation(op: &str) -> Self {
        LoginFlow {
            operation: Some(op.into()),
            ..Default::default()
        }
    }

    /// Login through a raw `POST` path.
    #[must_use]
    pub fn path(path: &str) -> Self {
        LoginFlow {
            operation: None,
            path: Some(path.into()),
            ..Default::default()
        }
    }

    /// Body template; `"{username}"` / `"{password}"` are substituted.
    #[must_use]
    pub fn body(mut self, body: Value) -> Self {
        self.body = body;
        self
    }

    /// Sends the body as form fields instead of JSON (non-string values as JSON text).
    #[must_use]
    pub fn form(mut self) -> Self {
        self.form = true;
        self
    }

    /// Reads the token from this JSON pointer instead of `/token`.
    #[must_use]
    pub fn token_at(mut self, pointer: &str) -> Self {
        self.token = pointer.into();
        self
    }

    /// Reads the token lifetime (seconds) from this JSON pointer.
    #[must_use]
    pub fn expires_at(mut self, pointer: &str) -> Self {
        self.expires_in = Some(pointer.into());
        self
    }
}

/// A named actor from the configuration (`actors:` in `apitest.nova`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ActorConfig {
    /// Service the actor calls; also the login service when `auth` names none.
    pub service: Option<String>,
    /// Credentials for every request of the actor.
    pub auth: Auth,
    /// Extra headers; values may contain `${NAME}` placeholders.
    pub headers: BTreeMap<String, String>,
    /// `User-Agent` for the actor's requests (taken literally).
    pub user_agent: Option<String>,
}

impl ActorConfig {
    /// Every string field that may hold `${NAME}` (see [`Auth::template_fields`]).
    /// Header values count as secret: they usually carry keys or tokens.
    pub(crate) fn template_fields(&self) -> Vec<TemplateField<'_>> {
        let mut fields = self.auth.template_fields();
        fields.extend(
            self.headers.iter().map(|(name, template)| {
                TemplateField::new(format!("headers.{name}"), template, true)
            }),
        );

        fields
    }
}

/// A string field of an [`Auth`] or [`ActorConfig`] that may hold `${NAME}`.
pub(crate) struct TemplateField<'a> {
    /// Where the field is, e.g. `auth.password` or `headers.X-Api-Key`.
    pub(crate) path: String,
    /// The field's value before interpolation.
    pub(crate) template: &'a str,
    /// Whether the resolved value is a secret that must be masked in output.
    pub(crate) secret: bool,
}

impl<'a> TemplateField<'a> {
    /// A field at `path` holding `template`.
    fn new(path: impl Into<String>, template: &'a str, secret: bool) -> Self {
        TemplateField {
            path: path.into(),
            template,
            secret,
        }
    }
}

/// `${NAME}` → `cx.env("NAME")`; `$${` escapes a literal `${`.
///
/// # Errors
///
/// Fails when a `${` has no closing `}` or names no variable, or when a
/// referenced variable is not set in any source (see [`Ctx::env`]). The error
/// never quotes the template: it may hold a secret.
pub fn interpolate(s: &str, cx: &Ctx) -> Result<String> {
    interpolate_with(s, |name| cx.env(name))
}

/// The variable names `s` refers to with `${NAME}`, in order, with repeats.
///
/// # Errors
///
/// Fails like [`interpolate`] on a malformed template.
pub(crate) fn references(s: &str) -> Result<Vec<String>> {
    let mut names = vec![];
    interpolate_with(s, |name| {
        names.push(name.to_string());
        Ok(String::new())
    })?;

    Ok(names)
}

/// `${NAME}` → `lookup("NAME")`; `$${` escapes a literal `${`.
///
/// # Errors
///
/// Fails when a `${` has no closing `}` or names no variable, or when `lookup`
/// fails.
pub(crate) fn interpolate_with(
    s: &str,
    mut lookup: impl FnMut(&str) -> Result<String>,
) -> Result<String> {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find("${") {
        if rest[..i].ends_with('$') {
            out.push_str(&rest[..i - 1]);
            out.push_str("${");
            rest = &rest[i + 2..];
            continue;
        }

        out.push_str(&rest[..i]);
        let at = s.len() - rest.len() + i;
        let end = rest[i..]
            .find('}')
            .ok_or_else(|| anyhow!("unterminated ${{ at byte {at}"))?;
        let name = &rest[i + 2..i + end];
        if name.is_empty() {
            bail!("empty variable name `${{}}` at byte {at}");
        }

        out.push_str(&lookup(name)?);
        rest = &rest[i + end + 1..];
    }

    out.push_str(rest);
    Ok(out)
}

/// Standard base64 (RFC 4648 §4) with `=` padding.
pub(crate) fn base64(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
        for (i, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            if i <= chunk.len() {
                out.push(T[(n >> shift & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }

    out
}

/// An acquired credential header and when to stop reusing it.
#[derive(Clone)]
struct Cached {
    /// Header name and value to send.
    header: (String, String),
    /// Refresh deadline; `None` means the credential is reused for the whole run.
    expires: Option<Instant>,
}

/// One cache entry. Its async mutex is held while acquiring, so concurrent
/// requests for the same identity wait for a single login.
type Slot = Arc<tokio::sync::Mutex<Option<Cached>>>;

/// Process-wide credential cache, keyed by endpoint + identity (secrets hashed).
fn cache() -> &'static Mutex<HashMap<String, Slot>> {
    static C: OnceLock<Mutex<HashMap<String, Slot>>> = OnceLock::new();
    C.get_or_init(Default::default)
}

/// First 8 bytes of SHA-256 as 16 hex digits: keeps secrets out of cache keys.
fn digest(s: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(s.as_bytes())
        .iter()
        .take(8)
        .fold(String::with_capacity(16), |mut out, b| {
            // Writing to a String cannot fail.
            let _ = write!(out, "{b:02x}");
            out
        })
}

/// The header an [`Auth`] produces for a request. Dynamic credentials are cached;
/// `refresh` forces a new login / token.
///
/// Every secret involved is registered with the run's
/// [`Redactor`](crate::redact::Redactor) as soon as it is known: the variables
/// behind secret fields before interpolation can fail, the resolved secrets, and
/// the header value. The header is registered after the credential cache, which
/// outlives runs, so a run that gets a cached token still masks it.
pub(crate) async fn resolve(
    cx: &Ctx,
    auth: &Auth,
    refresh: bool,
) -> Result<Option<(String, String)>> {
    let redactor = &cx.config().redactor;
    register_secret_variables(cx, auth);
    let auth = auth.interpolated(cx)?;
    for f in auth.template_fields().iter().filter(|f| f.secret) {
        redactor.secret(f.template);
    }

    let header = resolve_header(cx, &auth, refresh).await?;
    if let Some((_, value)) = &header {
        redactor.header_value(value);
    }

    Ok(header)
}

/// Registers the values of the variables that `auth`'s secret fields refer to.
fn register_secret_variables(cx: &Ctx, auth: &Auth) {
    for f in auth.template_fields().iter().filter(|f| f.secret) {
        for name in references(f.template).unwrap_or_default() {
            if let Some(v) = cx.env_opt(&name) {
                cx.config().redactor.secret(&v);
            }
        }
    }
}

/// The header an interpolated [`Auth`] produces (see [`resolve`]).
async fn resolve_header(cx: &Ctx, auth: &Auth, refresh: bool) -> Result<Option<(String, String)>> {
    match auth {
        Auth::None => Ok(None),
        Auth::Bearer { token } => Ok(Some(("Authorization".into(), format!("Bearer {token}")))),
        Auth::Basic { username, password } => Ok(Some(basic_header(username, password))),
        Auth::Header { name, value } => Ok(Some((name.clone(), value.clone()))),
        Auth::Login {
            service,
            username,
            password,
        } => resolve_login(cx, service.as_deref(), username, password, refresh)
            .await
            .map(Some),
        Auth::ClientCredentials(c) => resolve_client_credentials(cx, c, refresh).await.map(Some),
    }
}

/// `Authorization: Basic base64(username:password)`.
fn basic_header(username: &str, password: &str) -> (String, String) {
    (
        "Authorization".into(),
        format!(
            "Basic {}",
            base64(format!("{username}:{password}").as_bytes())
        ),
    )
}

/// A login header from the cache, or from a fresh login through the service's flow.
async fn resolve_login(
    cx: &Ctx,
    service: Option<&str>,
    username: &str,
    password: &str,
    refresh: bool,
) -> Result<(String, String)> {
    let service = service.ok_or_else(|| {
        anyhow!("login as '{username}': no service (use Actor::on(service).login(..))")
    })?;
    let flow = cx.config().auth.get(service).cloned().unwrap_or_default();
    let base = cx
        .config()
        .service_url(Some(service))
        .unwrap_or(&cx.config().base_url)
        .to_string();
    let key = format!(
        "login|{base}|{service}|{username}|{}|{}",
        digest(password),
        digest(&serde_json::to_string(&flow)?)
    );

    cached(&key, refresh, || {
        login(cx, service, &flow, username, password)
    })
    .await
    .with_context(|| format!("login as '{username}' on '{service}' failed"))
}

/// A client-credentials header from the cache, or from a fresh token request.
async fn resolve_client_credentials(
    cx: &Ctx,
    c: &ClientCredentials,
    refresh: bool,
) -> Result<(String, String)> {
    let key = format!(
        "cc|{}|{}|{}|{}",
        c.token_url,
        c.client_id,
        c.scope.as_deref().unwrap_or(""),
        digest(&c.client_secret)
    );

    cached(&key, refresh, || client_credentials(cx, c))
        .await
        .with_context(|| {
            format!(
                "client-credentials token for '{}' from {} failed",
                c.client_id, c.token_url
            )
        })
}

/// The cached header for `key` while it is fresh (and `refresh` is false),
/// otherwise the header from `acquire`, which then replaces the cache entry.
async fn cached<F, Fut>(key: &str, refresh: bool, acquire: F) -> Result<(String, String)>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<Cached>>,
{
    let slot = cache()
        .lock()
        .unwrap()
        .entry(key.to_string())
        .or_default()
        .clone();

    // Single-flight: concurrent requests for the same identity wait for one login.
    let mut guard = slot.lock().await;
    if !refresh
        && let Some(c) = guard.as_ref()
        && c.expires.is_none_or(|e| Instant::now() < e)
    {
        return Ok(c.header.clone());
    }

    let fresh = acquire().await?;
    let header = fresh.header.clone();
    *guard = Some(fresh);
    Ok(header)
}

/// Refresh a bit before expiry: 30 s, or a third of the lifetime for short-lived tokens.
fn expiry(seconds: Option<u64>) -> Option<Instant> {
    seconds.map(|s| {
        let margin = (s / 3).min(30);
        Instant::now() + Duration::from_secs(s.saturating_sub(margin))
    })
}

/// Replaces `{username}` / `{password}` in every string of a JSON template.
fn substitute(v: &Value, username: &str, password: &str) -> Value {
    match v {
        Value::String(s) => Value::String(
            s.replace("{username}", username)
                .replace("{password}", password),
        ),
        Value::Array(a) => Value::Array(
            a.iter()
                .map(|x| substitute(x, username, password))
                .collect(),
        ),
        Value::Object(o) => Value::Object(
            o.iter()
                .map(|(k, x)| (k.clone(), substitute(x, username, password)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Logs in through `flow` and extracts the token header and its expiry.
async fn login(
    cx: &Ctx,
    service: &str,
    flow: &LoginFlow,
    username: &str,
    password: &str,
) -> Result<Cached> {
    let body = substitute(&flow.body, username, password);
    let req = login_request(cx, service, flow)?;
    let req = if flow.form {
        let fields = form_fields(&body);
        let refs: Vec<(&str, &str)> = fields
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        req.form(&refs)
    } else {
        req.json(body)
    };

    let resp = ensure_success(req.await?)?;
    let token: String = resp
        .get(&flow.token)
        .or_else(|_| resp.get("/access_token"))
        .with_context(|| format!("no token at '{}' in the login response", flow.token))?;
    let lifetime = flow
        .expires_in
        .as_deref()
        .and_then(|p| resp.get::<u64>(p).ok())
        .or_else(|| resp.get::<u64>("/expires_in").ok());
    let value = match &flow.scheme {
        Some(s) => format!("{s} {token}"),
        None => token,
    };

    Ok(Cached {
        header: (flow.header.clone(), value),
        expires: expiry(lifetime),
    })
}

/// The login request without a body: the contract operation if the service has
/// a contract, else the raw `POST` path.
fn login_request(cx: &Ctx, service: &str, flow: &LoginFlow) -> Result<Request<'static>> {
    match (&flow.operation, &flow.path) {
        (Some(op), _) if cx.contract(service).is_ok() => {
            Ok(Request::op(Some(cx.clone()), Some(service.to_string()), op))
        }
        (_, Some(path)) => Ok(Request::raw(Some(cx.clone()), "POST", path).service(service)),
        (Some(op), None) => bail!(
            "service '{service}' has no contract with operation '{op}'; set `path` in its login flow"
        ),
        (None, None) => bail!("login flow for '{service}' has neither `operation` nor `path`"),
    }
}

/// The top-level fields of a JSON object as form fields (strings unquoted, the
/// rest as JSON text). Anything but an object yields no fields.
fn form_fields(body: &Value) -> Vec<(String, String)> {
    body.as_object()
        .map(|o| {
            o.iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        v.as_str().map_or_else(|| v.to_string(), str::to_string),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Passes 2xx responses through; anything else fails with the status and the
/// first 300 characters of the body.
fn ensure_success(resp: Response) -> Result<Response> {
    if !(200..300).contains(&resp.status) {
        bail!(
            "status {}: {}",
            resp.status,
            resp.text.chars().take(300).collect::<String>()
        );
    }

    Ok(resp)
}

/// Requests a token with the client-credentials grant.
async fn client_credentials(cx: &Ctx, c: &ClientCredentials) -> Result<Cached> {
    let mut fields = vec![("grant_type", "client_credentials".to_string())];
    if !c.basic_auth {
        fields.push(("client_id", c.client_id.clone()));
        fields.push(("client_secret", c.client_secret.clone()));
    }

    if let Some(s) = &c.scope {
        fields.push(("scope", s.clone()));
    }

    if let Some(a) = &c.audience {
        fields.push(("audience", a.clone()));
    }

    let refs: Vec<(&str, &str)> = fields.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let mut req = Request::raw(Some(cx.clone()), "POST", &c.token_url).form(&refs);
    if c.basic_auth {
        let (name, value) = basic_header(&c.client_id, &c.client_secret);
        req = req.header(&name, value);
    }

    let resp = ensure_success(req.await?)?;
    let token: String = resp.get("/access_token")?;
    let kind = resp
        .get::<String>("/token_type")
        .unwrap_or_else(|_| "Bearer".into());
    // RFC 6749 token_type is case-insensitive; normalise the common one.
    let kind = if kind.eq_ignore_ascii_case("bearer") {
        "Bearer".to_string()
    } else {
        kind
    };

    Ok(Cached {
        header: ("Authorization".into(), format!("{kind} {token}")),
        expires: expiry(resp.get::<u64>("/expires_in").ok()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc4648() {
        for (i, o) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(i.as_bytes()), o);
        }
        assert_eq!(
            base64(b"Aladdin:open sesame"),
            "QWxhZGRpbjpvcGVuIHNlc2FtZQ=="
        );
    }

    #[test]
    fn substitutes_credentials() {
        let v = substitute(
            &json!({ "email": "{username}", "nested": ["{password}"], "n": 1 }),
            "a@b",
            "pw",
        );
        assert_eq!(v, json!({ "email": "a@b", "nested": ["pw"], "n": 1 }));
    }

    #[test]
    fn auth_is_plain_data() {
        let a = Auth::login("hub", "${HUB_ADMIN_EMAIL}", "${HUB_ADMIN_PASSWORD}");
        let v = serde_json::to_value(&a).unwrap();
        assert_eq!(v["type"], "login");
        assert_eq!(serde_json::from_value::<Auth>(v).unwrap(), a);
    }

    #[test]
    fn templates_name_their_variables_and_errors_quote_nothing() {
        assert_eq!(
            references("Bearer ${T}-${U}-$${LITERAL}-${T}").unwrap(),
            ["T", "U", "T"]
        );
        let unterminated = references("hunter2${OPEN").unwrap_err().to_string();
        assert_eq!(unterminated, "unterminated ${ at byte 7");
        let empty = references("hunter2${}").unwrap_err().to_string();
        assert!(empty.contains("empty variable name"), "{empty}");
        assert!(!empty.contains("hunter2"), "{empty}");
    }
}
