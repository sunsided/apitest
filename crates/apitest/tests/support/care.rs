//! Demo "care" platform: three services sharing one in-memory world.
//!
//! * **hub**: staff side. Seeded admin `admin@hub.test` / `admin-secret` manages
//!   users; social-service users identify patients by activation code, open cases,
//!   store therapy data and move cases through their states.
//! * **app**: patient side. Registration sends a confirmation e-mail (via the mail
//!   service, after a short delay), confirming means opening the link, then the
//!   patient can log in and create activation codes.
//! * **mail**: an e-mail mock (`GET /messages?to=…`, `DELETE /messages?to=…`).
//!
//! `GET <hub>/_stats` reports leftover test data so tests can prove the rollback.

#![allow(dead_code, clippy::result_large_err)] // axum handlers return `Response` errors early

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use apitest::{Auth, LoginFlow, RunConfig, TestServer};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

pub const ADMIN_EMAIL: &str = "admin@hub.test";
pub const ADMIN_PASSWORD: &str = "admin-secret";

#[derive(Clone)]
struct HubUser {
    email: String,
    password: String,
    role: String,
}

#[derive(Clone)]
struct Patient {
    email: String,
    password: String,
    demo: bool,
    confirmed: bool,
}

#[derive(Clone)]
struct Case {
    patient_id: String,
    social_worker_id: String,
    status: String,
    therapy: Option<Value>,
}

#[derive(Default)]
struct World {
    hub_users: HashMap<String, HubUser>,
    hub_tokens: HashMap<String, String>,
    patients: HashMap<String, Patient>,
    confirm_tokens: HashMap<String, String>,
    app_tokens: HashMap<String, String>,
    codes: HashMap<String, String>,
    cases: HashMap<String, Case>,
    mails: Vec<Value>,
}

#[derive(Clone)]
struct St {
    world: Arc<Mutex<World>>,
    ids: Arc<AtomicU64>,
    /// Public URL of the app service (for links in e-mails).
    app_url: Arc<Mutex<String>>,
}

impl St {
    fn id(&self, prefix: &str) -> String {
        format!("{prefix}-{}", self.ids.fetch_add(1, Ordering::Relaxed))
    }
}

fn err(status: StatusCode, msg: &str) -> Response {
    (status, Json(json!({ "error": msg }))).into_response()
}

fn bearer(h: &HeaderMap) -> Option<String> {
    h.get("authorization")?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::to_string)
}

#[derive(Deserialize)]
struct Creds {
    email: String,
    password: String,
}

// ----------------------------------------------------------------------------- hub

fn hub_user(s: &St, h: &HeaderMap, role: &str) -> Result<String, Response> {
    let w = s.world.lock().unwrap();
    let uid = bearer(h)
        .and_then(|t| w.hub_tokens.get(&t).cloned())
        .ok_or_else(|| err(StatusCode::UNAUTHORIZED, "not logged in"))?;
    match w.hub_users.get(&uid) {
        Some(u) if u.role == role => Ok(uid),
        _ => Err(err(StatusCode::FORBIDDEN, &format!("requires role {role}"))),
    }
}

fn case_json(id: &str, c: &Case) -> Value {
    let mut v = json!({ "id": id, "patient_id": c.patient_id, "social_worker_id": c.social_worker_id, "status": c.status });
    if let Some(t) = &c.therapy {
        v["therapy"] = t.clone();
    }
    v
}

async fn hub_login(State(s): State<St>, Json(c): Json<Creds>) -> Response {
    let mut w = s.world.lock().unwrap();
    let Some((uid, _)) = w
        .hub_users
        .iter()
        .find(|(_, u)| u.email == c.email && u.password == c.password)
    else {
        return err(StatusCode::UNAUTHORIZED, "bad credentials");
    };
    let uid = uid.clone();
    let token = s.id("hub-tok");
    w.hub_tokens.insert(token.clone(), uid);
    Json(json!({ "token": token })).into_response()
}

