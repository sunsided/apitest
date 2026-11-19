# Contributing to apitest

Thanks for helping out. This file explains how the repository is set up and what a change
needs before it can be merged.

## Setup

1. Install Rust via [rustup](https://rustup.rs/). The MSRV is declared in `Cargo.toml`
   (`rust-version`); development happens on current stable.
2. Install [Task](https://taskfile.dev/installation/). All commands below go through it. Run
   `task` to list them.
3. Optional tools: the tasks tell you when one is missing.
   - [cargo-nextest](https://nexte.st/) for faster tests (and the cross-process lock tests);
   - [cargo-deny](https://github.com/EmbarkStudios/cargo-deny) and
     [cargo-hack](https://github.com/taiki-e/cargo-hack);
   - [cargo-llvm-cov](https://github.com/taiki-e/cargo-llvm-cov) for coverage;
   - [typos](https://github.com/crate-ci/typos);
   - [actionlint](https://github.com/rhysd/actionlint) and [zizmor](https://docs.zizmor.sh/);
   - nightly Rust plus [cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz) for fuzzing;
   - Node.js for the npm package (`task npm:check`, `task npm:smoke`) and Python 3.10+ for the
     bindings (`task py:test`);
   - [GitVersion](https://gitversion.net/) 7 (`dotnet tool install -g GitVersion.Tool`) for
     releases; set `DO_NOT_TRACK=1` to opt out of its telemetry.
4. Install the git hooks with [prek](https://prek.j178.dev/) (`prek install`), or with
   `pre-commit install`. Fast checks run on commit; clippy and tests run on push.

## Everyday commands

| Command | What it does |
|---|---|
| `task test` | Tests (nextest if installed) and doctests |
| `task test:prop` | Tests with 10,000 proptest cases |
| `task lint` | clippy (pedantic), separator ban, README link check, typos, review index, workflow lints |
| `task fmt` | Format the code |
| `task doc` | Build the docs, private items included, with warnings as errors |
| `task ci` | Everything CI runs. Run it before you open a PR |
| `task py:test` | Build the Python extension with maturin and run pytest |
| `task npm:smoke` | Pack the npm packages for this machine and run the CLI through them |
| `task fuzz:smoke` | Run every fuzz target briefly (needs nightly) |
| `task clean` | Remove build output (fuzz corpora and crash artifacts are kept) |

## Code style

- Every item has a doc comment, private items included.
- No `mod.rs`: use `foo.rs` plus `foo/`.
- No `// ----` separator comments. Split the file into modules instead.
- Keep functions small, and extract helpers with descriptive names.
- `clippy::pedantic` must pass. If a lint is wrong in one place, use
  `#[expect(lint, reason = "...")]` on that item. Never `#[allow]`.
- Unsafe code is forbidden (`#![forbid(unsafe_code)]` in every crate).
- README and doc links use absolute URLs (crates.io and npm render READMEs outside GitHub).

Review rules by area are listed in
[docs/review/README.md](https://github.com/sunsided/apitest/blob/main/docs/review/README.md).

## Branches

This repository uses **GitHub Flow**:

- `main` is always releasable.
- Work happens on `feature/<topic>` branches. Bug fixes are feature branches too.
- Open a PR into `main`.

No other prefixes (`fix/`, `chore/`, `deps/`, ...). `task lint:branch`, the pre-push hook and
CI check the branch name. Dependabot's `dependabot/*` branches are the only exception.
Copilot's `copilot/*` branches are renamed to `feature/*` before anyone works on them.

## Commits and pull requests

- Commit subjects use the **imperative mood**, like Git itself: "Add parser for pair lists",
  "Fix overflow in tile index". Do not write "Added ...", "Adds ..." or `feat: ...`.
- Keep the subject to 72 characters or fewer, capitalized, with no trailing period. Then add
  a blank line and a body that explains *why*. The `commit-msg` hook checks the subject.
- Every PR description says what changed and why: release notes are generated from merged
  PRs. Mark incompatible changes as **Breaking**.
- PRs are merged with a merge commit, or squashed using the PR title and description. Write
  both with care. Head branches are deleted after the merge.
- Keep PRs focused. CI must be green.
- Do not bump versions by hand. Releases are tag-driven (see below).

## Releases

Versions follow [Semantic Versioning 2.0](https://semver.org/spec/v2.0.0.html), as Cargo
implements it (in `0.x`, a minor bump is breaking). All crates, the npm packages and the
Python package share one version.

Versions come from GitVersion (`GitVersion.yml`): `main` is `X.Y.Z-alpha.N`, `release/*` and
`hotfix/*` branches are `X.Y.Z-beta.N`, and a `vX.Y.Z` tag is the stable release.
`Cargo.toml` holds the next `-alpha` placeholder; set the following one after a release.

To release, run `task release:tag` on `main` (`PRE=1` tags the current pre-release instead).
It asks before it creates and pushes the tag. The tag then triggers:

- `publish-crates-io.yml`: verifies the tagged commit and publishes `apitest-macros`, `apitest`
  and `apitest-cli` to crates.io via trusted publishing, from the protected `crates-io`
  environment;
- `release-binaries.yml`: builds auditable binaries for Linux (glibc and musl), macOS and
  Windows with SBOMs and attestations, creates the GitHub Release with generated notes, and
  publishes `@apitest/cli` and its platform packages to npm via trusted publishing, from the
  protected `npm` environment.

Verify a downloaded binary with `gh attestation verify <archive-or-binary> --repo sunsided/apitest`.

## License

By contributing, you agree that your contributions are licensed under EUPL-1.2 OR MIT OR
Apache-2.0, the same terms as the project. See the License section of the README.
