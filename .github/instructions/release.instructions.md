---
applyTo: "**/Cargo.toml,GitVersion.yml,.github/workflows/publish-crates-io.yml,.github/workflows/release-binaries.yml,npm/**,crates/apitest-py/pyproject.toml,deny.toml"
---

<!-- generated from docs/review/release.md by scripts/review-index.py; edit the source instead -->

# Crates and releases

- Versions are SemVer 2.0. Pre-releases (`-beta.1`) are fine, but build metadata (`+...`) is
  not.
- GitVersion owns the version. `[workspace.package]` holds the next `-alpha` placeholder and
  every member uses `version.workspace = true`; the workspace path dependencies carry the
  same version. Stamping always runs `scripts/zero-version.sh` before `cargo set-version`.
  Flag hand-edited versions in feature PRs.
- `npm/cli/package.json` follows the same version (`npm/scripts/set-version.mjs` at release
  time); `pyproject.toml` takes it from Cargo (`dynamic = ["version"]`).
- `apitest`, `apitest-macros` and `apitest-cli` publish to crates.io. They have
  `description`, `license`, `repository`, `readme`, `keywords` (at most 5) and `categories`
  (valid crates.io slugs), and inherit `rust-version`. `apitest-py` keeps `publish = false`.
- Adding or renaming an npm platform touches `npm/cli/platforms.json`, the
  `optionalDependencies` in `npm/cli/package.json` and the target matrix in
  `release-binaries.yml` together.
- New dependencies must pass `cargo deny check`: allowed licenses, no advisories, no git or
  unknown-registry sources. Advisory ignores carry a reason.
- `rust-version` is a signal, not a contract. Raising it is allowed when a dependency or a
  feature benefits. It needs a minor release and a changelog note, never a patch release.
- Every PR description has a "what changed and why" section: release notes are generated
  from merged PRs.
