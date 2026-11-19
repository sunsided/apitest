//! Authentication as data.
//!
//! ```ignore
//! Actor::on("hub").bearer(token)                       // static token
//! Actor::on("hub").basic("user", "pw")                 // HTTP Basic
//! Actor::on("hub").api_key("X-Api-Key", key)           // any header
//! Actor::on("hub").login("admin@hub.test", "secret")   // lazy login via the service's LoginFlow
//! Actor::on("reports").client_credentials(ClientCredentials::new(token_url, id, secret))
//! actor("hub_admin")?                                  // named actor from apitest.nova / RunConfig
//! cx.bearer(token);                                    // everything in this scope
//! ```
//!
//! Dynamic credentials (`login`, `client_credentials`) are resolved when a request
//! is sent, cached per service + identity for the run (one login, even with many
//! parallel requests), refreshed before they expire, and re-acquired once if the
//! service answers `401` (e.g. because a test revoked the token).

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::context::Ctx;
use crate::http::Request;

/// How an actor authenticates.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Auth {
    #[default]
    None,
    /// `Authorization: Bearer <token>`.
    Bearer { token: String },
    /// `Authorization: Basic base64(username:password)`.
    Basic { username: String, password: String },
    /// Any header, e.g. `X-Api-Key: …`.
    Header { name: String, value: String },
    /// Username/password exchanged for a token via the service's [`LoginFlow`].
    Login {
        service: Option<String>,
        username: String,
        password: String,
    },
    /// OAuth 2.0 client-credentials grant.
    ClientCredentials(ClientCredentials),
}

impl Auth {
    pub fn bearer(token: impl Into<String>) -> Auth {
        Auth::Bearer {
            token: token.into(),
        }
    }
    pub fn basic(username: impl Into<String>, password: impl Into<String>) -> Auth {
        Auth::Basic {
            username: username.into(),
            password: password.into(),
        }
    }
    pub fn header(name: impl Into<String>, value: impl Into<String>) -> Auth {
        Auth::Header {
            name: name.into(),
            value: value.into(),
        }
    }
    pub fn login(service: &str, username: impl Into<String>, password: impl Into<String>) -> Auth {
        Auth::Login {
            service: Some(service.into()),
            username: username.into(),
            password: password.into(),
        }
    }

    pub(crate) fn is_dynamic(&self) -> bool {
        matches!(self, Auth::Login { .. } | Auth::ClientCredentials(_))
    }

    /// Replaces `${NAME}` in all string fields with variables / environment values.
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
    pub token_url: String,
    pub client_id: String,
    pub client_secret: String,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub audience: Option<String>,
    /// Send the client credentials as HTTP Basic instead of form fields.
    #[serde(default)]
    pub basic_auth: bool,
}

