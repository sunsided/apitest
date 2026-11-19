//! Trace propagation, User-Agent precedence and authentication.

use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use apitest::Response;
use apitest::prelude::*;
use apitest::report::Outcome;
use apitest::script::rhai::load_str as rhai_suites;
use apitest::trace::SpanContext;
use axum::extract::{Form, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response as AxumResponse};
use axum::routing::{get, post};
use axum::{Json, Router};

// ------------------------------------------------------------------- service

#[derive(Default)]
struct Svc {
    tokens: Mutex<HashSet<String>>,
    logins: AtomicU64,
    cc_tokens: AtomicU64,
}

fn headers_json(h: &HeaderMap) -> Value {
    let m: BTreeMap<String, String> = h
        .iter()
        .map(|(k, v)| {
            (
                k.as_str().to_string(),
                v.to_str().unwrap_or_default().to_string(),
            )
        })
        .collect();
    json!(m)
}

async fn echo(h: HeaderMap) -> Json<Value> {
    Json(headers_json(&h))
}

async fn login(State(s): State<Arc<Svc>>, Json(b): Json<Value>) -> AxumResponse {
    // Slow on purpose, so concurrent first requests really race for the login.
    tokio::time::sleep(Duration::from_millis(50)).await;
    if b["password"] != "pw" {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "bad credentials" })),
        )
            .into_response();
    }
    let n = s.logins.fetch_add(1, Ordering::SeqCst) + 1;
    let token = format!("tok-{}-{n}", b["username"].as_str().unwrap_or("?"));
    s.tokens.lock().unwrap().insert(token.clone());
    Json(json!({ "token": token })).into_response()
}

async fn me(State(s): State<Arc<Svc>>, h: HeaderMap) -> AxumResponse {
    let token = h
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    if s.tokens.lock().unwrap().contains(token) {
        Json(json!({ "token": token })).into_response()
    } else {
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "invalid token" })),
        )
            .into_response()
    }
}

async fn revoke_all(State(s): State<Arc<Svc>>) -> StatusCode {
    s.tokens.lock().unwrap().clear();
    StatusCode::NO_CONTENT
}

async fn oauth(State(s): State<Arc<Svc>>, Form(f): Form<BTreeMap<String, String>>) -> AxumResponse {
    if f.get("grant_type").map(String::as_str) != Some("client_credentials")
        || f.get("client_id").map(String::as_str) != Some("svc")
        || f.get("client_secret").map(String::as_str) != Some("s3cret")
    {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "invalid_client" })),
        )
            .into_response();
    }
    let n = s.cc_tokens.fetch_add(1, Ordering::SeqCst) + 1;
    let token = format!("cc-{n}");
    s.tokens.lock().unwrap().insert(token.clone());
    Json(json!({ "access_token": token, "token_type": "bearer", "expires_in": 3, "scope": f.get("scope") })).into_response()
}

async fn stats_handler(State(s): State<Arc<Svc>>) -> Json<Value> {
    Json(
        json!({ "logins": s.logins.load(Ordering::SeqCst), "cc_tokens": s.cc_tokens.load(Ordering::SeqCst) }),
    )
}

async fn start() -> (TestServer, Arc<Svc>) {
    let svc = Arc::new(Svc::default());
    let app = Router::new()
        .route("/echo", get(echo))
        .route("/login", post(login))
        .route("/me", get(me))
        .route("/revoke-all", post(revoke_all))
        .route("/oauth/token", post(oauth))
        .route("/stats", get(stats_handler))
        .with_state(svc.clone());
    (TestServer::axum(app).await.unwrap(), svc)
}

fn cfg(srv: &TestServer) -> RunConfig {
    RunConfig::new(srv.base_url())
        .service("svc", srv.base_url())
        .quiet(true)
}

/// Collects values from inside tests for assertions afterwards.
#[derive(Clone, Default)]
struct Probe(Arc<Mutex<Vec<Value>>>);
impl Probe {
    fn push(&self, v: Value) {
        self.0.lock().unwrap().push(v);
    }
    fn all(&self) -> Vec<Value> {
        self.0.lock().unwrap().clone()
    }
}

