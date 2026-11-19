//! Behavioural tests of the runner: contract drift, guaranteed cleanup,
//! scheduling, lockfile, reports and load.

#[path = "support/demo_server.rs"]
mod demo;
#[path = "support/rust_suite.rs"]
mod rust_suite;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use apitest::contract::{Lockfile, Severity};
use apitest::load::{Executor, LoadPlan, Scenario, run_load};
use apitest::prelude::*;
use apitest::report::{Outcome, junit_xml};
use apitest::script::{LoadOptions, load_suites};

async fn server(v: demo::Variant) -> TestServer {
    TestServer::axum(demo::app(v)).await.unwrap()
}

fn cfg(srv: &TestServer) -> RunConfig {
    RunConfig::new(srv.base_url())
        .var("API_USER", "demo")
        .var("API_PASS", "secret")
        .quiet(true)
}

async fn stats(srv: &TestServer) -> Value {
    reqwest::get(format!("{}/_stats", srv.base_url()))
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn passes_and_cleans_up_against_pinned_version() {
    let srv = server(demo::Variant::V1_0).await;
    let report = run_suite(&rust_suite::billing_suite().unwrap(), &cfg(&srv)).await;
    report.assert_passed();
    assert_eq!(
        stats(&srv).await,
        json!({ "tokens": 0, "invoices": 0 }),
        "teardown must leave nothing behind"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn compatible_minor_version_passes() {
    let srv = server(demo::Variant::V1_1).await;
    let report = run_suite(&rust_suite::billing_suite().unwrap(), &cfg(&srv)).await;
    report.assert_passed();
    let drift = &report.drift[0];
    assert_eq!(drift.live_version, "1.1.0");
    assert!(!drift.is_breaking());
    assert!(
        drift
            .findings
            .iter()
            .any(|f| f.location.contains("due_date") && f.severity == Severity::Info)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn breaking_major_version_fails_loudly_before_any_test_runs() {
    let srv = server(demo::Variant::V2_0).await;
    let report = run_suite(&rust_suite::billing_suite().unwrap(), &cfg(&srv)).await;
    assert!(!report.passed());
    assert!(
        report
            .setup_error
            .as_deref()
            .unwrap()
            .contains("incompatible")
    );
    assert!(
        report
            .tests
            .iter()
            .all(|t| matches!(t.outcome, Outcome::Skipped { .. }))
    );
    let breaking: Vec<_> = report.drift[0]
        .findings
        .iter()
        .filter(|f| f.severity == Severity::Breaking)
        .map(|f| format!("{} @ {}: {}", f.operation, f.location, f.message))
        .collect();
    println!("{}", report.render());
    assert!(
        breaking
            .iter()
            .any(|f| f.contains("createInvoice @ request.amount_cents")),
        "{breaking:#?}"
    );
    assert!(
        breaking
            .iter()
            .any(|f| f.contains("response 201.amount: property removed")),
        "{breaking:#?}"
    );
    assert!(
        breaking
            .iter()
            .any(|f| f.contains("response 200.id: type changed")),
        "{breaking:#?}"
    );
    // No test ran, so nothing was created.
    assert_eq!(stats(&srv).await, json!({ "tokens": 0, "invoices": 0 }));
}

#[tokio::test(flavor = "multi_thread")]
async fn runtime_validation_catches_drift_when_drift_check_is_off() {
    let srv = server(demo::Variant::V2_0).await;
    let mut c = cfg(&srv);
    c.check_drift = false;
    let suite = rust_suite::billing_suite().unwrap().test(
        "v2 payload against v1 contract",
        |cx| async move {
            // Create with the v2 field name via a raw request, then read it through the pinned contract.
            let raw = cx
                .request("POST", "/invoices")
                .json(json!({ "amount_cents": 5, "currency": "EUR" }))
                .send()
                .await?
                .expect(201)?;
            let id = raw.pointer("/id").cloned().unwrap();
            let cleanup_id = id.clone();
            cx.defer("delete", move |cx| async move {
                cx.request("DELETE", "/invoices/{id}")
                    .path_param("id", cleanup_id)
                    .send()
                    .await?
                    .expect(204)?;
                Ok(())
            });
            cx.call("billing", "getInvoice")?
                .path_param("id", id)
                .send()
                .await?;
            Ok(())
        },
    );
    let report = run_suite(&suite, &c).await;
    let t = report
        .tests
        .iter()
        .find(|t| t.name == "v2 payload against v1 contract")
        .unwrap();
    let Outcome::Failed { message } = &t.outcome else {
        panic!("expected failure, got {:?}", t.outcome)
    };
    println!("{message}");
    assert!(
        message.contains("contract violation: response 200 of getInvoice"),
        "{message}"
    );
    assert!(message.contains("/id"), "{message}");
    assert_eq!(stats(&srv).await, json!({ "tokens": 0, "invoices": 0 }));
}

#[tokio::test(flavor = "multi_thread")]
async fn cleanups_run_after_failures_panics_and_timeouts() {
    let srv = server(demo::Variant::V1_0).await;
    let create = |cx: Ctx| async move {
        let inv = cx
            .call("billing", "createInvoice")?
            .json(json!({ "amount": 7, "currency": "EUR" }))
            .send()
            .await?
            .expect(201)?;
        let id = inv.pointer("/id").cloned().unwrap();
        cx.defer("delete", move |cx| async move {
            cx.call("billing", "deleteInvoice")?
                .path_param("id", id)
                .send()
                .await?
                .expect(204)?;
            Ok(())
        });
        anyhow::Ok(())
    };
    let suite = Suite::new("cleanup")
        .pinned("billing", rust_suite::specs_dir().join("billing-1.0.yaml"))
        .unwrap()
        .fixture_with_teardown(
            "token",
            Scope::Suite,
            |cx| async move {
                let t = cx
                    .call("billing", "issueToken")?
                    .form(&[("user", "demo"), ("pass", "secret")])
                    .send()
                    .await?
                    .expect(200)?;
                let tok = t
                    .pointer("/access_token")
                    .and_then(Value::as_str)
                    .unwrap()
                    .to_string();
                cx.default_header("Authorization", format!("Bearer {tok}"));
                Ok(tok)
            },
            |cx| async move {
                cx.call("billing", "revokeToken")?
                    .send()
                    .await?
                    .expect(204)?;
                Ok(())
            },
        )
        .test("fails", move |cx| async move {
            create(cx).await?;
            bail!("boom")
        })
        .test("panics", move |cx| async move {
            create(cx).await?;
            panic!("kaboom")
        })
        .test_with(
            "times out",
            TestOpts::serial().timeout(Duration::from_millis(200)),
            move |cx| async move {
                create(cx).await?;
                tokio::time::sleep(Duration::from_secs(10)).await;
                Ok(())
            },
        );
    let report = run_suite(&suite, &cfg(&srv)).await;
    let msgs: Vec<String> = report
        .tests
        .iter()
        .map(|t| match &t.outcome {
            Outcome::Failed { message } => message.clone(),
            o => format!("{o:?}"),
        })
        .collect();
    assert!(msgs[0].contains("boom"));
    assert!(msgs[1].contains("panicked: kaboom"));
    assert!(msgs[2].contains("timed out"));
    assert!(
        report.tests.iter().all(|t| t.teardown_errors.is_empty()),
        "{}",
        report.render()
    );
    assert_eq!(stats(&srv).await, json!({ "tokens": 0, "invoices": 0 }));
}

#[tokio::test(flavor = "multi_thread")]
async fn serial_tests_run_alone_and_parallel_tests_overlap() {
    let running = Arc::new(AtomicUsize::new(0));
    let max_seen = Arc::new(AtomicUsize::new(0));
    let serial_saw_others = Arc::new(AtomicUsize::new(0));
    let mut suite = Suite::new("sched");
    for i in 0..4 {
        let (r, m) = (running.clone(), max_seen.clone());
        suite = suite.test_with(&format!("parallel {i}"), TestOpts::parallel(), move |_| {
            let (r, m) = (r.clone(), m.clone());
            async move {
                let now = r.fetch_add(1, Ordering::SeqCst) + 1;
                m.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(150)).await;
                r.fetch_sub(1, Ordering::SeqCst);
                Ok(())
            }
        });
    }
    let (r, s) = (running.clone(), serial_saw_others.clone());
    suite = suite.test_with("serial", TestOpts::serial(), move |_| {
        let (r, s) = (r.clone(), s.clone());
        async move {
            s.store(r.load(Ordering::SeqCst), Ordering::SeqCst);
            Ok(())
        }
    });
    // Two parallel tests sharing a resource must not overlap.
    let shared = Arc::new(AtomicUsize::new(0));
    let overlap = Arc::new(AtomicUsize::new(0));
    for i in 0..2 {
        let (sh, ov) = (shared.clone(), overlap.clone());
        suite = suite.test_with(
            &format!("db {i}"),
            TestOpts::parallel().resource("db"),
            move |_| {
                let (sh, ov) = (sh.clone(), ov.clone());
                async move {
                    if sh.fetch_add(1, Ordering::SeqCst) > 0 {
                        ov.fetch_add(1, Ordering::SeqCst);
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    sh.fetch_sub(1, Ordering::SeqCst);
                    Ok(())
                }
            },
        );
    }
    let mut c = RunConfig::new("http://127.0.0.1:9");
    c.max_parallel = 8;
    let started = Instant::now();
    let report = run_suite(&suite, &c).await;
    report.assert_passed();
    assert_eq!(
        max_seen.load(Ordering::SeqCst),
        4,
        "parallel tests should overlap"
    );
    assert!(
        started.elapsed() < Duration::from_millis(550),
        "took {:?}",
        started.elapsed()
    );
    assert_eq!(
        serial_saw_others.load(Ordering::SeqCst),
        0,
        "serial test must run alone"
    );
    assert_eq!(
        overlap.load(Ordering::SeqCst),
        0,
        "resource lock must serialize"
    );
    // Report order follows declaration order, not completion order.
    assert_eq!(report.tests[4].name, "serial");
}

#[tokio::test(flavor = "multi_thread")]
async fn lockfile_records_used_surface_and_narrows_drift_checks() {
    let dir = std::env::temp_dir().join(format!("apitest-lock-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let lock = dir.join("apitest.lock.nova");
    let _ = std::fs::remove_file(&lock);

    let srv = server(demo::Variant::V1_0).await;
    let c = cfg(&srv).lockfile(&lock).update_lock(true);
    run_suite(&rust_suite::billing_suite().unwrap(), &c)
        .await
        .assert_passed();

    let text = std::fs::read_to_string(&lock).unwrap();
    println!("{text}");
    let parsed = Lockfile::load(&lock).unwrap().unwrap();
    let used = &parsed.contract["billing"].used;
    let create = used.iter().find(|u| u.op == "createInvoice").unwrap();
    assert!(create.request_fields.contains("amount"));
    assert!(create.statuses.contains(&201));
    assert!(used.iter().all(|u| u.op != "payInvoice"));

    // With the lockfile, only used operations are checked against the live spec.
    let srv11 = server(demo::Variant::V1_1).await;
    let report = run_suite(
        &rust_suite::billing_suite().unwrap(),
        &cfg(&srv11).lockfile(&lock),
    )
    .await;
    report.assert_passed();
    assert!(
        report.drift[0].scope.contains("from lockfile"),
        "{}",
        report.drift[0].scope
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn utoipa_style_in_process_live_document() {
    let srv = server(demo::Variant::V1_0).await;
    // With utoipa this would be `serde_json::to_value(ApiDoc::openapi())?`.
    let doc: Value = serde_yaml_ng::from_str(
        &std::fs::read_to_string(rust_suite::specs_dir().join("billing-2.0.yaml")).unwrap(),
    )
    .unwrap();
    let mut suite = rust_suite::billing_suite().unwrap();
    let pinned =
        Contract::pinned("billing", rust_suite::specs_dir().join("billing-1.0.yaml")).unwrap();
    suite.contracts = vec![Arc::new(pinned.live_document(doc))];
    let report = run_suite(&suite, &cfg(&srv)).await;
    assert!(report.drift[0].is_breaking());
    assert_eq!(report.drift[0].source, "in-process document");
}

#[tokio::test(flavor = "multi_thread")]
async fn junit_report_is_well_formed_enough() {
    let srv = server(demo::Variant::V2_0).await;
    let report = run_suite(&rust_suite::billing_suite().unwrap(), &cfg(&srv)).await;
    let xml = junit_xml(&[report]);
    assert!(xml.starts_with("<?xml"));
    assert!(xml.contains(r#"name="[contract] billing""#));
    assert!(xml.contains(r#"<failure type="ContractDrift""#));
    assert!(xml.contains("<skipped"));
}

#[tokio::test(flavor = "multi_thread")]
async fn script_suites_pass_and_clean_up() {
    for file in ["billing.rhai", "billing.js"] {
        let srv = server(demo::Variant::V1_0).await;
        let suites = load_suites(
            &rust_suite::suites_dir().join(file),
            &LoadOptions { js_workers: 2 },
        )
        .unwrap();
        for s in &suites {
            run_suite(s, &cfg(&srv)).await.assert_passed();
        }
        assert_eq!(
            stats(&srv).await,
            json!({ "tokens": 0, "invoices": 0 }),
            "{file}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn load_reuses_functional_tests() {
    let srv = server(demo::Variant::V1_0).await;
    let plan = LoadPlan::new(rust_suite::billing_suite().unwrap())
        .scenario(
            Scenario::new(
                "create and fetch invoice",
                Executor::ConstantVus {
                    vus: 4,
                    duration: Duration::from_millis(800),
                },
            )
            .threshold("iterations.error_rate == 0")
            .unwrap()
            .threshold("http.p95 < 500ms")
            .unwrap(),
        )
        .scenario(Scenario::new(
            "unknown invoice is 404",
            Executor::Iterations {
                vus: 2,
                iterations: 20,
            },
        ));
    let mut c = cfg(&srv);
    c.validate_every = 10;
    let report = run_load(&plan, &c).await;
    println!("{}", report.render());
    assert!(report.passed(), "{}", report.render());
    assert!(report.scenarios[0].iterations.count > 10);
    assert_eq!(report.scenarios[1].iterations.count, 20);
    assert_eq!(stats(&srv).await, json!({ "tokens": 0, "invoices": 0 }));
}

#[tokio::test(flavor = "multi_thread")]
async fn js_load_with_one_runtime_per_vu_and_vu_scoped_login() {
    let srv = server(demo::Variant::V1_0).await;
    let suite = load_suites(
        &rust_suite::suites_dir().join("billing.js"),
        &LoadOptions { js_workers: 4 },
    )
    .unwrap()
    .remove(0);
    let plan = LoadPlan::new(suite).scenario(
        Scenario::new(
            "create and fetch invoice",
            Executor::RampingVus {
                stages: vec![
                    Stage {
                        duration: Duration::from_millis(300),
                        target: 4,
                    },
                    Stage {
                        duration: Duration::from_millis(300),
                        target: 0,
                    },
                ],
            },
        )
        .threshold("iterations.error_rate < 1%")
        .unwrap(),
    );
    let report = run_load(&plan, &cfg(&srv)).await;
    println!("{}", report.render());
    assert!(report.passed(), "{}", report.render());
    assert_eq!(stats(&srv).await, json!({ "tokens": 0, "invoices": 0 }));
}
