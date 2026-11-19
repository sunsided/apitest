//! Helpers that keep everyday tests short. Everything here also works without
//! passing `cx` around: it uses the context of the running fixture, hook, test or
//! cleanup ([`crate::context::current`]).
//!
//! ```ignore
//! #[apitest::test]
//! async fn onboarding(social_worker: Actor) -> Result<()> {
//!     let app = Actor::on("app");
//!     let email = unique_email("patient");
//!
//!     step("patient registers", app.call("registerPatient")
//!         .json(json!({ "email": email, "password": PW }))
//!         .expect(201)).await?;
//!
//!     let mail: Mail = eventually!(Actor::on("mail").get("/messages").query("to", &email).expect(200).await?.get("/0"))?;
//!     visit(&link_containing(&mail.body, "/confirm")?).await?;
//!     ...
//! }
//! ```

use std::collections::BTreeMap;
use std::future::{Future, IntoFuture};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::context::{Ctx, current};
use crate::http::{Request, Response};
use crate::report::StepReport;

// ---------------------------------------------------------------------------
// Actors
// ---------------------------------------------------------------------------

/// Someone (or something) calling an API: a service plus credentials.
///
/// Actors are plain data (serializable), so fixtures can return them and tests
/// receive them as parameters. Requests made through an actor go to its service's
/// base URL ([`crate::RunConfig::service`]) and, for `call`, to the contract of the
/// same name (override with [`Actor::contract`]).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Actor {
    /// Display name for logs ("hub admin", an e-mail address, ...).
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub service: Option<String>,
    #[serde(default)]
    pub contract: Option<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub auth: crate::auth::Auth,
}

impl Actor {
    /// An anonymous actor on a service.
    pub fn on(service: &str) -> Actor {
        Actor {
            service: Some(service.to_string()),
            ..Default::default()
        }
    }

    /// An actor with no service: only absolute URLs (see also [`visit`]).
    pub fn anywhere() -> Actor {
        Actor::default()
    }

    pub fn named(mut self, name: impl Into<String>) -> Actor {
        self.name = Some(name.into());
        self
    }

    /// Uses a different contract than the service name for `call`.
    pub fn contract(mut self, name: &str) -> Actor {
        self.contract = Some(name.to_string());
        self
    }

    /// `Authorization: Bearer <token>`.
    pub fn bearer(self, token: impl Into<String>) -> Actor {
        self.auth(crate::auth::Auth::bearer(token))
    }

    /// Alias of [`Actor::bearer`].
    pub fn with_bearer(self, token: impl AsRef<str>) -> Actor {
        self.bearer(token.as_ref())
    }

    /// HTTP Basic auth.
    pub fn basic(self, username: impl Into<String>, password: impl Into<String>) -> Actor {
        self.auth(crate::auth::Auth::basic(username, password))
    }

    /// An API key (or any credential) in a header.
    pub fn api_key(self, header: impl Into<String>, key: impl Into<String>) -> Actor {
        self.auth(crate::auth::Auth::header(header, key))
    }

    /// Logs in lazily through this service's [`crate::auth::LoginFlow`]: no request is
    /// made until the actor is used, the token is cached for the run, and a `401`
    /// triggers one re-login. The actor's display name becomes the username.
    pub fn login(self, username: impl Into<String>, password: impl Into<String>) -> Actor {
        let username = username.into();
        let name = self.name.clone().unwrap_or_else(|| username.clone());
        let service = self.service.clone();
        Actor {
            name: Some(name),
            ..self
        }
        .auth(crate::auth::Auth::Login {
            service,
            username,
            password: password.into(),
        })
    }

    /// OAuth 2.0 client credentials, cached until shortly before they expire.
    pub fn client_credentials(self, creds: crate::auth::ClientCredentials) -> Actor {
        self.auth(crate::auth::Auth::ClientCredentials(creds))
    }

