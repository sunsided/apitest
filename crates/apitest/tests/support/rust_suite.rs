//! The billing suite written in Rust (Rhai and JS versions live in `demo/suites/`).

use std::path::{Path, PathBuf};
use std::time::Duration;

use apitest::prelude::*;

/// Directory with the pinned billing specs (`billing-1.0.yaml` and friends).
pub fn specs_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../demo/specs")
}

/// Directory with the Rhai and JS versions of the billing suite.
pub fn suites_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../demo/suites")
}

/// The billing suite: pinned to the 1.0 contract (tolerant), one login for the whole suite,
/// and tests that each clean up what they create.
///
/// # Errors
///
/// Fails when the pinned spec cannot be loaded.
pub fn billing_suite() -> Result<Suite> {
    let contract = Contract::pinned("billing", specs_dir().join("billing-1.0.yaml"))?
        .live_spec("/openapi.json")
        .mode(Compat::Tolerant);

    Ok(Suite::new("billing-rust")
        .contract(contract)
        // Suite fixture with explicit teardown: one login for the whole suite.
        .fixture_with_teardown("token", Scope::Suite, log_in, revoke_token)
        .test_with(
            "create and fetch invoice",
            TestOpts::parallel(),
            create_and_fetch_invoice,
        )
        .test_with(
            "unknown invoice is 404",
            TestOpts::parallel(),
            unknown_invoice_is_404,
        )
        .test_with(
            "invalid amount is rejected by the service",
            TestOpts::parallel(),
            invalid_amount_is_rejected,
        )
        // Per-test setup/teardown: the setup's value is visible to the body via cx.get().
        .test_with(
            "per-test setup and teardown",
            TestOpts::parallel()
                .resource("invoices")
                .setup(create_usd_invoice)
                .teardown(delete_usd_invoice),
            usd_invoice_is_visible,
        )
        // Serial: counts invoices, so nothing else may run at the same time.
        .test_with(
            "listing sees exactly what this test created",
            TestOpts::serial(),
            listing_sees_own_invoices,
        )
        .test_with(
            "slow test with its own timeout",
            TestOpts::parallel().timeout(Duration::from_secs(5)),
            slow_test,
        ))
}

/// Suite fixture: logs in with `API_USER` / `API_PASS` and makes every later request
/// carry the token. Yields the token.
async fn log_in(cx: Ctx) -> Result<String> {
    let (user, pass) = (cx.env("API_USER")?, cx.env("API_PASS")?);
    let tok = cx
        .call("billing", "issueToken")?
        .form(&[("user", &user), ("pass", &pass)])
        .send()
        .await?
        .expect(200)?;
    let token = tok
        .pointer("/access_token")
        .and_then(Value::as_str)
        .context("no token")?
        .to_string();

    // Every request from this suite (and its tests) carries the token.
    cx.default_header("Authorization", format!("Bearer {token}"));
    Ok(token)
}

/// Teardown of [`log_in`]: revokes the suite's token.
async fn revoke_token(cx: Ctx) -> Result<()> {
    cx.call("billing", "revokeToken")?
        .send()
        .await?
        .expect(204)?;
    Ok(())
}

/// Registers a deferred cleanup that deletes the invoice with the given id.
fn delete_later(cx: &Ctx, id: Value) {
    cx.defer("delete invoice", move |cx| async move {
        cx.call("billing", "deleteInvoice")?
            .path_param("id", id)
            .send()
            .await?
            .expect(204)?;
        Ok(())
    });
}

/// A created invoice can be read back with the same amount.
async fn create_and_fetch_invoice(cx: Ctx) -> Result<()> {
    let inv = cx
        .call("billing", "createInvoice")?
        .json(json!({ "amount": 4200, "currency": "EUR" }))
        .send()
        .await?
        .expect(201)?;
    let id = inv.pointer("/id").cloned().context("no id")?;
    // Cleanup is registered right where the resource is created.
    delete_later(&cx, id.clone());

    let got = cx
        .call("billing", "getInvoice")?
        .path_param("id", id)
        .send()
        .await?
        .expect(200)?;
    ensure!(
        got.pointer("/amount") == Some(&json!(4200)),
        "amount mismatch: {}",
        got.text
    );
    Ok(())
}

/// Reading an id that was never issued is a `404`.
async fn unknown_invoice_is_404(cx: Ctx) -> Result<()> {
    cx.call("billing", "getInvoice")?
        .path_param("id", 999_999)
        .send()
        .await?
        .expect(404)?;
    Ok(())
}

/// The service itself refuses a zero amount with `422`.
async fn invalid_amount_is_rejected(cx: Ctx) -> Result<()> {
    // A raw request bypasses the contract layer, which would refuse amount 0 client-side.
    cx.request("POST", "/invoices")
        .json(json!({ "amount": 0, "currency": "EUR" }))
        .send()
        .await?
        .expect(422)?;
    Ok(())
}

/// Per-test setup: creates a USD invoice and stores its id as `invoice_id`.
async fn create_usd_invoice(cx: Ctx) -> Result<()> {
    let inv = cx
        .call("billing", "createInvoice")?
        .json(json!({ "amount": 100, "currency": "USD" }))
        .send()
        .await?
        .expect(201)?;
    cx.set("invoice_id", inv.pointer("/id"))?;
    Ok(())
}

/// Per-test teardown: deletes the invoice [`create_usd_invoice`] created.
async fn delete_usd_invoice(cx: Ctx) -> Result<()> {
    let id: u64 = cx.get_as("invoice_id")?;
    cx.call("billing", "deleteInvoice")?
        .path_param("id", id)
        .send()
        .await?
        .expect(204)?;
    Ok(())
}

/// The invoice from the per-test setup is visible to the test body.
async fn usd_invoice_is_visible(cx: Ctx) -> Result<()> {
    let id: u64 = cx.get_as("invoice_id")?;
    let got = cx
        .call("billing", "getInvoice")?
        .path_param("id", id)
        .send()
        .await?
        .expect(200)?;
    ensure!(got.pointer("/currency") == Some(&json!("USD")));
    Ok(())
}

/// The EUR listing contains exactly the two invoices this test created.
async fn listing_sees_own_invoices(cx: Ctx) -> Result<()> {
    for amount in [1, 2] {
        let inv = cx
            .call("billing", "createInvoice")?
            .json(json!({ "amount": amount, "currency": "EUR" }))
            .send()
            .await?
            .expect(201)?;
        delete_later(&cx, inv.pointer("/id").cloned().unwrap_or_default());
    }

    let list = cx
        .call("billing", "listInvoices")?
        .query("currency", "EUR")
        .send()
        .await?
        .expect(200)?;
    let n = list.body.as_array().map(Vec::len).unwrap_or_default();
    ensure!(n == 2, "expected 2 invoices, found {n}");
    Ok(())
}

/// A test that takes a moment, well within its own timeout.
async fn slow_test(_cx: Ctx) -> Result<()> {
    tokio::time::sleep(Duration::from_millis(50)).await;
    Ok(())
}
