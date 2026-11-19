//! miette for the terminal, plain text everywhere else.
//!
//! A failed login inside a step gives a three-level cause chain. The terminal
//! rendering shows it as a miette diagnostic; the plain summary, `JUnit` XML and
//! JSON carry the same text without colours or miette glyphs.

use apitest::prelude::*;
use apitest::report::{Outcome, SuiteReport, junit_xml};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};

/// miette's error mark, cause arrows and the ANSI escape introducer.
const GLYPHS: [&str; 4] = ["×", "├─▶", "╰─▶", "\x1b["];

/// Runs a one-test suite whose step logs in as `mallory` against a service that
/// rejects every login with `401`.
async fn failed_login() -> SuiteReport {
    let app = Router::new()
        .route(
            "/login",
            post(|| async {
                (
                    StatusCode::UNAUTHORIZED,
                    Json(json!({ "error": "bad credentials" })),
                )
                    .into_response()
            }),
        )
        .route("/me", get(|| async { "never reached" }));
    let srv = TestServer::axum(app).await.unwrap();
    let suite = Suite::new("diag").test("sign in", |_| async move {
        step("as mallory", async {
            Actor::on("svc").login("mallory", "wrong").get("/me").await
        })
        .await?;
        Ok(())
    });
    let cfg = RunConfig::new(srv.base_url())
        .service("svc", srv.base_url())
        .quiet(true)
        .login_flow("svc", LoginFlow::path("/login"));

    run_suite(&suite, &cfg).await
}

/// Fails if `text` (described as `what`) contains any miette glyph or colour.
fn assert_plain(what: &str, text: &str) {
    for g in GLYPHS {
        assert!(!text.contains(g), "{what} contains {g:?}:\n{text}");
    }
}

/// The terminal rendering shows the whole cause chain, coloured only on request.
#[tokio::test]
async fn terminal_output_is_a_miette_diagnostic() {
    let report = failed_login().await;
    let out = report.render_terminal(false);
    assert!(out.contains("× step 'as mallory' failed"), "{out}");
    assert!(
        out.contains("├─▶ login as 'mallory' on 'svc' failed"),
        "{out}"
    );
    assert!(out.contains("╰─▶ "), "{out}");
    assert!(out.contains("bad credentials"), "{out}");
    assert!(!out.contains('\x1b'), "no colours unless asked:\n{out}");
    assert!(report.render_terminal(true).contains("\x1b["));
}

/// `render`, `JUnit` and JSON carry the joined message, and the chain is not serialized.
#[tokio::test]
async fn reports_stay_plain_text() {
    let report = failed_login().await;
    let Outcome::Failed { message, chain } = &report.tests[0].outcome else {
        panic!("expected failure")
    };
    assert!(chain.len() >= 3, "{chain:?}");
    assert_eq!(message, &chain.join(": "));

    let plain = report.render();
    assert_plain("render()", &plain);
    assert!(plain.contains(message.as_str()), "{plain}");

    let xml = junit_xml(std::slice::from_ref(&report));
    assert_plain("JUnit", &xml);
    assert!(xml.contains("login as &apos;mallory&apos; on &apos;svc&apos; failed"));

    let json = serde_json::to_value(&report).unwrap();
    assert_eq!(
        json["tests"][0]["outcome"],
        json!({ "status": "failed", "message": message }),
        "the chain is terminal-only"
    );
}

/// The panic message of `assert_passed` is the plain summary.
#[tokio::test]
async fn assert_passed_panics_with_plain_text() {
    let report = failed_login().await;
    let panic = std::panic::catch_unwind(|| report.assert_passed()).unwrap_err();
    let msg = panic.downcast_ref::<String>().cloned().unwrap_or_default();
    assert!(msg.contains("login as 'mallory' on 'svc' failed"), "{msg}");
    assert_plain("assert_passed", &msg);
}

/// A failing suite fixture keeps its cause chain for the terminal, but not in JSON.
#[tokio::test]
async fn suite_setup_failures_keep_their_chain() {
    let suite = Suite::new("setup")
        .fixture("db", Scope::Suite, |_| async {
            Err::<(), _>(anyhow!("connection refused")).context("connecting to postgres")
        })
        .test("t", |_| async { Ok(()) });
    let report = run_suite(&suite, &RunConfig::new("http://127.0.0.1:9").quiet(true)).await;

    let out = report.render_terminal(false);
    assert!(out.contains("× fixture 'db' (suite) setup"), "{out}");
    assert!(out.contains("├─▶ connecting to postgres"), "{out}");
    assert!(out.contains("╰─▶ connection refused"), "{out}");

    let plain = report.render();
    assert_plain("render()", &plain);
    assert!(
        plain.contains(
            "setup failed: fixture 'db' (suite) setup: connecting to postgres: connection refused"
        ),
        "{plain}"
    );
    assert!(
        serde_json::to_value(&report)
            .unwrap()
            .get("setup_chain")
            .is_none()
    );
}

/// A hand-built chain that disagrees with the message is ignored, so the terminal
/// never shows different text than `JUnit`.
#[tokio::test]
async fn a_chain_that_disagrees_with_the_message_is_ignored() {
    let mut report = failed_login().await;
    report.tests[0].outcome = Outcome::Failed {
        message: "what JUnit says".into(),
        chain: vec!["something else".into()],
    };
    let out = report.render_terminal(false);
    assert!(out.contains("× what JUnit says"), "{out}");
    assert!(!out.contains("something else"), "{out}");
}
