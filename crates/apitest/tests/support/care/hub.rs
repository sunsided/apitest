//! The hub: staff logins, user management and the case lifecycle.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use serde_json::{Value, json};

use super::{Case, Creds, HubUser, St, bearer, err, spec};

/// Builds the hub router over the shared state.
pub(super) fn router(s: St) -> Router {
    Router::new()
        .route("/auth/login", post(login))
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
            get(|| async { Json(spec(include_str!("../../../../../demo/care/hub-1.0.yaml"))) }),
        )
        .with_state(s)
}

/// Resolves the logged-in hub user and checks their role.
///
/// Returns the user id, or the ready-made `401` (no valid token) or `403` (wrong role)
/// response, boxed to keep the `Result` small.
fn hub_user(s: &St, h: &HeaderMap, role: &str) -> Result<String, Box<Response>> {
    let w = s.world.lock().unwrap();
    let uid = bearer(h)
        .and_then(|t| w.hub_tokens.get(&t).cloned())
        .ok_or_else(|| Box::new(err(StatusCode::UNAUTHORIZED, "not logged in")))?;

    match w.hub_users.get(&uid) {
        Some(u) if u.role == role => Ok(uid),
        _ => Err(Box::new(err(
            StatusCode::FORBIDDEN,
            &format!("requires role {role}"),
        ))),
    }
}

/// Renders a case as the API returns it; `therapy` is only present once stored.
fn case_json(id: &str, c: &Case) -> Value {
    let mut v = json!({ "id": id, "patient_id": c.patient_id, "social_worker_id": c.social_worker_id, "status": c.status });
    if let Some(t) = &c.therapy {
        v["therapy"] = t.clone();
    }

    v
}

/// `POST /auth/login`: exchanges staff credentials for a session token.
async fn login(State(s): State<St>, Json(c): Json<Creds>) -> Response {
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

/// `POST /users` (admin only): creates a staff user; `409` if the e-mail is taken.
async fn create_user(State(s): State<St>, h: HeaderMap, Json(b): Json<Value>) -> Response {
    if let Err(r) = hub_user(&s, &h, "admin") {
        return *r;
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

/// `DELETE /users/{id}` (admin only): removes a staff user and ends their sessions.
async fn delete_user(State(s): State<St>, h: HeaderMap, Path(id): Path<String>) -> Response {
    if let Err(r) = hub_user(&s, &h, "admin") {
        return *r;
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

/// `POST /identifications` (social service): redeems an activation code and opens a case.
async fn identify(State(s): State<St>, h: HeaderMap, Json(b): Json<Value>) -> Response {
    let sw = match hub_user(&s, &h, "social_service") {
        Ok(u) => u,
        Err(r) => return *r,
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

/// `GET /cases/{id}` (social service): returns one case.
async fn get_case(State(s): State<St>, h: HeaderMap, Path(id): Path<String>) -> Response {
    if let Err(r) = hub_user(&s, &h, "social_service") {
        return *r;
    }

    let w = s.world.lock().unwrap();
    match w.cases.get(&id) {
        Some(c) => Json(case_json(&id, c)).into_response(),
        None => err(StatusCode::NOT_FOUND, "no such case"),
    }
}

/// `PATCH /cases/{id}` (social service): changes the status; starting needs therapy data.
async fn update_case(
    State(s): State<St>,
    h: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    if let Err(r) = hub_user(&s, &h, "social_service") {
        return *r;
    }

    let mut w = s.world.lock().unwrap();
    let Some(case) = w.cases.get_mut(&id) else {
        return err(StatusCode::NOT_FOUND, "no such case");
    };

    if let Some(status) = body["status"].as_str() {
        if status == "in_progress" && case.therapy.is_none() {
            return err(
                StatusCode::CONFLICT,
                "a case needs therapy data before it can start",
            );
        }

        case.status = status.to_string();
    }

    Json(case_json(&id, case)).into_response()
}

/// `PUT /cases/{id}/therapy` (social service): stores the request body as therapy data.
async fn put_therapy(
    State(s): State<St>,
    h: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    if let Err(r) = hub_user(&s, &h, "social_service") {
        return *r;
    }

    let mut w = s.world.lock().unwrap();
    let Some(case) = w.cases.get_mut(&id) else {
        return err(StatusCode::NOT_FOUND, "no such case");
    };

    case.therapy = Some(body);
    Json(case_json(&id, case)).into_response()
}

/// `DELETE /cases/{id}` (social service): removes a case.
async fn delete_case(State(s): State<St>, h: HeaderMap, Path(id): Path<String>) -> Response {
    if let Err(r) = hub_user(&s, &h, "social_service") {
        return *r;
    }

    match s.world.lock().unwrap().cases.remove(&id) {
        Some(_) => StatusCode::NO_CONTENT.into_response(),
        None => err(StatusCode::NOT_FOUND, "no such case"),
    }
}

/// `GET /_stats`: counts of everything tests may leave behind (the seeded admin excluded).
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
