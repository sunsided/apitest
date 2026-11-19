//! The Gherkin frontend against the demo billing service: the demo feature,
//! drift, tag selection, failures, cleanups, redaction, `within` and custom steps.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use apitest::SuiteReport;
use apitest::cucumber::{FeatureOptions, Steps, load, load_str};
use apitest::prelude::*;
use apitest::report::{Outcome, junit_xml};
use apitest::script::{LoadOptions, load_suites};

use crate::demo;

/// Starts the billing service in the given version.
async fn server(v: demo::Variant) -> TestServer {
    TestServer::axum(demo::app(v)).await.unwrap()
}

/// A quiet run config against `srv` with the demo credentials as variables.
fn cfg(srv: &TestServer) -> RunConfig {
    RunConfig::new(srv.base_url())
        .var("API_USER", "demo")
        .var("API_PASS", "secret")
        .quiet(true)
}

/// The service's leftover counters (`{"tokens": n, "invoices": n}`).
async fn stats(srv: &TestServer) -> Value {
    reqwest::get(format!("{}/_stats", srv.base_url()))
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

/// `demo/suites/billing.feature`.
fn feature() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../demo/suites/billing.feature")
}

/// The pinned billing spec, for inline features.
fn billing_spec() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../demo/specs/billing-1.0.yaml")
}

/// Compiles inline feature source with the built-in steps.
fn inline(src: &str) -> Suite {
    load_str(
        src,
        Path::new("inline.feature"),
        &FeatureOptions::default(),
        Arc::new(Steps::builtin()),
    )
    .unwrap()
}

/// The failure message of the only test of `report`.
fn failure(report: &SuiteReport) -> &str {
    match &report.tests[0].outcome {
        Outcome::Failed { message, .. } => message,
        other => panic!("expected a failure, got {other:?}"),
    }
}

/// The demo feature passes, reports its Gherkin steps as written and leaves nothing behind.
#[tokio::test(flavor = "multi_thread")]
async fn demo_feature_passes_and_cleans_up() {
    let srv = server(demo::Variant::V1_0).await;
    let suites = load_suites(&feature(), &LoadOptions::default()).unwrap();
    assert_eq!(suites.len(), 1);
    let report = run_suite(&suites[0], &cfg(&srv)).await;
    report.assert_passed();
    let names: Vec<&str> = report.tests.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "create and fetch invoice",
            "unknown invoice is 404 (id=999999, status=404)",
            "listing sees exactly what this test created",
        ]
    );

    let steps: Vec<&str> = report.tests[0]
        .steps
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    assert!(
        steps.contains(&"And \"anon\" calls \"issueToken\" expecting 200 with:"),
        "{steps:?}"
    );
    assert!(steps.contains(&"Then the status is 200"), "{steps:?}");
    assert_eq!(
        stats(&srv).await,
        json!({ "tokens": 0, "invoices": 0 }),
        "afterwards-steps must leave nothing behind"
    );
}

/// A compatible minor version passes too.
#[tokio::test(flavor = "multi_thread")]
async fn compatible_minor_version_passes() {
    let srv = server(demo::Variant::V1_1).await;
    let suite = load(&feature(), &FeatureOptions::default())
        .unwrap()
        .remove(0);
    run_suite(&suite, &cfg(&srv)).await.assert_passed();
}

