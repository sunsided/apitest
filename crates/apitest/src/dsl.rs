//! Helpers that keep everyday tests short. Everything here also works without
//! passing `cx` around: it uses the context of the running fixture, hook, test or
//! cleanup ([`crate::context::current`]).
//!
//! The items live in four submodules and are re-exported here, so the public
//! paths stay `apitest::dsl::*`:
//!
//! - [`Actor`]: who calls an API, with which credentials.
//! - context-free helpers: [`step`], [`undo`], [`defer`], [`visit`], [`eventually!`](crate::eventually), ...
//! - test data: [`unique`], [`unique_email`], [`links`], [`link_containing`].
//! - JSON matching: [`json_includes`] and [`assert_json!`](crate::assert_json).
//!
//! A test usually reads like this (normally an `#[apitest::test]` function):
//!
//! ```no_run
//! # use apitest::prelude::*;
//! # const PW: &str = "secret";
//! # #[derive(serde::Deserialize)]
//! # struct Mail { body: String }
//! async fn onboarding() -> Result<()> {
//!     let app = Actor::on("app");
//!     let email = unique_email("patient");
//!
//!     step("patient registers", app.call("registerPatient")
//!         .json(json!({ "email": email, "password": PW }))
//!         .expect(201)).await?;
//!
//!     let mail: Mail = eventually!(Actor::on("mail").get("/messages").query("to", &email).expect(200).await?.get("/0"))?;
//!     visit(&link_containing(&mail.body, "/confirm")?).await?;
//!     Ok(())
//! }
//! ```

mod actor;
mod data;
mod helpers;
mod json;

pub use actor::Actor;
pub use data::{link_containing, links, unique, unique_email};
#[doc(hidden)]
pub use helpers::Retry;
pub use helpers::{actor, current_trace, cx, defer, env, step, undo, visit};
pub use json::{AsJson, json_includes};