    pub fn auth(mut self, auth: crate::auth::Auth) -> Actor {
        self.auth = auth;
        self
    }

    /// A named actor from the configuration (`actors:` in `apitest.nova`, `RunConfig::actor`).
    pub fn from_config(name: &str) -> Result<Actor> {
        Actor::from_config_in(&current()?, name)
    }

    pub(crate) fn from_config_in(cx: &Ctx, name: &str) -> Result<Actor> {
        let c = cx.config().actors.get(name).ok_or_else(|| {
            let known: Vec<&str> = cx.config().actors.keys().map(String::as_str).collect();
            anyhow!(
                "no actor named '{name}' is configured (known: {})",
                if known.is_empty() {
                    "none".into()
                } else {
                    known.join(", ")
                }
            )
        })?;
        let mut auth = c.auth.clone();
        if let crate::auth::Auth::Login {
            service: s @ None, ..
        } = &mut auth
        {
            *s = c.service.clone();
        }
        let mut a = Actor {
            name: Some(name.to_string()),
            service: c.service.clone(),
            auth,
            ..Default::default()
        };
        for (k, v) in &c.headers {
            a = a.with_header(k.clone(), crate::auth::interpolate(v, cx)?);
        }
        if let Some(ua) = &c.user_agent {
            a = a.with_header("User-Agent", ua.clone());
        }
        Ok(a)
    }

    /// `User-Agent` for every request of this actor.
    pub fn user_agent(self, ua: impl Into<String>) -> Actor {
        self.with_header("User-Agent", ua)
    }

    /// Resolves dynamic credentials now (logs in / fetches a token) and returns an
    /// actor with the resulting static header, e.g. to report the login as a step.
    pub async fn signed_in(&self) -> Result<Actor> {
        let cx = current()?;
        let mut a = self.clone();
        if let Some((name, value)) = crate::auth::resolve(&cx, &self.auth, false).await? {
            a.auth = crate::auth::Auth::header(name, value);
        }
        Ok(a)
    }

    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Actor {
        self.headers.insert(name.into(), value.into());
        self
    }

    /// The bearer token, if this actor has a static one.
    pub fn token(&self) -> Option<&str> {
        match &self.auth {
            crate::auth::Auth::Bearer { token } => Some(token),
            crate::auth::Auth::Header { name, value }
                if name.eq_ignore_ascii_case("authorization") =>
            {
                value.strip_prefix("Bearer ")
            }
            _ => self
                .headers
                .get("Authorization")
                .and_then(|h| h.strip_prefix("Bearer ")),
        }
    }

    fn decorate(&self, mut req: Request<'static>) -> Request<'static> {
        for (k, v) in &self.headers {
            req = req.header(k, v.clone());
        }
        if self.auth != crate::auth::Auth::None {
            req = req.auth(self.auth.clone());
        }
        req
    }

    /// A contract call by operationId.
    pub fn call(&self, operation: &str) -> Request<'static> {
        let contract = self.contract.clone().or_else(|| self.service.clone());
        self.decorate(Request::op(None, contract, operation))
    }

    /// A raw request to a path on this actor's service (or an absolute URL).
    pub fn request(&self, method: &str, path: &str) -> Request<'static> {
        let mut req = Request::raw(None, method, path);
        if let Some(s) = &self.service {
            req = req.service(s);
        }
        self.decorate(req)
    }

    pub fn get(&self, path: &str) -> Request<'static> {
        self.request("GET", path)
    }
    pub fn post(&self, path: &str) -> Request<'static> {
        self.request("POST", path)
    }
    pub fn put(&self, path: &str) -> Request<'static> {
        self.request("PUT", path)
    }
    pub fn patch(&self, path: &str) -> Request<'static> {
        self.request("PATCH", path)
    }
    pub fn delete(&self, path: &str) -> Request<'static> {
        self.request("DELETE", path)
    }
}

