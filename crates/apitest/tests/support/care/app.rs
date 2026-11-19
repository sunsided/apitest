//! The app: patient registration, e-mail confirmation, login and activation codes.

use std::sync::atomic::Ordering;
use std::time::Duration;

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Creds, Patient, St, bearer, err, spec};

/// Delay before a confirmation e-mail shows up, like a real mail pipeline.
const MAIL_DELAY: Duration = Duration::from_millis(250);

/// Builds the app router over the shared state.
pub(super) fn router(s: St) -> Router {
    Router::new()
        .route("/patients", post(register))
        .route("/patients/me", delete(delete_patient))
        .route("/confirm", get(confirm))
        .route("/auth/login", post(login))
        .route("/activation-codes", post(create_code))
        .route(
            "/openapi.json",
            get(|| async { Json(spec(include_str!("../../../../../demo/care/app-1.0.yaml"))) }),
        )
        .with_state(s)
}

/// Resolves the logged-in patient's id, or the ready-made `401` response.
fn patient_of(s: &St, h: &HeaderMap) -> Result<String, Box<Response>> {
    let w = s.world.lock().unwrap();
    bearer(h)
        .and_then(|t| w.app_tokens.get(&t).cloned())
        .ok_or_else(|| Box::new(err(StatusCode::UNAUTHORIZED, "not logged in")))
}

/// `POST /patients`: registers an unconfirmed patient and mails the confirmation link.
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
                confirmed: false,
            },
        );
        w.confirm_tokens.insert(token.clone(), id.clone());
        (id, token)
    };

    send_confirmation_later(s, email.clone(), token);
    (StatusCode::CREATED, Json(json!({ "id": id, "email": email, "demo": b["demo"].as_bool().unwrap_or(false), "status": "pending_confirmation" })))
        .into_response()
}

/// Puts the confirmation e-mail for `token` into the mailbox of `to` after [`MAIL_DELAY`].
fn send_confirmation_later(s: St, to: String, token: String) {
    tokio::spawn(async move {
        tokio::time::sleep(MAIL_DELAY).await;
        let link = format!("{}/confirm?token={token}", s.app_url.lock().unwrap());
        s.world.lock().unwrap().mails.push(json!({
            "to": to,
            "subject": "Please confirm your e-mail address",
            "html": format!("<p>Welcome!</p><p><a href=\"{link}\">Confirm my address</a></p>"),
        }));
    });
}

/// Query of the confirmation link.
#[derive(Deserialize)]
struct TokenQuery {
    /// The one-time confirmation token from the e-mail.
    token: String,
}

/// `GET /confirm?token=…`: marks the patient confirmed; the token works only once.
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

/// `POST /auth/login`: exchanges patient credentials for a token; `403` until confirmed.
async fn login(State(s): State<St>, Json(c): Json<Creds>) -> Response {
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

/// `POST /activation-codes`: creates a one-time code a social worker redeems on the hub.
async fn create_code(State(s): State<St>, h: HeaderMap) -> Response {
    let pid = match patient_of(&s, &h) {
        Ok(p) => p,
        Err(r) => return *r,
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

/// `DELETE /patients/me`: removes the patient with their tokens and codes.
async fn delete_patient(State(s): State<St>, h: HeaderMap) -> Response {
    let pid = match patient_of(&s, &h) {
        Ok(p) => p,
        Err(r) => return *r,
    };

    let mut w = s.world.lock().unwrap();
    w.patients.remove(&pid);
    w.app_tokens.retain(|_, p| p != &pid);
    w.codes.retain(|_, p| p != &pid);
    w.confirm_tokens.retain(|_, p| p != &pid);
    StatusCode::NO_CONTENT.into_response()
}
