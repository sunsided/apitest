# apitest

[![crates.io](https://img.shields.io/crates/v/apitest.svg)](https://crates.io/crates/apitest)
[![docs.rs](https://img.shields.io/docsrs/apitest)](https://docs.rs/apitest)
[![CI](https://github.com/sunsided/apitest/actions/workflows/ci.yml/badge.svg)](https://github.com/sunsided/apitest/actions/workflows/ci.yml)
[![codecov](https://codecov.io/gh/sunsided/apitest/graph/badge.svg)](https://codecov.io/gh/sunsided/apitest)
[![MSRV](https://img.shields.io/crates/msrv/apitest)](https://crates.io/crates/apitest)
[![License](https://img.shields.io/crates/l/apitest)](https://github.com/sunsided/apitest#license)
[![unsafe forbidden](https://img.shields.io/badge/unsafe-forbidden-success.svg)](https://github.com/rust-secure-code/safety-dance/)

API tests and load tests with scoped fixtures that always clean up, OpenAPI contract pinning with drift
checks, and suites written in Rust, Rhai, JavaScript or Gherkin. Run them from `cargo test` or nextest, from your
own code, or with the [`apitest` CLI](https://crates.io/crates/apitest-cli).

- **Setup and teardown at every level.** Suite, virtual user, test and fixture scopes, plus per-test hooks
  and `defer` cleanups. Cleanups run in LIFO order, even when a test fails, panics or times out.
- **Contract pinning.** Suites bind to a checked-in OpenAPI document by `operationId`. Requests and responses
  are validated against it, and before any test runs, the service's live spec is diffed against the pin,
  limited to the operations and fields the suites actually use.
- **Functional and load runs from the same tests.** Any test can be a load scenario with constant VUs, a
  shared iteration budget or ramping stages, HDR latency histograms and k6-style thresholds
  (`http.p95 < 300ms`).
- **In-process servers over real TCP.** `TestServer::axum(router)` (or `TestServer::spawn` for any framework)
  serves your app on an ephemeral port, so requests go through sockets, middleware, routing and extractors.
- **Actors and auth.** Bearer, Basic, API keys, lazy cached logins and OAuth2 client credentials, configured
  in code or in `apitest.nova`. Secrets are masked as `***` in all output.
- **Standard reports.** JUnit XML and JSON; contract drift and setup failures show up as their own test cases.

## Usage

```toml
[dev-dependencies]
apitest = { version = "0.1", features = ["harness", "macros"] }   # + "rhai" / "js" for script suites
```

Serve your app in-process over real TCP and run a suite against it:

```rust
use apitest::prelude::*;
use axum::{Json, Router, routing::get};

#[tokio::main]
async fn main() -> Result<()> {
    let app = Router::new().route("/health", get(|| async { Json(json!({ "status": "ok" })) }));
    let server = TestServer::axum(app).await?;

    let suite = Suite::new("health").test("reports ok", |cx| async move {
        let response = cx.request("GET", "/health").send().await?.expect(200)?;
        ensure!(response.get::<String>("/status")? == "ok", "unexpected status");
        Ok(())
    });

    let report = run_suite(&suite, &RunConfig::new(server.base_url()).quiet(true)).await;
    ensure!(report.passed(), "suite failed");
    Ok(())
}
```

Inside `#[tokio::test]`, `run_suite(&suite, &cfg).await.assert_passed()` works too.

## Tests as annotated functions

With the `harness` and `macros` features, a module is a suite, and fixtures are injected into tests by
parameter name. Put this in a test target with `harness = false`, and it runs under `cargo test` and
`cargo nextest run`, with filters and rust-analyzer's ▶ Run working as usual. (Not compiled as a doctest:
it needs your app and an OpenAPI spec.)

```rust,ignore
mod billing {
    use apitest::prelude::*;

    #[apitest::suite]
    fn billing() -> Result<Suite> {
        Suite::new("billing").pinned("billing", "specs/billing-1.0.yaml")
    }

    /// One login per suite, revoked when the suite ends.
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
}

fn main() {
    apitest::harness::main(|| async {
        let server = apitest::TestServer::axum(my_app::router()).await?;
        let cfg = apitest::RunConfig::new(server.base_url());

        Ok(apitest::harness::Plan::new(cfg).registered()?.keep(server))
    })
}
```

`.rollback(..)` registers the cleanup only once the creation succeeded. The most common helpers, all in
`apitest::prelude`:

| Need | Write |
|---|---|
| someone calling an API | `Actor::on("hub")` plus `.login(user, pw)`, `.bearer(t)`, `.basic(u, p)`, `.api_key(h, k)`, `.client_credentials(..)`, or `actor("hub_admin")?` from config |
| a contract call / raw request | `actor.call("createUser")` / `actor.get("/messages")`; `.expect(201)` is checked on await |
| a value from a response | `resp.get::<String>("/id")?` (JSON pointer plus deserialize) or `resp["id"]` |
| clean up later | `undo(request)?`, or `defer("label", \|\| async { .. })?` for anything else |
| wait for an async effect | `eventually!(expr)?`, `eventually!(within: Duration::from_secs(60), { .. })?` |
| readable failure reports | `step("what happens", future_or_request).await?` |
| check part of a body | `assert_json!(resp, { "status": "in_progress" })` |
| unique test data | `unique("case")`, `unique_email("patient")` |
| a check after all suites | `apitest::harness::Plan::finally("nothing left behind", \|\| async { .. })` (feature `harness`, not in the prelude) |

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

Tests are `serial` by default. `parallel` tests overlap up to `max_parallel`, except those that name the same
resource (`resources = ["db"]`). Under nextest, which runs each test in its own process, the same rules are
enforced with file locks shared by the run.

## Contract pinning

When the live API drifts from the pinned spec in a way that affects what the suite uses, the suite fails
before any test runs:

```text
suite billing-rust
  contract billing: pinned v1.0.0 vs live v2.0.0 (http://127.0.0.1:42523/openapi.json) → BREAKING
    ✗ breaking createInvoice @ request.amount_cents: field is now required but this suite does not send it
    ✗ breaking createInvoice @ response 201.id: type changed from integer to string
    ! warning  createInvoice @ response 201.status: enum gained values "draft"
    · info     createInvoice @ response 201.amount_cents: new property
    ...
  ✗ setup failed: live API is incompatible with pinned contract(s): billing
  - create and fetch invoice [0 ms] (skipped: contract drift)
```

- `RunConfig::lockfile(path)` with `update_lock(true)` records the parameters, request fields and statuses
  the suites used (the CLI uses `apitest.lock.nova`); the drift check only looks at that surface.
- Besides the live spec, you can diff against named specs (files or URLs, e.g. what QA and prod run) with
  `RunConfig::drift_target` and `drift_against`, so the check doesn't need a running service.
- With utoipa, pass the generated document directly:
  `Contract::pinned("api", "contracts/api.yaml")?.live_document(serde_json::to_value(ApiDoc::openapi())?)`.

## Load testing

Functional tests double as load scenarios (feature `load`, on by default). (Not compiled as a doctest: it
needs a suite and a running service.)

```rust,ignore
use apitest::load::{Executor, LoadPlan, Scenario, run_load};

let plan = LoadPlan::new(billing_suite()?).scenario(
    Scenario::new("create and fetch invoice", Executor::ConstantVus { vus: 20, duration: Duration::from_secs(30) })
        .threshold("http.p95 < 50ms")?
        .threshold("iterations.error_rate < 1%")?,
);
let report = run_load(&plan, &cfg).await;
```

```text
load billing-js
  scenario create and fetch invoice (test 'create and fetch invoice', 2.6s)
    http       n=7729  failed=0  rate= 3017.2/s  avg= 1.22ms p50= 1.12ms p90= 1.93ms p95= 2.21ms p99= 2.91ms
    iterations n=2571  failed=0  rate= 1003.7/s  avg= 5.57ms p50= 5.56ms p90= 7.33ms p95= 7.90ms p99= 9.71ms
    ✓ http.p95 < 50ms  (actual 2.207)
    ✓ iterations.error_rate < 1%  (actual 0.000)
```

`suite` fixtures run once, `vu` fixtures once per virtual user (a login per simulated user, say), and `test`
fixtures once per iteration.

## Script suites

With the `rhai` or `js` feature, `apitest::script::load_suites` loads suites written in Rhai or JavaScript.
They get the same `cx` API and semantics as Rust suites, and fixture values are JSON, so a Rust fixture can
feed a JS test:

```js
const s = suite("billing");
s.contract("billing", "../specs/billing-1.0.yaml", { liveSpec: "/openapi.json" });

s.test("create invoice", { parallel: true }, (cx) => {
  const inv = cx.call("billing", "createInvoice",
    { json: { amount: 4200, currency: "EUR" } }).expect(201).body;
  cx.defer(() => cx.call("billing", "deleteInvoice", { path: { id: inv.id } }).expect(204));
});
```

The [repository README](https://github.com/sunsided/apitest#readme) covers authentication, `.env` files and
redaction, trace propagation, drift targets, nextest and the scripting frontends in depth.

## Features

| Feature   | Default | Enables |
|-----------|---------|---------|
| `load`    | yes     | Load-test executors, latency histograms, thresholds |
| `axum`    | yes     | `TestServer::axum` for serving axum routers in-process |
| `rhai`    | no      | Rhai scripting frontend |
| `js`      | no      | JavaScript (QuickJS) scripting frontend |
| `cucumber` | no     | Gherkin frontend: `.feature` files with a generic step library, `--tags` expressions |
| `harness` | no      | `cargo test` / nextest integration (pulls in clap via libtest-mimic) |
| `macros`  | no      | `#[apitest::test]`, `#[apitest::fixture]`, `#[apitest::suite]` |
| `diagnostics` | no  | `SuiteReport::render_terminal`: failures as miette diagnostics on the terminal (reports stay plain text) |
| `full`    | no      | All of the above |

`--no-default-features` leaves the core: runner, contracts, HTTP, reports and NOVA config.

## MSRV

Rust 1.95. Raising it is a minor-version change.

## License

Licensed under EUPL-1.2, MIT or Apache-2.0, at your option. See the [repository](https://github.com/sunsided/apitest#license) for details.
