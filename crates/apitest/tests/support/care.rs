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

// A `#[path]`-included file resolves child modules next to itself, hence the explicit paths.
#[path = "care/app.rs"]
mod app;
#[path = "care/hub.rs"]
mod hub;
#[path = "care/mail.rs"]
mod mail;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use apitest::{Auth, LoginFlow, RunConfig, TestServer};
use axum::Json;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};

/// E-mail address of the hub admin that every fresh world is seeded with.
pub const ADMIN_EMAIL: &str = "admin@hub.test";

/// Password of the seeded hub admin ([`ADMIN_EMAIL`]).
pub const ADMIN_PASSWORD: &str = "admin-secret";

/// A staff account on the hub.
#[derive(Clone)]
struct HubUser {
    /// Login e-mail address; unique across all hub users.
    email: String,
    /// Plain-text password (this is a mock, nothing is hashed).
    password: String,
    /// Either `admin` or `social_service`; decides which hub endpoints the user may call.
    role: String,
}

/// A patient account on the app.
#[derive(Clone)]
struct Patient {
    /// Login e-mail address; unique across all patients.
    email: String,
    /// Plain-text password (this is a mock, nothing is hashed).
    password: String,
    /// `false` until the patient opens the confirmation link; login is refused before that.
    confirmed: bool,
}

/// A case a social worker opened for a patient.
#[derive(Clone)]
struct Case {
    /// Id of the [`Patient`] the case is about.
    patient_id: String,
    /// Id of the social-service [`HubUser`] that opened the case.
    social_worker_id: String,
    /// Lifecycle state, starting at `new`; `in_progress` requires [`Case::therapy`].
    status: String,
    /// Therapy data as stored by the social worker; `None` until the first `PUT`.
    therapy: Option<Value>,
}

/// The shared in-memory state of all three services.
#[derive(Default)]
struct World {
    /// Hub staff by user id.
    hub_users: HashMap<String, HubUser>,
    /// Hub session tokens, mapped to the user id they belong to.
    hub_tokens: HashMap<String, String>,
    /// Patients by patient id.
    patients: HashMap<String, Patient>,
    /// Pending e-mail confirmation tokens, mapped to the patient id.
    confirm_tokens: HashMap<String, String>,
    /// App session tokens, mapped to the patient id.
    app_tokens: HashMap<String, String>,
    /// Unused activation codes, mapped to the patient id that created them.
    codes: HashMap<String, String>,
    /// Cases by case id.
    cases: HashMap<String, Case>,
    /// Every e-mail sent and not yet deleted, as `{to, subject, html}` objects.
    mails: Vec<Value>,
}

/// The axum state handed to every handler of every service.
#[derive(Clone)]
struct St {
    /// The world all three services read and write.
    world: Arc<Mutex<World>>,
    /// Counter for ids and tokens; shared so ids stay unique across services.
    ids: Arc<AtomicU64>,
    /// Public URL of the app service (for links in e-mails).
    app_url: Arc<Mutex<String>>,
}

impl St {
    /// Returns a fresh id of the form `{prefix}-{n}`, unique for the lifetime of the world.
    fn id(&self, prefix: &str) -> String {
        format!("{prefix}-{}", self.ids.fetch_add(1, Ordering::Relaxed))
    }
}

/// Builds a JSON error response `{"error": msg}` with the given status.
fn err(status: StatusCode, msg: &str) -> Response {
    (status, Json(json!({ "error": msg }))).into_response()
}

/// Extracts the token from an `Authorization: Bearer …` header, if present.
fn bearer(h: &HeaderMap) -> Option<String> {
    h.get("authorization")?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::to_string)
}

/// Login request body shared by the hub and the app.
#[derive(Deserialize)]
struct Creds {
    /// The account's e-mail address.
    email: String,
    /// The account's password.
    password: String,
}

/// Parses an embedded `OpenAPI` YAML document into JSON.
///
/// # Panics
///
/// Panics when the embedded document is not valid YAML, which is a bug in the fixture.
fn spec(yaml: &str) -> Value {
    serde_saphyr::from_str(yaml).expect("embedded spec")
}

/// The three services, each on its own ephemeral port.
pub struct Care {
    /// The staff service.
    pub hub: TestServer,
    /// The patient service.
    pub app: TestServer,
    /// The e-mail mock.
    pub mail: TestServer,
}

impl Care {
    /// Starts all three services over one freshly seeded world.
    ///
    /// # Errors
    ///
    /// Fails when a server cannot bind its port.
    pub async fn start() -> anyhow::Result<Care> {
        let s = St {
            world: Arc::new(Mutex::new(seeded_world())),
            ids: Arc::new(AtomicU64::new(1)),
            app_url: Arc::default(),
        };

        let hub = TestServer::axum(hub::router(s.clone())).await?;
        let app = TestServer::axum(app::router(s.clone())).await?;
        *s.app_url.lock().unwrap() = app.base_url();
        let mail = TestServer::axum(mail::router(s.clone())).await?;

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
            .login_flow("hub", email_login())
            .login_flow("app", email_login())
            .actor(
                "hub_admin",
                "hub",
                Auth::login("hub", "${HUB_ADMIN_EMAIL}", "${HUB_ADMIN_PASSWORD}"),
            )
            .quiet(true)
    }

    /// Leftover test data, as reported by the hub.
    ///
    /// # Errors
    ///
    /// Fails when the hub is unreachable or does not answer with JSON.
    pub async fn leftovers(hub_url: &str) -> anyhow::Result<Value> {
        Ok(reqwest::get(format!("{hub_url}/_stats"))
            .await?
            .json()
            .await?)
    }
}

/// A world that contains only the hub admin ([`ADMIN_EMAIL`] / [`ADMIN_PASSWORD`]).
fn seeded_world() -> World {
    let mut world = World::default();
    world.hub_users.insert(
        "user-admin".into(),
        HubUser {
            email: ADMIN_EMAIL.into(),
            password: ADMIN_PASSWORD.into(),
            role: "admin".into(),
        },
    );

    world
}

/// The `login` operation with an `{email, password}` body, as both hub and app expect it.
fn email_login() -> LoginFlow {
    LoginFlow::operation("login").body(json!({ "email": "{username}", "password": "{password}" }))
}
