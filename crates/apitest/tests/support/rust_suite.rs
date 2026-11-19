//! The billing suite written in Rust (Rhai and JS versions live in `demo/suites/`).

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use apitest::prelude::*;

pub fn specs_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../demo/specs")
}

pub fn suites_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../demo/suites")
}

pub fn billing_suite() -> Result<Suite> {
    let contract = Contract::pinned("billing", specs_dir().join("billing-1.0.yaml"))?
        .live_spec("/openapi.json")
        .mode(Compat::Tolerant);

    Ok(Suite::new("billing-rust")
        .contract(contract)
        // Suite fixture with explicit teardown: one login for the whole suite.
        .fixture_with_teardown(
            "token",
            Scope::Suite,
            |cx| async move {
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
            },
            |cx| async move {
                cx.call("billing", "revokeToken")?
                    .send()
                    .await?
                    .expect(204)?;
                Ok(())
            },
        )
        .test_with(
            "create and fetch invoice",
            TestOpts::parallel(),
            |cx| async move {
                let inv = cx
                    .call("billing", "createInvoice")?
                    .json(json!({ "amount": 4200, "currency": "EUR" }))
                    .send()
                    .await?
                    .expect(201)?;
                let id = inv.pointer("/id").cloned().context("no id")?;
                // Cleanup is registered right where the resource is created.
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
                ensure!(
                    got.pointer("/amount") == Some(&json!(4200)),
                    "amount mismatch: {}",
                    got.text
                );
                Ok(())
            },
        )
        .test_with(
            "unknown invoice is 404",
            TestOpts::parallel(),
            |cx| async move {
                cx.call("billing", "getInvoice")?
                    .path_param("id", 999_999)
                    .send()
                    .await?
                    .expect(404)?;
                Ok(())
            },
        )
        .test_with(
            "invalid amount is rejected by the service",
            TestOpts::parallel(),
            |cx| async move {
                // A raw request bypasses the contract layer, which would refuse amount 0 client-side.
                cx.request("POST", "/invoices")
                    .json(json!({ "amount": 0, "currency": "EUR" }))
                    .send()
                    .await?
                    .expect(422)?;
                Ok(())
            },
        )
        // Per-test setup/teardown: the setup's value is visible to the body via cx.get().
        .test_with(
            "per-test setup and teardown",
            TestOpts::parallel()
                .resource("invoices")
                .setup(|cx| async move {
                    let inv = cx
                        .call("billing", "createInvoice")?
                        .json(json!({ "amount": 100, "currency": "USD" }))
                        .send()
                        .await?
                        .expect(201)?;
                    cx.set("invoice_id", inv.pointer("/id"))?;
                    Ok(())
                })
                .teardown(|cx| async move {
                    let id: u64 = cx.get_as("invoice_id")?;
                    cx.call("billing", "deleteInvoice")?
                        .path_param("id", id)
                        .send()
                        .await?
                        .expect(204)?;
                    Ok(())
                }),
            |cx| async move {
                let id: u64 = cx.get_as("invoice_id")?;
                let got = cx
                    .call("billing", "getInvoice")?
                    .path_param("id", id)
                    .send()
                    .await?
                    .expect(200)?;
                ensure!(got.pointer("/currency") == Some(&json!("USD")));
                Ok(())
            },
        )
        // Serial: counts invoices, so nothing else may run at the same time.
        .test_with(
            "listing sees exactly what this test created",
            TestOpts::serial(),
            |cx| async move {
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
            },
        )
        .test_with(
            "slow test with its own timeout",
            TestOpts::parallel().timeout(Duration::from_secs(5)),
            |_cx| async move {
                tokio::time::sleep(Duration::from_millis(50)).await;
                Ok(())
            },
        ))
}
