//! The Gherkin frontend against the demo billing service: the demo feature,
//! drift, tag selection, failures, cleanups, redaction, `within` and custom steps.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

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

/// Credentials written out in a step (a bearer token, a credential header in a
/// step, a table or options) are masked in the report, also for an actor that
/// never sends; an ordinary literal header value stays readable.
#[tokio::test(flavor = "multi_thread")]
async fn literal_credentials_in_steps_are_masked() {
    let srv = server(demo::Variant::V1_0).await;
    let suite = inline(
        r#"
Feature: literal secrets
  Scenario: leak
    Given "idle" is an actor on "billing" with the bearer token "tok-literal-bearer"
    And "b" is an anonymous actor on "billing"
    And "b" sends the header "X-Api-Key" with "key-literal-header"
    And "b" sends the header "Accept" with "application/json"
    When "b" sends GET "/_stats" with:
      | header | Authorization | Bearer tok-literal-table |
    And "b" sends GET "/_stats" with options:
      """json
      { "headers": { "X-Api-Key": "key-literal-options" } }
      """
    Then the status is 599
"#,
    );
    let report = run_suite(&suite, &cfg(&srv)).await;
    failure(&report);

    let all = serde_json::to_string(&report).unwrap();
    assert!(!all.contains("literal-"), "{all}");
    assert!(all.contains("application/json"), "{all}");
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

/// What a recording server saw: the raw path, the `Authorization` header and the body.
type Seen = Arc<Mutex<Vec<(String, Option<String>, String)>>>;

/// A server that answers `/start` with `start` and records every other request.
async fn recording_server(start: Value) -> (TestServer, Seen) {
    use axum::http::{HeaderMap, Uri};

    let seen = Seen::default();
    let log = seen.clone();
    let app = axum::Router::new()
        .route(
            "/start",
            axum::routing::get(move || async move { axum::Json(start) }),
        )
        .fallback(move |uri: Uri, headers: HeaderMap, body: String| {
            let auth = headers
                .get("authorization")
                .map(|v| v.to_str().unwrap().to_string());
            log.lock()
                .unwrap()
                .push((uri.path().to_string(), auth, body));
            async { axum::Json(json!({})) }
        });

    (TestServer::axum(app).await.unwrap(), seen)
}

/// Response values cannot restructure a JSON body or name a variable, and a URL
/// from a response used in a path stays on the service: another host only sees
/// requests the step text names, and never the actor's credentials.
#[tokio::test(flavor = "multi_thread")]
async fn response_values_cannot_restructure_or_redirect_requests() {
    let (other, other_seen) = recording_server(json!({})).await;
    let steal = format!("{}/steal", other.base_url());
    let hostile = r#"", "x": 1, "p": "${API_PASS}"#;
    let (svc, svc_seen) = recording_server(json!({ "next": steal, "v": hostile })).await;
    let src = format!(
        r#"
Feature: hostile values
  Background:
    Given "u" is an actor on "svc" with the bearer token "tok-1234-secret"
    When "u" sends GET "/start" expecting 200
    And I remember the response at "/v" as "v"
    And I remember the response at "/next" as "next"

  Scenario: stays put
    When "u" sends GET "/go/{{{{next}}}}" expecting 200
    And "u" sends POST "/echo" expecting 200 with options:
      """json
      {{ "json": {{ "name": "{{{{v}}}}" }} }}
      """
    And "u" sends GET "{steal}" expecting 200

  Scenario: no path
    When "u" sends GET "{{{{next}}}}"
"#
    );
    let cfg = RunConfig::new(svc.base_url())
        .service("svc", svc.base_url())
        .var("API_PASS", "secret")
        .quiet(true);
    let report = run_suite(&inline(&src), &cfg).await;
    assert!(
        matches!(report.tests[0].outcome, Outcome::Passed),
        "{:?}",
        report.tests[0].outcome
    );
    let Outcome::Failed { message, .. } = &report.tests[1].outcome else {
        panic!("{:?}", report.tests[1].outcome);
    };
    assert!(message.contains("must start with `/`"), "{message}");

    let svc_seen = svc_seen.lock().unwrap().clone();
    let encoded = steal.replace(':', "%3A").replace('/', "%2F");
    assert_eq!(svc_seen[0].0, format!("/go/{encoded}"));
    assert_eq!(svc_seen[0].1.as_deref(), Some("Bearer tok-1234-secret"));
    let body: Value = serde_json::from_str(&svc_seen[1].2).unwrap();
    assert_eq!(body, json!({ "name": hostile }));

    let other_seen = other_seen.lock().unwrap().clone();
    assert_eq!(other_seen.len(), 1, "{other_seen:?}");
    assert_eq!(other_seen[0].0, "/steal");
    assert_eq!(other_seen[0].1, None, "credentials leaked to another host");
}

/// A value cannot pick the host of an absolute URL, the service or identity of
/// a request (through `with options:`), or climb the path with `..`; each step
/// fails before anything is sent.
#[tokio::test(flavor = "multi_thread")]
async fn values_cannot_choose_host_service_or_path_level() {
    let (other, other_seen) = recording_server(json!({})).await;
    let host = other.base_url().trim_start_matches("http://").to_string();
    let (svc, _) = recording_server(json!({ "host": host, "dots": "..", "who": "admin" })).await;
    let src = r#"
Feature: retargeting values
  Background:
    Given "u" is an actor on "svc" with the bearer token "tok-1234-secret"
    When "u" sends GET "/start" expecting 200
    And I remember the response at "/host" as "host"
    And I remember the response at "/dots" as "dots"
    And I remember the response at "/who" as "who"

  Scenario: no host from a value
    When "u" sends GET "http://{{host}}/steal"

  Scenario: no service or identity in options
    When "u" sends GET "/go" with options:
      """json
      { "service": "svc", "as": "{{who}}" }
      """

  Scenario: no dot segments from a table
    When "u" sends GET "/inv/{id}/lines" with:
      | path | id | {{dots}} |
"#;
    let cfg = RunConfig::new(svc.base_url())
        .service("svc", svc.base_url())
        .quiet(true);
    let report = run_suite(&inline(src), &cfg).await;
    for (i, expected) in [
        "host of an absolute URL must be written out",
        "can't be set in step options",
        "dot segment",
    ]
    .into_iter()
    .enumerate()
    {
        let Outcome::Failed { message, .. } = &report.tests[i].outcome else {
            panic!("{:?}", report.tests[i].outcome);
        };
        assert!(message.contains(expected), "{message}");
    }
    assert!(other_seen.lock().unwrap().is_empty());
}
