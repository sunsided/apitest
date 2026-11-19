---
applyTo: ".github/workflows/**,Taskfile.dist.yaml,**/Taskfile.dist.yaml,.pre-commit-config.yaml,scripts/**"
---

<!-- generated from docs/review/ci.md by scripts/review-index.py; edit the source instead -->

# CI, Taskfile and hooks stay in sync

- The `ci` task in `Taskfile.dist.yaml` mirrors the jobs in `.github/workflows/ci.yml`
  (`branch`, `fmt`, `clippy`, `style`, `doc`, `test`, `features`, `deny`, `msrv`, `npm`,
  `python`, `fuzz`). A change that adds, removes or renames a CI job, or changes its flags,
  makes the same change to `task ci` and the task it calls, and the other way round. Only
  `coverage` is CI-only.
- Test, coverage and doctest invocations exclude `apitest-py` (`TEST_SCOPE` in the
  Taskfile): its pyo3 test binary cannot link without libpython. The `python` job covers it
  through maturin. Keep the exclusion in every variant, including `publish-crates-io.yml`.
- Local hooks in `.pre-commit-config.yaml` call Taskfile tasks. They must not duplicate
  commands.
- Every `uses:` is pinned to a full 40-character commit SHA, with a `# vX.Y.Z` comment. Reject
  tag or branch refs.
- The workflow sets `permissions: {}` at the top, and each job sets the least privilege it
  needs. Every `actions/checkout` sets `persist-credentials: false`.
- No `pull_request_target`. No `${{ github.event.* }}` or other untrusted expressions
  directly in `run:`. Pass them through `env:`.
- Do not rename `publish-crates-io.yml` with its `crates-io` environment, or
  `release-binaries.yml` with its `npm` environment. crates.io and npm trusted publishing are
  bound to these names.
- `scripts/check-branch.sh` is the single definition of allowed branch names. It is used by
  the Taskfile, the hook and the CI `branch` job.
- New jobs that should gate merges must also be added to branch protection. Mention that in
  the PR.