async fn echo_record(probe: &Probe, label: &str) -> Result<()> {
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

// ------------------------------------------------------------------- tracing

#[tokio::test(flavor = "multi_thread")]
async fn w3c_traceparent_by_default_one_trace_per_test() {
    let (srv, _) = start().await;
    let probe = Probe::default();
    let (p1, p2) = (probe.clone(), probe.clone());
    let suite = Suite::new("trace")
        .test("first", move |_| {
            let p = p1.clone();
            async move {
                echo_record(&p, "first").await?;
                echo_record(&p, "first").await
            }
        })
        .test("second", move |_| {
            let p = p2.clone();
            async move { echo_record(&p, "second").await }
        });
    run_suite(&suite, &cfg(&srv)).await.assert_passed();

    let rows = probe.all();
    let mut spans = HashSet::new();
    for r in &rows {
        let received = r["received"]["traceparent"].as_str().unwrap();
        let tp = Traceparent::parse(received).unwrap();
        assert!(tp.sampled());
        assert_eq!(
            tp.trace_id, r["test_trace"],
            "header carries the test's trace id"
        );
        assert_eq!(
            received, r["sent"]["traceparent"],
            "resp.request shows what was sent"
        );
        let span: SpanContext = serde_json::from_value(r["span"].clone()).unwrap();
        assert_eq!(
            span.traceparent(),
            received,
            "resp.trace matches the header"
        );
        assert!(spans.insert(tp.span_id), "every request is a new span");
        assert_eq!(
            r["received"]["user-agent"],
            format!("apitest/{}", env!("CARGO_PKG_VERSION"))
        );
    }
    assert_eq!(
        rows[0]["test_trace"], rows[1]["test_trace"],
        "same trace within a test"
    );
    assert_ne!(
        rows[0]["test_trace"], rows[2]["test_trace"],
        "a new trace per test"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn all_formats_carry_the_same_ids() {
    let (srv, _) = start().await;
    let probe = Probe::default();
    let p = probe.clone();
    let suite = Suite::new("formats").test("all", move |_| {
        let p = p.clone();
        async move { echo_record(&p, "all").await }
    });
    let mut c = cfg(&srv);
    c.tracing = TracingConfig {
        formats: vec![
            TraceFormat::W3c,
            TraceFormat::B3,
            TraceFormat::B3multi,
            TraceFormat::Xray,
            TraceFormat::Gcp,
            TraceFormat::Datadog,
        ],
        tracestate: Some("apitest=1".into()),
        test_baggage: true,
        baggage: BTreeMap::from([("env".into(), "ci".into())]),
        ..Default::default()
    };
    run_suite(&suite, &c).await.assert_passed();
    let r = &probe.all()[0]["received"];
    let span: SpanContext = serde_json::from_value(probe.all()[0]["span"].clone()).unwrap();
    let (t, s) = (&span.trace_id, &span.span_id);
    assert_eq!(r["tracestate"], "apitest=1");
    assert_eq!(
        r["baggage"],
        "apitest.suite=formats,apitest.test=all,env=ci"
    );
    assert_eq!(r["b3"], format!("{t}-{s}-1"));
    assert_eq!(r["x-b3-traceid"], *t);
    assert_eq!(r["x-b3-spanid"], *s);
    assert_eq!(
        r["x-amzn-trace-id"],
        format!("Root=1-{}-{};Parent={s};Sampled=1", &t[..8], &t[8..])
    );
    assert_eq!(
        r["x-cloud-trace-context"],
        format!("{t}/{};o=1", u64::from_str_radix(s, 16).unwrap())
    );
    assert_eq!(
        r["x-datadog-trace-id"],
        u64::from_str_radix(&t[16..], 16).unwrap().to_string()
    );
    assert_eq!(r["x-datadog-tags"], format!("_dd.p.tid={}", &t[..16]));
}

#[tokio::test(flavor = "multi_thread")]
async fn tracing_is_opt_out_at_every_level() {
    let (srv, _) = start().await;
    let probe = Probe::default();
    let p = probe.clone();
    let tests = move |suite: Suite| {
        let p = p.clone();
        suite.test("t", move |_| {
            let p = p.clone();
            async move {
                let plain = Actor::on("svc").get("/echo").await?;
                let off = Actor::on("svc").get("/echo").no_trace().await?;
                let forced = Actor::on("svc").get("/echo").trace(true).await?;
                let own = Actor::on("svc")
                    .get("/echo")
                    .header(
                        "traceparent",
                        "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
                    )
                    .await?;
                p.push(json!([
                    plain.body["traceparent"],
                    off.body["traceparent"],
                    forced.body["traceparent"],
                    own.body["traceparent"]
                ]));
                Ok(())
            }
        })
    };
    // Enabled (default), globally disabled, disabled for one suite.
    run_suite(&tests(Suite::new("on")), &cfg(&srv))
        .await
        .assert_passed();
    run_suite(
        &tests(Suite::new("off")),
        &cfg(&srv).tracing(TracingConfig::disabled()),
    )
    .await
    .assert_passed();
    run_suite(&tests(Suite::new("suite-off").no_tracing()), &cfg(&srv))
        .await
        .assert_passed();

    let own = json!("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01");
    for (i, row) in probe.all().iter().enumerate() {
        let enabled = i == 0;
        assert_eq!(row[0].is_string(), enabled, "plain request, run {i}");
        assert!(row[1].is_null(), "no_trace() always suppresses");
        assert!(row[2].is_string(), "trace(true) always sends");
        assert_eq!(row[3], own, "an explicit traceparent is never replaced");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn runs_inside_a_parent_trace() {
    let (srv, _) = start().await;
    let probe = Probe::default();
    let p = probe.clone();
    let suite = Suite::new("child").test("t", move |_| {
        let p = p.clone();
        async move { echo_record(&p, "t").await }
    });
    let mut c = cfg(&srv);
    c.tracing.parent = Some("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01".into());
    run_suite(&suite, &c).await.assert_passed();
    let tp =
        Traceparent::parse(probe.all()[0]["received"]["traceparent"].as_str().unwrap()).unwrap();
    assert_eq!(
        tp.trace_id, "4bf92f3577b34da6a3ce929d0e0e4736",
        "inherits the parent's trace id"
    );
    assert_ne!(tp.span_id, "00f067aa0ba902b7", "but sends its own span");
}

// ---------------------------------------------------------------- user agent

#[tokio::test(flavor = "multi_thread")]
async fn user_agent_precedence() {
    let (srv, _) = start().await;
    let probe = Probe::default();
    let p = probe.clone();
    let suite = Suite::new("ua").user_agent("suite/1").test("t", move |cx| {
        let p = p.clone();
        async move {
            let ua = |r: Response| r.body["user-agent"].clone();
            let suite_level = ua(Actor::on("svc").get("/echo").await?);
            cx.default_header("User-Agent", "scope/1");
            let scope_level = ua(Actor::on("svc").get("/echo").await?);
            let actor_level = ua(Actor::on("svc").user_agent("actor/1").get("/echo").await?);
            let request_level = ua(Actor::on("svc")
                .user_agent("actor/1")
                .get("/echo")
                .user_agent("request/1")
                .await?);
            p.push(json!([
                suite_level,
                scope_level,
                actor_level,
                request_level
            ]));
            Ok(())
        }
    });
    run_suite(&suite, &cfg(&srv).user_agent("global/1"))
        .await
        .assert_passed();
    let global = Suite::new("ua-global").test("t", {
        let p = probe.clone();
        move |_| {
            let p = p.clone();
            async move {
                p.push(Actor::on("svc").get("/echo").await?.body["user-agent"].clone());
                Ok(())
            }
        }
    });
    run_suite(&global, &cfg(&srv).user_agent("global/1"))
        .await
        .assert_passed();
    assert_eq!(
        probe.all(),
        vec![
            json!(["suite/1", "scope/1", "actor/1", "request/1"]),
            json!("global/1")
        ]
    );
}

// ---------------------------------------------------------------------- auth

#[tokio::test(flavor = "multi_thread")]
async fn static_credentials_and_precedence() {
    let (srv, _) = start().await;
    let probe = Probe::default();
    let p = probe.clone();
    let suite = Suite::new("auth").test("t", move |cx| {
        let p = p.clone();
        async move {
            let authz = |r: Response| r.body["authorization"].clone();
            let svc = Actor::on("svc");
            let bearer = authz(svc.clone().bearer("abc").get("/echo").await?);
            let basic = authz(
                svc.clone()
                    .basic("Aladdin", "open sesame")
                    .get("/echo")
                    .await?,
            );
            let key = svc
                .clone()
                .api_key("X-Api-Key", "k-123")
                .get("/echo")
                .await?
                .body["x-api-key"]
                .clone();
            cx.bearer("scope-token");
            let scope = authz(svc.get("/echo").await?);
            let actor_beats_scope = authz(svc.clone().bearer("actor-token").get("/echo").await?);
            let explicit_beats_actor = authz(
                svc.clone()
                    .bearer("actor-token")
                    .get("/echo")
                    .header("Authorization", "Custom xyz")
                    .await?,
            );
            p.push(json!([
                bearer,
                basic,
                key,
                scope,
                actor_beats_scope,
                explicit_beats_actor
            ]));
            Ok(())
        }
    });
    run_suite(&suite, &cfg(&srv)).await.assert_passed();
    assert_eq!(
        probe.all()[0],
        json!([
            "Bearer abc",
            "Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ==",
            "k-123",
            "Bearer scope-token",
            "Bearer actor-token",
            "Custom xyz"
        ])
    );
}

async fn stats(srv: &TestServer) -> Value {
    reqwest::get(format!("{}/stats", srv.base_url()))
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn lazy_login_is_cached_single_flight_and_retried_on_401() {
    let (srv, _) = start().await;
    let suite = Suite::new("login").test("t", |_| async move {
        let alice = Actor::on("svc").login("alice", "pw"); // no request yet
        // Ten concurrent first requests: exactly one login.
        let calls = (0..10).map(|_| alice.get("/me").expect(200));
        for r in futures_join(calls).await {
            r?;
        }
        // The service forgets all tokens; the next request re-logs in transparently.
        Actor::on("svc").post("/revoke-all").expect(204).await?;
        alice.get("/me").expect(200).await?;
        Ok(())
    });
    run_suite(
        &suite,
        &cfg(&srv).login_flow("svc", LoginFlow::path("/login")),
    )
    .await
    .assert_passed();
    assert_eq!(stats(&srv).await["logins"], 2);
}

/// Minimal join_all without pulling in the futures crate.
async fn futures_join<F: std::future::IntoFuture>(fs: impl IntoIterator<Item = F>) -> Vec<F::Output>
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

#[tokio::test(flavor = "multi_thread")]
async fn login_failures_say_who_and_where() {
    let (srv, _) = start().await;
    let suite = Suite::new("login").test("t", |_| async move {
        Actor::on("svc")
            .login("mallory", "wrong")
            .get("/me")
            .await?;
        Ok(())
    });
    let report = run_suite(
        &suite,
        &cfg(&srv).login_flow("svc", LoginFlow::path("/login")),
    )
    .await;
    let Outcome::Failed { message } = &report.tests[0].outcome else {
        panic!("expected failure")
    };
    assert!(
        message.contains("login as 'mallory' on 'svc' failed"),
        "{message}"
    );
    assert!(message.contains("bad credentials"), "{message}");
}

#[tokio::test(flavor = "multi_thread")]
async fn client_credentials_are_cached_until_shortly_before_expiry() {
    let (srv, _) = start().await;
    let token_url = format!("{}/oauth/token", srv.base_url());
    let suite = Suite::new("cc").test("t", move |_| {
        let token_url = token_url.clone();
        async move {
            let service = Actor::on("svc").client_credentials(
                ClientCredentials::new(token_url, "svc", "s3cret").scope("read"),
            );
            service.get("/me").expect(200).await?;
            service.get("/me").expect(200).await?;
            // expires_in = 3 s → refreshed 1 s early.
            tokio::time::sleep(Duration::from_millis(2100)).await;
            service.get("/me").expect(200).await?;
            Ok(())
        }
    });
    run_suite(&suite, &cfg(&srv)).await.assert_passed();
    assert_eq!(stats(&srv).await["cc_tokens"], 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn named_actors_from_config_with_variables() {
    let (srv, _) = start().await;
    let c = cfg(&srv)
        .login_flow("svc", LoginFlow::path("/login"))
        .actor(
            "alice",
            "svc",
            Auth::login("svc", "${ALICE_USER}", "${ALICE_PASSWORD}"),
        )
        .var("ALICE_USER", "alice")
        .var("ALICE_PASSWORD", "pw");
    let suite = Suite::new("named").test("t", |_| async move {
        let me = actor("alice")?.get("/me").expect(200).await?;
        ensure!(
            me["token"]
                .as_str()
                .unwrap_or_default()
                .starts_with("tok-alice-")
        );
        Ok(())
    });
    run_suite(&suite, &c).await.assert_passed();

    // The same actor from a JS suite: `{ as: "alice" }`, plus trace access.
    let js = r#"
        const s = suite("js");
        s.test("as named actor", (cx) => {
          const me = cx.http("GET", "/me", { as: "alice" }).expect(200);
          assert(me.body.token.startsWith("tok-alice-"), "token from the configured login");
          const echo = cx.http("GET", "/echo", { service: "svc" }).expect(200);
          assert(echo.request.headers.traceparent === echo.body.traceparent, "resp.request shows the sent header");
          assert(echo.trace.trace_id === cx.trace().trace_id, "same trace as the test");
          cx.bearer("from-js");
          assertEq(cx.http("GET", "/echo").body.authorization, "Bearer from-js");
        });
    "#;
    for s in apitest::script::js::load_str(js, std::path::Path::new("."), 1).unwrap() {
        run_suite(&s, &c).await.assert_passed();
    }

    // And from Rhai.
    let rhai = r#"
        let s = suite("rhai");
        s.test("trace and auth", |cx| {
            let echo = cx.http("GET", "/echo").expect(200);
            assert_eq(echo.request.headers.traceparent, echo.body.traceparent);
            assert_eq(echo.trace.trace_id, cx.trace().trace_id);
            cx.basic("u", "p");
            assert_eq(cx.http("GET", "/echo").body.authorization, "Basic dTpw");
        });
        s
    "#;
    for s in rhai_suites(rhai, std::path::Path::new(".")).unwrap() {
        run_suite(&s, &c).await.assert_passed();
    }
}
