//! The billing suite written with attribute macros: module = suite, fixtures are
//! injected by parameter name, test ids are module paths.
//!
//! cargo test -p apitest --features full --test macro_harness
//! cargo test -p apitest --features full --test macro_harness -- billing::create_and_fetch_invoice --exact
//! cargo nextest run -p apitest --features full --test macro_harness

#[path = "support/demo_server.rs"]
mod demo;
#[path = "support/rust_suite.rs"]
#[allow(unused)]
mod rust_suite;

mod billing {
    use apitest::prelude::*;
    use serde::{Deserialize, Serialize};

    /// Base suite for this module: the pinned contract.
    #[apitest::suite]
    fn billing() -> Result<Suite> {
        Ok(Suite::new("billing").contract(
            Contract::pinned(
                "billing",
                super::rust_suite::specs_dir().join("billing-1.0.yaml"),
            )?
            .live_spec("/openapi.json"),
        ))
    }

    #[derive(Serialize, Deserialize)]
    pub struct Session {
        token: String,
    }

    /// Suite-scoped login; `logout` runs when the suite ends, whatever happened.
    #[apitest::fixture(scope = "suite", teardown = logout)]
    async fn session(cx: Ctx) -> Result<Session> {
        let r = cx
            .call("billing", "issueToken")?
            .form(&[
                ("user", &cx.env("API_USER")?),
                ("pass", &cx.env("API_PASS")?),
            ])
            .send()
            .await?
            .expect(200)?;
        let token = r
            .pointer("/access_token")
            .and_then(Value::as_str)
            .context("no token")?
            .to_string();
        cx.default_header("Authorization", format!("Bearer {token}"));
        Ok(Session { token })
    }

    async fn logout(cx: Ctx) -> Result<()> {
        cx.call("billing", "revokeToken")?
            .send()
            .await?
            .expect(204)?;
        Ok(())
    }

    /// Test-scoped fixture that depends on another fixture (injected by name).
    #[apitest::fixture(scope = "test")]
    async fn usd_invoice(cx: Ctx, session: Session) -> Result<u64> {
        ensure!(!session.token.is_empty());
        let inv = cx
            .call("billing", "createInvoice")?
            .json(json!({ "amount": 100, "currency": "USD" }))
            .send()
            .await?
            .expect(201)?;
        let id = inv
            .pointer("/id")
            .and_then(Value::as_u64)
            .context("no id")?;
        cx.defer("delete usd invoice", move |cx| async move {
            cx.call("billing", "deleteInvoice")?
                .path_param("id", id)
                .send()
                .await?
                .expect(204)?;
            Ok(())
        });
        Ok(id)
    }

    #[apitest::test(parallel)]
    async fn create_and_fetch_invoice(cx: Ctx) -> Result<()> {
        let inv = cx
            .call("billing", "createInvoice")?
            .json(json!({ "amount": 4200, "currency": "EUR" }))
            .send()
            .await?
            .expect(201)?;
        let id = inv.pointer("/id").cloned().context("no id")?;
        let del = id.clone();
        cx.defer("delete invoice", move |cx| async move {
            cx.call("billing", "deleteInvoice")?
                .path_param("id", del)
                .send()
                .await?
                .expect(204)?;
            Ok(())
        });
        let got = cx
            .call("billing", "getInvoice")?
            .path_param("id", id)
            .send()
            .await?
            .expect(200)?;
        ensure!(got.pointer("/amount") == Some(&json!(4200)));
        Ok(())
    }

    #[apitest::test(parallel, resources = ["invoices"], timeout_ms = 5000)]
    async fn fixture_injection(cx: Ctx, usd_invoice: u64) -> Result<()> {
        let got = cx
            .call("billing", "getInvoice")?
            .path_param("id", usd_invoice)
            .send()
            .await?
            .expect(200)?;
        ensure!(got.pointer("/currency") == Some(&json!("USD")));
        Ok(())
    }

    #[apitest::test(parallel, name = "unknown invoice is 404")]
    async fn unknown_invoice(cx: Ctx) -> Result<()> {
        cx.call("billing", "getInvoice")?
            .path_param("id", 999_999)
            .send()
            .await?
            .expect(404)?;
        Ok(())
    }

    #[apitest::test(serial)]
    async fn listing_sees_exactly_what_this_test_created(cx: Ctx) -> Result<()> {
        for amount in [1, 2] {
            let inv = cx
                .call("billing", "createInvoice")?
                .json(json!({ "amount": amount, "currency": "EUR" }))
                .send()
                .await?
                .expect(201)?;
            let id = inv.pointer("/id").cloned().unwrap_or_default();
            cx.defer("delete invoice", move |cx| async move {
                cx.call("billing", "deleteInvoice")?
                    .path_param("id", id)
                    .send()
                    .await?
                    .expect(204)?;
                Ok(())
            });
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
}

/// Items in the crate root form a suite named after the test binary.
#[apitest::test]
async fn service_publishes_its_spec(cx: apitest::Ctx) -> anyhow::Result<()> {
    cx.request("GET", "/openapi.json")
        .send()
        .await?
        .expect(200)?;
    Ok(())
}

fn main() {
    apitest::harness::main(|| async {
        // One in-process server per test process (one per test under nextest).
        // DEMO_VARIANT=2.0 shows how contract drift is reported (it fails every test).
        let variant: demo::Variant = std::env::var("DEMO_VARIANT")
            .as_deref()
            .unwrap_or("1.0")
            .parse()
            .map_err(anyhow::Error::msg)?;
        let server = apitest::TestServer::axum(demo::app(variant)).await?;
        let cfg = apitest::RunConfig::new(server.base_url())
            .var("API_USER", "demo")
            .var("API_PASS", "secret")
            .quiet(true);
        Ok(apitest::harness::Plan::new(cfg).registered()?.keep(server))
    })
}
