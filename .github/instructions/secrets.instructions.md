---
applyTo: "crates/apitest/src/redact.rs,crates/apitest/src/dotenv.rs,crates/apitest/src/runner/vars.rs,crates/apitest/src/auth.rs,crates/apitest/src/dsl/actor.rs,crates/apitest/src/report.rs,crates/apitest/src/context.rs,crates/apitest/src/script/**,crates/apitest/src/cucumber/world.rs,crates/apitest/src/cucumber/builtin.rs,crates/apitest-cli/src/main.rs,crates/apitest-cli/src/vars.rs,crates/apitest-cli/src/call.rs,crates/apitest-cli/src/call/**"
---

<!-- generated from docs/review/secrets.md by scripts/review-index.py; edit the source instead -->

# Variables and secrets

- Secrets never reach output unmasked. Anything written to a terminal, a log or a report
  (`cx.log`, script `print`, `SuiteReport`, `LoadReport`, drift messages, CLI output and
  errors) goes through the run's `Redactor`, or `redact::console` where no run is at hand.
  A new output path needs a test that a registered secret comes out as `***`.
- A credential is registered with the redactor as soon as it is known: the variables behind
  secret fields before interpolation can fail, the resolved value, and the header a login or
  token request returns (also on a cache hit).
- Errors never quote what may hold a secret: `${NAME}` templates (give a byte position),
  `.env` lines or values (give the kind of error and a position), response bodies of a login
  only through a report or the CLI, which mask them.
- The process environment is only read. Variables from `.env` files live in
  `RunConfig::env_file_vars`; never call `std::env::set_var` (it is `unsafe` in edition 2024).
- The lookup order is `--var`, the environment, `.env` files, `vars:`. A change to it, or to
  the up-front `check_variables`, updates the README section on variables.
- `.env` parsing and redaction handle untrusted input: keep the property tests in
  `tests/it/props.rs` (no registered secret survives, redaction is idempotent, errors quote
  no value) and the `dotenv` fuzz target passing.
