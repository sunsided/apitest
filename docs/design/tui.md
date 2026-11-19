# Design: `apitest tui`, a visual runner and workbench

Status: proposal · Scope: optional CLI subcommand, behind a cargo feature

## Why, and what it is not

`apitest run` prints a report when a suite is done. While it runs, you see `cx.log` lines and nothing else: not which test is running, which step is slow, or how this run compares to the last one. Rerunning one failing test means editing `--only` and paying for suite setup again, and seeing what a request actually sent means adding logging.

`apitest tui` is a full-screen terminal UI (ratatui) over the same library. Its main job is to be a **visual test runner and rerunner** with live timings. Besides that, it gives the existing commands (`call`, contract drift, `load`, variable resolution) an interactive view.

Non-goals:

- **Not a replacement for `apitest run` in CI.** JUnit, JSON and lockfile output don't change, and the TUI adds nothing to them.
- **Not an editor.** Tests stay files. The TUI opens `$EDITOR` at a test, and watch mode picks up the change.
- **Not for agents.** Agents keep using the CLI and files. The [event stream](#event-stream) gives them live progress in a form they can read (`apitest run --events ndjson`).

## Command

```text
apitest tui [FILES]... [--config apitest.nova] [--only NAME]... [--watch]
            [--history-dir .apitest/runs] [--no-history] [--capture headers|bodies]
            [--cargo [--test NAME]...] [--socket-dir DIR] [--no-agent]
            [RUN AND DRIFT FLAGS, as for apitest run]
```

- Same config, `.env` handling, variable check and drift check as `apitest run`, through the same code. If the variable check fails, the TUI doesn't start a run. In M1 it shows the same error `apitest run` prints (the names that are missing or empty), on a full-screen error view, and exits with `q`. Once the Env tab exists (M2), it starts there instead, with those names in red.
- **stdin and stdout must be terminals.** stdout to draw on, stdin for key events (with stdin piped or closed, the UI would draw but couldn't be driven or quit). Otherwise the command fails with ``apitest tui needs a terminal; use `apitest run` (add `--events ndjson` for live progress)``. It does not quietly turn into `apitest run`.
- **Agents.** When `detect-coding-agent` reports `kind: agent` with medium or high confidence (see [mcp-exploration.md](mcp-exploration.md#agent-aware-cli-output)), the command fails with the same pointer. `--no-agent` (or `APITEST_AGENT=0`) overrides that. These are the global `--agent` / `--no-agent` flags that [mcp-exploration.md](mcp-exploration.md#agent-aware-cli-output) adds to every command, not a TUI-only switch.
- Lives in a new crate `crates/apitest-tui` (library). `apitest-cli` gets a `tui` feature that depends on it, so a CLI build without the feature doesn't pull in ratatui. Dependencies: `ratatui` 0.30 (MSRV 1.88), `crossterm` 0.29, `notify` 8 for watch mode. All of them build on the workspace MSRV of 1.95.

## Layout

Five tabs. **Run** is the default and gets most of the effort. The others are thin views over code that exists already.

```text
 apitest  [Run]  Call  Contracts  Load  Env            run #14 · 12.4 s · ▲ 0.8 s vs #13
┌ Suites ─────────────────────────────────┐┌ billing::create_invoice ─────────── ✗ 1.92 s ┐
│ ▾ billing                    6/7  4.1 s ││ Steps                                       │
│   ✓ list_invoices            212 ms     ││  ✓ create customer          180 ms          │
│   ✗ create_invoice          1.92 s  ▲   ││  ✓ create draft             240 ms          │
│   ⠋ void_invoice             0.8 s…     ││  ✗ finalize               1.50 s  ▲ +1.1 s   │
│   · refund (queued)                     ││    POST /invoices/{id}/finalize  500 1.49 s │
│ ▸ care_flow                  3/3  6.2 s ││                                             │
│ ⚠ [contract] hub   compatible           ││ × step 'finalize' failed                    │
│                                         ││ ├─▶ POST /invoices/42/finalize: expected    │
│                                         ││ │   201, got 500                            │
│                                         ││ ╰─▶ body: {"error":"ledger timeout"}        │
├ Timeline ───────────────────────────────┤│                                             │
│ list_invoices    ██                     ││ Requests  Response  Logs  History           │
│ create_invoice     ███▒▒▒▒▒▒▒▒▒▒        ││                                             │
│ void_invoice       ███████…             │└─────────────────────────────────────────────┘
└─────────────────────────────────────────┘
 r rerun · f failed · a all · x stop · w watch: off · s sort · t timings · b baseline · ? help
```

- **Left:** suites as a tree (suite → test → step → request), with live status (queued, running with a spinner and elapsed time, passed, failed, skipped), duration, and a ▲/▼ marker when a duration differs noticeably from the [baseline](#history-and-comparison). Suite-level entries (`[contract] name`, `[setup]`, `[teardown]`) appear where the harness shows them today.
- **Below it:** a timeline of the running suite. Each test is a bar on a shared time axis, so serial barriers, parallel overlap and waits for `resources` locks are visible. A test waiting for a lock is drawn hatched (`▒`).
- **Right:** the selected item. For a test: the step trail with timings, the failure as a miette diagnostic, and sub-tabs for requests, the selected response, log lines and the test's history.

## Run tab

### Running and rerunning

This is the complete key map of the Run tab. The key bar and the `?` help screen are generated from it, listing only the keys the build supports. The [prototype](#effort) has `r`, `f`, `a`, `/`, `e`, navigation, `?` and a `q` that waits for a running run to finish. `x`, `w`, `s`, `t` and `b` come with cancellation, watch mode, timing views and history.

| Key | Action |
|---|---|
| `r` | Rerun the selected test, or every test of the selected suite |
| `f` | Rerun the tests that failed in the last run |
| `a` | Run everything again |
| `x` | Stop the current run. Cleanups still run (see [Cancellation](#cancellation)) |
| `w` | Watch mode on or off |
| `/` | Filter the tree by name, like `--only` |
| `s` | Sort tests within a suite: declaration order (default), duration, or delta to the baseline |
| `t` | Cycle the [timing views](#timings) in the right pane |
| `b` | Pin the selected run as the baseline, or unpin it (see [History](#history-and-comparison)) |
| `e` | Open `$EDITOR` at the selected test |
| `Tab`, `←` `→`, `Enter` | Move between panes, collapse and expand tree nodes |
| `1`–`5` | Switch tabs |
| `?` | Help |
| `q` | Quit. During a run it asks first (see [Cancellation](#cancellation)) |

A rerun is a **fresh `run_suite`** that selects exactly the chosen test ids. Suite and VU fixtures, `before_all` and the drift check run again. That costs a little time but keeps fixture semantics identical to `apitest run` and nextest. If the TUI kept a suite scope alive across reruns, tests would see state left by earlier runs, and teardown order would depend on how the user had clicked. A "warm suite" mode is listed under [open questions](#open-questions).

`RunConfig::only` matches substrings today, so selecting `create` would also run `create_invoice_twice`. Reruns need exact ids. This design adds `RunConfig::select: Selection` (`All`, `Matching(Vec<String>)` for the existing `--only`, `Exact(BTreeSet<String>)` with harness ids). `only` stays as it is for compatibility, and setting both is an error, not a silent precedence.

### Watch mode

`notify` watches the run's suite files, `apitest.nova`, the `.env` files the run read, and local spec files of pinned contracts and drift targets. Changes are debounced (300 ms), then:

- a changed suite file reruns that suite (the selection filter still applies);
- a changed `apitest.nova`, `.env` file or spec reloads the config, checks variables again and reruns everything selected.

If a run is still going, it is cancelled first and its cleanups run before the new run starts. Watch mode covers suites the CLI loads (`.rhai`, `.js`). Rust suites are compiled into a test binary, see [Rust suites](#rust-suites).

### Timings

Every test, step, fixture, hook, cleanup and request is timed by the runner already, or will be with the [event stream](#event-stream). The TUI collects them into three views (`t` cycles through them):

- **Waterfall** for one test: steps and the requests inside them on a time axis, with fixture setup and cleanups shown as their own bars. It shows where the time goes, for example a `step` that is mostly an `eventually!` loop polling a mail mock.
- **Slowest:** the top N steps and requests across the run, sorted by duration, each with its baseline delta.
- **Percentiles per operation:** count, p50, p90, p99 and max for each `contract:operationId`, or for each `METHOD path-template` for requests without a contract. These use `hdrhistogram`, as `load` does. Grouping by the path template (`/invoices/{id}`), not the sent URL, keeps the number of rows small and keeps ids and tokens in paths out of the table.

Timings come from the library's `Instant` measurements, the same numbers as `StepReport::duration_ms` and `Response::elapsed_ms`, so the TUI, JSON report and JUnit agree.

### History and comparison

A run is written to `--history-dir` (default `.apitest/runs/`, which also needs a `.gitignore` entry) when its `RunFinished` event arrives (a merged `--cargo` run: when cargo exits, see [Rust suites](#rust-suites)), as one JSON file: the `SuiteReport`s, the per-request timings, the run's **fingerprint** (below) as `fingerprint: Option<String>` with its readable `fingerprint_inputs`, the selection as a `RecordedSelection` (see [Rust suites](#rust-suites)), the run's suite set, its `mode` (and in `--cargo` mode `compiled`, `cargo_targets`, `build_profile`, `target` and `rustflags_env`), the host as `host: Option<String>`, the cause chains in a `chains` field (below), the run's `outcome` (a `RunOutcome`, as in `RunFinished`), the final `Dropped` counts as `dropped: { requests, logs, lifecycle }`, `dropped_exact: bool` (`true` unless a merged `--cargo` run had a stream end without `RunFinished`, see [Rust suites](#rust-suites); absent means `true`), `truncated: bool` (`true` if the stream carried `Truncated`; absent means `false`), and the git revision if there is one. The file carries `version: u32` (1 at first). Required in version 1: `version`, the reports, `outcome`, `dropped`, `selection` and the suite set. Every other field has a default when absent: `fingerprint_inputs` an empty list (it is also empty whenever `fingerprint` is `None`, since the per-hash inputs are then in `fingerprints`), `chains`, `fingerprints` and `hosts` an empty map, `fingerprint`, `host`, `cargo_targets`, `build_profile`, `target` and the git revision `None`, `rustflags_env` an empty map, `mode` `in_process`, `compiled` an empty list, `dropped_exact` `true`, `truncated` `false`. A field added later, including a new counter inside `dropped`, must come with a default, so it doesn't bump the version. Only a change that old readers would misread bumps `version`. A file with a higher version than the TUI supports, or missing a required field, or that doesn't parse, is listed as `unreadable` with the reason, can't be opened or pinned, and is never a baseline candidate. A merged `--cargo` run whose processes disagree also stores `fingerprints: BTreeMap<String, FingerprintEntry>` and `hosts: BTreeMap<String, Vec<String>>` (see [Rust suites](#rust-suites)). The reports skip their cause chains when serialized, so `chains` holds them next to the reports, keyed by suite name (`setup_chain`) and by suite name and test id (`Outcome::Failed::chain`), the same data as `TestFinished.chain` and `SuiteFinished.setup_chain`. A run opened from history therefore renders the full miette diagnostic, not only the top-level message. The last 50 runs are kept. `--no-history` writes nothing. The default directory is printed on the first run, so it isn't a hidden location.

- **Fingerprint.** A SHA-256 over everything in the resolved `RunConfig` that changes what is measured: `base_url` and `services`, the pinned spec hashes, `test_timeout`, `hook_timeout`, `max_parallel`, `validate_every`, whether tracing is on, the `--capture` mode, and every variable as `name=value`. Values of variables the `Redactor` treats as secrets go in as their name only, so a changed password doesn't split the history and no secret is hashed (an unsalted hash of a short password could be guessed). The fingerprint is stored with the readable list of what went in, minus secret values, so the TUI can name the field that differs. The **selection is not part of it** and is stored separately, so a rerun of one test compares against the full runs before it. A test's duration is measured from `TestStarted`, after it got its resource locks and concurrency slot, so time spent waiting for neighbours is not in it. Server-side contention and warm connection pools still differ: a test measured alone runs against a quieter server than one measured among seven parallel neighbours. So only **full runs** are **baseline candidates**: selection `All` over the whole comparison basis defined below. The history file records the run's suite set next to the selection. The comparison basis is a set of **suite names**: the names of the suites in the files the current invocation loads when the TUI picks baselines, that is the `FILES` given on the command line, or else the `suites:` list from the config, which holds literal paths (nothing expands globs). With `FILES`, a full run covers exactly those files. The history file records the suite names of the run, so both sides use the same key. In `--cargo` mode, the suites live in the compiled test binaries, and which tests exist depends on everything that shapes the build: packages, features, the target, and `cfg` flags from the command line, the environment or cargo's config files. Rather than reconstruct that from cargo's inputs, the TUI asks the result. After the build and before running, it lists the tests with the same build arguments and no filter (`cargo nextest list --message-format json`, or each test binary's `--list` under `cargo test`) and records them as `compiled`. The list invocation runs with `APITEST_EVENTS` removed from its environment, so its processes never connect to the run's socket and the ids come from stdout only. If the list step fails, the TUI doesn't start the tests: it shows the error (the command and its output) and records nothing, as for a build error. If it succeeds with no tests, the run goes ahead but is not comparable, and the header says `no tests compiled`. Each id is prefixed with its binary (`<package>::<target>::<test id>`), so a test id that exists in two targets stays two entries. This happens once per run in the TUI, not in every test process. In `--cargo` mode the basis is the current run's `compiled` set, so two builds that compile different tests never share a basis, however the difference came about, and the current run is its own basis without needing an earlier one. The run also records `cargo_targets` for display: the canonical workspace manifest path, the packages and the test target names. The build profile (`--release`, `--profile` under `cargo test`, `--cargo-profile` under nextest) and the target triple are recorded as `build_profile` and `target`. Under nextest the triple comes from the platform in the list output. Under `cargo test`, the TUI builds with `--no-run --message-format json` first, which it needs anyway to find the binaries for `--list`: cargo puts the binaries under `target/<triple>/` exactly when a target is set (by flag, environment or config), so the triple is read from that path, and when there is none it is the host triple from `rustc -vV`. Both must match for a candidate, since they change the client's own speed. Codegen flags in `RUSTFLAGS` are deliberately not part of the key, because they rarely change speed as much as the profile does. For display only, the history file stores `rustflags_env: BTreeMap<String, String>`, the rustflags variables cargo reads (`RUSTFLAGS`, `CARGO_ENCODED_RUSTFLAGS`, `CARGO_BUILD_RUSTFLAGS`, `CARGO_TARGET_<triple>_RUSTFLAGS` and `..._ENCODED_RUSTFLAGS`) verbatim as set; flags from cargo config files aren't included. The header says `rustflags differ` when the map of any run in the baseline differs from the current run's. Candidacy in `--cargo` mode also requires the run to be complete: the build and the list step succeeded, cargo exited without a build error, and every process ended with `RunFinished`. A failed build has no `compiled` set and therefore no basis, and it can't be pinned. History files also record `mode` (`in_process` or `cargo`), and a candidate must have the same mode as the current run, so a Rust suite and an interpreted suite that share a name never compare against each other. A run is a full run only if it has the same `mode` as the current run and its recorded suite-name set equals the basis, or in `--cargo` mode it is complete, its `compiled` set equals the basis and its `build_profile` and `target` match. When a suite is added, removed or renamed, earlier runs stop being candidates and the header says so (`no baseline · suite set changed`) until new full runs exist. A test keeps no baseline from runs taken under a different suite set, even if the test itself didn't change, because its neighbours did. This also settles a shared `--history-dir` (see [Open questions](#open-questions)): runs written by another configuration or checkout with a different suite set are listed and can be opened, but are never candidates, and the header says `suite set changed` if no candidate is left. Runs with any narrower selection are **compared against** baselines but never become one: `Exact` (in-process reruns from the TUI), `Matching` (`--only`), `External` with any value (a `--cargo` run whose test set cargo or nextest narrowed, including the TUI's own `--exact` reruns; [Rust suites](#rust-suites) lists the conditions), and watch-mode reruns of one suite, which have selection `All` but not every suite. A handful of partial runs can't pull a test's baseline toward its quiet-server time and hide a regression.
- **Files and ids.** A file is named `<YYYYMMDDTHHMMSS.mmm>Z-<8 hex chars>.json` in UTC, for example `20261119T145306.123Z-3f9a0c1e.json`. The fixed width makes the names sort by time. Two TUIs (or a TUI and a later `apitest run --history`) on the same checkout never pick the same name. It is written to a temporary file in the same directory and then renamed, so a reader never sees half a file. The short `#14` in the UI is only a display number, counted from the files present when the TUI starts, and nothing stores it. Pruning to 50 runs deletes the oldest files by name, and a file that is already gone is not an error.
- **Cancelled runs** are written too, with outcome `cancelled`, with the tests that never finished as `Skipped { reason: "cancelled" }`. They show up in the history list but are never used for baselines.
- **Baseline:** by default, for each test, the median of its durations in the last 5 runs where **that test passed**, the run wasn't `cancelled`, `crashed` or truncated, and the run is a full run with a defined (not mixed) fingerprint and a single host, and has the same fingerprint and host as the current run. A failed test stops at the failing step, so its duration would make the baseline look faster than it is. Steps and requests use the same rule: only runs where the step or request succeeded count. The fingerprint covers endpoints, parallelism, lock waits and overhead, so comparing across it would mostly measure the environment. The git revision is not part of the key, because comparing across commits is the point. With `b`, the user can pin one run as the baseline instead. `b` refuses runs that are unusable in themselves (cancelled, crashed, truncated, mixed fingerprint or host, or not a full run, which includes any `External` selection) and names the reason. A run with a different fingerprint or host can be pinned, and the header discloses the difference (see below). A pinned run can still contain tests, steps and requests that didn't pass. A failed test stops at the failing step, so its duration isn't comparable. Their rows have no baseline value and show `—` with `failed in #13` or `skipped in #13` instead of a delta. The pin is stored as the run's **file name** in `<history-dir>/pinned`, next to the runs it names, never as `#N`, and the header shows the display number for that file. Pruning never deletes the pinned file. A pinned run with a different fingerprint or host is allowed, and the header names what differs (`vs #13 · max_parallel 8 → 4`, `vs #13 · host ci-runner-3`). The header always shows which baseline is active (`vs #13` or `vs median of 5`).
- **Deltas:** shown when a duration differs from the baseline by more than 20 % **and** more than 20 ms. Both thresholds can be set in `apitest.nova` (`tui.regression`). The two conditions keep a 3 ms request that takes 5 ms from being flagged. One run is noisy, and every delta shows how many runs its baseline is based on.
- **Runs pane:** a list of the kept run files, newest first, with display number, time, outcome, selection, fingerprint and host, and the pinned run marked. Unreadable files are listed here with their reason. `Enter` opens a run (its reports and timings replace the live view until `Esc`), and `b` pins or unpins the selected one.
- **History sub-tab:** one test's outcome and duration over the kept runs, as a sparkline, so a test that is flaky or slowly getting slower stands out.

What is written there is the redacted report, the same data as `--json`. Response bodies are never written to history, even with `--capture bodies`.

### Failures, requests and masking

- **Failures** are shown with miette, through `diagnostic::render_chain` without colour, inside a ratatui paragraph, with ratatui styling around it. This fits the decision that miette is for terminal output, and it avoids converting ANSI codes (`ansi-to-tui`). JUnit, JSON and the history files stay plain text.
- **Requests:** every request of the selected test, with method, path template, status, duration, operation, contract validation result and trace id. The trace id can be copied, to look the request up in the service logs.
- **Capture:** by default only that metadata is kept. `--capture headers` adds request and response headers, and `--capture bodies` also adds bodies, at most 64 KiB each, in memory only. The values of `Authorization`, `Proxy-Authorization`, any other header ending in `-Authorization`, `Cookie` and `Set-Cookie` are never captured. Header names are compared case-insensitively (`eq_ignore_ascii_case`, as `http.rs` already does), because requests and responses keep the original case of their header names. Everything else passes through the run's `Redactor` (secrets from `${…}` credentials and `redact:` patterns) **inside the library, before the event leaves the runner**. That gives the same guarantee as `run_suite`, which redacts its report before returning it. The TUI never receives unmasked values, so it can't leak them by accident.
- **Limits:** the `Redactor` masks values it knows (resolved secrets and patterns). A token the API hands out during the test, such as an `access_token` in a login response, is not a known secret. `--capture bodies` would show it, unless a pattern covers it. The TUI states this in the capture status line. Name-based masking, as planned for `apitest mcp`, would close the gap for both, and should be built once and shared.

## Event stream

Most of the work is in the library, and it's useful beyond the TUI. Today the runner only reports at the end: `run_suite` returns a `SuiteReport`, steps are collected on the test frame (`Ctx::record_step`) and handed back with the test report, and request timings are only aggregated for `load` (`Metrics::record_http`). Nothing is observable while a suite runs, except `cx.log` on stderr.

### What gets added

A module `apitest::events` with an `Event` enum and a sink on `RunConfig`:

```rust
pub struct RunConfig {
    // ...
    /// Receives run events as they happen. `None` (the default) costs nothing.
    pub events: Option<EventSink>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    RunStarted {
        version: u32,
        run: RunId,
        fingerprint: String,
        fingerprint_inputs: Vec<(String, String)>, // readable, secret values left out
        host: String,
        suites: Vec<SuitePlan>,                    // test ids, concurrency, resources
    },
    SuiteStarted { suite: String },
    DriftChecked { suite: String, reports: Vec<DriftReport> },
    ScopeStarted { path: String, kind: ScopeEvent },          // fixture, hook, test setup
    ScopeFinished { path: String, kind: ScopeEvent, ms: f64, error: Option<String> },
    TestWaiting { id: String, on: Vec<String> },              // resource locks, semaphore
    TestStarted { id: String },
    StepStarted { test: String, step: StepId, parent: Option<StepId>, name: String },
    StepFinished { test: String, step: StepId, ms: f64, error: Option<String> },
    Request(RequestEvent),                                    // see below
    Log { scope: String, line: String },
    CleanupFinished { scope: String, label: String, ms: f64, error: Option<String> },
    TestFinished { report: TestReport, chain: Vec<String> },          // chain: see below
    SuiteFinished { report: SuiteReport, setup_chain: Vec<String> },
    LoadTick(LoadSnapshot),                                   // once per second during `load`
    Dropped { requests: u64, logs: u64, lifecycle: u64 },     // cumulative, see backpressure
    RunFinished { run: RunId, outcome: RunOutcome, ms: f64 }, // the last event of a stream; a stream without it is incomplete (see End of run)
    Truncated { lifecycle_bytes: u64 },                       // see backpressure
    #[serde(other)]
    Unknown,                                                  // only when reading; never emitted
}

// JSON: {"status": "passed"}, ..., {"status": "crashed", "message": "..."}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RunOutcome {
    Passed,                    // every suite passed
    Failed,                    // a test, setup, drift check or teardown failed
    Cancelled,                 // stopped with `x`, by watch mode, or by Ctrl-C
    Crashed { message: String }, // the runner panicked; no SuiteFinished may exist
}

pub struct RequestEvent {
    pub test: Option<String>,
    pub step: Option<StepId>,
    pub method: String,
    pub path_template: String,       // before `{id}` substitution
    pub operation: Option<String>,   // "billing:createInvoice"
    pub status: Option<u16>,         // None: transport error
    pub ms: f64,
    pub trace_id: Option<String>,
    pub validation: Validation,      // NotValidated | Ok | Violation(String)
    pub capture: Option<Capture>,    // headers / bodies, already redacted
}
```

| Where | Emits | Change |
|---|---|---|
| `runner::run_suite_unmasked` (private; `run_suite` and `run_suites` go through it) | `SuiteStarted`, `DriftChecked`, `SuiteFinished` | a few lines |
| `runner::setup_fixtures`, `run_hooks`, `run_test` | `ScopeStarted`/`Finished` for fixtures, hooks, test setup and teardown | a few lines each |
| `runner::schedule` | `TestWaiting` before taking locks and a permit, `TestStarted` after | small |
| `runner::run_one` | `TestFinished` | one line |
| `dsl::step` | `StepStarted` **before** the work (today a step is only recorded when it ends), `StepFinished` | small, plus a task-local step stack for `parent` |
| `http::Ctx::send` | `Request`, next to the existing `Metrics::record_http` | small; keep the unrendered `path` |
| `Ctx::unwind` | `CleanupFinished` | small |
| `Ctx::log`, Rhai `print`/`debug`, JS `console.log` | `Log` **instead of** stderr when a sink is set | see below |
| `load::run_load` | `LoadTick` from a `Metrics` snapshot | needs a snapshot API on `Metrics` |

- **Step nesting.** `StepReport` is flat today ("outer steps after their inner ones"). Events carry `StepId` and `parent`, so the tree can nest steps. `StepReport` gets an optional `parent` too, so a finished report can be drawn the same way. It is marked `#[serde(skip_serializing_if = "Option::is_none")]` and only set while an event sink is active, so `apitest run --json` without `--events` stays byte for byte the same. With `--events`, the JSON report gains the field. That extends the schema without breaking readers that don't know it, and the [non-goals](#why-and-what-it-is-not) mean exactly this: nothing changes unless the new feature is used. JUnit has no place for it and doesn't change.
- **stderr.** A full-screen UI breaks as soon as anything writes to stderr. With a sink set, `Ctx::log` and the script `print` handlers (which go through `redact::console` today) send `Log` events instead. Without a sink, behaviour stays exactly as now. `--quiet` keeps its meaning: `Ctx::log` is gated on `config.quiet` today, and that gate stays in front of the sink, so `apitest run --quiet --events ndjson` emits no `Log` events from `cx.log`. Script `print` output isn't gated by `--quiet` today and stays that way.
- **Redaction at the boundary.** `EventSink::emit` runs every string through the run's `Redactor` before sending the event, the same way `SuiteReport::redact` does. Events are serializable, so the redaction tests can check the serialized form for leaked secrets.
- **Backpressure.** The runner never waits for a slow consumer. All events go through **one ordered queue**, so a consumer sees them in the order they were emitted. Events fall into two classes, which differ only in whether they can be refused:
  - **Lifecycle:** every variant except `Request` and `Log`. They are a handful per step, but that still grows with tests times steps, and `TestFinished` and `SuiteFinished` carry whole reports. So lifecycle events have their own byte limit, 64 MiB by default, far above what a normal run queues. If a consumer stalls long enough to reach it, the sink stops queuing anything except one `Truncated { lifecycle_bytes }` event, the final `Dropped` and the final `RunFinished`. All three are small and always queued. Lifecycle events lost after that are counted in `Dropped.lifecycle`, so the final `Dropped` says how much is missing. The run itself carries on, and its report and exit code don't change. A consumer that sees `Truncated` shows the stream as incomplete, and a history file written from it is marked truncated and never used for baselines.
  - **Bulk:** `Request` and `Log`, limited by a byte budget (8 MiB by default). The sink adds an event's serialized size to an atomic counter when it queues the event, and the consumer subtracts it when it takes the event out. A bulk event that would exceed the budget is not queued but counted. So a chatty script (a `print` loop, `debug` inside `eventually!`) can't grow memory without limit, and captured bodies count against the same budget: even with `--capture bodies`, the queue holds at most about 8 MiB of bulk events. Both limits are **per stream**. In `--cargo` mode, every test process has its own sink, so the harness uses smaller limits there (1 MiB bulk and 8 MiB lifecycle). Only running processes hold buffers. Under nextest, that is at most its `test-threads` (the CPU count by default), and a process that finishes gives its writer 2 s to flush before it drops the rest and exits. A stalled TUI therefore holds at most `test-threads × 9 MiB` across processes, which this design accepts.
  - `Dropped { requests, logs, lifecycle }` carries the **cumulative** counts since `RunStarted`. It is a lifecycle event, sent at most once a second while the counts grow and once more right before `RunFinished`, so a consumer can diff consecutive values and show the loss.
  - Under `load`, per-request events are off and `LoadTick` carries aggregates.
- **End of run.** `RunFinished` is the last event of a run on every path except a runtime shutdown (below), with an `outcome`: `passed`, `failed` (including a suite that failed during setup or the drift check), `cancelled`, or `crashed`. Test bodies, hooks and fixtures already run on their own tasks (`guarded`), where a panic becomes a test failure. For a panic in the runner itself, the public `run_suite` and `run_suites` both delegate to one internal `run_observed(Vec<Suite>, RunConfig)`. It owns the run's single `RunStarted` and `RunFinished`, and it is the only place that spawns. Suites inside it run through an internal function that is not wrapped again, so the wrapper never nests. `spawn` needs a `Send + 'static` future, so `run_observed` takes owned copies: `Suite` is already cloned that way today (`run_suite` does `Arc::new(suite.clone())`), and `RunConfig` is `Clone`. A panic in one suite ends the task, the remaining suites don't run, and the one `RunFinished` reports `crashed`. Spawning only happens when a sink is set, and the caller-facing contract is the same either way. After emitting `RunFinished { outcome: crashed }`, `run_observed` re-raises the panic with `std::panic::resume_unwind(payload)`. A caller of `run_suite` sees the panic it would have seen without a sink, so `apitest run` and `apitest run --events` exit the same way, and the stream's `crashed` matches the process result. `run_observed` never aborts its own task. Stopping a run goes through the cancellation token (see [Cancellation](#cancellation)), and the task then returns normally with the partial report, its unfinished tests skipped. Because this happens inside the library, every entry point that sets a sink gets it: `apitest run --events`, the TUI, and the harness under `--cargo`. (`apitest mcp` doesn't use the event stream.) When a sink is set, `run_observed` spawns the run's inner task and awaits its `JoinHandle`, so it is the one that sees a `JoinError`, and it checks it first. Without a sink there is no spawn and no `JoinError`: the run is awaited directly and a panic propagates as it does today. Nothing else holds that handle and stopping goes through the token, so `is_cancelled()` can only mean the runtime is shutting down. Then `run_observed` itself is being dropped too and can't emit anything: the stream ends without `RunFinished`. So the invariant every consumer relies on is: a stream is complete only if it ends with `RunFinished`. A stream that closes without it is incomplete, and the consumer decides its outcome: `cancelled` if the consumer itself requested cancellation, otherwise `crashed` with the message `stream ended without RunFinished`. An in-process TUI writes no history file for such a run. That is the one exception to writing cancelled runs (see [History and comparison](#history-and-comparison)): in-process, the stream only ends this way when the runtime shuts down, which happens when the TUI itself exits, and no report exists to write. Under `--cargo` the same rule applies per process, with the message `test process exited without RunFinished (tests: <ids>)` (see [Rust suites](#rust-suites)). For a `JoinError` with `is_panic()`, `into_panic()` gives the payload, which is downcast to `&str` or `String`, and any other payload becomes the message `panic with a non-string payload`. `RunFinished { outcome: crashed }` is emitted after that, outside the unwinding, so the sink never runs inside a `Drop` during a panic. That works even if no `SuiteFinished` was sent. It is queued after every event the run emitted, so when a consumer reads it, every accepted bulk event is already in front of it. History files are written at that point, so the per-request timings in them are complete apart from the counted drops, and the file records the final `Dropped` counts. The TUI must survive the re-raised panic. It is a separate crate and calls the public `run_suites` (with a sink set, which goes through `run_observed`) on a task of its own, and awaits that task, so the panic arrives there as a `JoinError` with `is_panic()`. That can happen before the TUI's consumer has read `RunFinished` from the queue, so on a `JoinError` the TUI first drains the queue until `RunFinished` or until the queue is closed, writes the history file from `RunFinished` as usual, and only then shows the run as crashed with the message from `RunFinished`. It stays open.
- **Cost when unused.** `events: None` is checked once per emit site, and no event is built.

### `apitest run --events ndjson`

With the stream in the library, the CLI can write it as newline-delimited JSON: `--events ndjson` to stderr, or `--events ndjson=path` to a file. It is machine output, so it's plain text without miette, like JUnit and JSON. This gives CI and agents live progress, and it's how a TUI could follow a run in another process (see [Rust suites](#rust-suites)). `RunStarted.version` is the event schema version (1 at first), the same for every run in a stream. It goes up only for changes that break readers: removing or renaming a variant or field, or changing a field's meaning. New variants and new optional fields don't change it, so a consumer ignores variants and fields it doesn't know, and refuses a stream with a version above the one it supports, with an error that names both versions. A serde derive rejects unknown variants by default, so `Event` has a catch-all `#[serde(other)] Unknown` unit variant, which serde supports for internally tagged enums. It is **one type** for writing and reading, not a mirror that could fall out of date: the TUI and other Rust consumers deserialize into the same `apitest::events::Event` the runner emits. None of the event types use `deny_unknown_fields`. `RunOutcome` is serialized like the existing `Outcome`, with a `status` tag in snake case. For `Event` to deserialize, every payload type derives `Serialize, Deserialize`. That means the existing `TestReport`, `StepReport`, `Outcome`, `SuiteReport`, `DriftReport` and its findings gain `Deserialize`. Every new type in `apitest::events` derives both from the start: `RunId`, `StepId`, `SuitePlan`, `ScopeEvent`, `RequestEvent`, `Validation`, `Capture`, `LoadSnapshot` and `RunOutcome`. The reports skip their cause chains when serialized (`Outcome::Failed::chain`, `SuiteReport::setup_chain`), so `--json` stays as it is. The chains travel next to the report instead, in `TestFinished.chain` and `SuiteFinished.setup_chain`, and the consumer puts them back. That way a TUI following another process still renders the full miette diagnostic. `Unknown` is kept off the wire by `EventSink::emit` itself: it returns early for `Unknown` in every build, with a `debug_assert!` on top to catch the bug in tests, and a unit test checks that no emitted stream contains it. Consumers in other languages are told the same rule in the schema docs.

### Cancellation

Stopping a run (`x`), restarting it in watch mode and quitting the TUI all need to cancel a running suite **and still run its cleanups**. The runner can't do that today. The README lists Ctrl-C as a known gap ("needs a cancellation token threaded through the runner"), and `apitest mcp` needs the same thing. This design depends on that work: a `CancellationToken` in `Shared`. On cancel, the runner stops scheduling tests, gives running test bodies a grace period (default 5 s) before dropping them, and then unwinds every open scope as usual. Cleanups are not cancelled, only bounded by `hook_timeout`. Every selected test still gets a `TestFinished`: tests that were never started and tests whose bodies were dropped report `Skipped { reason: "cancelled" }`, and the run ends with `RunFinished { outcome: cancelled, .. }`. Dropping a body drops its in-flight request on the client side, but the server may still be processing it. A cleanup can therefore race a request that is still running on the server, for example a `DELETE` arriving before the `POST` it undoes has committed. The test's cleanups run after the grace period, which makes this unlikely, but they can't rule it out. Quitting with `q` during a run asks first, and a second `q` waits for the cleanups and then exits.

## Other tabs

These reuse code the CLI already has, so each is small. They come after the Run tab.

- **Call.** An interactive `apitest call`: pick a service, an actor (`--as`) and an operation from the pinned spec, or a method and path, edit query, headers and a JSON body, and send. The response shows as a collapsible JSON tree with headers, timing and validation. Earlier calls stay in a history list. `S` appends the call as a step to a suite file, in the file's language. Auth follows `call`'s rules: an actor must be configured explicitly, and nothing falls back to a default login. Everything shown passes through the `Redactor`, as `call` does today.
- **Contracts.** A matrix of pinned contracts (rows) against drift targets (`live`, `dev`, `qa`, `prod`, `branch`, …, columns) with ok, compatible or breaking in each cell, from `RunConfig::check_drift_targets`. Opening a cell shows the diff for the used surface only. `u` writes `--update-lock` after a confirmation.
- **Load.** Starts a scenario like `apitest load` and shows RPS, error rate and latency percentiles as sparklines, refreshed every second from `LoadTick`, with thresholds drawn as lines. Needs a snapshot API on `load::Metrics`, which today only exposes a final report.
- **Env.** Every variable the run uses, with its source (`--var`, process environment, `.env.local`, `.env`, `vars:`) and the sources it hides. Secret values are masked, and missing or empty ones are red. Needs `RunConfig::lookup_source(name)`, a variant of `try_lookup` that also returns where the value came from.

## Rust suites

Rust suites (`#[apitest::test]`, `harness::main`) are compiled into a test binary, so the CLI can't load them as it loads `.rhai` and `.js`. A later step: `apitest tui --cargo [--test api]` runs `cargo test --test api` (or `cargo nextest run`) with `APITEST_EVENTS=unix:<socket path>#<token>` (on Windows, `tcp:127.0.0.1:<port>#<token>`), and the harness sets the event sink from that variable. The TUI then follows the run as it follows an in-process one. Rerunning a test passes `--exact <id>`, and watch mode watches the crate sources and rebuilds. With nextest, every test runs in its own process.

- **One connection per process.** The TUI listens on a socket, and every test process opens its own connection. A FIFO shared by all processes would interleave lines larger than `PIPE_BUF` (4 KiB on Linux), and `TestFinished`, `SuiteFinished`, `RunStarted` and `Log` events are routinely larger than that. With one stream per process, each stream is plain ndjson that needs no extra framing and no sizing to `PIPE_BUF`. The per-stream budgets from [Backpressure](#event-stream) still apply: bulk events past the budget are dropped and counted, and oversized lifecycle events are truncated.
- **Runs per process.** Each process sends its own stream, `RunStarted` … `RunFinished`, so the completeness rule from [End of run](#event-stream) holds per stream. The TUI merges the streams into **one run**, which ends when cargo exits, and writes **one history file** for it with the merged `SuiteReport`s. A per-process `RunFinished` writes nothing. The run's outcome is `crashed` if any process crashed, otherwise `cancelled` if the run was cancelled, otherwise `failed` if any process failed, otherwise `passed`. A connection that closes without `RunFinished` (a process was killed) counts as crashed, with the message `test process exited without RunFinished (tests: <ids>)`, and those tests are shown as crashed. The merge rules in full:
  - **Fingerprint.** `RunStarted` carries each process's `fingerprint`, its readable `fingerprint_inputs` and its `host`, so the TUI has everything an in-process run has. If all processes agree, the merged run has that fingerprint. If they differ (different `.env` files or pinned specs), the history file stores `fingerprint: None` and `fingerprints: BTreeMap<String, FingerprintEntry>`, keyed by fingerprint hash, with `FingerprintEntry { inputs: Vec<(String, String)>, tests: Vec<String> }` holding that fingerprint's readable inputs (the same shape as `fingerprint_inputs`) and the ids of the tests that **started** under it, collected from `TestStarted`, not from the plans in `RunStarted.suites` (a killed process planned tests it never started), the header names the input that differs, and the candidate rule (which requires a defined fingerprint) excludes the run. Hosts are handled the same way: if processes report different hosts, `host: Option<String>` is `None` and `hosts: BTreeMap<String, Vec<String>>` maps each host to the ids of the tests that started on it, so the header can name them, and the run is excluded too.
  - **Selection.** The processes don't know the whole run's selection, so the TUI records its own. The history file's `selection` is a `RecordedSelection`: the library's `All`, `Matching` and `Exact`, plus `External { filter: Option<String>, partition: Option<String> }` for what only cargo or nextest knows. The TUI records `All` only if it passed cargo and nextest no test-selection argument at all: no test filter, `--exact`, `--skip`, `--ignored` or `--include-ignored`, no nextest `--run-ignored` other than `default` (which is the same as leaving it out), no filter expression (`-E`), and no `--partition`. It also needs a nextest configuration that doesn't filter on its own: the TUI reads the active nextest profile (from `.config/nextest.toml`, `--config-file` and `--profile`), and a `default-filter` in it, or any `--tool-config-file`, makes the run `External` too. Otherwise it records `External`, with `filter` holding the selection arguments as given (joined with spaces) and `partition` the `--partition` value. A TUI rerun under `--cargo` passes `--exact <id>`, so it is recorded as `External { filter: Some("--exact <id>"), .. }`, not `Exact`. `External` is never a baseline candidate, whatever its values, because the TUI can't tell which tests a nextest filter or partition left out.
  - **Drops.** The run's `Dropped` counts are the per-field sums of each stream's final `Dropped`. A stream that ended without `RunFinished` contributes the last `Dropped` it sent, which can be older than its real loss. If any stream ended that way, the merged counts are a lower bound: the UI shows them as `≥ N dropped`, and the history file stores `dropped_exact: false`.
  - **Suites.** Merging is keyed by suite name. `SuitePlan`s are combined by test id, and only the first `SuiteStarted` for a suite is shown. `SuiteReport`s for one suite are combined test-wise: tests by id, `setup_error` is `Some` if any process had one, holding one line per process tagged with its test ids (`setup_chain` likewise). Teardown errors are concatenated with the same tags, and drift reports deduplicated by contract and target. A suite's `duration_ms` is the wall-clock time from its first `SuiteStarted` to its last `SuiteFinished`, not a sum. Timing views count each test, step and request once, from the process that ran it.
  - **Cancellation.** When the TUI itself cancelled the run, a connection that closes without `RunFinished` counts as cancelled, not crashed.
  - **Several crashes.** The merged `Crashed { message }` has one line per crashed process, each with its own message and test ids.
  - **Truncation.** If any stream was truncated, the whole run is marked truncated and is never a baseline candidate.
  - **Late events.** When cargo exits, the TUI waits up to 2 s for open connections to send `RunFinished`, then closes them, applies the rule for connections without `RunFinished`, and only then writes the history file. Cargo exiting with a build error before any process connected is `failed`, with the compiler output in the Logs view.
- **Socket and trust.** On Unix, the TUI creates a private directory under `--socket-dir` if given (Unix only: on Windows, passing `--socket-dir` is an error, because there is no socket file), else under the first of `$XDG_RUNTIME_DIR`, `$TMPDIR` and `/tmp` that is set and not empty (the XDG Base Directory spec treats an empty value as unset). It uses `mkdtemp`, which creates the directory with mode `0700` atomically, before anything is written to it. It then writes an `owner` file with its pid and process start time, through a temporary file and a rename so the file is never half-written, and binds `ev.sock` inside the directory. Only the user can connect, and a new directory per run means there is no stale socket file to collide with. The directory is removed on exit. On startup, the TUI removes leftover `apitest-tui-*` directories in the parent it is about to use (the `--socket-dir` path, a variable's directory, or `/tmp`), and nowhere else, so a custom `--socket-dir` is swept by the TUI runs that use it, under the same rules, but only those owned by the current uid, with mode `0700`, containing an `owner` file whose pid no longer runs or now belongs to a process with a different start time (pid reuse). A directory without a valid `owner` file (a TUI that died between `mkdtemp` and the rename) is removed only if it is owned by the current uid, has mode `0700`, and is older than 24 hours. That leaves a short window in which such a directory survives, which this design accepts. The check is on the full socket path, `<dir>/ev.sock`, against the usable length of `sun_path`: its size minus the terminating NUL, so 107 bytes on Linux and 103 on macOS. If the path is longer, the TUI fails with an error that always names the path. The error also says where the directory came from (`--socket-dir`, a variable, or `/tmp`) and suggests `--socket-dir` with a short directory, or for `TMPDIR` a shorter value. It doesn't suggest changing `XDG_RUNTIME_DIR`, which the system manages. Only an unset or empty variable is skipped. A `--socket-dir`, or a variable that is set, that is unusable (missing, not a directory, not writable, or `mkdtemp` fails in it) is an error that names the flag or variable and its value, plus the OS error, and the TUI does not fall through to the next candidate, since that would be a hidden fallback. On Windows, it listens on `127.0.0.1` port 0, so the OS picks a free port. Any local process could connect there, so `APITEST_EVENTS` also carries a random per-run token. Unix uses the same token, so both sides work the same way. The exact values are `unix:<path>#<token>` and `tcp:127.0.0.1:<port>#<token>`, where the token is 32 lowercase hex characters. The harness splits at the **last** `#`, so a `#` in the path is fine, and it rejects a value whose token part isn't 32 hex characters. Each connection sends the token as its first line, and the TUI closes connections that don't.
- **Opening.** The TUI binds the socket **before** it spawns cargo. If a test process can't connect, the harness fails at startup with an error that names the variable and the address, instead of running without events.
- **Reader gone.** If the TUI quits or crashes while tests run, writes fail (`EPIPE`, or a reset connection). The sink then turns itself off for the rest of that process and logs one line to stderr. It never fails a test and never retries.
- **Inheritance.** Unlike a CLI flag, an environment variable reaches every process the tests spawn. The harness reads `APITEST_EVENTS` once and removes it from its own environment, so a test that starts another apitest binary doesn't connect to the TUI by accident.

## Testing

- **Events:** library tests with a collecting sink, against `demo_server` and the care-flow services: order (a step starts before its requests and ends after them), nesting, timings that match the final report, cancellation with cleanups, `Dropped` once the byte budget is used up, `RunFinished` after every accepted event and with each outcome (including a runner panic), `Truncated` once the lifecycle limit is reached, and no secret in the serialized events.
- **UI:** ratatui's `TestBackend` with snapshot tests of the tabs for fixed event sequences, so layout changes show up in review. Key handling is tested on the app state without a terminal.
- **End to end:** start the TUI in a pseudo-terminal against `demo_server`, run, rerun a failed test, trigger watch mode by touching a file, quit during a run and check that cleanups ran.

## Effort

For someone who knows the codebase, with the current level of tests:

| Milestone | Piece | Estimate |
|---|---|---|
| M1 | Event stream in the library: `Event` types, sink, emit sites, step nesting, redaction at the boundary, byte-bounded bulk channel, `RunFinished` on every path, `Deserialize` on the report types with chains carried alongside, tests | 4–5 days |
| M1 | Exact test selection (`Selection`) | 0.5 day |
| M1 | Cancellation token in the runner (shared with `apitest mcp` M1; nothing extra if that lands first) | 1–2 days |
| M1 | `apitest run --events ndjson` | 0.5 day |
| M1 | TUI shell: crate, event loop, tabs, key map, help, terminal restore on panic, agent and non-TTY checks | 1.5 days |
| M1 | Run tab: live tree, timeline, detail pane with step trail, miette, requests and logs; rerun one, failed or all; filter | 3–4 days |
| M1 | Timing views: waterfall, slowest, percentiles per operation | 2 days |
| M1 | History and comparison: atomic run files, comparable baselines, cancelled runs, deltas, history sparkline | 2–2.5 days |
| M1 | Watch mode | 1–1.5 days |
| M1 | Snapshot and end-to-end tests, README section | 2 days |
| **M1** | **visual runner and rerunner with live timings** | **17.5–21.5 days, about 3.5–4.5 weeks** |
| M2 | Contracts tab | 2–3 days |
| M2 | Call tab, including "save as step" | 3–4 days |
| M2 | Env tab plus `lookup_source` | 1 day |
| M2 | Load tab plus `Metrics` snapshots and `LoadTick` | 2–3 days |
| **M2** | | **8–11 days, about 2 weeks** |
| M3 | Rust suites through `APITEST_EVENTS` and `cargo test` / nextest: private socket with token, merging streams into one run (fingerprints, cancellation, crashes, truncation, late events) | 3–4 days |
| | **Total** | **28.5–36.5 days, about 6–7 weeks** |

A first prototype that is useful on its own consists of the event stream (4–5 days), exact selection (0.5), the TUI shell (1.5) and the Run tab (3–4), for 9–11 days. It leaves out history, timing views and watch mode. Its key map is the reduced one listed under [Running and rerunning](#running-and-rerunning). It has no `x` (stop), because that needs the cancellation token, so quitting during a run waits for the run to finish. Adding `apitest run --events ndjson` takes another half day.

## Open questions

- **Warm suites.** Rerunning one test currently runs suite fixtures again. For suites with expensive setup (seeding a database, starting containers), an opt-in "keep the suite scope" mode would make reruns much faster, at the price of tests seeing what earlier runs left behind. Is that worth offering, and should it be per suite (`tui: { warm: true }`)?
- **History location.** `.apitest/runs/` is per checkout. Should history optionally be shared (a directory on a team drive, or the JSON reports CI already writes), so comparisons can use CI runs as the baseline?
- **Regression thresholds.** Are 20 % and 20 ms sensible defaults for API tests, or should the baseline use percentiles across more runs before flagging anything?
- **Name-based masking.** Captured bodies have the same gap as `apitest mcp` (tokens the API issues during a run). Building name-based masking once in `redact.rs` would serve both. Should it be part of M1 here?