// ---------------------------------------------------------------------------
// Context-free helpers
// ---------------------------------------------------------------------------

/// A named actor from the configuration; see [`Actor::from_config`].
pub fn actor(name: &str) -> Result<Actor> {
    Actor::from_config(name)
}

/// The trace of the current test (see [`crate::trace`]).
pub fn current_trace() -> Result<crate::trace::TraceContext> {
    Ok(current()?.trace())
}

/// The current context (for the rare case you need it explicitly).
pub fn cx() -> Result<Ctx> {
    current()
}

/// A `--var` / `RunConfig::vars` value, else the environment variable.
pub fn env(key: &str) -> Result<String> {
    current()?.env(key)
}

/// Registers a prepared request as a cleanup of the current scope. It runs when
/// the scope ends (LIFO), whether the test passed or not.
///
/// ```ignore
/// undo(patient.call("deletePatient").expect(204))?;
/// ```
pub fn undo(req: Request<'static>) -> Result<()> {
    current()?.undo(req);
    Ok(())
}

/// Registers an arbitrary async cleanup of the current scope.
pub fn defer<F, Fut>(label: impl Into<String>, f: F) -> Result<()>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = Result<()>> + Send + 'static,
{
    current()?.defer(label, move |_cx| f());
    Ok(())
}

/// Runs a named step: it is logged, timed, listed in the report (with ✓/✗ for
/// failed tests and in JUnit `system-out`), and its name is added to the error.
/// Accepts any future or request: `step("…", async { … })`, `step("…", req.expect(201))`.
pub fn step<T, F>(name: impl Into<String>, work: F) -> impl Future<Output = Result<T>>
where
    F: IntoFuture<Output = Result<T>>,
{
    // Convert eagerly: the returned future then only holds concrete types, which
    // keeps it `Send` (rustc cannot always prove that through generic `IntoFuture`s).
    let name = name.into();
    let work = work.into_future();
    async move {
        let cx = current().ok();
        if let Some(cx) = &cx {
            cx.log(format!("▸ {name}"));
        }
        let started = Instant::now();
        let result = work.await;
        if let Some(cx) = &cx {
            cx.record_step(StepReport {
                name: name.clone(),
                duration_ms: started.elapsed().as_secs_f64() * 1000.0,
                error: result.as_ref().err().map(|e| format!("{e:#}")),
            });
        }
        result.with_context(|| format!("step '{name}' failed"))
    }
}

/// Backoff state for [`eventually!`](crate::eventually): 50 ms growing to 1 s.
#[doc(hidden)]
pub struct Retry {
    started: Instant,
    timeout: Duration,
    delay: Duration,
    attempts: u32,
}

impl Retry {
    pub fn new(timeout: Duration) -> Self {
        Retry {
            started: Instant::now(),
            timeout,
            delay: Duration::from_millis(50),
            attempts: 0,
        }
    }

    /// Waits before the next attempt, or returns the final error once the time is up.
    pub async fn wait(&mut self, err: anyhow::Error) -> Result<()> {
        self.attempts += 1;
        if self.started.elapsed() + self.delay > self.timeout {
            return Err(err.context(format!(
                "still failing after {} attempts in {:.1}s",
                self.attempts,
                self.started.elapsed().as_secs_f64()
            )));
        }
        tokio::time::sleep(self.delay).await;
        self.delay = self.delay.mul_f32(1.5).min(Duration::from_secs(1));
        Ok(())
    }
}

