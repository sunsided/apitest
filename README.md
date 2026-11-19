# apitest

[![CI](https://github.com/sunsided/apitest/actions/workflows/ci.yml/badge.svg)](https://github.com/sunsided/apitest/actions/workflows/ci.yml)
[![codecov](https://codecov.io/gh/sunsided/apitest/graph/badge.svg)](https://codecov.io/gh/sunsided/apitest)
[![License](https://img.shields.io/badge/license-EUPL--1.2%20OR%20MIT%20OR%20Apache--2.0-blue)](#license)

| Crate | |
|---|---|
| [`apitest`](https://crates.io/crates/apitest) | [![crates.io](https://img.shields.io/crates/v/apitest.svg)](https://crates.io/crates/apitest) [![docs.rs](https://img.shields.io/docsrs/apitest)](https://docs.rs/apitest) |
| [`apitest-macros`](https://crates.io/crates/apitest-macros) | [![crates.io](https://img.shields.io/crates/v/apitest-macros.svg)](https://crates.io/crates/apitest-macros) |
| [`apitest-cli`](https://crates.io/crates/apitest-cli) | [![crates.io](https://img.shields.io/crates/v/apitest-cli.svg)](https://crates.io/crates/apitest-cli) |

API testing and load testing in Rust, with:

- **Reliable setup/teardown at every level.** Suite, virtual user, test, fixture and per-test hooks, plus `cx.defer(...)` cleanups. Cleanups run in LIFO order **even when a test fails, panics or times out**.
- **Contract pinning.** Suites bind to a checked-in OpenAPI document via `operationId`. Requests and responses are validated against it. Before any test runs, the service's *live* spec is diffed against the pin, limited to the operations and fields the suites actually use (recorded in `apitest.lock.nova`). Instead of, or besides, the live spec you can pick named comparison specs (files or URLs, e.g. what dev, QA and prod run, or your local branch), so the check doesn't need a running service. Incompatible changes fail loudly, reported per target.
- **One core, four frontends.** Suites can be written in **Rust, Rhai, JavaScript (QuickJS) or Python**. Every frontend gets the same `cx` API and the same semantics. Fixture values are JSON, so a Rust fixture can feed a JS test.
- **Functional and load runs from the same tests.** Any test can be a load scenario: constant VUs, a shared iteration budget, or ramping stages. You get HDR latency histograms and k6-style thresholds (`http.p95 < 300ms`).
- **Library or standalone.** Use it from `cargo test` (libtest-mimic harness, with `#[apitest::test]` attribute macros if you like), with nextest (process-per-test aware), from your own code, from pytest, via the `apitest` CLI, or via `npx @apitest/cli`.
- **In-process servers over real TCP.** `TestServer::axum(router)` (or `TestServer::spawn` for any framework) serves your app on an ephemeral port. Tests go through the whole ingress path: socket, HTTP, middleware, routing, extractors. Handlers are never called directly.
- **Standard reports.** JUnit XML (GitHub, GitLab, Jenkins, IDEs) and JSON. Contract drift and suite setup/teardown failures show up as their own test cases.
- **NOVA for config.** `apitest.nova` (project config) and `apitest.lock.nova` use [NOVA](https://novafile.dev/), with schemas in [`crates/apitest/schemas/`](https://github.com/sunsided/apitest/tree/main/crates/apitest/schemas) (`apitest-config.novs`, `apitest-lock.novs`). Both files are validated against them on load, with every issue reported by path. OpenAPI documents stay YAML/JSON because that's their standard format.

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->

- [Layout](#layout)
- [Lifecycle](#lifecycle)
- [Writing tests](#writing-tests)
- [Authentication](#authentication)
- [Tracing and headers](#tracing-and-headers)
- [The same suite in four languages](#the-same-suite-in-four-languages)
  - [Rust suite](#rust-suite)
  - [Rhai suite](#rhai-suite)
  - [JavaScript suite](#javascript-suite)
  - [Python suite](#python-suite)
  - [Per-test setup and teardown](#per-test-setup-and-teardown)
- [Contract pinning](#contract-pinning)
- [Using it as a library (axum example)](#using-it-as-a-library-axum-example)
  - [Native-feeling Rust tests: attribute macros](#native-feeling-rust-tests-attribute-macros)
  - [nextest](#nextest)
- [CLI](#cli)
  - [Terminal diagnostics](#terminal-diagnostics)
  - [One-off requests: `apitest call`](#one-off-requests-apitest-call)
- [Load testing](#load-testing)
- [Python bindings](#python-bindings)
- [Tests](#tests)
- [npm](#npm)
- [Open questions / next steps](#open-questions--next-steps)
- [License](#license)

<!-- END doctoc -->

## Layout

Four crates: the library, its proc macros, the CLI and the Python bindings. Library users don't pull in CLI dependencies. The npm package (`npm/`) wraps the CLI binary.

```
crates/apitest      the library
  features: load, axum (default) · rhai, js, harness, macros, diagnostics (opt-in) · full (all of them)
            --no-default-features = core only (runner, contracts, HTTP, reports, NOVA config)
  src/context.rs    scope chain (run → suite → vu → test), values, default headers, defer/unwind, guarded execution
  src/suite.rs      Suite / Fixture / TestCase / TestOpts (concurrency, resources, timeout, setup, teardown)
  src/runner.rs     drift check → setup → serial/parallel scheduler → teardown → lockfile
  src/contract.rs   OpenAPI loading ($ref inlining, 3.0 `nullable`), validation, usage recording, drift diff, lockfile
  src/http.rs       language-neutral RequestOpts / Response, contract-aware send
  src/load.rs       executors, metrics, thresholds
  src/script/       SyncCtx (blocking facade) + rhai.rs + js.rs
  src/serve.rs      TestServer (in-process, real TCP)
  src/harness.rs    cargo test / nextest integration
  src/registry.rs   collects #[apitest::test|fixture|suite] registrations (linkme)
  src/report.rs     console, JUnit XML
  src/diagnostic.rs miette rendering for the terminal (feature diagnostics)
  src/config.rs     apitest.nova
crates/apitest-macros  #[apitest::test], #[apitest::fixture], #[apitest::suite]
crates/apitest-cli  the `apitest` binary (clap); depends on apitest with rhai, js, load, diagnostics
crates/apitest-py   pyo3 extension (`apitest._native`) + the `apitest` Python package (maturin)
npm/                @apitest/cli: launcher + per-platform binary packages + TypeScript types for JS suites
demo/               billing service specs (v1.0 pinned, v1.1 compatible, v2.0 breaking), suites in Rhai/JS, apitest.nova
```

`apitest`, `apitest-macros` and `apitest-cli` publish to crates.io with one shared version. The Python bindings build in CI but are not on PyPI yet.

As a library you'd typically add it as a dev-dependency:

```toml
[dev-dependencies]
apitest = { version = "…", features = ["harness", "macros"] }   # + "rhai" / "js" for script suites
```

`harness` brings in clap transitively (libtest-mimic parses `cargo test` arguments with it). That's why it's opt-in and belongs in `[dev-dependencies]`.

## Lifecycle

```text
contract drift check (live spec vs pinned, limited to the used surface)
suite fixtures (setup) → before_all / setup
  vu fixtures                                   (one VU in functional runs, N under load)
    per test:
      test fixtures → before_each → test setup → body
      → test teardown → after_each → test cleanups + fixture teardowns (LIFO)
  vu cleanups
after_all / teardown → suite cleanups + fixture teardowns (LIFO)
```

- Teardown steps run even if their setup counterpart, the body or another teardown failed. A fixture's teardown is registered only after its setup succeeded.
- A failed cleanup fails the test (`passed with cleanup errors` is not a pass).
- Bodies run on their own task with a timeout, so a panic or hang becomes a reported failure and the cleanups still run.

**Concurrency.** Tests are `serial` by default: a serial test waits for running tests to finish and runs alone. `parallel` tests overlap up to `max_parallel`. Parallel tests that name the same **resource** (`resources: ["db"]`) never overlap. The report always lists tests in declaration order.

## Writing tests

The regular case should read like the scenario it tests. Here is a multi-actor, multi-service flow, condensed from [`crates/apitest/tests/care_flow.rs`](https://github.com/sunsided/apitest/blob/main/crates/apitest/tests/care_flow.rs):

1. An existing Hub admin creates a Social Service user in the global setup.
2. A patient registers on the app as a demo user and confirms their e-mail by opening the link from the mail mock.
3. The patient creates an activation code.
4. The social worker uses the code to identify the patient, records therapy data and moves the case to "in progress".
5. Everything is rolled back at the end.

```rust
/// Global setup: the admin creates a Social Service user, removed after the suite.
#[apitest::fixture(scope = "suite")]
async fn social_worker(hub_admin: Actor) -> Result<Actor> {
    let email = unique_email("social");

    hub_admin
        .call("createUser")
        .json(json!({ "email": email, "password": PASSWORD, "role": "social_service" }))
        .expect(201)
        .rollback(|user| hub_admin.call("deleteUser").path("id", &user["id"]).expect(204))
        .await?;

    Ok(Actor::on("hub").login(email, PASSWORD)) // logs in lazily on first use
}

#[apitest::test]
async fn patient_onboarding(social_worker: Actor) -> Result<()> {
    let (app, mail) = (Actor::on("app"), Actor::on("mail"));
    let email = unique_email("patient");

    step(
        "patient registers as a demo user",
        app.call("registerPatient")
            .json(json!({ "email": email, "password": PASSWORD, "demo": true }))
            .expect(201),
    )
    .await?;
    undo(mail.delete("/messages").query("to", &email))?;

    step("patient confirms the e-mail by opening the link", async {
        let message: Mail = eventually!(
            mail.get("/messages")
                .query("to", &email)
                .expect(200)
                .await?
                .get("/0")
        )?;

        visit(&link_containing(&message.html, "/confirm")?).await
    })
    .await?;

    let patient = step(
        "patient logs in",
        Actor::on("app").login(&email, PASSWORD).signed_in(),
    )
    .await?;
    undo(patient.call("deletePatient").expect(204))?;

    let code: String = step(
        "patient creates an activation code",
        patient.call("createActivationCode").expect(201),
    )
    .await?
    .get("/code")?;

    let case = step(
        "social worker identifies the patient by code",
        social_worker
            .call("identifyPatient")
            .json(json!({ "activation_code": code }))
            .expect(201)
            .rollback(|case| social_worker.call("deleteCase").path("id", &case["id"]).expect(204)),
    )
    .await?;

    step(
        "social worker records therapy data",
        social_worker
            .call("putTherapy")
            .path("id", &case["id"])
            .json(json!({ "plan": "CBT", "sessions_per_week": 2 }))
            .expect(200),
    )
    .await?;

    step(
        "case moves to in progress",
        social_worker
            .call("updateCase")
            .path("id", &case["id"])
            .json(json!({ "status": "in_progress" }))
            .expect(200),
    )
    .await?;

    let current = social_worker
        .call("getCase")
        .path("id", &case["id"])
        .expect(200)
        .await?;

    assert_json!(current, { "status": "in_progress", "therapy": { "plan": "CBT" } });
    Ok(())
}
```

When a step fails, the step trail leads the failure message, and rollback still runs. The harness's `finally` check below confirms that nothing was left behind even though the test failed half-way:

```text
---- onboarding::patient_onboarding ----
steps:
  ✓ patient registers as a demo user [2 ms]
  ✓ patient confirms the e-mail by opening the link [420 ms]
  ✓ patient logs in [1 ms]
  ✓ patient creates an activation code [1 ms]
  ✓ social worker identifies the patient by code [1 ms]
  ✗ social worker records therapy data [0 ms]

step 'social worker records therapy data' failed: contract violation: request body of putTherapy does not match pinned 'hub' v1.0.0:
  - /sessions_per_week: 0 is less than the minimum of 1

test apitest::[finally] everything was rolled back ... ok
```

**What changed, compared with the builder style:**

```rust
// before
let inv = cx
    .call("billing", "createInvoice")?
    .json(body)
    .send()
    .await?
    .expect(201)?;
let id = inv.pointer("/id").cloned().context("no id")?;

cx.defer("delete invoice", move |cx| async move {
    cx.call("billing", "deleteInvoice")?
        .path_param("id", id)
        .send()
        .await?
        .expect(204)?;
    Ok(())
});

// after
user.call("createInvoice")
    .json(body)
    .expect(201)
    .rollback(|inv| user.call("deleteInvoice").path("id", &inv["id"]).expect(204))
    .await?;
```

**Cheat sheet** (everything is in `apitest::prelude`):

| Need | Write |
|---|---|
| someone calling an API | `Actor::on("hub")` plus `.login(user, pw)`, `.bearer(t)`, `.basic(u, p)`, `.api_key(h, k)`, `.client_credentials(..)`, or `actor("hub_admin")?` from config. Actors are serializable, so fixtures return them and tests take them as parameters |
| auth for a whole scope | `cx.bearer(token)`, `cx.basic(u, p)`, `cx.auth(Auth::login("hub", u, p))` |
| trace / headers sent | `cx.trace()` / `current_trace()?`, `resp.trace`, `resp.request.header("traceparent")`, `.no_trace()`, `.user_agent(..)` |
| a contract call / raw request | `actor.call("createUser")` / `actor.get("/messages")`. Await it directly; `.expect(201)` and `.expect_json(json!({..}))` are checked on await |
| a value from a response | `resp.get::<String>("/id")?` (JSON pointer plus deserialize, with an error showing the body); `resp["id"]` (like `serde_json::Value`) |
| create, then clean up | `.rollback(\|r\| actor.call("deleteX").path("id", &r["id"]))` registers the cleanup only once the creation succeeded |
| clean up later | `undo(request)?` for a prepared request; `defer("label", \|\| async { .. })?` for anything else |
| wait for an async effect | `eventually!(expr)?`, `eventually!(within: Duration::from_secs(60), { .. })?` |
| readable reports | `step("what happens", future_or_request).await?` |
| check part of a body | `assert_json!(resp, { "status": "in_progress" })` (partial match, error names the path) |
| test data | `unique("case")`, `unique_email("patient")` (unique across runs and parallel tests) |
| e-mail links | `link_containing(&mail.html, "/confirm")?`, then `visit(&link).await?` |
| several services | `RunConfig::new(..).service("hub", url)` or `services:` in `apitest.nova`. A contract uses the service named like it unless `.service(..)` says otherwise; with no services configured, everything uses `base_url` |
| the context after all | add a `cx: Ctx` parameter (any position) or call `cx()` |
| a check after all suites | `Plan::finally("nothing left behind", \|\| async { .. })` |

These helpers find the current context through a tokio task-local, which the runner installs for every fixture, hook, test and cleanup. Tasks you `tokio::spawn` yourself don't inherit it; pass `cx` there. `eventually!` is a macro on purpose: a closure-based `eventually(async || ..)` runs into rustc's "`Send` is not general enough" limitation inside `Send` test futures, while the macro inlines the retried block into the test's own future.

## Authentication

Auth is data on an actor (or a scope), resolved when a request is sent:

```rust
Actor::on("hub").bearer(token)                         // Authorization: Bearer …
Actor::on("hub").basic("user", "pw")                   // Authorization: Basic …
Actor::on("hub").api_key("X-Api-Key", key)             // any header
Actor::on("hub").login("admin@hub.test", "secret")     // via the service's login flow, lazily
let creds = ClientCredentials::new(token_url, id, secret).scope("reports.read");
Actor::on("reports").client_credentials(creds)         // OAuth2 client credentials grant
actor("hub_admin")?                                    // named actor from apitest.nova / RunConfig
cx.bearer(token);                                      // requests in this scope without own auth
```

- **Logins are lazy and cached.** `.login(..)` makes no request. The first request logs in, and concurrent first requests share that single login. The token is cached per service and identity for the run, refreshed before `expires_in`, and re-acquired once if the service answers `401` (for example after a test revoked it). Failures say who and where: `login as 'mallory' on 'svc' failed: status 401: {"error":"bad credentials"}`.
- **Login flows are configured once per service.** The default is to `POST` the contract operation `login` with `{username, password}` and read the token from `/token` or `/access_token`.
- **OAuth2 client credentials** use form or Basic client authentication, `scope` and `audience`, and are cached until shortly before they expire.
- **Precedence:** an explicit `Authorization` header (or `bearer`) on the request, then the request's `.auth(..)`, then the actor, then the named actor in `{ as }`, then the scope.
- **Scripts:** `cx.bearer(..)`, `cx.basic(..)`, `cx.auth({ type: "login", … })`, plus `{ as: "hub_admin" }` or `{ auth: { type: "basic", … } }` per request.

```text
# apitest.nova
auth:
  hub:
    operation: login
    body:
      email: "{username}"
      password: "{password}"
actors:
  hub_admin:
    service: hub
    auth:
      type: login
      username: "${HUB_ADMIN_EMAIL}"
      password: "${HUB_ADMIN_PASSWORD}"
  reporting:
    service: reports
    auth:
      type: client_credentials
      token_url: https://idp.example.test/oauth/token
      client_id: reporting
      client_secret: "${REPORTING_SECRET}"
```

## Tracing and headers

Every request carries trace context unless you opt out. W3C Trace Context is the default; other formats can be added or used instead:

| `formats:` | Headers |
|---|---|
| `w3c` (default; also Azure Monitor) | `traceparent: 00-<trace-id>-<span-id>-01`, plus `tracestate` and `baggage` if configured |
| `b3` / `b3multi` | `b3: <trace>-<span>-1` / `X-B3-TraceId`, `X-B3-SpanId`, `X-B3-Sampled` |
| `xray` | `X-Amzn-Trace-Id: Root=1-<time>-<rest>;Parent=<span>;Sampled=1` |
| `gcp` | `X-Cloud-Trace-Context: <trace>/<span decimal>;o=1` |
| `datadog` | `x-datadog-trace-id`, `x-datadog-parent-id`, `x-datadog-sampling-priority`, `x-datadog-tags: _dd.p.tid=…` |

- **One trace per test by default** (`scope: request | test | suite | run`), and a new span for every request. All formats carry the same 128-bit ID, so a failing test's requests can be found in any backend.
- **Opt out** globally (`tracing: { enabled: false }` or `--no-trace`), per suite (`.no_tracing()`) or per request (`.no_trace()` or `{ trace: false }`). `.trace(true)` forces trace headers even when tracing is off. A `traceparent` you set yourself is never replaced.
- **Read it in tests:** `cx.trace()` returns the test's trace. Every response has `resp.trace` (the span that was sent) and `resp.request` (method, URL and all sent headers), so you can check what a service echoes or logs. `Traceparent::parse(..)` validates echoed values. The same works in Rhai, JS and Python.
- **Parent traces:** `tracing: { parent: "00-…-01" }` makes every request join that trace. `inherit_env: true` uses `TRACEPARENT`/`TRACESTATE` from the environment (OpenTelemetry's environment-carrier convention). This is off by default because CI platforms often export a `TRACEPARENT` of their own, which would put every test into a single trace.
- **Baggage:** static `baggage:` entries, and `test_baggage: true` adds `apitest.suite` and `apitest.test`, so a service's logs can name the test.

**User-Agent** defaults to `apitest/<version>`. Override it globally (`RunConfig::user_agent`, `user_agent:` in `apitest.nova`, `--user-agent`), per suite (`Suite::user_agent`), per scope (`cx.default_header("User-Agent", ..)`), per actor (`.user_agent(..)`) or per request (`.user_agent(..)`). The innermost one wins.

## The same suite in four languages

### Rust suite

Attribute macros and actors; the builder API also exists.

```rust
#[apitest::suite]
fn billing() -> Result<Suite> {
    Suite::new("billing").pinned("billing", "specs/billing-1.0.yaml")
}

/// One login per suite; revoked when the suite ends.
#[apitest::fixture(scope = "suite")]
async fn user() -> Result<Actor> {
    let anon = Actor::on("billing");
    let token: String = anon
        .call("issueToken")
        .form(&[("user", "demo"), ("pass", "secret")])
        .expect(200)
        .await?
        .get("/access_token")?;

    let user = anon.with_bearer(token);
    undo(user.call("revokeToken").expect(204))?;
    Ok(user)
}

#[apitest::test(parallel)]
async fn create_invoice(user: Actor) -> Result<()> {
    user.call("createInvoice")
        .json(json!({ "amount": 4200, "currency": "EUR" }))
        .expect(201)
        .rollback(|inv| user.call("deleteInvoice").path("id", &inv["id"]).expect(204))
        .await?;
    Ok(())
}
```

### Rhai suite

`cx.op` instead of `cx.call`, because `call` is reserved in Rhai.

```rhai
let s = suite("billing");
s.contract("billing", "../specs/billing-1.0.yaml",
           #{ live_spec: "/openapi.json" });

s.fixture("token", "suite",
  |cx| {
    let r = cx.op("billing", "issueToken",
      #{ form: #{ user: "demo", pass: "secret" } }).expect(200);
    cx.header("Authorization", `Bearer ${r.body.access_token}`);
    r.body.access_token
  },
  |cx| { cx.op("billing", "revokeToken").expect(204); });

s.test("create invoice", #{ parallel: true }, |cx| {
  let id = cx.op("billing", "createInvoice",
    #{ json: #{ amount: 4200, currency: "EUR" } }).expect(201).body.id;
  cx.defer(|cx| {
    cx.op("billing", "deleteInvoice", #{ path: #{ id: id } }).expect(204);
  });
});
s
```

### JavaScript suite

Synchronous like k6, with one QuickJS runtime per worker or VU.

```js
const s = suite("billing");
s.contract("billing", "../specs/billing-1.0.yaml",
           { liveSpec: "/openapi.json" });

s.fixture("token", "vu",
  (cx) => {
    const r = cx.call("billing", "issueToken",
      { form: { user: "demo", pass: "secret" } }).expect(200);
    cx.header("Authorization", `Bearer ${r.body.access_token}`);
    return r.body.access_token;
  },
  (cx) => cx.call("billing", "revokeToken").expect(204));

s.test("create invoice", { parallel: true }, (cx) => {
  const inv = cx.call("billing", "createInvoice",
    { json: { amount: 4200, currency: "EUR" } }).expect(201).body;
  cx.defer(() => cx.call("billing", "deleteInvoice",
    { path: { id: inv.id } }).expect(204));
});
```

### Python suite

pytest-style `yield` fixtures.

```python
s = apitest.Suite("billing")
s.contract("billing", "specs/billing-1.0.yaml",
           live_spec="/openapi.json")

@s.fixture(scope="suite")
def token(cx):
    r = cx.call("billing", "issueToken",
                form={"user": "demo", "pass": "secret"}).expect(200)
    cx.header("Authorization", f"Bearer {r.body['access_token']}")
    yield r.body["access_token"]
    cx.call("billing", "revokeToken").expect(204)

@s.test(parallel=True)
def create_invoice(cx):
    inv = cx.call("billing", "createInvoice",
                  json={"amount": 4200, "currency": "EUR"}).expect(201).body
    cx.defer(lambda cx: cx.call("billing", "deleteInvoice",
                                path={"id": inv["id"]}).expect(204))

report = s.run("http://127.0.0.1:8080")
```

### Per-test setup and teardown

Per-test setup/teardown and resource locks look like this in Rhai (other languages are equivalent):

```rhai
s.test("per-test setup and teardown", #{
    parallel: true, resources: ["invoices"], timeout_ms: 5000,
    setup: |cx| {
        let invoice = cx.op("billing", "createInvoice", #{ json: #{ amount: 1, currency: "USD" } })
            .expect(201);
        cx.set("invoice_id", invoice.body.id)
    },
    teardown: |cx| {
        cx.op("billing", "deleteInvoice", #{ path: #{ id: cx.get("invoice_id") } })
            .expect(204)
    },
}, |cx| { /* body */ });
```

## Contract pinning

```text
suite billing-rust
  contract billing: pinned v1.0.0 vs live v2.0.0 (http://127.0.0.1:42523/openapi.json) → BREAKING
    ✗ breaking createInvoice @ request.amount_cents: field is now required but this suite does not send it
    ! warning  createInvoice @ request.amount: field we send is no longer documented (may be ignored)
    ✗ breaking createInvoice @ response 201.amount: property removed
    ✗ breaking createInvoice @ response 201.id: type changed from integer to string
    ! warning  createInvoice @ response 201.status: enum gained values "draft"
    · info     createInvoice @ response 201.amount_cents: new property
    ✗ breaking getInvoice @ path param 'id': type changed from integer to string
    ...
  ✗ setup failed: live API is incompatible with pinned contract(s): billing
  - create and fetch invoice [0 ms] (skipped: contract drift)
```

- **Direction matters.** For requests, the live API must still accept what *we send*: new required fields are breaking, and so are narrowed types or removed enum values. For responses, the live API must not produce anything *we don't expect*: removed or no-longer-required properties are breaking, as are widened types (including newly `nullable`) and dropped statuses.
- **Two modes.** `tolerant` (default) treats additive response changes (new optional properties, new operations) as info and new enum values as a warning. `strict` treats any change to the used surface as breaking.
- **Lockfile.** `--update-lock` / `RunConfig::update_lock` records, per operation, the parameters, request fields and statuses the suites used. The drift check then only looks at that surface. If the pinned spec's hash changes, the recorded usage is discarded and every pinned operation is checked again.

  ```text
  contract:
    billing:
      spec: specs/billing-1.0.yaml
      version: 1.0.0
      sha256: 23f81c9d…
      used:
        - op: createInvoice
          request_fields: [amount, currency]
          statuses: [201]
        - op: getInvoice
          params: [id]
          statuses: [200, 404]
  ```
- **Runtime validation** still catches drift when the live spec lies or isn't published: `contract violation: response 200 of getInvoice does not match pinned 'billing' v1.0.0: /id: "2" is not of type "integer"`. Under load it is sampled (`validate_every`).
- **utoipa:** pass the generated document directly, with no HTTP fetch: `Contract::pinned("api", "contracts/api-3.2.yaml")?.live_document(serde_json::to_value(ApiDoc::openapi())?)`.
- **CI without a running service:** `apitest contract diff pinned.yaml candidate.yaml` exits 1 on breaking changes.
- **Comparison specs (drift targets).** The live spec is one target, `live`. Name others in `apitest.nova`, one spec per contract, as a file path (relative to the config) or an `http(s)://` URL:

  ```text
  drift:
    against: [live, prod]          # default selection; omitted = [live]
    targets:
      dev:
        billing: https://billing.dev.example.com/openapi.json
      qa:
        billing: https://billing.qa.example.com/openapi.json
      prod:
        billing: specs/billing-prod.yaml
      branch:
        billing: ../billing-service/openapi.yaml
  ```

  `--against dev,prod` (or `RunConfig::drift_against`) picks targets for one run and replaces `drift.against`. `all` means `live` plus every named target. A selection without `live` fetches nothing from the service, so `apitest contract check --against prod` works offline. `--against-spec billing=path/or/url` checks a one-off spec (target `cli`) without touching the config.

  Each contract is diffed against each selected target with the same lockfile-limited diff. Each pair gets its own report line, JSON entry and `[contract] billing @ prod` case in JUnit and the harness (`live` keeps the plain `[contract] billing` name). A breaking result from any selected target fails the suite. Targets without a spec for a contract skip it. A selected target naming a contract that no project suite has is an error in the CLI, since it is almost always a typo. In Rust:

  ```rust
  let cfg = RunConfig::new(url)
      .drift_target(
          "prod",
          DriftTarget::new().file("billing", "specs/billing-prod.yaml"),
      )
      .drift_against(["live", "prod"]);
  ```

## Using it as a library (axum example)

```rust
// tests/api.rs, with `harness = false` in Cargo.toml
fn main() {
    apitest::harness::main(|| async {
        // real TCP on an ephemeral port
        let server = apitest::TestServer::axum(my_app::router(test_state())).await?;
        let cfg = apitest::RunConfig::new(server.base_url()).var("API_USER", "demo");
        let scripts =
            apitest::script::load_suites("tests/api/billing.js".as_ref(), &Default::default())?;

        Ok(apitest::harness::Plan::new(cfg)
            .suite(my_suites::billing()?) // Rust
            .suites(scripts)              // and/or scripts
            .keep(server))
    })
}
```

```text
$ cargo test -p apitest --features full --test billing_harness
test billing-rust::[contract] billing                          ... ok
test billing-rust::create and fetch invoice                    ... ok
test billing-rust::listing sees exactly what this test created ... ok
test billing-js::per-test setup and teardown                   ... ok
...
test result: ok. 17 passed; 0 failed
```

Filters (`cargo test -- create`) are applied before the run, so suite fixtures still run exactly once. Set `APITEST_JUNIT=path.xml` to also get a JUnit report. Inside `#[tokio::test]`, `run_suite(&suite, &cfg).await.assert_passed()` works too. For frameworks other than axum, use `TestServer::spawn(|listener, shutdown| async move { … })`.

### Native-feeling Rust tests: attribute macros

With the `macros` feature, suites can be written as plain annotated functions (see `crates/apitest/tests/macro_harness.rs`):

```rust
mod billing {                                   // the module is the suite
    use apitest::prelude::*;

    #[apitest::suite]                           // optional: contracts, hooks, …
    fn billing() -> Result<Suite> {
        let contract = Contract::pinned("billing", "specs/billing-1.0.yaml")?
            .live_spec("/openapi.json");

        Ok(Suite::new("billing").contract(contract))
    }

    #[apitest::fixture(scope = "suite", teardown = logout)]
    async fn session(cx: Ctx) -> Result<Session> { … }
    async fn logout(cx: Ctx) -> Result<()> { … }

    #[apitest::fixture]                         // test-scoped, depends on `session` by name
    async fn usd_invoice(cx: Ctx, session: Session) -> Result<u64> { … }

    #[apitest::test(parallel, resources = ["invoices"], timeout_ms = 5000)]
    async fn fixture_injection(cx: Ctx, usd_invoice: u64) -> Result<()> { … }

    #[apitest::test(serial)]
    async fn listing_sees_exactly_what_this_test_created(cx: Ctx) -> Result<()> { … }
}

fn main() {
    apitest::harness::main(|| async {
        let server = apitest::TestServer::axum(app()).await?;
        let cfg = apitest::RunConfig::new(server.base_url());

        Ok(apitest::harness::Plan::new(cfg).registered()?.keep(server))
    })
}
```

- Registrations go into `linkme` distributed slices; `Plan::registered()` collects them. There's no life-before-main code and no central list to maintain.
- Parameters after `cx` are fixtures, resolved **by name** and deserialized into the parameter type. Fixtures can depend on fixtures the same way.
- Test ids are module paths (`billing::fixture_injection`). rust-analyzer recognises tests by a `#[test]`-like attribute on the *expanded* item, so the macro also emits a `#[cfg(test)] #[test] fn <name>()` marker with the original span. `harness = false` targets aren't compiled with `--test`, so the marker is removed there. rust-analyzer then shows ▶ Run and "test-mod billing" and runs `cargo test --test <target> -- billing::fixture_injection --exact --nocapture --include-ignored`, which the harness resolves to exactly that test, including the fixtures it needs. This was verified against rust-analyzer 1.97 over LSP. If such a test ends up in a regular libtest target by mistake, the marker panics with an explanation instead of passing silently.
- Why not expand to real `#[test]` functions? libtest has no "after all tests" hook and no serial/parallel control, so suite teardown and the concurrency rules couldn't be guaranteed.

### nextest

nextest runs every test in its own process (`--exact <id>`). The harness detects it (`NEXTEST_RUN_ID`) and adapts:

| | `cargo test` | `cargo nextest run` |
|---|---|---|
| suite / VU fixtures | once per suite | once per test process |
| `serial` | runs alone in the suite | exclusive file lock shared by all apitest processes of the run |
| `parallel` + `resources` | in-process semaphore + mutexes | shared lock + one exclusive lock per resource |
| drift check | once per suite | cached for the run (keyed by pin, live URL, used surface) |
| suite-level failures (drift, setup, teardown) | own `[contract]` / `[setup]` / `[teardown]` entries | folded into every affected test's failure, since nextest only runs listed tests |

The locks live in `$TMPDIR/apitest-<run id>/`. `tests/nextest_locks.rs` checks the guarantees across processes. With `APITEST_NEXTEST_LOCKS=0` (escape hatch) the same tests fail, which shows that they really test the locks. With in-process servers, each nextest process has its own server and port, so each checks its own live spec. Against a shared service, the cache makes it one check per run.

## CLI

Install with `cargo install --path crates/apitest-cli` (binary: `apitest`), or use `cargo run -p apitest-cli --` as below.

```bash
cargo run -p apitest --example demo_server -- 8080 1.0      # or 1.1 / 2.0
```

```bash
cd demo && cargo run -p apitest-cli -- run --update-lock   # uses demo/apitest.nova
```

```bash
cargo run -p apitest-cli -- load demo/suites/billing.js -t "create and fetch invoice" \
  --stages "10s:20,30s:20,10s:0" --threshold "http.p95 < 50ms" --threshold "iterations.error_rate < 1%"
```

```bash
cargo run -p apitest-cli -- contract check demo/suites/billing.rhai -u http://127.0.0.1:8080
```

```bash
cargo run -p apitest-cli -- contract diff demo/specs/billing-1.0.yaml demo/specs/billing-2.0.yaml
```

```bash
cd demo && cargo run -p apitest-cli -- contract check --against qa,next   # drift targets from apitest.nova, no server needed
```

`demo/apitest.nova`:

```text
base_url: http://127.0.0.1:8080
lockfile: apitest.lock.nova
max_parallel: 8
test_timeout: @duration("30s")
vars:
  API_USER: demo
suites:
  - suites/billing.rhai
  - suites/billing.js
reports:
  junit: ../target/apitest/junit.xml
drift:
  targets:
    qa:
      billing: specs/billing-1.1.yaml
    next:
      billing: specs/billing-2.0.yaml
```

Exit codes: 0 passed, 1 tests or thresholds failed, 2 usage or config error.

### Terminal diagnostics

With the `diagnostics` feature (part of `full`, and always on in the CLI), `SuiteReport::render_terminal(color)` renders failures as [miette](https://docs.rs/miette) diagnostics with their whole cause chain:

```text
  ✗ sign in [3 ms]
      ✗ as mallory [3 ms]
        × step 'as mallory' failed
        ├─▶ login as 'mallory' on 'svc' failed
        ╰─▶ status 401: {"error":"bad credentials"}
```

The CLI uses it for suite output and for its own errors. Colour is only used on a terminal, and `NO_COLOR`, `CLICOLOR`, `CLICOLOR_FORCE` and `TERM=dumb` are respected (`apitest::diagnostic::use_color`). `render`, JUnit, JSON, `assert_passed` and the libtest harness messages stay plain text, because CI and IDE tooling parses them. The cause chain is kept on `Outcome::Failed` and `SuiteReport::setup_chain`, but it is not serialized.

### One-off requests: `apitest call`

`apitest call <service> <method> <path>` sends a single request without a suite or an OpenAPI spec. It reads the same `apitest.nova`, so services, named actors, vars, the User-Agent and tracing all apply.

```bash
apitest call hub GET /users/me --as hub_admin \
  --var HUB_ADMIN_EMAIL=admin@hub.test --var HUB_ADMIN_PASSWORD=...
apitest call hub POST /users --as hub_admin --json '{"email":"a@b.test"}'    # or --json @user.json / @- for stdin
apitest call hub PUT /notes/7 --data @note.md -H 'Content-Type: text/markdown'
apitest call mail GET /messages --query to=a@b.test -i                      # -i: status code and headers first
apitest call hub DELETE /users/7 --as hub_admin --expect 204 -v             # -v: request/response summary on stderr
```

- **Bodies** are UTF-8 text: `--json`, `--data` (inline, `@file`, `@-` for stdin; `@@` starts a literal `@`) or `--form k=v`.
- **Output.** The response body goes to stdout as received, so it pipes into `jq`. `-i` puts `HTTP <status>` and the response headers before it (a repeated header shows its last value). `-v` writes the request as sent and the response status to stderr, with cookies, `Authorization`, the header a login flow or API key uses, and the actor's configured `headers:` redacted. Errors go to stderr as [terminal diagnostics](#terminal-diagnostics). `--timeout` (default 30s, including a login) stops a hanging call.
- **Exit codes.** 0 for a 2xx status (or the `--expect` status), 1 for any other status, 2 for usage, config, auth or connection errors.
- **Nothing is guessed, especially not auth.** The service must be listed under `services:` (`base_url` is not a fallback), and the path, like a login flow's `path`, must start with `/`, so credentials can't be sent to another host. Without `--as`, the request carries no credentials. An actor that logs in needs a login flow with a `path` under `auth:`, because there is no spec to find a `login` operation in. If it's missing, the call fails before sending anything:

  ```text
    × actor 'hub_admin' logs in as '${HUB_ADMIN_EMAIL}' on 'hub', but there is no login flow for 'hub'
    │ help: `apitest call` has no OpenAPI spec and doesn't guess a login endpoint; configure it in apitest.nova, e.g.
    │   auth:
    │     hub:
    │       path: /login
    │       body:
    │         username: "{username}"
    │         password: "{password}"
    │       token: /token
  ```

- **Actors stay intact.** An actor must belong to the service it's sent to, and a `-H` for a header the actor sets (its credentials, `headers:` or `user_agent:`) is refused instead of silently replacing the actor's value.
- **Redirects stay on the service.** Redirects are followed only within the service's origin (scheme, host and port), at most 10 times. A redirect to another origin is not followed: the 3xx is the result (exit 1), and its target is named on stderr, so an API key, an actor's `headers:` or a `-H` value never reach another host.

## Load testing

```text
load billing-js
  scenario create and fetch invoice (test 'create and fetch invoice', 2.6s)
    http       n=7729  failed=0  rate= 3017.2/s  avg= 1.22ms p50= 1.12ms p90= 1.93ms p95= 2.21ms p99= 2.91ms
    iterations n=2571  failed=0  rate= 1003.7/s  avg= 5.57ms p50= 5.56ms p90= 7.33ms p95= 7.90ms p99= 9.71ms
    ✓ http.p95 < 50ms  (actual 2.207)
    ✓ iterations.error_rate < 1%  (actual 0.000)
```

- `suite` fixtures run once, `vu` fixtures once per VU (for example a login per simulated user), `test` fixtures once per iteration.
- The JS frontend gives every worker its own QuickJS runtime, and the load runner sizes the pool to the VU count, so JS VUs run in parallel (k6's model). Python releases the GIL during HTTP. Rhai shares one engine (`sync` feature) across blocking threads.
- `http.error_rate` counts transport errors and 5xx; `iterations.error_rate` counts failed iterations.

## Python bindings

```bash
cargo build -p apitest --example demo_server
```

```bash
cd crates/apitest-py && maturin develop && pytest -v
```

`Suite.run(...)` returns the report as a dict, `Suite.check(...)` raises `SuiteFailed`, and `Suite.load(...)` runs a scenario. Tracebacks from Python callbacks end up in the failure messages.

## Tests

| | |
|---|---|
| `cargo test -p apitest --features full --test scenarios` | drift (compatible/breaking/in-process doc), runtime validation, cleanup after failure/panic/timeout, serial vs parallel vs resources, lockfile, JUnit, Rhai/JS suites, Rust and JS load (ramping) |
| `cargo test -p apitest --features full --test billing_harness` | the libtest harness with Rust, Rhai and JS suites against an in-process axum app |
| `cargo test -p apitest --features full --test headers` | trace formats and correlation, opt-out at every level, parent traces, User-Agent precedence, all auth kinds, single-flight cached logins with 401 re-login, client-credentials expiry, named actors from Rust, JS and Rhai |
| `cargo test -p apitest --features full --test it diagnostics` | miette cause chains in `render_terminal`, plain text in `render`, JUnit, JSON and `assert_passed` |
| `cargo test -p apitest --features full --test care_flow` | the multi-actor, multi-service onboarding flow (Hub, App, mail mock) with step reports, rollback and a `finally` leftover check |
| `cargo test -p apitest --features full --test macro_harness` | the same suite via `#[apitest::test]` / `#[apitest::fixture]` / `#[apitest::suite]` (`DEMO_VARIANT=2.0` shows drift) |
| `cargo nextest run -p apitest --features full` | all harness targets process-per-test; `nextest_locks` checks the cross-process serial/resource rules |
| `npx -p typescript tsc -p demo/suites/jsconfig.json` | type-checks the JS demo suite against `npm/cli/types/index.d.ts` |
| `pytest crates/apitest-py/tests` | Python suite against v1.0 and v2.0, cleanup, load |

Each scenario checks `GET /_stats` afterwards to prove that no tokens or invoices were left behind.

## npm

`npx @apitest/cli run suites/` or `npm i -D @apitest/cli`. It's the native binary, delivered the way esbuild, Biome and SWC do it: a launcher package plus per-platform packages selected through `optionalDependencies` (`os`/`cpu`/`libc`), with no postinstall script and no download at runtime. It also ships `types/index.d.ts`, so JS suites get autocompletion and type checking (`// @ts-check` + `/// <reference types="@apitest/cli" />`). Suites still run in QuickJS, not Node. Details, local testing and the release flow are in [`npm/README.md`](https://github.com/sunsided/apitest/blob/main/npm/README.md). The `@apitest` npm scope still needs to be claimed (`apitest` on npm is someone else's package).

## Open questions / next steps

- **Source spans in diagnostics.** Failures render with miette on the terminal (see [Terminal diagnostics](#terminal-diagnostics)), but without source spans yet. In order of effort:
  - Rhai errors already carry line and column, so failures can point at the `cx.op(..)` call in the `.rhai` file.
  - NOVA config errors get spans through `novafile-text`'s `miette` feature.
  - JS needs scripts evaluated with their file names.
  - Rust tests need the caller's location recorded on the request.
  - Pointing into the OpenAPI document needs a position-preserving YAML parser.
- **Ergonomics in Rhai, JS and Python.** Actors, `step`, `eventually`, `rollback` and partial JSON matching exist in Rust only so far. Scripts can already target other services with `{ service: "mail" }` in the request options.
- **Schema normalisation.** OpenAPI 3.0 `nullable` next to `allOf`/`$ref` isn't rewritten yet; only `nullable` with a plain `type` is.
- **Run-scoped fixtures.** `Scope::Run` exists but isn't used: fixtures shared across suites (a login for all suites) would go there. `Plan::finally` covers the "after everything" check for now.

- **Scripts are synchronous.** That's simple and k6-like, but a JS `async` API on rquickjs's async runtime would allow fan-out inside one test.
- **Rhai `before_request` hooks** aren't possible yet: a hook would re-enter the worker that's blocked on the request. Default headers cover auth for now. A Rust-side `RequestHook` trait would be easy to add.
- **Drift diff coverage.** It handles types, properties, required, enums, items, params, statuses and bodies. Not yet: `oneOf`/`anyOf`/`allOf`, discriminators, headers, content types other than JSON, security schemes, and recursive `$ref`s beyond depth 32.
- **Report-only drift targets.** Every selected target gates the suite today. A per-target `fail: false` would let `dev` report drift without blocking the run.
- **Per-test contract usage.** The lockfile records usage per contract; recording it per test would make "which tests depend on X" queries possible.
- **Reports.** JUnit and JSON exist; CTRF, TAP or a live event stream (JSON lines) for dashboards would be next.
- **Ctrl-C.** Cleanups currently run on normal completion, failures, panics and timeouts, but not on SIGINT. That needs a cancellation token threaded through the runner.
- **Type-safe Rust clients.** Generating progenitor clients from the pinned spec would add compile-time pinning on top of the runtime check.
- **NOVA schemas.** Ship `.novs` schemas for `apitest.nova` and the lockfile so editors can validate them.

## License

Licensed under any of

- European Union Public Licence, Version 1.2 ([LICENSES/EUPL-1.2.txt](https://github.com/sunsided/apitest/blob/main/LICENSES/EUPL-1.2.txt))
- MIT license ([LICENSES/MIT.txt](https://github.com/sunsided/apitest/blob/main/LICENSES/MIT.txt))
- Apache License, Version 2.0 ([LICENSES/Apache-2.0.txt](https://github.com/sunsided/apitest/blob/main/LICENSES/Apache-2.0.txt))

at your option. The examples (`crates/*/examples/`) and the demo project (`demo/`) are public domain
([CC0-1.0](https://github.com/sunsided/apitest/blob/main/LICENSES/CC0-1.0.txt)), so you can copy them into anything.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this work by you
shall be licensed as above, without any additional terms or conditions.
