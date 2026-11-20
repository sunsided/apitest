//! `apitest call` end to end: the real binary against an in-process service.

use std::io::Write as _;
use std::path::{Path as FsPath, PathBuf};
use std::process::Stdio;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::{Path, Query};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
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

/// Bytes that are not valid UTF-8 in any position, for binary downloads.
const BINARY: &[u8] = &[0x89, b'P', b'N', b'G', 0x00, 0xff, 0xfe, 0xc3, 0x28];

/// `GET /binary`: [`BINARY`] as `application/octet-stream`.
async fn binary() -> impl IntoResponse {
    ([("content-type", "application/octet-stream")], BINARY)
}

/// `GET /hop`: a same-origin redirect to [`echo`].
async fn hop() -> Redirect {
    Redirect::to("/echo")
}

/// `GET /loop`: a same-origin redirect to itself, forever.
async fn redirect_loop() -> Redirect {
    Redirect::to("/loop")
}

/// The test service, redirecting `/away` to `away` (another origin).
fn app(away: &str) -> Router {
    let away = away.to_string();
    Router::new()
        .route("/login", post(login))
        .route("/me", get(me))
        .route("/echo", post(echo).get(echo))
        .route("/status/{n}", get(status))
        .route("/slow", get(slow))
        .route("/hop", get(hop))
        .route("/loop", get(redirect_loop))
        .route("/binary", get(binary))
        .route("/away", get(move || async move { Redirect::to(&away) }))
}

/// Starts the test service; returns its base URL.
fn serve() -> String {
    serve_app(app("http://127.0.0.1:9/"))
}

/// Starts `app` on a free port in a background thread; returns its base URL.
fn serve_app(app: Router) -> String {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            tx.send(l.local_addr().unwrap()).unwrap();
            axum::serve(l, app).await.unwrap();
        });
    });

    format!("http://{}", rx.recv().unwrap())
}

/// Numbers the project directories of this test process, so no two tests share one.
static NEXT_PROJECT: AtomicU64 = AtomicU64::new(0);

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
    let n = NEXT_PROJECT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("apitest-call-{}-{n}", std::process::id()));
    // `create_dir` fails on a leftover instead of sharing it with another test.
    std::fs::create_dir(&dir).unwrap();

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

/// `apitest call <args>` in `dir`, without colours or credential env vars.
fn command(dir: &FsPath, args: &[&str]) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_apitest"));
    c.current_dir(dir)
        .arg("call")
        .args(args)
        .env_remove("HUB_USER")
        .env_remove("HUB_PASSWORD")
        .env_remove("CLICOLOR_FORCE")
        .env("NO_COLOR", "1");

    c
}

/// Runs `apitest call <args>` in `dir` with an empty stdin.
fn apitest(dir: &FsPath, args: &[&str]) -> Output {
    command(dir, args).output().unwrap()
}

