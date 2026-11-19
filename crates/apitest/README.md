# apitest

[![crates.io](https://img.shields.io/crates/v/apitest.svg)](https://crates.io/crates/apitest)
[![docs.rs](https://img.shields.io/docsrs/apitest)](https://docs.rs/apitest)
[![CI](https://github.com/sunsided/apitest/actions/workflows/ci.yml/badge.svg)](https://github.com/sunsided/apitest/actions/workflows/ci.yml)
[![codecov](https://codecov.io/gh/sunsided/apitest/graph/badge.svg)](https://codecov.io/gh/sunsided/apitest)
[![MSRV](https://img.shields.io/crates/msrv/apitest)](https://crates.io/crates/apitest)
[![License](https://img.shields.io/crates/l/apitest)](https://github.com/sunsided/apitest#license)
[![unsafe forbidden](https://img.shields.io/badge/unsafe-forbidden-success.svg)](https://github.com/rust-secure-code/safety-dance/)

API tests and load tests with scoped fixtures that always clean up, OpenAPI contract pinning with drift
checks, and suites written in Rust, Rhai or JavaScript. Run them from `cargo test` or nextest, from your
own code, or with the [`apitest` CLI](https://crates.io/crates/apitest-cli).

## Usage

```toml
[dev-dependencies]
apitest = { version = "0.0.1", features = ["harness", "macros"] }
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

The [repository README](https://github.com/sunsided/apitest#readme) covers the lifecycle, attribute macros, authentication, tracing,
contract pinning and load testing in depth.

## Features

| Feature   | Default | Enables |
|-----------|---------|---------|
| `load`    | yes     | Load-test executors, latency histograms, thresholds |
| `axum`    | yes     | `TestServer::axum` for serving axum routers in-process |
| `rhai`    | no      | Rhai scripting frontend |
| `js`      | no      | JavaScript (QuickJS) scripting frontend |
| `harness` | no      | `cargo test` / nextest integration (pulls in clap via libtest-mimic) |
| `macros`  | no      | `#[apitest::test]`, `#[apitest::fixture]`, `#[apitest::suite]` |
| `diagnostics` | no  | `SuiteReport::render_terminal`: failures as miette diagnostics on the terminal (reports stay plain text) |
| `full`    | no      | All of the above |

`--no-default-features` leaves the core: runner, contracts, HTTP, reports and NOVA config.

## MSRV

Rust 1.95. Raising it is a minor-version change.

## License

Licensed under EUPL-1.2, MIT or Apache-2.0, at your option. See the [repository](https://github.com/sunsided/apitest#license) for details.
