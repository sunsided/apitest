//! A small axum "billing" service used as the system under test.
//!
//! It exists in three versions so contract pinning can be demonstrated:
//! * `1.0` matches the pinned spec,
//! * `1.1` adds things (compatible),
//! * `2.0` renames `amount` → `amount_cents` and makes ids strings (breaking).
//!
//! `GET /_stats` exposes open tokens and stored invoices, so tests can prove
//! that teardown really cleaned up.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::{Form, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

/// The API version the service speaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Variant {
    /// Exactly the pinned `billing-1.0.yaml` contract.
    V1_0,
    /// Compatible additions on top of 1.0 (`billing-1.1.yaml`).
    V1_1,
    /// Breaking: `amount` becomes `amount_cents` and ids become strings (`billing-2.0.yaml`).
    V2_0,
}

impl std::str::FromStr for Variant {
    type Err = String;

    /// Accepts `1.0`/`v1`/`1`, `1.1` and `2.0`/`v2`/`2`; anything else is an error message.
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "1.0" | "v1" | "1" => Ok(Variant::V1_0),
            "1.1" => Ok(Variant::V1_1),
            "2.0" | "v2" | "2" => Ok(Variant::V2_0),
            o => Err(format!("unknown variant {o} (1.0 | 1.1 | 2.0)")),
        }
    }
}

/// The `OpenAPI` document served by [`Variant::V1_0`].
const SPEC_1_0: &str = include_str!("../../../../demo/specs/billing-1.0.yaml");
/// The `OpenAPI` document served by [`Variant::V1_1`].
const SPEC_1_1: &str = include_str!("../../../../demo/specs/billing-1.1.yaml");
/// The `OpenAPI` document served by [`Variant::V2_0`].
const SPEC_2_0: &str = include_str!("../../../../demo/specs/billing-2.0.yaml");

/// A stored invoice, independent of how a [`Variant`] renders it.
#[derive(Clone)]
struct Invoice {
    /// Server-assigned id, unique for the lifetime of the service.
    id: u64,
    /// Amount in minor units (cents); always positive.
    amount: i64,
    /// Currency code as sent by the client, e.g. `EUR`.
    currency: String,
    /// Payment status; every invoice starts (and stays) `open`.
    status: &'static str,
    /// Optional free-text note from the client.
    note: Option<String>,
}

/// The service's in-memory storage.
#[derive(Default)]
struct Db {
    /// Bearer tokens that are currently valid.
    tokens: HashSet<String>,
    /// Invoices by id.
    invoices: HashMap<u64, Invoice>,
}

/// The axum state shared by all handlers.
#[derive(Clone)]
pub struct AppState {
    /// The API version to speak.
    variant: Variant,
    /// Tokens and invoices.
    db: Arc<Mutex<Db>>,
    /// Counter for invoice ids and token suffixes, starting at 1.
    next: Arc<AtomicU64>,
}

/// Builds the billing service for one API version, with empty storage.
pub fn app(variant: Variant) -> Router {
    let state = AppState {
        variant,
        db: Arc::default(),
        next: Arc::new(AtomicU64::new(1)),
    };

    Router::new()
        .route("/auth/token", post(issue_token))
        .route("/auth/revoke", post(revoke))
        .route("/invoices", get(list_invoices).post(create_invoice))
        .route("/invoices/{id}", get(get_invoice).delete(delete_invoice))
        .route("/openapi.json", get(openapi))
        .route("/_stats", get(stats))
        .with_state(state)
}

/// Builds a JSON error response `{"error": msg}` with the given status.
fn err(status: StatusCode, msg: &str) -> Response {
    (status, Json(json!({ "error": msg }))).into_response()
}

/// Extracts the token from an `Authorization: Bearer …` header, if present.
fn bearer(headers: &HeaderMap) -> Option<String> {
    headers
        .get("authorization")?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::to_string)
}

/// Whether the request carries a token the service issued and has not revoked.
fn authorized(state: &AppState, headers: &HeaderMap) -> bool {
    bearer(headers).is_some_and(|t| state.db.lock().unwrap().tokens.contains(&t))
}

/// Renders an invoice in the shape of the given API version.
fn render(v: Variant, inv: &Invoice) -> Value {
    match v {
        Variant::V2_0 => {
            json!({ "id": inv.id.to_string(), "amount_cents": inv.amount, "currency": inv.currency, "status": inv.status, "note": inv.note })
        }
        _ => {
            json!({ "id": inv.id, "amount": inv.amount, "currency": inv.currency, "status": inv.status, "note": inv.note })
        }
    }
}

/// Form body of the token request.
#[derive(Deserialize)]
struct Creds {
    /// User name; only `demo` is accepted.
    user: String,
    /// Password; only `secret` is accepted.
    pass: String,
}

