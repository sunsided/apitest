//! The service under test: echoes headers, logs users in and issues client-credentials tokens.

use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use apitest::prelude::*;
use axum::extract::{Form, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response as AxumResponse};
use axum::routing::{get, post};
use axum::{Json, Router};

/// The service's state.
#[derive(Default)]
pub(super) struct Svc {
    /// Tokens `/me` accepts, from both password logins and client credentials.
    tokens: Mutex<HashSet<String>>,
    /// Number of successful password logins so far.
    logins: AtomicU64,
    /// Number of client-credentials tokens issued so far.
    cc_tokens: AtomicU64,
}

/// Starts the service on an ephemeral port.
///
/// # Panics
///
/// Panics when the server cannot bind.
pub(super) async fn start() -> (TestServer, Arc<Svc>) {
    let svc = Arc::new(Svc::default());
    let app = Router::new()
        .route("/echo", get(echo))
        .route("/login", post(login))
        .route("/me", get(me))
        .route("/revoke-all", post(revoke_all))
        .route("/oauth/token", post(oauth))
        .route("/stats", get(stats_handler))
        .with_state(svc.clone());

    (TestServer::axum(app).await.unwrap(), svc)
}

/// The login and token counters (`{"logins": n, "cc_tokens": n}`) of a running service.
///
/// # Panics
///
/// Panics when the service is unreachable or does not answer with JSON.
pub(super) async fn stats(srv: &TestServer) -> Value {
    reqwest::get(format!("{}/stats", srv.base_url()))
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

/// Request headers as a JSON object of name to value; non-UTF-8 values become empty.
fn headers_json(h: &HeaderMap) -> Value {
    let m: BTreeMap<String, String> = h
        .iter()
        .map(|(k, v)| {
            (
                k.as_str().to_string(),
                v.to_str().unwrap_or_default().to_string(),
            )
        })
        .collect();

    json!(m)
}

/// `GET /echo`: returns the request headers it received.
async fn echo(h: HeaderMap) -> Json<Value> {
    Json(headers_json(&h))
}

/// `POST /login`: `{username, password}` → `{token}`; any user with password `pw` succeeds.
async fn login(State(s): State<Arc<Svc>>, Json(b): Json<Value>) -> AxumResponse {
    // Slow on purpose, so concurrent first requests really race for the login.
    tokio::time::sleep(Duration::from_millis(50)).await;
    if b["password"] != "pw" {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "bad credentials" })),
        )
            .into_response();
    }

    let n = s.logins.fetch_add(1, Ordering::SeqCst) + 1;
    let token = format!("tok-{}-{n}", b["username"].as_str().unwrap_or("?"));
    s.tokens.lock().unwrap().insert(token.clone());
    Json(json!({ "token": token })).into_response()
}

/// `GET /me`: echoes the bearer token if it is valid, `401` otherwise.
async fn me(State(s): State<Arc<Svc>>, h: HeaderMap) -> AxumResponse {
    let token = h
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");

    if s.tokens.lock().unwrap().contains(token) {
        Json(json!({ "token": token })).into_response()
    } else {
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "invalid token" })),
        )
            .into_response()
    }
}

/// `POST /revoke-all`: forgets every token, forcing clients to log in again.
async fn revoke_all(State(s): State<Arc<Svc>>) -> StatusCode {
    s.tokens.lock().unwrap().clear();
    StatusCode::NO_CONTENT
}

/// `POST /oauth/token`: client-credentials grant for client `svc` / `s3cret`; tokens
/// claim to expire after 3 s.
async fn oauth(State(s): State<Arc<Svc>>, Form(f): Form<BTreeMap<String, String>>) -> AxumResponse {
    if f.get("grant_type").map(String::as_str) != Some("client_credentials")
        || f.get("client_id").map(String::as_str) != Some("svc")
        || f.get("client_secret").map(String::as_str) != Some("s3cret")
    {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "invalid_client" })),
        )
            .into_response();
    }

    let n = s.cc_tokens.fetch_add(1, Ordering::SeqCst) + 1;
    let token = format!("cc-{n}");
    s.tokens.lock().unwrap().insert(token.clone());
    Json(json!({ "access_token": token, "token_type": "bearer", "expires_in": 3, "scope": f.get("scope") })).into_response()
}

/// `GET /stats`: the login and client-credentials counters.
async fn stats_handler(State(s): State<Arc<Svc>>) -> Json<Value> {
    Json(
        json!({ "logins": s.logins.load(Ordering::SeqCst), "cc_tokens": s.cc_tokens.load(Ordering::SeqCst) }),
    )
}