async fn create_user(State(s): State<St>, h: HeaderMap, Json(b): Json<Value>) -> Response {
    if let Err(r) = hub_user(&s, &h, "admin") {
        return r;
    }
    let email = b["email"].as_str().unwrap_or_default().to_string();
    let mut w = s.world.lock().unwrap();
    if w.hub_users.values().any(|u| u.email == email) {
        return err(StatusCode::CONFLICT, "email taken");
    }
    let id = s.id("user");
    let role = b["role"].as_str().unwrap_or("social_service").to_string();
    w.hub_users.insert(
        id.clone(),
        HubUser {
            email: email.clone(),
            password: b["password"].as_str().unwrap_or_default().into(),
            role: role.clone(),
        },
    );
    (
        StatusCode::CREATED,
        Json(json!({ "id": id, "email": email, "role": role })),
    )
        .into_response()
}

async fn delete_user(State(s): State<St>, h: HeaderMap, Path(id): Path<String>) -> Response {
    if let Err(r) = hub_user(&s, &h, "admin") {
        return r;
    }
    let mut w = s.world.lock().unwrap();
    match w.hub_users.remove(&id) {
        Some(_) => {
            w.hub_tokens.retain(|_, uid| uid != &id);
            StatusCode::NO_CONTENT.into_response()
        }
        None => err(StatusCode::NOT_FOUND, "no such user"),
    }
}

async fn identify(State(s): State<St>, h: HeaderMap, Json(b): Json<Value>) -> Response {
    let sw = match hub_user(&s, &h, "social_service") {
        Ok(u) => u,
        Err(r) => return r,
    };
    let code = b["activation_code"].as_str().unwrap_or_default();
    let mut w = s.world.lock().unwrap();
    let Some(patient_id) = w.codes.remove(code) else {
        return err(StatusCode::NOT_FOUND, "unknown or used activation code");
    };
    let id = s.id("case");
    let case = Case {
        patient_id,
        social_worker_id: sw,
        status: "new".into(),
        therapy: None,
    };
    let body = case_json(&id, &case);
    w.cases.insert(id, case);
    (StatusCode::CREATED, Json(body)).into_response()
}

async fn get_case(State(s): State<St>, h: HeaderMap, Path(id): Path<String>) -> Response {
    if let Err(r) = hub_user(&s, &h, "social_service") {
        return r;
    }
    let w = s.world.lock().unwrap();
    match w.cases.get(&id) {
        Some(c) => Json(case_json(&id, c)).into_response(),
        None => err(StatusCode::NOT_FOUND, "no such case"),
    }
}

async fn update_case(
    State(s): State<St>,
    h: HeaderMap,
    Path(id): Path<String>,
    Json(b): Json<Value>,
) -> Response {
    if let Err(r) = hub_user(&s, &h, "social_service") {
        return r;
    }
    let mut w = s.world.lock().unwrap();
    let Some(c) = w.cases.get_mut(&id) else {
        return err(StatusCode::NOT_FOUND, "no such case");
    };
    if let Some(status) = b["status"].as_str() {
        if status == "in_progress" && c.therapy.is_none() {
            return err(
                StatusCode::CONFLICT,
                "a case needs therapy data before it can start",
            );
        }
        c.status = status.to_string();
    }
    Json(case_json(&id, c)).into_response()
}

async fn put_therapy(
    State(s): State<St>,
    h: HeaderMap,
    Path(id): Path<String>,
    Json(b): Json<Value>,
) -> Response {
    if let Err(r) = hub_user(&s, &h, "social_service") {
        return r;
    }
    let mut w = s.world.lock().unwrap();
    let Some(c) = w.cases.get_mut(&id) else {
        return err(StatusCode::NOT_FOUND, "no such case");
    };
    c.therapy = Some(b);
    Json(case_json(&id, c)).into_response()
}

async fn delete_case(State(s): State<St>, h: HeaderMap, Path(id): Path<String>) -> Response {
    if let Err(r) = hub_user(&s, &h, "social_service") {
        return r;
    }
    match s.world.lock().unwrap().cases.remove(&id) {
        Some(_) => StatusCode::NO_CONTENT.into_response(),
        None => err(StatusCode::NOT_FOUND, "no such case"),
    }
}

async fn stats(State(s): State<St>) -> Json<Value> {
    let w = s.world.lock().unwrap();
    Json(json!({
        "staff": w.hub_users.values().filter(|u| u.role != "admin").count(),
        "patients": w.patients.len(),
        "activation_codes": w.codes.len(),
        "cases": w.cases.len(),
        "mails": w.mails.len(),
    }))
}