/// Retries an expression until it succeeds (default: up to 10 s, backoff 50 ms → 1 s).
/// The expression is an async block body: it may use `.await` and `?`, and its
/// last expression is the `Result` to check. Evaluates to that `Result`.
///
/// ```ignore
/// let mail: Mail = eventually!(mailbox.get("/messages").query("to", &email).expect(200).await?.get("/0"))?;
/// let job: Job = eventually!(within: Duration::from_secs(60), {
///     let job: Job = api.call("getJob").path("id", &id).expect(200).await?.get("")?;
///     anyhow::ensure!(job.done, "job still running");
///     Ok(job)
/// })?;
/// ```
///
/// A macro rather than a function taking a closure: the retried block is inlined
/// into the test's own future, so borrowing locals just works and the test stays `Send`.
#[macro_export]
macro_rules! eventually {
    (within: $timeout:expr, $($body:tt)+) => {{
        let mut __retry = $crate::dsl::Retry::new($timeout);
        loop {
            let __attempt: $crate::__private_anyhow::Result<_> = async { $($body)+ }.await;
            match __attempt {
                ::core::result::Result::Ok(v) => break ::core::result::Result::Ok(v),
                ::core::result::Result::Err(e) => {
                    if let ::core::result::Result::Err(e) = __retry.wait(e).await {
                        break ::core::result::Result::Err(e);
                    }
                }
            }
        }
    }};
    ($($body:tt)+) => {
        $crate::eventually!(within: ::std::time::Duration::from_secs(10), $($body)+)
    };
}

/// `GET` an absolute URL, e.g. a link from an e-mail, following redirects.
/// Fails on 4xx/5xx.
pub async fn visit(url: &str) -> Result<Response> {
    let resp = Actor::anywhere().get(url).await?;
    if resp.status >= 400 {
        bail!(
            "visiting {url} returned {}: {}",
            resp.status,
            resp.text.chars().take(300).collect::<String>()
        );
    }
    Ok(resp)
}

// ---------------------------------------------------------------------------
// Test data
// ---------------------------------------------------------------------------

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn run_tag() -> &'static str {
    static TAG: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    TAG.get_or_init(|| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        format!(
            "{:x}",
            (nanos ^ ((std::process::id() as u128) << 64)) & 0xff_ffff
        )
    })
}

