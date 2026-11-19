//! `apitest call` end to end: the real binary against an in-process service.

use std::path::{Path as FsPath, PathBuf};
use std::process::{Command, Output};

use axum::extract::{Path, Query};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};

/// The login flow `hub_admin` needs; the default config has none.
const LOGIN_FLOW: &str = "auth:
  hub:
    path: /login
    body:
      email: \"{username}\"
      password: \"{password}\"
    token: /data/jwt
";

/// `POST /login`: a token for the one known user, `401` otherwise.
async fn login(Json(b): Json<Value>) -> Response {
    if b["email"] == "admin@hub.test" && b["password"] == "s3cret" {
        Json(json!({ "data": { "jwt": "tok-admin" } })).into_response()
    } else {
        (StatusCode::UNAUTHORIZED, "bad credentials").into_response()
    }
}

/// `GET /me`: the user for the token from [`login`], `401` without it.
async fn me(h: HeaderMap) -> Response {
    match h.get("authorization").and_then(|v| v.to_str().ok()) {
        Some("Bearer tok-admin") => Json(json!({ "user": "admin" })).into_response(),
        Some(_) => StatusCode::UNAUTHORIZED.into_response(),
        None => (StatusCode::UNAUTHORIZED, "no credentials").into_response(),
    }
}

/// `/echo`: the query, body and the headers the tests look at, as JSON.
async fn echo(
    h: HeaderMap,
    Query(q): Query<serde_json::Map<String, Value>>,
    body: String,
) -> Json<Value> {
    let header = |n: &str| h.get(n).and_then(|v| v.to_str().ok()).map(str::to_string);
    Json(json!({
        "query": q,
        "body": body,
        "content_type": header("content-type"),
        "tenant": header("x-tenant"),
        "api_key": header("x-api-key"),
        "authorization": header("authorization"),
    }))
}

/// `GET /slow`: answers after five seconds, for `--timeout`.
async fn slow() -> &'static str {
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    "late"
}

/// `GET /status/{n}`: an empty response with status `n`.
async fn status(Path(n): Path<u16>) -> StatusCode {
    StatusCode::from_u16(n).unwrap()
}

/// Starts the service on a free port in a background thread; returns its base URL.
fn serve() -> String {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let app = Router::new()
                .route("/login", post(login))
                .route("/me", get(me))
                .route("/echo", post(echo).get(echo))
                .route("/status/{n}", get(status))
                .route("/slow", get(slow));
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            tx.send(l.local_addr().unwrap()).unwrap();
            axum::serve(l, app).await.unwrap();
        });
    });

    format!("http://{}", rx.recv().unwrap())
}

/// A temporary project directory, removed when the test ends.
struct Project(PathBuf);

impl std::ops::Deref for Project {
    type Target = FsPath;

    fn deref(&self) -> &FsPath {
        &self.0
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A project whose `apitest.nova` lists the service at `url` as `hub`, with a
/// `hub_admin` actor that logs in and a `monitor` actor with an API key, plus
/// the `extra` config lines.
fn project(url: &str, extra: &str) -> Project {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("apitest-call-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    let cfg = format!(
        "services:
  hub: {url}
actors:
  hub_admin:
    service: hub
    auth:
      type: login
      username: \"${{HUB_USER}}\"
      password: \"${{HUB_PASSWORD}}\"
  monitor:
    service: hub
    auth:
      type: header
      name: X-Api-Key
      value: k-123
  tenant:
    service: hub
    headers:
      X-Tenant: \"7\"
    user_agent: synthetic/1.0
{extra}"
    );
    std::fs::write(dir.join("apitest.nova"), cfg).unwrap();

    Project(dir)
}

/// Runs `apitest call <args>` in `dir`, without colours or credential env vars.
fn apitest(dir: &FsPath, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_apitest"))
        .current_dir(dir)
        .arg("call")
        .args(args)
        .env_remove("HUB_USER")
        .env_remove("HUB_PASSWORD")
        .env_remove("CLICOLOR_FORCE")
        .env("NO_COLOR", "1")
        .output()
        .unwrap()
}

/// The process's stdout as text.
fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

/// The process's stderr as text.
fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// Runs a call that must succeed and parses its stdout as JSON.
fn echo_json(dir: &FsPath, args: &[&str]) -> Value {
    let o = apitest(dir, args);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));

    serde_json::from_str(&stdout(&o)).unwrap()
}