fn spec(yaml: &str) -> Value {
    serde_yaml_ng::from_str(yaml).expect("embedded spec")
}

fn hub_router(s: St) -> Router {
    Router::new()
        .route("/auth/login", post(hub_login))
        .route("/users", post(create_user))
        .route("/users/{id}", delete(delete_user))
        .route("/identifications", post(identify))
        .route(
            "/cases/{id}",
            get(get_case).patch(update_case).delete(delete_case),
        )
        .route("/cases/{id}/therapy", put(put_therapy))
        .route("/_stats", get(stats))
        .route(
            "/openapi.json",
            get(|| async { Json(spec(include_str!("../../../../demo/care/hub-1.0.yaml"))) }),
        )
        .with_state(s)
}

// ----------------------------------------------------------------------------- app

fn patient_of(s: &St, h: &HeaderMap) -> Result<String, Response> {
    let w = s.world.lock().unwrap();
    bearer(h)
        .and_then(|t| w.app_tokens.get(&t).cloned())
        .ok_or_else(|| err(StatusCode::UNAUTHORIZED, "not logged in"))
}

async fn register(State(s): State<St>, Json(b): Json<Value>) -> Response {
    let email = b["email"].as_str().unwrap_or_default().to_string();
    let (id, token) = {
        let mut w = s.world.lock().unwrap();
        if w.patients.values().any(|p| p.email == email) {
            return err(StatusCode::CONFLICT, "already registered");
        }
        let id = s.id("patient");
        let token = s.id("confirm");
        w.patients.insert(
            id.clone(),
            Patient {
                email: email.clone(),
                password: b["password"].as_str().unwrap_or_default().into(),
                demo: b["demo"].as_bool().unwrap_or(false),
                confirmed: false,
            },
        );
        w.confirm_tokens.insert(token.clone(), id.clone());
        (id, token)
    };
    // The e-mail arrives a little later, like a real mail pipeline.
    let (s2, to) = (s.clone(), email.clone());
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(250)).await;
        let link = format!("{}/confirm?token={token}", s2.app_url.lock().unwrap());
        s2.world.lock().unwrap().mails.push(json!({
            "to": to,
            "subject": "Please confirm your e-mail address",
            "html": format!("<p>Welcome!</p><p><a href=\"{link}\">Confirm my address</a></p>"),
        }));
    });
    (StatusCode::CREATED, Json(json!({ "id": id, "email": email, "demo": b["demo"].as_bool().unwrap_or(false), "status": "pending_confirmation" })))
        .into_response()
}

#[derive(Deserialize)]
struct TokenQuery {
    token: String,
}

async fn confirm(State(s): State<St>, Query(q): Query<TokenQuery>) -> Response {
    let mut w = s.world.lock().unwrap();
    match w
        .confirm_tokens
        .remove(&q.token)
        .and_then(|pid| w.patients.get_mut(&pid).map(|p| p.confirmed = true))
    {
        Some(()) => Html("<h1>Thanks, your address is confirmed.</h1>").into_response(),
        None => (StatusCode::NOT_FOUND, Html("<h1>Link expired</h1>")).into_response(),
    }
}

async fn app_login(State(s): State<St>, Json(c): Json<Creds>) -> Response {
    let mut w = s.world.lock().unwrap();
    let Some((pid, p)) = w
        .patients
        .iter()
        .find(|(_, p)| p.email == c.email && p.password == c.password)
    else {
        return err(StatusCode::UNAUTHORIZED, "bad credentials");
    };
    if !p.confirmed {
        return err(StatusCode::FORBIDDEN, "e-mail address not confirmed");
    }
    let pid = pid.clone();
    let token = s.id("app-tok");
    w.app_tokens.insert(token.clone(), pid);
    Json(json!({ "token": token })).into_response()
}

async fn create_code(State(s): State<St>, h: HeaderMap) -> Response {
    let pid = match patient_of(&s, &h) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let n = s.ids.fetch_add(1, Ordering::Relaxed);
    let code = format!("{:04X}-{:04X}", n * 7919 % 65536, n * 104_729 % 65536);
    s.world.lock().unwrap().codes.insert(code.clone(), pid);
    (
        StatusCode::CREATED,
        Json(json!({ "code": code, "expires_in": 900 })),
    )
        .into_response()
}