/// A value unique across this run and concurrent runs: `prefix-<run>-<n>`.
pub fn unique(prefix: &str) -> String {
    format!(
        "{prefix}-{}-{}",
        run_tag(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// A unique e-mail address on the reserved `.test` TLD.
pub fn unique_email(prefix: &str) -> String {
    format!("{}@example.test", unique(prefix))
}

/// All `http(s)://` links in a text (plain text, HTML or JSON-escaped).
pub fn links(text: &str) -> Vec<String> {
    let mut out = vec![];
    let mut rest = text;
    loop {
        let start = match (rest.find("http://"), rest.find("https://")) {
            (Some(a), Some(b)) => a.min(b),
            (Some(a), None) | (None, Some(a)) => a,
            (None, None) => break,
        };
        let tail = &rest[start..];
        let end = tail
            .find(|c: char| {
                c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | ')' | ']' | '\\')
            })
            .unwrap_or(tail.len());
        out.push(
            tail[..end]
                .trim_end_matches(['.', ',', ';'])
                .replace("&amp;", "&"),
        );
        rest = &tail[end..];
    }
    out
}

/// The first link in `text` containing `needle`.
pub fn link_containing(text: &str, needle: &str) -> Result<String> {
    links(text)
        .into_iter()
        .find(|l| l.contains(needle))
        .ok_or_else(|| {
            anyhow!(
                "no link containing '{needle}' in: {}",
                text.chars().take(300).collect::<String>()
            )
        })
}

// ---------------------------------------------------------------------------
// JSON matching
// ---------------------------------------------------------------------------

/// Partial match: every field in `expected` must be present in `actual` with an
/// equal (recursively partially matching) value. Arrays must have the same length
/// and match element-wise. Extra fields in `actual` are ignored.
///
/// The error names the first mismatching path: `at /therapy/plan: expected "CBT", got "DBT"`.
pub fn json_includes(actual: &Value, expected: &Value) -> Result<()> {
    fn walk(a: &Value, e: &Value, path: &mut String) -> Result<()> {
        match (a, e) {
            (Value::Object(ao), Value::Object(eo)) => {
                for (k, ev) in eo {
                    let len = path.len();
                    path.push('/');
                    path.push_str(k);
                    match ao.get(k) {
                        Some(av) => walk(av, ev, path)?,
                        None => bail!("at {path}: missing (expected {ev})"),
                    }
                    path.truncate(len);
                }
                Ok(())
            }
            (Value::Array(aa), Value::Array(ea)) => {
                if aa.len() != ea.len() {
                    bail!(
                        "at {}: expected {} elements, got {}",
                        show(path),
                        ea.len(),
                        aa.len()
                    );
                }
                for (i, (av, ev)) in aa.iter().zip(ea).enumerate() {
                    let len = path.len();
                    path.push_str(&format!("/{i}"));
                    walk(av, ev, path)?;
                    path.truncate(len);
                }
                Ok(())
            }
            (Value::Number(an), Value::Number(en)) if an.as_f64() == en.as_f64() => Ok(()),
            _ if a == e => Ok(()),
            _ => bail!("at {}: expected {e}, got {a}", show(path)),
        }
    }
    fn show(p: &str) -> &str {
        if p.is_empty() { "<root>" } else { p }
    }
    walk(actual, expected, &mut String::new())
}

/// `assert_json!(value, { "status": "in_progress", "therapy": { "plan": "CBT" } })`:
/// returns early with an error unless `value` partially matches (see [`json_includes`]).
/// `value` can be a `serde_json::Value` or a [`Response`] (its body is used).
#[macro_export]
macro_rules! assert_json {
    ($actual:expr, $($expected:tt)+) => {
        $crate::dsl::json_includes(
            $crate::dsl::AsJson::as_json(&$actual),
            &$crate::__serde_json::json!($($expected)+),
        )?
    };
}

/// Things [`assert_json!`] accepts.
pub trait AsJson {
    fn as_json(&self) -> &Value;
}
impl AsJson for Value {
    fn as_json(&self) -> &Value {
        self
    }
}
impl AsJson for Response {
    fn as_json(&self) -> &Value {
        &self.body
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn partial_json_matching() {
        let actual = json!({ "id": 1, "status": "in_progress", "therapy": { "plan": "CBT", "sessions": 2 }, "tags": ["a", "b"] });
        json_includes(
            &actual,
            &json!({ "status": "in_progress", "therapy": { "plan": "CBT" } }),
        )
        .unwrap();
        json_includes(&actual, &json!({ "tags": ["a", "b"] })).unwrap();
        let e = json_includes(&actual, &json!({ "therapy": { "plan": "DBT" } }))
            .unwrap_err()
            .to_string();
        assert_eq!(e, r#"at /therapy/plan: expected "DBT", got "CBT""#);
        let e = json_includes(&actual, &json!({ "owner": "x" }))
            .unwrap_err()
            .to_string();
        assert!(e.contains("at /owner: missing"), "{e}");
        let e = json_includes(&actual, &json!({ "tags": ["a"] }))
            .unwrap_err()
            .to_string();
        assert!(e.contains("expected 1 elements, got 2"), "{e}");
    }

    #[test]
    fn link_extraction() {
        let mail = r#"<p>Hi! <a href="http://127.0.0.1:4000/confirm?token=abc&amp;x=1">Confirm</a>.</p> Or visit https://example.test/help."#;
        assert_eq!(
            links(mail),
            vec![
                "http://127.0.0.1:4000/confirm?token=abc&x=1",
                "https://example.test/help"
            ]
        );
        assert_eq!(
            link_containing(mail, "/confirm").unwrap(),
            "http://127.0.0.1:4000/confirm?token=abc&x=1"
        );
        assert!(link_containing(mail, "/nope").is_err());
    }

    #[test]
    fn unique_values_differ() {
        assert_ne!(unique("a"), unique("a"));
        assert!(unique_email("p").ends_with("@example.test"));
    }
}