/// `POST /auth/token`: issues a bearer token for `demo` / `secret`, `401` otherwise.
async fn issue_token(State(s): State<AppState>, Form(c): Form<Creds>) -> Response {
    if c.user != "demo" || c.pass != "secret" {
        return err(StatusCode::UNAUTHORIZED, "bad credentials");
    }

    let token = format!("tok-{}", s.next.fetch_add(1, Ordering::Relaxed));
    s.db.lock().unwrap().tokens.insert(token.clone());
    Json(json!({ "access_token": token, "expires_in": 3600 })).into_response()
}

/// `POST /auth/revoke`: invalidates the presented token; always `204`.
async fn revoke(State(s): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(t) = bearer(&headers) {
        s.db.lock().unwrap().tokens.remove(&t);
    }

    StatusCode::NO_CONTENT.into_response()
}

/// Query of the invoice listing.
#[derive(Deserialize)]
struct ListQuery {
    /// Only list invoices in this currency; all invoices when absent.
    currency: Option<String>,
}

/// `GET /invoices`: lists invoices ordered by id, optionally filtered by currency.
async fn list_invoices(
    State(s): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ListQuery>,
) -> Response {
    if !authorized(&s, &headers) {
        return err(StatusCode::UNAUTHORIZED, "missing or invalid token");
    }

    let db = s.db.lock().unwrap();
    let mut items: Vec<&Invoice> = db
        .invoices
        .values()
        .filter(|i| q.currency.as_ref().is_none_or(|c| &i.currency == c))
        .collect();
    items.sort_by_key(|i| i.id);

    Json(
        items
            .into_iter()
            .map(|i| render(s.variant, i))
            .collect::<Vec<_>>(),
    )
    .into_response()
}

/// `POST /invoices`: stores a new open invoice; `422` on a missing or non-positive amount
/// or a missing currency.
async fn create_invoice(
    State(s): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if !authorized(&s, &headers) {
        return err(StatusCode::UNAUTHORIZED, "missing or invalid token");
    }

    let amount_key = if s.variant == Variant::V2_0 {
        "amount_cents"
    } else {
        "amount"
    };
    let Some(amount) = body
        .get(amount_key)
        .and_then(Value::as_i64)
        .filter(|a| *a > 0)
    else {
        return err(
            StatusCode::UNPROCESSABLE_ENTITY,
            &format!("`{amount_key}` must be a positive integer"),
        );
    };
    let Some(currency) = body.get("currency").and_then(Value::as_str) else {
        return err(StatusCode::UNPROCESSABLE_ENTITY, "`currency` is required");
    };

    let inv = Invoice {
        id: s.next.fetch_add(1, Ordering::Relaxed),
        amount,
        currency: currency.to_string(),
        status: "open",
        note: body.get("note").and_then(Value::as_str).map(str::to_string),
    };
    s.db.lock().unwrap().invoices.insert(inv.id, inv.clone());
    (StatusCode::CREATED, Json(render(s.variant, &inv))).into_response()
}

/// `GET /invoices/{id}`: returns one invoice, `404` if unknown.
async fn get_invoice(State(s): State<AppState>, Path(id): Path<u64>) -> Response {
    match s.db.lock().unwrap().invoices.get(&id) {
        Some(inv) => Json(render(s.variant, inv)).into_response(),
        None => err(StatusCode::NOT_FOUND, "no such invoice"),
    }
}

/// `DELETE /invoices/{id}`: removes one invoice, `404` if unknown.
async fn delete_invoice(State(s): State<AppState>, Path(id): Path<u64>) -> Response {
    match s.db.lock().unwrap().invoices.remove(&id) {
        Some(_) => StatusCode::NO_CONTENT.into_response(),
        None => err(StatusCode::NOT_FOUND, "no such invoice"),
    }
}

/// `GET /openapi.json`: the variant's spec, converted from YAML to JSON.
///
/// # Panics
///
/// Panics when an embedded spec is not valid YAML, which is a bug in the fixture.
async fn openapi(State(s): State<AppState>) -> Response {
    let yaml = match s.variant {
        Variant::V1_0 => SPEC_1_0,
        Variant::V1_1 => SPEC_1_1,
        Variant::V2_0 => SPEC_2_0,
    };
    let doc: Value = serde_saphyr::from_str(yaml).expect("embedded spec is valid YAML");

    Json(doc).into_response()
}

/// `GET /_stats`: number of valid tokens and stored invoices, to prove cleanup.
async fn stats(State(s): State<AppState>) -> Response {
    let db = s.db.lock().unwrap();
    Json(json!({ "tokens": db.tokens.len(), "invoices": db.invoices.len() })).into_response()
}