async fn delete_patient(State(s): State<St>, h: HeaderMap) -> Response {
    let pid = match patient_of(&s, &h) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let mut w = s.world.lock().unwrap();
    w.patients.remove(&pid);
    w.app_tokens.retain(|_, p| p != &pid);
    w.codes.retain(|_, p| p != &pid);
    w.confirm_tokens.retain(|_, p| p != &pid);
    StatusCode::NO_CONTENT.into_response()
}

fn app_router(s: St) -> Router {
    Router::new()
        .route("/patients", post(register))
        .route("/patients/me", delete(delete_patient))
        .route("/confirm", get(confirm))
        .route("/auth/login", post(app_login))
        .route("/activation-codes", post(create_code))
        .route(
            "/openapi.json",
            get(|| async { Json(spec(include_str!("../../../../demo/care/app-1.0.yaml"))) }),
        )
        .with_state(s)
}

// ----------------------------------------------------------------------------- mail

#[derive(Deserialize)]
struct ToQuery {
    to: String,
}

async fn messages(State(s): State<St>, Query(q): Query<ToQuery>) -> Json<Value> {
    let w = s.world.lock().unwrap();
    Json(Value::Array(
        w.mails
            .iter()
            .filter(|m| m["to"] == q.to)
            .cloned()
            .collect(),
    ))
}

async fn delete_messages(State(s): State<St>, Query(q): Query<ToQuery>) -> StatusCode {
    s.world.lock().unwrap().mails.retain(|m| m["to"] != q.to);
    StatusCode::NO_CONTENT
}

fn mail_router(s: St) -> Router {
    Router::new()
        .route("/messages", get(messages).delete(delete_messages))
        .with_state(s)
}

// ----------------------------------------------------------------------------- startup

/// The three services, each on its own ephemeral port.
pub struct Care {
    pub hub: TestServer,
    pub app: TestServer,
    pub mail: TestServer,
}

impl Care {
    pub async fn start() -> anyhow::Result<Care> {
        let mut world = World::default();
        world.hub_users.insert(
            "user-admin".into(),
            HubUser {
                email: ADMIN_EMAIL.into(),
                password: ADMIN_PASSWORD.into(),
                role: "admin".into(),
            },
        );
        let s = St {
            world: Arc::new(Mutex::new(world)),
            ids: Arc::new(AtomicU64::new(1)),
            app_url: Arc::default(),
        };
        let hub = TestServer::axum(hub_router(s.clone())).await?;
        let app = TestServer::axum(app_router(s.clone())).await?;
        *s.app_url.lock().unwrap() = app.base_url();
        let mail = TestServer::axum(mail_router(s.clone())).await?;
        Ok(Care { hub, app, mail })
    }

    /// A run config with the three services and the admin credentials as variables.
    pub fn config(&self) -> RunConfig {
        RunConfig::new(self.hub.base_url())
            .service("hub", self.hub.base_url())
            .service("app", self.app.base_url())
            .service("mail", self.mail.base_url())
            .var("HUB_ADMIN_EMAIL", ADMIN_EMAIL)
            .var("HUB_ADMIN_PASSWORD", ADMIN_PASSWORD)
            // Both services log in with {email, password} → {token}.
            .login_flow(
                "hub",
                LoginFlow::operation("login")
                    .body(json!({ "email": "{username}", "password": "{password}" })),
            )
            .login_flow(
                "app",
                LoginFlow::operation("login")
                    .body(json!({ "email": "{username}", "password": "{password}" })),
            )
            .actor(
                "hub_admin",
                "hub",
                Auth::login("hub", "${HUB_ADMIN_EMAIL}", "${HUB_ADMIN_PASSWORD}"),
            )
            .quiet(true)
    }

    /// Leftover test data, as reported by the hub.
    pub async fn leftovers(hub_url: &str) -> anyhow::Result<Value> {
        Ok(reqwest::get(format!("{hub_url}/_stats"))
            .await?
            .json()
            .await?)
    }
}
