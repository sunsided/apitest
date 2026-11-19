//! Helpers that run inside suites and carry observations out to the test's assertions.

use std::sync::{Arc, Mutex};

use apitest::prelude::*;

/// Collects values from inside tests for assertions afterwards.
#[derive(Clone, Default)]
pub(super) struct Probe(Arc<Mutex<Vec<Value>>>);

impl Probe {
    /// Records one observation.
    pub(super) fn push(&self, v: Value) {
        self.0.lock().unwrap().push(v);
    }

    /// Every observation so far, in recording order.
    pub(super) fn all(&self) -> Vec<Value> {
        self.0.lock().unwrap().clone()
    }
}

/// Calls `GET /echo` and records what was received, what was sent, the request's span
/// and the test's trace id under `label`.
pub(super) async fn echo_record(probe: &Probe, label: &str) -> Result<()> {
    let resp = Actor::on("svc").get("/echo").expect(200).await?;
    probe.push(json!({
        "label": label,
        "received": resp.body,
        "sent": resp.request.headers,
        "span": resp.trace,
        "test_trace": current_trace()?.trace_id,
    }));

    Ok(())
}

/// Minimal `join_all` without pulling in the futures crate.
///
/// Every future runs as its own task inside the current test context.
///
/// # Panics
///
/// Panics outside a test context or when a task panics.
pub(super) async fn futures_join<F: std::future::IntoFuture>(
    fs: impl IntoIterator<Item = F>,
) -> Vec<F::Output>
where
    F::IntoFuture: Send + 'static,
    F::Output: Send + 'static,
{
    let cx = apitest::current().unwrap();
    let handles: Vec<_> = fs
        .into_iter()
        .map(|f| tokio::spawn(cx.enter(f.into_future())))
        .collect();

    let mut out = vec![];
    for h in handles {
        out.push(h.await.unwrap());
    }

    out
}
