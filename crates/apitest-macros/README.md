# apitest-macros

[![crates.io](https://img.shields.io/crates/v/apitest-macros.svg)](https://crates.io/crates/apitest-macros)
[![docs.rs](https://img.shields.io/docsrs/apitest-macros)](https://docs.rs/apitest-macros)
[![CI](https://github.com/sunsided/apitest/actions/workflows/ci.yml/badge.svg)](https://github.com/sunsided/apitest/actions/workflows/ci.yml)
[![MSRV](https://img.shields.io/crates/msrv/apitest-macros)](https://crates.io/crates/apitest-macros)
[![License](https://img.shields.io/crates/l/apitest-macros)](https://github.com/sunsided/apitest#license)
[![unsafe forbidden](https://img.shields.io/badge/unsafe-forbidden-success.svg)](https://github.com/rust-secure-code/safety-dance/)

Attribute macros for [`apitest`](https://crates.io/crates/apitest): `#[apitest::test]`, `#[apitest::fixture]`
and `#[apitest::suite]`. Don't depend on this crate directly. Enable the `macros` feature of `apitest`, which
re-exports them:

```toml
[dev-dependencies]
apitest = { version = "0.1", features = ["harness", "macros"] }
```

## Usage

A module is a suite, and a test target with `harness = false` runs it under `cargo test` and nextest:

```rust,ignore
mod billing {
    use apitest::prelude::*;

    #[apitest::suite]                           // optional: contracts, hooks, …
    fn billing() -> Result<Suite> {
        Suite::new("billing").pinned("billing", "specs/billing-1.0.yaml")
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

| Macro | On | Arguments |
|---|---|---|
| `#[apitest::suite]` | `fn() -> Result<Suite>` | `suite = ".."` (default: module path) |
| `#[apitest::fixture]` | `async fn` returning `Result<T>`, `T: Serialize` | `scope = "suite" \| "vu" \| "test"` (default `test`), `teardown = path`, `name = ".."`, `suite = ".."` |
| `#[apitest::test]` | `async fn` returning `Result<()>` | `parallel` \| `serial` (default), `resources = [..]`, `timeout_ms = N`, `tags = [..]`, `name = ".."`, `suite = ".."` |

- Parameters are fixtures, resolved **by name** and deserialized into the parameter type. A `cx: Ctx`
  parameter, in any position, gets the test's context. Fixtures can depend on other fixtures the same way.
- Registrations go into `linkme` distributed slices that `Plan::registered()` collects: no code runs before
  `main`, and there's no central list to maintain.
- Test ids are module paths (`billing::fixture_injection`). Each test also gets a `#[test]` marker for
  rust-analyzer, so ▶ Run works in the editor and runs exactly that test with the fixtures it needs.
- They don't expand to plain `#[test]` functions because libtest has no "after all tests" hook and no
  serial/parallel control, so suite teardown and the concurrency rules couldn't be guaranteed.

See the [`apitest` documentation](https://docs.rs/apitest) and the
[repository README](https://github.com/sunsided/apitest#native-feeling-rust-tests-attribute-macros) for more.

## License

Licensed under EUPL-1.2, MIT or Apache-2.0, at your option. See the [repository](https://github.com/sunsided/apitest#license) for details.