impl ClientCredentials {
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
    pub fn scope(mut self, scope: impl Into<String>) -> Self {
        self.scope = Some(scope.into());
        self
    }
    pub fn audience(mut self, audience: impl Into<String>) -> Self {
        self.audience = Some(audience.into());
        self
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
    /// JSON pointer to the token in the response.
    pub token: String,
    /// JSON pointer to the lifetime in seconds, if the service reports one.
    pub expires_in: Option<String>,
    /// Header to put the token in, and its scheme (`None` = raw token).
    pub header: String,
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
    #[must_use]
    pub fn form(mut self) -> Self {
        self.form = true;
        self
    }
    #[must_use]
    pub fn token_at(mut self, pointer: &str) -> Self {
        self.token = pointer.into();
        self
    }
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
    pub service: Option<String>,
    pub auth: Auth,
    pub headers: BTreeMap<String, String>,
    pub user_agent: Option<String>,
}

/// `${NAME}` → `cx.env("NAME")`; `$${` escapes a literal `${`.
pub fn interpolate(s: &str, cx: &Ctx) -> Result<String> {
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
        let end = rest[i..]
            .find('}')
            .ok_or_else(|| anyhow!("unterminated ${{ in '{s}'"))?;
        let name = &rest[i + 2..i + end];
        out.push_str(&cx.env(name)?);
        rest = &rest[i + end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

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

#[derive(Clone)]
struct Cached {
    header: (String, String),
    expires: Option<Instant>,
}

type Slot = Arc<tokio::sync::Mutex<Option<Cached>>>;

fn cache() -> &'static Mutex<HashMap<String, Slot>> {
    static C: OnceLock<Mutex<HashMap<String, Slot>>> = OnceLock::new();
    C.get_or_init(Default::default)
}

fn digest(s: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(s.as_bytes())
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The header an [`Auth`] produces for a request. Dynamic credentials are cached;
/// `refresh` forces a new login / token.
pub(crate) async fn resolve(
    cx: &Ctx,
    auth: &Auth,
    refresh: bool,
) -> Result<Option<(String, String)>> {
    let auth = auth.interpolated(cx)?;
    match &auth {
        Auth::None => Ok(None),
        Auth::Bearer { token } => Ok(Some(("Authorization".into(), format!("Bearer {token}")))),
        Auth::Basic { username, password } => Ok(Some((
            "Authorization".into(),
            format!(
                "Basic {}",
                base64(format!("{username}:{password}").as_bytes())
            ),
        ))),
        Auth::Header { name, value } => Ok(Some((name.clone(), value.clone()))),
        Auth::Login {
            service,
            username,
            password,
        } => {
            let service = service.clone().ok_or_else(|| {
                anyhow!("login as '{username}': no service (use Actor::on(service).login(..))")
            })?;
            let flow = cx.config().auth.get(&service).cloned().unwrap_or_default();
            let base = cx
                .config()
                .service_url(Some(&service))
                .unwrap_or(&cx.config().base_url)
                .to_string();
            let key = format!(
                "login|{base}|{service}|{username}|{}|{}",
                digest(password),
                digest(&serde_json::to_string(&flow)?)
            );
            cached(&key, refresh, || {
                login(cx, &service, &flow, username, password)
            })
            .await
            .with_context(|| format!("login as '{username}' on '{service}' failed"))
            .map(Some)
        }
        Auth::ClientCredentials(c) => {
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
                .map(Some)
        }
    }
}

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

async fn login(
    cx: &Ctx,
    service: &str,
    flow: &LoginFlow,
    username: &str,
    password: &str,
) -> Result<Cached> {
    let body = substitute(&flow.body, username, password);
    let mut req = match (&flow.operation, &flow.path) {
        (Some(op), _) if cx.contract(service).is_ok() => {
            Request::op(Some(cx.clone()), Some(service.to_string()), op)
        }
        (_, Some(path)) => Request::raw(Some(cx.clone()), "POST", path).service(service),
        (Some(op), None) => bail!(
            "service '{service}' has no contract with operation '{op}'; set `path` in its login flow"
        ),
        (None, None) => bail!("login flow for '{service}' has neither `operation` nor `path`"),
    };
    req = if flow.form {
        let fields: Vec<(String, String)> = body
            .as_object()
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
            .unwrap_or_default();
        let refs: Vec<(&str, &str)> = fields
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        req.form(&refs)
    } else {
        req.json(body)
    };
    let resp = req.await?;
    if !(200..300).contains(&resp.status) {
        bail!(
            "status {}: {}",
            resp.status,
            resp.text.chars().take(300).collect::<String>()
        );
    }
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
        req = req.header(
            "Authorization",
            format!(
                "Basic {}",
                base64(format!("{}:{}", c.client_id, c.client_secret).as_bytes())
            ),
        );
    }
    let resp = req.await?;
    if !(200..300).contains(&resp.status) {
        bail!(
            "status {}: {}",
            resp.status,
            resp.text.chars().take(300).collect::<String>()
        );
    }
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
}