#[test]
fn logs_in_as_a_named_actor_through_the_configured_flow() {
    let url = serve();
    let dir = project(&url, LOGIN_FLOW);
    let o = apitest(
        &dir,
        &[
            "hub",
            "get",
            "/me",
            "--as",
            "hub_admin",
            "--var",
            "HUB_USER=admin@hub.test",
            "--var",
            "HUB_PASSWORD=s3cret",
            "-v",
        ],
    );
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(stdout(&o), r#"{"user":"admin"}"#);

    // Verbose output names the request but never the token.
    let err = stderr(&o);
    assert!(err.contains(&format!("> GET {url}/me")), "{err}");
    assert!(err.contains("Authorization: Bearer ***"), "{err}");
    assert!(!err.contains("tok-admin"), "{err}");
}

#[test]
fn a_login_actor_without_a_login_flow_is_an_error_not_a_guess() {
    let dir = project(&serve(), "");
    let o = apitest(&dir, &["hub", "GET", "/me", "--as", "hub_admin"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(
        stderr(&o).contains("no login flow for 'hub'"),
        "{}",
        stderr(&o)
    );
    assert_eq!(stdout(&o), "");
}

#[test]
fn missing_credentials_name_the_variable() {
    let dir = project(&serve(), LOGIN_FLOW);
    let o = apitest(&dir, &["hub", "GET", "/me", "--as", "hub_admin"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(
        stderr(&o).contains("variable 'HUB_USER' is not set"),
        "{}",
        stderr(&o)
    );
}

#[test]
fn without_as_no_credentials_are_sent() {
    let dir = project(&serve(), LOGIN_FLOW);
    let o = apitest(&dir, &["hub", "GET", "/me"]);
    assert_eq!(o.status.code(), Some(1));
    assert_eq!(stdout(&o), "no credentials");
    assert!(stderr(&o).contains("status 401 (expected 2xx)"));
}

#[test]
fn sends_headers_query_and_json() {
    let dir = project(&serve(), "");
    let v = echo_json(
        &dir,
        &[
            "hub",
            "POST",
            "/echo",
            "--as",
            "monitor",
            "-H",
            "X-Tenant: 42",
            "--query",
            "page=2",
            "--json",
            r#"{"a":1}"#,
        ],
    );
    assert_eq!(v["query"]["page"], "2");
    assert_eq!(v["body"], r#"{"a":1}"#);
    assert_eq!(v["content_type"], "application/json");
    assert_eq!(v["tenant"], "42");
    assert_eq!(v["api_key"], "k-123");
    assert_eq!(v["authorization"], Value::Null);
}

#[test]
fn sends_text_and_form_bodies() {
    let dir = project(&serve(), "");
    std::fs::write(dir.join("body.txt"), "hello").unwrap();
    let v = echo_json(
        &dir,
        &[
            "hub",
            "POST",
            "/echo",
            "--data",
            "@body.txt",
            "-H",
            "Content-Type: text/plain",
        ],
    );
    assert_eq!(v["body"], "hello");
    assert_eq!(v["content_type"], "text/plain");

    let v = echo_json(
        &dir,
        &["hub", "POST", "/echo", "--form", "x=1", "--form", "y=2"],
    );
    assert_eq!(v["body"], "x=1&y=2");

    let o = apitest(
        &dir,
        &["hub", "POST", "/echo", "--json", "{}", "--data", "x"],
    );
    assert_eq!(o.status.code(), Some(2), "--json and --data are exclusive");
}

#[test]
fn headers_an_actor_sets_cannot_be_replaced() {
    let dir = project(&serve(), "");
    for (actor, header) in [
        ("monitor", "X-Api-Key: other"),
        ("monitor", "Authorization: Bearer x"),
        ("tenant", "x-tenant: 42"),
        ("tenant", "User-Agent: mine"),
    ] {
        let o = apitest(&dir, &["hub", "GET", "/echo", "--as", actor, "-H", header]);
        assert_eq!(o.status.code(), Some(2), "{actor} {header}");
        assert!(
            stderr(&o).contains(&format!("conflicts with a header actor '{actor}' sets")),
            "{}",
            stderr(&o)
        );
    }

    // An actor without credentials leaves `Authorization` to the caller.
    let v = echo_json(
        &dir,
        &[
            "hub",
            "GET",
            "/echo",
            "--as",
            "tenant",
            "-H",
            "Authorization: Bearer mine",
        ],
    );
    assert_eq!(v["authorization"], "Bearer mine");
    assert_eq!(v["tenant"], "7");
}

#[test]
fn exit_code_follows_the_status() {
    let dir = project(&serve(), "");
    let code = |args: &[&str]| apitest(&dir, args).status.code();
    assert_eq!(code(&["hub", "GET", "/status/204"]), Some(0));
    assert_eq!(code(&["hub", "GET", "/status/404"]), Some(1));
    assert_eq!(
        code(&["hub", "GET", "/status/404", "--expect", "404"]),
        Some(0)
    );
    assert_eq!(
        code(&["hub", "GET", "/status/200", "--expect", "201"]),
        Some(1)
    );
}

#[test]
fn include_prints_status_and_headers_first() {
    let dir = project(&serve(), "");
    let o = apitest(&dir, &["hub", "GET", "/status/204", "-i"]);
    assert!(stdout(&o).starts_with("HTTP 204\r\n"), "{}", stdout(&o));
    assert!(stdout(&o).ends_with("\r\n\r\n"), "{}", stdout(&o));
}

#[test]
fn only_configured_services_and_relative_paths() {
    let dir = project(&serve(), "");
    let o = apitest(&dir, &["app", "GET", "/x"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(
        stderr(&o).contains("unknown service 'app'"),
        "{}",
        stderr(&o)
    );

    let o = apitest(
        &dir,
        &["hub", "GET", "https://example.test/x", "--as", "monitor"],
    );
    assert_eq!(o.status.code(), Some(2));
    assert!(stderr(&o).contains("must start with '/'"), "{}", stderr(&o));
}

#[test]
fn verbose_redacts_api_keys_and_actor_headers() {
    let dir = project(&serve(), "");
    let o = apitest(&dir, &["hub", "GET", "/echo", "--as", "monitor", "-v"]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));

    let err = stderr(&o);
    assert!(err.contains("X-Api-Key: ***"), "{err}");
    assert!(!err.contains("k-123"), "{err}");
}

#[test]
fn times_out_instead_of_hanging() {
    let dir = project(&serve(), "");
    let o = apitest(&dir, &["hub", "GET", "/slow", "--timeout", "200ms"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(stderr(&o).contains("no response within"), "{}", stderr(&o));
}
