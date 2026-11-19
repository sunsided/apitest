//! # apitest
//!
//! API testing and load testing built around three ideas:
//!
//! 1. **Scoped fixtures with guaranteed teardown.** Fixtures live in a scope
//!    (`run` → `suite` → `vu` → `test`). Anything registered with
//!    [`Ctx::defer`] runs in LIFO order when its scope ends, even if the test
//!    failed, panicked or timed out.
//! 2. **Contracts are pinned per operation.** A suite binds to an `OpenAPI`
//!    document by `operationId`. Requests and responses are validated against
//!    the pinned spec, the operations and fields a suite actually uses are
//!    recorded in `apitest.lock`, and the live spec of the service is diffed
//!    against the pinned one *for that used surface only*.
//! 3. **One core, many frontends.** Suites are plain values. They can be
//!    written in Rust, Rhai, JavaScript or Python, and run functionally, as
//!    load tests, from the `apitest` CLI or from `cargo test`.
//!
//! Every value that crosses a language boundary (fixture values, request
//! options, responses) is a [`serde_json::Value`], so a fixture written in
//! Rust can be consumed by a JS test and vice versa.

#![forbid(unsafe_code)]

/// Compiles and runs the README examples as doctests.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

pub mod auth;
pub mod config;
pub mod context;
pub mod contract;
pub mod dsl;
pub mod http;
pub mod report;
pub mod runner;
pub mod script;
pub mod serve;
pub mod suite;
pub mod trace;

#[cfg(feature = "diagnostics")]
pub mod diagnostic;
#[cfg(feature = "harness")]
pub mod harness;
#[cfg(feature = "macros")]
pub mod registry;

/// `#[apitest::test]`, `#[apitest::fixture]`, `#[apitest::suite]` (feature `macros`).
#[cfg(feature = "macros")]
pub use apitest_macros::{fixture, suite, test};

/// Items the attribute macros' generated code refers to. Not public API.
#[cfg(feature = "macros")]
#[doc(hidden)]
pub mod __private {
    pub use crate::registry::{FIXTURES, FixtureDef, SUITES, SuiteDef, TESTS, TestDef};
    pub use anyhow;
    pub use linkme;
    pub use serde_json;
}

#[cfg(feature = "load")]
pub mod load;

pub use context::{Ctx, Scope};
pub use contract::{Compat, Contract};
pub use http::{RequestOpts, Response};
pub use report::{Outcome, SuiteReport, TestReport};
pub use runner::{RunConfig, run_suite, run_suites};
pub use serve::TestServer;
pub use suite::{Concurrency, Suite, TestOpts};
pub use trace::{TraceFormat, TraceScope, TracingConfig};

#[doc(hidden)]
pub use anyhow as __private_anyhow;
#[doc(hidden)]
pub use serde_json as __serde_json;

pub use auth::{Auth, ClientCredentials, LoginFlow};
pub use context::current;
pub use dsl::Actor;

/// Re-exports for writing suites in Rust.
pub mod prelude {
    pub use crate::auth::{Auth, ClientCredentials, LoginFlow};
    pub use crate::context::{Ctx, Scope};
    pub use crate::contract::{Compat, Contract};
    pub use crate::dsl::{
        Actor, actor, current_trace, defer, env, json_includes, link_containing, links, step, undo,
        unique, unique_email, visit,
    };
    pub use crate::http::{RequestOpts, Response};
    #[cfg(feature = "load")]
    pub use crate::load::{Executor, LoadPlan, Scenario, Stage};
    pub use crate::runner::{RunConfig, run_suite};
    pub use crate::serve::TestServer;
    pub use crate::suite::{Concurrency, Suite, TestOpts};
    pub use crate::trace::{TraceContext, TraceFormat, TraceScope, Traceparent, TracingConfig};
    pub use crate::{assert_json, eventually};
    pub use anyhow::{Context as _, Result, anyhow, bail, ensure};
    pub use serde_json::{Value, json};
}
