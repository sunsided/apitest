---
applyTo: "crates/apitest/src/contract.rs,crates/apitest/src/contract/**,crates/apitest/src/runner/drift.rs,crates/apitest-cli/src/drift.rs,crates/apitest/src/http.rs,crates/apitest/src/runner.rs,crates/apitest/schemas/**,demo/specs/**"
---

<!-- generated from docs/review/contracts.md by scripts/review-index.py; edit the source instead -->

# Contracts and drift

- Specs come from the network (`live_spec`) and are untrusted. Loading must not panic or
  recurse without bound (`$ref` inlining is depth-limited); the `spec_from_bytes` and
  `drift_diff` fuzz targets cover this. A change to loading or diffing runs them
  (`task fuzz:smoke`).
- Drift is judged from the client's side: a request must still be accepted by the live spec,
  and a response must not produce anything the client does not expect. A change that flips
  a severity (`Info`/`Warning`/`Breaking`) needs a test that shows the case.
- With a lockfile, only operations and fields the suites used are compared. Without one, all
  pinned operations are. Do not widen the used-surface diff silently.
- A spec never drifts from itself (property test and fuzz assertion). Keep it that way.
- Lockfile and config formats change only together with their NOVA schemas in
  `crates/apitest/schemas/` and a schema version bump.