/// Runs `apitest call <args>` in `dir` with `input` on stdin.
fn apitest_with_stdin(dir: &FsPath, args: &[&str], input: &[u8]) -> Output {
    let mut child = command(dir, args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // A process that exits before reading closes the pipe; that is not a test failure.
    let _ = child.stdin.take().unwrap().write_all(input);

    child.wait_with_output().unwrap()
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
    // Checked before the request, all at once, with the fields that use them.
    let err = stderr(&o);
    assert!(
        err.contains("HUB_USER (used by actors.hub_admin.auth.username)"),
        "{err}"
    );
    assert!(
        err.contains("HUB_PASSWORD (used by actors.hub_admin.auth.password)"),
        "{err}"
    );
}

#[test]
fn credentials_from_env_files_and_redaction_patterns() {
    let dir = project(&serve(), LOGIN_FLOW);
    std::fs::write(dir.join(".env"), "HUB_USER=admin@hub.test\n").unwrap();
    std::fs::write(dir.join(".env.local"), "HUB_PASSWORD=s3cret\n").unwrap();

    // The user comes from .env, the password from .env.local.
    let args = ["hub", "GET", "/me", "--as", "hub_admin"];
    let o = apitest(&dir, &[&args[..], &["--redact", "adm[a-z]+"]].concat());
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    // {"user":"admin"}: the pattern masks the body on stdout too.
    assert_eq!(stdout(&o), r#"{"user":"***"}"#);

    // --no-env-file reads neither file, so the variables are missing.
    let o = apitest(&dir, &[&args[..], &["--no-env-file"]].concat());
    assert_eq!(o.status.code(), Some(2));
    assert!(
        stderr(&o).contains("HUB_PASSWORD (used by"),
        "{}",
        stderr(&o)
    );

    // --env-file replaces the project's files and must exist.
    std::fs::write(
        dir.join("ci.env"),
        "HUB_USER=admin@hub.test\nHUB_PASSWORD=s3cret\n",
    )
    .unwrap();
    let o = apitest(&dir, &[&args[..], &["--env-file", "ci.env"]].concat());
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let o = apitest(&dir, &[&args[..], &["--env-file", "nope.env"]].concat());
    assert_eq!(o.status.code(), Some(2));
    assert!(
        stderr(&o).contains("nope.env: file not found"),
        "{}",
        stderr(&o)
    );
}

#[test]
fn env_file_errors_and_secrets_never_reach_the_terminal() {
    let dir = project(&serve(), LOGIN_FLOW);
    std::fs::write(dir.join(".env"), "HUB_USER='hunter2-unterminated\n").unwrap();
    let o = apitest(&dir, &["hub", "GET", "/me", "--as", "hub_admin"]);
    assert_eq!(o.status.code(), Some(2));
    let err = stderr(&o);
    assert!(err.contains("parsing") && err.contains(".env"), "{err}");
    assert!(!err.contains("hunter2"), "{err}");

    // A wrong password: the final error is masked too, here the login
    // response body "bad credentials" through a pattern.
    std::fs::write(
        dir.join(".env"),
        "HUB_USER=admin@hub.test\nHUB_PASSWORD=wrong-pass\n",
    )
    .unwrap();
    let args = [
        "hub",
        "GET",
        "/me",
        "--as",
        "hub_admin",
        "--redact",
        "cred[a-z]+",
    ];
    let o = apitest(&dir, &args);
    assert_eq!(o.status.code(), Some(2));
    let err = stderr(&o);
    assert!(err.contains("status 401: bad ***"), "{err}");
    assert!(
        !err.contains("credentials") && !err.contains("wrong-pass"),
        "{err}"
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
    // hub_admin's HUB_USER / HUB_PASSWORD are unset: only the `--as` actor must resolve.
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
    // The service echoes the actor's API key and stdout masks it. Only the exact
    // registered value is masked, so "***" also shows that "k-123" was sent.
    assert_eq!(v["api_key"], "***");
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
    // A -H credential is registered as a secret, so the echo shows it masked.
    assert_eq!(v["authorization"], "***");
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

#[test]
fn query_strings_in_the_path_are_kept() {
    let dir = project(&serve(), "");
    let v = echo_json(&dir, &["hub", "GET", "/echo?a=1", "--query", "page=2"]);
    assert_eq!(v["query"]["a"], "1");
    assert_eq!(v["query"]["page"], "2");
}

#[test]
fn verbose_redacts_headers_from_the_actor_config() {
    let dir = project(&serve(), "");
    let o = apitest(&dir, &["hub", "GET", "/echo", "--as", "tenant", "-v"]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert!(stderr(&o).contains("X-Tenant: ***"), "{}", stderr(&o));
}

#[test]
fn same_origin_redirects_keep_the_actors_credentials() {
    let dir = project(&serve(), LOGIN_FLOW);
    let v = echo_json(&dir, &["hub", "GET", "/hop", "--as", "monitor"]);
    // Stdout masks the registered key, so "***" shows it was sent after the hop.
    assert_eq!(v["api_key"], "***");

    let v = echo_json(
        &dir,
        &[
            "hub",
            "GET",
            "/hop",
            "--as",
            "hub_admin",
            "--var",
            "HUB_USER=admin@hub.test",
            "--var",
            "HUB_PASSWORD=s3cret",
        ],
    );
    assert_eq!(v["authorization"], "***");
}

#[test]
fn redirect_loops_stop_after_ten_hops() {
    let dir = project(&serve(), "");
    let o = apitest(&dir, &["hub", "GET", "/loop"]);
    assert_eq!(o.status.code(), Some(2), "{}", stderr(&o));
    assert!(stderr(&o).contains("too many redirects"), "{}", stderr(&o));
}

#[test]
fn cross_origin_redirects_are_not_followed() {
    let seen: Arc<Mutex<Vec<HeaderMap>>> = Arc::default();
    let record = Arc::clone(&seen);
    let other = serve_app(Router::new().fallback(move |h: HeaderMap| {
        record.lock().unwrap().push(h);
        async { "elsewhere" }
    }));
    let dir = project(&serve_app(app(&format!("{other}/steal"))), "");

    let o = apitest(&dir, &["hub", "GET", "/away", "--as", "monitor"]);
    assert_eq!(o.status.code(), Some(1), "{}", stderr(&o));
    let err = stderr(&o);
    assert!(err.contains("status 303 (expected 2xx)"), "{err}");
    assert!(
        err.contains(&format!("redirect to {other}/steal not followed")),
        "{err}"
    );
    // The other host is never contacted, so it never sees the API key.
    assert!(seen.lock().unwrap().is_empty());
}

#[test]
fn binary_bodies_are_written_byte_for_byte() {
    let dir = project(&serve(), "");
    let o = apitest(&dir, &["hub", "GET", "/binary"]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(o.stdout, BINARY);

    // A redaction pattern still applies; the body then loses its exact bytes.
    let o = apitest(&dir, &["hub", "GET", "/binary", "--redact", "PNG"]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert!(
        stdout(&o).contains("***") && !stdout(&o).contains("PNG"),
        "{}",
        stdout(&o)
    );
}

#[test]
fn an_expected_redirect_is_not_reported() {
    let dir = project(&serve(), "");
    let o = apitest(&dir, &["hub", "GET", "/away", "--expect", "303"]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(stderr(&o), "");
}

#[test]
fn verbose_shows_the_query_and_redacts_credentials_from_flags() {
    let url = serve();
    let dir = project(&url, "");
    let o = apitest(
        &dir,
        &[
            "hub",
            "GET",
            "/echo?a=1",
            "--query",
            "page=2",
            "--query",
            "access_token=q-secret",
            "-H",
            "X-Auth-Token: h-secret",
            "-H",
            "Authorization: Token two words",
            "-v",
        ],
    );
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));

    let err = stderr(&o);
    assert!(
        err.contains(&format!("> GET {url}/echo?a=1&page=2&access_token=***")),
        "{err}"
    );
    assert!(err.contains("X-Auth-Token: ***"), "{err}");
    assert!(err.contains("Authorization: Token ***"), "{err}");
    for secret in ["q-secret", "h-secret", "two words"] {
        assert!(!err.contains(secret), "{secret} leaked: {err}");
    }

    // A secret query value is masked beyond -v too, here in the echo on stdout,
    // while a name that only contains `key` is ordinary data.
    let out = stdout(&o);
    assert!(out.contains(r#""access_token":"***""#), "{out}");
    let v = echo_json(
        &dir,
        &["hub", "GET", "/echo", "--query", "keyword=red shoes", "-v"],
    );
    assert_eq!(v["query"]["keyword"], "red shoes");
}

#[test]
fn bodies_come_from_stdin_files_or_at_at() {
    let dir = project(&serve(), "");
    let args = ["hub", "POST", "/echo", "--json", "@-"];
    let o = apitest_with_stdin(&dir, &args, br#"{"from":"stdin"}"#);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(
        serde_json::from_slice::<Value>(&o.stdout).unwrap()["body"],
        r#"{"from":"stdin"}"#
    );

    std::fs::write(dir.join("user.json"), r#"{"from":"file"}"#).unwrap();
    let v = echo_json(&dir, &["hub", "POST", "/echo", "--json", "@user.json"]);
    assert_eq!(v["body"], r#"{"from":"file"}"#);

    let v = echo_json(&dir, &["hub", "POST", "/echo", "--data", "@@handle"]);
    assert_eq!(v["body"], "@handle");

    let o = apitest(&dir, &["hub", "POST", "/echo", "--data", "@"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(
        stderr(&o).contains("'@' needs a file name"),
        "{}",
        stderr(&o)
    );
}

#[test]
fn bad_input_is_refused_before_the_body_is_read() {
    let dir = project(&serve(), "");
    for (args, why) in [
        (
            &["hub", "POST", "/echo", "-H", "X Foo: v", "--json", "@-"][..],
            "invalid header name 'X Foo'",
        ),
        (
            &["hub", "POST", "/echo", "-H", "X-A: a\nb", "--json", "@-"],
            "invalid value for header 'X-A'",
        ),
        (
            &["hub", "GE T", "/echo", "--json", "@-"],
            "invalid HTTP method",
        ),
        (
            &["hub", "POST", "/users/{id}", "--json", "@-"],
            "has no path parameters",
        ),
    ] {
        // Stdin is not JSON: reading it would fail differently.
        let o = apitest_with_stdin(&dir, args, b"not json");
        assert_eq!(o.status.code(), Some(2), "{args:?}");
        assert!(stderr(&o).contains(why), "{args:?}: {}", stderr(&o));
    }
}

#[test]
fn a_service_url_without_a_scheme_is_refused_by_name() {
    let dir = project(&serve(), "");
    std::fs::write(dir.join("other.nova"), "services:\n  hub: localhost:8080\n").unwrap();
    let o = apitest(&dir, &["hub", "GET", "/x", "--config", "other.nova"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(
        stderr(&o).contains("service 'hub' in other.nova has the URL 'localhost:8080'"),
        "{}",
        stderr(&o)
    );
}

#[test]
fn session_header_values_are_masked_after_their_scheme_word() {
    let dir = project(&serve(), "");
    let o = apitest(
        &dir,
        &[
            "hub",
            "GET",
            "/echo",
            "-H",
            "X-Session: session abc123",
            "-v",
        ],
    );
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert!(!stderr(&o).contains("abc123"), "{}", stderr(&o));
}

#[test]
fn secret_query_values_never_reach_stderr_even_in_errors() {
    // Nothing listens on port 9, so the connection is refused.
    let dir = project("http://127.0.0.1:9", "");
    let o = apitest(
        &dir,
        &[
            "hub",
            "GET",
            "/x",
            "--query",
            "access_token=xyz",
            "--query",
            "q=hello",
            "-v",
        ],
    );
    assert_eq!(o.status.code(), Some(2), "{}", stderr(&o));
    let err = stderr(&o);
    assert!(err.contains("GET http://127.0.0.1:9/x failed"), "{err}");
    assert!(!err.contains("xyz"), "{err}");

    let dir = project(&serve(), "");
    let o = apitest(
        &dir,
        &[
            "hub",
            "GET",
            "/echo",
            "--query",
            "access_token=xyz",
            "--query",
            "q=hello",
            "-v",
        ],
    );
    let err = stderr(&o);
    assert!(err.contains("q=hello"), "{err}");
    assert!(!err.contains("xyz"), "{err}");
}

/// Credentials written inline in the path, and query names such as
/// `oauth_signature`, are masked in an echoed body like `--query` ones, while an
/// idempotency key stays ordinary data (headers use the same name test).
#[test]
fn inline_and_signature_query_values_are_masked_on_stdout() {
    let dir = project(&serve(), "");
    let o = apitest(
        &dir,
        &[
            "hub",
            "GET",
            "/echo?access_token=inline-secret&oauth_signature=sig-secret",
            "--query",
            "idempotency_key=idem-1234",
        ],
    );
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));

    let out = stdout(&o);
    for secret in ["inline-secret", "sig-secret"] {
        assert!(!out.contains(secret), "{secret} leaked: {out}");
    }
    assert!(out.contains("idem-1234"), "{out}");
}

/// A password in the service URL never shows in a request error.
#[test]
fn service_userinfo_never_reaches_error_messages() {
    let dir = project("http://user:url-secret@127.0.0.1:9", "");
    let o = apitest(&dir, &["hub", "GET", "/x"]);
    assert_eq!(o.status.code(), Some(2), "{}", stderr(&o));

    let err = stderr(&o);
    assert!(err.contains("GET http://127.0.0.1:9/x failed"), "{err}");
    assert!(!err.contains("url-secret"), "{err}");
}