/// Breaking drift skips every scenario and shows up as a contract failure.
#[tokio::test(flavor = "multi_thread")]
async fn breaking_drift_skips_every_scenario() {
    let srv = server(demo::Variant::V2_0).await;
    let suite = load(&feature(), &FeatureOptions::default())
        .unwrap()
        .remove(0);
    let report = run_suite(&suite, &cfg(&srv)).await;
    assert!(!report.passed());
    assert!(report.drift[0].is_breaking());
    assert!(
        report
            .tests
            .iter()
            .all(|t| matches!(t.outcome, Outcome::Skipped { .. }))
    );
    let xml = junit_xml(&[report]);
    assert!(xml.contains(r#"name="[contract] billing""#), "{xml}");
}

/// `LoadOptions::tags` selects scenarios by tag expression.
#[tokio::test(flavor = "multi_thread")]
async fn tag_expressions_select_scenarios() {
    let srv = server(demo::Variant::V1_0).await;
    let opts = LoadOptions {
        tags: Some("@serial".into()),
        ..Default::default()
    };
    let suite = load_suites(&feature(), &opts).unwrap().remove(0);
    let names: Vec<&str> = suite.tests.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, vec!["listing sees exactly what this test created"]);
    run_suite(&suite, &cfg(&srv)).await.assert_passed();

    let bad = LoadOptions {
        tags: Some("@a not @b".into()),
        ..Default::default()
    };
    assert!(load_suites(&feature(), &bad).is_err());
}

/// A failing step fails the scenario with its location, and the cleanups
/// registered so far still run.
#[tokio::test(flavor = "multi_thread")]
async fn failure_names_the_step_and_still_cleans_up() {
    let srv = server(demo::Variant::V1_0).await;
    let src = format!(
        r#"@contract:billing={}
Feature: failing
  Scenario: wrong amount
    Given "anon" is an anonymous actor on "billing"
    And "anon" calls "issueToken" expecting 200 with:
      | form | user | ${{API_USER}} |
      | form | pass | ${{API_PASS}} |
    And "user" is an actor on "billing" with the bearer token "{{{{response:/access_token}}}}"
    And afterwards "user" calls "revokeToken" expecting 204
    When "user" calls "createInvoice" expecting 201 with the JSON body:
      """
      {{ "amount": 5, "currency": "EUR" }}
      """
    And afterwards "user" calls "deleteInvoice" expecting 204 with:
      | path | id | {{{{response:/id}}}} |
    Then the response at "/amount" is 6
    And the status is 201
"#,
        billing_spec().display()
    );
    let report = run_suite(&inline(&src), &cfg(&srv)).await;
    let message = failure(&report);
    assert!(
        message.contains("step 'Then the response at \"/amount\" is 6' failed"),
        "{message}"
    );
    assert!(message.contains("inline.feature:16"), "{message}");
    assert!(message.contains("expected 6, got 5"), "{message}");
    assert_eq!(stats(&srv).await, json!({ "tokens": 0, "invoices": 0 }));
}

/// Unresolved variables are errors, not empty strings.
#[tokio::test(flavor = "multi_thread")]
async fn missing_variables_fail() {
    let srv = server(demo::Variant::V1_0).await;
    let suite = inline(
        r#"
Feature: vars
  Scenario: missing
    Given "anon" is an anonymous actor on "billing"
    When "anon" sends GET "/invoices/${NO_SUCH_VAR}"
"#,
    );
    let report = run_suite(&suite, &cfg(&srv)).await;
    let message = failure(&report);
    assert!(
        message.contains("variable 'NO_SUCH_VAR' is not set"),
        "{message}"
    );
}

/// A registered secret that ends up in a failing step's message, its log or
/// its step trail is masked.
#[tokio::test(flavor = "multi_thread")]
async fn step_errors_are_redacted() {
    let srv = server(demo::Variant::V1_0).await;
    let suite = inline(
        r#"
Feature: secrets
  Scenario: leak
    Given "anon" is an anonymous actor on "billing"
    When "anon" sends GET "/_stats"
    Then the response at "/tokens" is "${TOKEN}"
"#,
    );
    let cfg = cfg(&srv).var("TOKEN", "tok-hunter2-secret");
    cfg.redactor.secret("tok-hunter2-secret");
    let report = run_suite(&suite, &cfg).await;
    let message = failure(&report);
    assert!(message.contains("expected \"***\""), "{message}");

    let all = serde_json::to_string(&report).unwrap();
    assert!(!all.contains("hunter2"), "{all}");
}

/// `within …,` re-sends the latest request until the check passes, and custom
/// Rust steps sit next to the built-in ones.
#[tokio::test(flavor = "multi_thread")]
async fn within_retries_and_custom_steps() {
    use axum::routing::get;

    let calls = Arc::new(AtomicUsize::new(0));
    let c = calls.clone();
    let app = axum::Router::new().route(
        "/job",
        get(move || {
            let n = c.fetch_add(1, Ordering::SeqCst);
            async move { axum::Json(json!({ "state": if n < 3 { "running" } else { "done" } })) }
        }),
    );
    let srv = TestServer::axum(app).await.unwrap();
    let steps = Steps::builtin()
        .step("the job state is final", |w, _a| async move {
            let state: String = w.response()?.get("/state")?;
            ensure!(
                ["done", "failed"].contains(&state.as_str()),
                "job is still {state}"
            );
            Ok(())
        })
        .unwrap();
    let src = r#"
Feature: waiting
  Scenario: job finishes
    Given "anon" is an anonymous actor on "svc"
    When "anon" sends GET "/job" expecting 200
    Then within 5 seconds, the response at "/state" is "done"
    And the job state is final
"#;
    let suite = load_str(
        src,
        Path::new("w.feature"),
        &FeatureOptions::default(),
        Arc::new(steps),
    )
    .unwrap();
    let cfg = RunConfig::new(srv.base_url())
        .service("svc", srv.base_url())
        .quiet(true);
    run_suite(&suite, &cfg).await.assert_passed();
    assert_eq!(calls.load(Ordering::SeqCst), 4);
}
