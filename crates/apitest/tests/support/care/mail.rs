//! The e-mail mock: lists and deletes the messages sent to one address.

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::Value;

use super::St;

/// Builds the mail router over the shared state.
pub(super) fn router(s: St) -> Router {
    Router::new()
        .route("/messages", get(messages).delete(delete_messages))
        .with_state(s)
}

/// Query selecting one mailbox.
#[derive(Deserialize)]
struct ToQuery {
    /// Recipient address; matched exactly.
    to: String,
}

/// `GET /messages?to=…`: every message sent to the address, oldest first.
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

/// `DELETE /messages?to=…`: empties the address's mailbox.
async fn delete_messages(State(s): State<St>, Query(q): Query<ToQuery>) -> StatusCode {
    s.world.lock().unwrap().mails.retain(|m| m["to"] != q.to);
    StatusCode::NO_CONTENT
}
