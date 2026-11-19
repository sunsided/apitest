# apitest-macros

[![crates.io](https://img.shields.io/crates/v/apitest-macros.svg)](https://crates.io/crates/apitest-macros)
[![docs.rs](https://img.shields.io/docsrs/apitest-macros)](https://docs.rs/apitest-macros)
[![CI](https://github.com/sunsided/apitest/actions/workflows/ci.yml/badge.svg)](https://github.com/sunsided/apitest/actions/workflows/ci.yml)
[![MSRV](https://img.shields.io/crates/msrv/apitest-macros)](https://crates.io/crates/apitest-macros)
[![License](https://img.shields.io/crates/l/apitest-macros)](https://github.com/sunsided/apitest#license)
[![unsafe forbidden](https://img.shields.io/badge/unsafe-forbidden-success.svg)](https://github.com/rust-secure-code/safety-dance/)

Attribute macros for [`apitest`](https://crates.io/crates/apitest): `#[apitest::test]`, `#[apitest::fixture]`
and `#[apitest::suite]`. Don't depend on this crate directly. Enable the `macros` feature of `apitest`, which
re-exports them, and see its [documentation](https://docs.rs/apitest) for usage.

## License

Licensed under EUPL-1.2, MIT or Apache-2.0, at your option. See the [repository](https://github.com/sunsided/apitest#license) for details.
