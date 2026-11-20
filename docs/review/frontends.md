---
title: Suite frontends and harness
applyTo: "crates/apitest/src/script/**,crates/apitest/src/script.rs,crates/apitest/src/cucumber.rs,crates/apitest/src/cucumber/**,crates/apitest/src/harness.rs,crates/apitest/src/registry.rs,crates/apitest/src/context.rs,crates/apitest/src/suite.rs,crates/apitest-macros/**,crates/apitest-py/**,npm/cli/types/**,demo/suites/**"
summary: Rust, Rhai, JS and Python see the same cx API and semantics, Gherkin compiles onto the same runner; cleanups always run; nextest locks hold across processes.
---

# Suite frontends and harness

- Rust, Rhai, JavaScript and Python suites get the same `cx` API and semantics. A new `cx`
  method lands in every frontend in the same PR, with the JS declaration in
  `npm/cli/types/index.d.ts` (type-checked by `task npm:check`) and the Python wrapper.
- Values crossing a language boundary are `serde_json::Value`. Do not add frontend-specific
  value types.
- Cleanups (`cx.defer`, fixture teardowns, `undo`, `rollback`) run in LIFO order even when a
  test fails, panics or times out. Changes to the runner or context keep a test that proves
  it for the failing path.
- `serial` and `resources` must hold across processes under nextest (`nextest_locks` test).
  Lock changes run `cargo nextest run` with several threads, not only `cargo test`.
- Gherkin `.feature` files compile into ordinary `Suite`s (`cucumber.rs`); they never get a
  runner, scheduler or report format of their own. Each Gherkin step runs as a `dsl::step`,
  step names stay the text as written (`${..}` and `{{..}}` unresolved), and every step is
  matched at load time, so undefined and ambiguous steps fail the load with `file:line`.
- A built-in step that sends a request goes through an `Actor` and `Request`, so validation,
  auth, tracing and redaction apply as in the other frontends. Steps that learn a credential
  from a variable or a response (bearer token, header value) register it with the run's
  `Redactor` right away.
- Values from responses and variables never change what a step does: JSON is parsed before
  `${..}`/`{{..}}` are resolved inside its strings (`World::interpolate_json`), values in a
  raw path are percent-encoded segments (`World::interpolate_path`), inserted text is never
  scanned for templates again, and an actor's credentials go only to its service's origin.
- The `--tags` parser and the `.feature` loader handle untrusted input: keep the property
  tests in `tests/it/props.rs` and the `gherkin` fuzz target passing, and keep arithmetic on
  user-given durations saturating.
- The attribute macros generate documented items only, so user crates with `missing_docs`
  stay clean.
