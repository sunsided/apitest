---
title: Rust code
applyTo: "**/*.rs"
summary: House style: docs on every item, no mod.rs, no separators, blank lines, helper extraction, pedantic clippy, no unsafe.
---

# Rust code

- Every module (`//!`), type, field, variant, function, constant and macro has a doc
  comment, private and `pub(crate)` items included. Docs state intent, invariants, units and
  error/panic conditions (`# Errors`, `# Panics`). They do not restate the name.
- No `mod.rs`: modules with children are `foo.rs` plus `foo/`. Integration tests on the
  standard harness live under `crates/apitest/tests/it/`. Tests with `harness = false`
  (libtest-mimic: `macro_harness`, `care_flow`, `nextest_locks`, `billing_harness`) stay
  separate binaries, because each owns its `main`.
- No separator comments (`// -----`, `// =====`). If a file needs sections, split it into
  submodules.
- Leave a blank line after the closing `}` of a block statement when another statement
  follows, and between logical phases of a function. No lumped code.
- One level of abstraction per function. If a block needs a "what" comment, extract it into
  a function named after that comment.
- `clippy::pedantic` clean. Suppression only with a scoped
  `#[expect(lint, reason = "...")]`. Never `#[allow]`, and never crate-level suppression.
- `#![forbid(unsafe_code)]` in every crate.
- New public API in `apitest` that parses input (specs, NOVA files, headers, thresholds) gets
  a property test in `tests/it/props.rs` or a fuzz target in `fuzz/`.
- Machine-read output stays plain text: `SuiteReport::render`, `junit_xml`, the JSON
  report, `assert_passed` and the libtest harness messages carry no colours or miette
  glyphs. miette rendering belongs in `SuiteReport::render_terminal` and the CLI only, behind
  the `diagnostics` feature.
