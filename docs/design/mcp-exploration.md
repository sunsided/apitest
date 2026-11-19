# Design: `apitest mcp`, an exploration session for agents

Status: proposal · Scope: optional CLI subcommand, behind a cargo feature

## Why, and what it is not

Writing, running and load testing suites already works through files and the CLI, and an agent with a shell can do all of that today. What it can't do is **poke at a live API as a specific user and be sure nothing is left behind**. Doing that with `curl` means handling tokens yourself, guessing request shapes, and cleaning up by hand, and a crashed exploration leaves data around.

`apitest mcp` is a stdio MCP server that holds one **exploration session**:

1. The agent acts as **authenticated actors** (named actors from `apitest.nova`, or users it creates during the session). It never sees their credentials or tokens.
2. Every write is paired with a **declarative undo** before it is sent. Undos are journalled to disk and run LIFO when the session ends, however it ends.
3. When the agent has found the path it cares about, it **exports** what it did as a `.rhai` or `.rs` test, using the same idioms as hand-written suites (`step`, `rollback`, `eventually!`, `assert_json!`).

Non-goals. These stay on the CLI and in files, not on MCP:

- running suites (`apitest run`), load tests (`apitest load`), contract checks and diffs
- editing test files (the agent uses its own file tools on the exported output)
- production traffic (see [Guards](#guards))

## Session model

```text
apitest mcp [--config apitest.nova] [--read-only] [--journal-dir .apitest/mcp]
            [--allow-service NAME]... [--allow-irreversible] [--idle 30m]
            [--export-dir tests/explored]
apitest mcp recover [--config apitest.nova] [--journal-dir .apitest/mcp]
```

`recover` replays leftover journals without starting a session (see [When undos run](#when-undos-run)). It needs the same config as the session did, to resolve services and actors again. A journal entry that names a service or actor the config no longer defines is reported in `left_behind`, not dropped. The journal lives outside the working tree by default (see [Undos are data](#undos-are-data-journalled-before-the-request-goes-out)). A `--journal-dir` inside the repository is an explicit opt-in and must be ignored by the user; `.gitignore` alone is not a security control.

- One process, one client, one session. The library gets a small public `apitest::session::Session` (feature `session`) that owns a root `Ctx` → suite `"mcp"` → test `"exploration"` chain. The CLI wraps it with [`rmcp`](https://crates.io/crates/rmcp) (feature `mcp` on `apitest-cli`). The same `Session` could later back a REPL.
- Contracts, services, actors, auth flows, tracing and User-Agent come from `apitest.nova` exactly as in `apitest run`. Contract validation stays on. A violation is **returned in the call result** rather than raised, because during exploration it's a finding, not a failure.
- Every call is a numbered **step** (`s1`, `s2`, …) with its request, response, trace id and validation result. The step list is the session's record, and export reads from it.

## Tools

Few tools with rich arguments. The agent should spend its tokens on the API, not on picking tools.

| Tool | Purpose |
|---|---|
| `session_info` | Services, base URLs, pinned contracts and versions, configured actor names (never their credentials), guard settings, current step count and pending undos. |
| `operations` | Lists a service's operations from the pinned spec (`operationId`, method, path, summary), with an optional filter. |
| `describe_operation` | Parameters, request schema and response schemas of one operation, with `$ref`s inlined (`contract.rs` already does this). Also names the operation that **likely undoes** it (see auto-undo below). |
| `actor` | Defines a session actor: `{ name, service, from: "hub_admin" }` (config), `{ login: { username, password } }` (only for users created in this session, see below), `{ bearer: "${s4.body.access_token}" }`, or `{ basic / api_key }`. Returns the name only. |
| `call` | Sends a request as an actor: `{ as, operation \| method+path, path, query, json, form, headers, expect?, undo?, label? }`. Returns `{ step, status, body, validation, trace_id, undo_registered }`. |
| `wait_for` | `eventually` as a tool: repeats a `call` until `until` (partial JSON match or a status) holds, or `within` runs out. Typical for mail mocks and async jobs. |
| `check` | Records an assertion on a step's response (`{ step, json: {..} }` partial match, or `status`). Fails immediately if it doesn't hold. Checks become assertions in the export. |
| `undo` | `{ to: "s5" }` rolls back to just before step 5 (runs undos registered after it, LIFO), so the agent can try another branch. No argument means everything. |
| `history` | The step list (compact), with labels, undo state and which steps are marked for export. |
| `export` | `{ format: "rust" \| "rhai", name, steps?: ["s1".."s9"], verify?: true, write?: path }`. Returns the generated file and warnings. See [Export](#export). |
| `end` | Runs all undos, verifies them, deletes the journal on success and returns a summary that lists anything left behind. |

**References instead of literals.** Any string in `call`, `actor`, `wait_for` or `check` may contain `${s3.body.id}` or `${s3.headers.location}`, plus generators `${unique("case")}` and `${unique_email("patient")}`. The server resolves them before sending, but it **records the reference, not the value**. That is what lets export write `let case = …; … .path("id", &case["id"])` instead of a hard-coded id. When the agent pastes a literal that equals an earlier response value, the call result carries a hint (`"42" is s3.body.id, use ${s3.body.id}`), and export warns about it.

MCP resources mirror the read-only parts: `apitest://contract/{service}` (the pinned spec) and `apitest://session/history`. Clients that read resources save tool calls, and the rest still get everything through tools.

## Auth

Auth reuses `auth.rs` unchanged (lazy, single-flight logins, refresh before expiry, one re-login on `401`, client credentials). The MCP layer only adds **secrecy toward the agent**:

- **Config actors** (`from: "hub_admin"`) resolve `${ENV}` credentials in the server. The agent knows the name, nothing else. Client credentials actors are config-only.
- **Session actors** are users the agent created during the session (register, then `login`). Their credentials are test data the agent chose, so they may be literals. Export writes them as such, usually through a `PASSWORD` constant. `actor { login: … }` is therefore accepted only when the username is a value this session generated or sent in an earlier step (a `${unique_email(..)}` or a step reference). Credentials of a pre-existing user are refused: such users must be configured as config actors, so a real staging password never enters the agent's context or an export.
- **Redaction.** Results never contain `Authorization`, `Cookie` or `Set-Cookie` values. Values whose names match a configurable list (`access_token`, `refresh_token`, `id_token`, `password`, `client_secret`, `token`, `reset_token`, `otp`, `sig`, …) are replaced in JSON bodies, in headers such as `Location`, and in query parameters and fragments (`#access_token=…`) of any URL, including URLs embedded in body strings such as a mail's HTML. They come back as `"<secret ${s4.body.access_token}>"`, or as a URL with the value swapped for its reference. The agent can pass the reference on to `actor { bearer: … }` or `visit`, and the server resolves it, but the agent never sees the value.
- **Limits of redaction.** This is a name-based denylist, so it's best effort. A token in a field called `data` is not caught, and neither is a token in a URL **path** (`/reset/<token>`, `/invite/<token>/accept`), which has no name to match. Services that put secrets in paths can declare those path patterns in config (`redact_paths: ["/reset/{token}"]`) so those segments are redacted too. Values the scenario needs to read are deliberately left visible as test data (an `activation_code` the patient hands to the social worker). Exports never contain resolved secret references: they emit code that extracts the value at run time (`resp.get("/access_token")`, `link_containing(..)`). Even so, review exported files before committing them, and keep the journal out of version control (see below).
- **Raw `bearer`/`Authorization` headers** in `call` are only accepted as references, so a token can't be smuggled in as a literal and end up in an exported file.

## Rollback

"Guaranteed" here means: **every write the server sent either has a recorded inverse that runs, or is reported by name as left behind**. That is achievable and testable. Silently promising more (undoing an e-mail that was sent) isn't.

### Every write needs an undo before it is sent

`POST`/`PUT`/`PATCH`/`DELETE` calls must come with one of:

- `undo: { as, operation, path: { id: "${self.body.id}" }, expect: 204, find? }`: a declarative request. `${self…}` refers to this call's own response. It is registered only if the call succeeds, as `.rollback(..)` does today. The optional `find` (for example `find: { operation: "listPatients", query: { email: <ref> }, match: { "/email": <ref> } }`, where `<ref>` is the same `${unique_email("patient")}` the request used, written as a plain JSON string with its inner quotes escaped) tells crash recovery how to find the created resource if the response was lost (see below). `match` values are literals or the same generator and step references the request used, so they resolve to the journalled values. Auto-undo fills `find` in only when the spec has exactly one list or search operation for the collection that accepts a matching filter.
- `undo: "auto"`: the server derives the inverse from the pinned spec and uses it if exactly one candidate fits. If none or several fit, the call is refused and the candidates are listed, so the agent picks or writes the undo itself.
  - `POST /cases` → 201 with `id`, and the spec has `DELETE /cases/{id}`: `deleteCase { id: ${self.body.id} }`.
  - `PUT` on an existing resource (a full replace): `GET` it first and `PUT` the previous body back. If that `GET` returns `404`, the `PUT` creates the resource (an upsert), and the inverse is a `DELETE` of the same path when the spec has one. Without a matching `DELETE`, auto is refused with that explanation.
  - `PATCH`: `GET` first and patch back **only the fields this call sent**, to their previous values. Never a blind full `PUT`, since that would also overwrite fields the call didn't touch.
  - A `PUT` or `PATCH` whose payload touches a field that can't be read back (`writeOnly: true` in the spec, or a name on the redaction list such as `password` or `api_key`) can't be fully undone, because there is no previous value to restore. Auto-undo refuses such a call. With an explicit undo, it runs for the readable fields, and the step is always reported in `left_behind` as `partial`, naming the fields that stay changed, so the guarantee above still holds.
  - `DELETE`: no auto inverse. Recreating a resource generally can't be derived (server-generated ids, defaults, side effects), so a delete needs an explicit undo or `none`. The one exception is a resource the spec lets the client create at a known id (`PUT /x/{id}`). Then auto means: `GET` it **before** sending the delete, journal that body, and `PUT` it back as the undo.
  - Restoring previous values races with concurrent writers. Before an undo, the server checks that the fields this call wrote still hold what the server stored after the write. If not, the undo is skipped and the step is reported as a conflict in `left_behind` rather than overwriting someone else's change. Details are in [Appendix A](#appendix-a-crash-recovery-and-conflicts).
- `undo: { none: "reason" }`: only for writes the API can't undo (sending a mail, triggering a webhook). This is refused unless the session runs with `--allow-irreversible`, and it appears as a warning in `end` and in the export.

`--read-only` refuses every write. That suits sessions against shared staging data.

### Undos are data, journalled before the request goes out

Today's rollbacks are closures, which can't survive a crash. In a session, an undo is a serializable `UndoSpec { actor, target, RequestOpts, expect, find? }` with unresolved `${self…}` references. `RequestOpts` is already `Serialize`. The order is:

1. Append `pending { step, trace_id, sent_at, request, undo_spec, captured, resolved, idempotency_key? }` to `<journal-dir>/<session-id>.jsonl` and fsync. `--journal-dir` defaults to a per-project directory outside the working tree, `$XDG_STATE_HOME/apitest/mcp/<project-hash>` (falling back to `~/.local/state/…`, or the platform's local app-data directory), with directory mode `0700` and files `0600`. Keeping it out of the tree means `git add -f`, an older checkout or a moved repository can't pick it up. An in-tree directory is an explicit opt-in via `--journal-dir`. `request` is the original write (method, path, query, body) with every `${…}` reference already resolved, so it can be resent as is. `captured` holds the previous values read before sending. `resolved` holds the value of every generator and step reference that the request, the undo or its `find` uses, including tokens from earlier steps. The step log keeps the references for export, and the journal keeps the values for recovery. That is everything recovery needs, journalled verbatim, so it is sensitive: see [Appendix A](#appendix-a-crash-recovery-and-conflicts) for its contents.
2. Send the request.
3. On success, resolve `${self…}` against the response and append `armed { step, resolved_undo, observed }`. Append `void { step }` only when the failure guarantees nothing was applied: the request was never sent (for example, the server refused it under `--read-only`), or the service answered `400` or `422` to a write on a single resource. A status class alone proves nothing: a proxy can reject a response after the backend committed, middleware can answer `401`/`403` after the write, and a batch endpoint can apply part of a payload before failing validation. So every other outcome (other `4xx`, `5xx`, timeouts, connection resets) leaves the entry `pending`, and it goes through the maybe-applied path below. Operations the config marks as non-atomic (`atomic: false`, for batch endpoints) never become `void`.

A `pending` entry with no outcome (the process died mid-flight) is treated as **maybe applied**. Recovery checks whether the write landed, undoes it if so, and reports anything it can't settle in `left_behind` with the request, its trace id and the reason. It never skips an entry silently. Config actor credentials are re-resolved from config on recovery. Session actor passwords and API-issued tokens exist only in the journal, so recovery reads those from it. [Appendix A](#appendix-a-crash-recovery-and-conflicts) has the rules per method.

### When undos run

| Ending | What happens |
|---|---|
| `end` tool, or `undo` | runs LIFO, then verifies |
| client disconnects (stdin EOF) | same as `end` |
| idle timeout (`--idle 30m`) | same as `end` |
| SIGINT / SIGTERM | same as `end`. This needs the cancellation token listed under *Ctrl-C* in the README's open questions, which is worth doing anyway. |
| SIGKILL, OOM, power loss | the journal survives. The next `apitest mcp` in the same directory refuses to start a new session until it has replayed it, or `apitest mcp recover` does it explicitly. |

Undo failures don't stop the remaining undos (the same rule as cleanups today). The journal keeps every failed entry, and `end` returns them as `left_behind: [{ step, label, undo, error }]`.

### Verification

After an undo ran, if the spec has a `GET` for the same path, the server checks for `404` (after a delete) or for the restored body (after a put). An optional `mcp.leftover_check` in `apitest.nova` points at a probe such as the demo's `GET /_stats`, which `end` compares to its value at session start. This is the session counterpart of `Plan::finally`.

## Export

`steps` (default: every step that succeeded and isn't a pure read whose result was never used) becomes a small **scenario IR**: actors, steps (call, wait, check), captured values, rollbacks and generators. Code generators then emit one file per IR. Failed attempts, steps rolled back with `undo { to }` and unused reads are left out unless they are named explicitly. A failed call can be kept as a negative test (`expect: 422`).

**Rust** maps one to one onto the existing DSL and is the first target:

```rust
#[apitest::test]
async fn activation_code_identifies_patient(hub_admin: Actor) -> Result<()> {
    let app = Actor::on("app");
    let email = unique_email("patient");

    step("register patient",
        app.call("registerPatient").json(json!({ "email": email, "password": PASSWORD, "demo": true })).expect(201)
            .rollback(|p| app.call("deletePatient").path("id", &p["id"]).expect(204))).await?;
    let patient = step("patient logs in", Actor::on("app").login(&email, PASSWORD).signed_in()).await?;
    let code: String = step("create activation code",
        patient.call("createActivationCode").expect(201)).await?.get("/code")?;
    let case = step("identify patient by code",
        hub_admin.call("identifyPatient").json(json!({ "activation_code": code })).expect(201)
            .rollback(|c| hub_admin.call("deleteCase").path("id", &c["id"]).expect(204))).await?;
    assert_json!(case, { "status": "new" });
    Ok(())
}
```

Config actors become fixture parameters (`hub_admin: Actor`). Labels become `step(..)` names, references become `let` bindings, `wait_for` becomes `eventually!`, `check` becomes `assert_json!`, and an `undo: { none }` write becomes a `// NOTE: not undoable: <reason>` comment. Generated with `quote` and formatted with `prettyplease`, so the output is ordinary code, not a template.

**Rhai** has to wait for, or come together with, the Rhai ergonomics already listed in the README (step, eventually, rollback). Without them the export still works but reads worse: `cx.op(.., #{ as: "hub_admin" })`, a `cx.defer` after each creation, and a hand-rolled polling loop. Recommendation: build `cx.step`, `cx.eventually` and a `rollback:` request option first, then emit Rhai that looks like the Rust above.

**`verify: true`** replays the IR (not the generated code, which would need a compile for Rust) in a fresh child scope against the same service, with real rollbacks, and reports pass or fail next to the file. Rhai output can also be run directly through `script::load_suites`, which additionally proves the emitted text parses.

**Writing files.** `export` returns the content. With `write`, it writes only under `--export-dir` and never overwrites unless `overwrite: true`. Both `name` and `write` come from the agent, so the path is not built from them freely:

- `name` must be a plain stem (`[a-z0-9_]+`), and `write` may only name the export directory itself or a subdirectory below it that already exists. The file is always `<dir>/<name>.<ext>`.
- The export directory is canonicalised once, at session start, and opened as a directory handle. Target directories are resolved below that handle without following symlinks (`openat` with `O_NOFOLLOW`/`O_DIRECTORY` per component), and a symlinked component is refused.
- A new file is created relative to that handle with `create_new` (`O_CREAT | O_EXCL | O_NOFOLLOW`), so it can't land on an existing file or a symlink planted after the check. With `overwrite: true`, the content goes to a fresh temporary file in the same directory, created the same way, which is then renamed over the target. A rename replaces a symlink itself, never its target.

Since every step goes through handles rather than path strings, swapping a component for a symlink between check and write gains nothing. On platforms without `openat`, export writes only directly into the canonicalised export directory. Export doesn't touch `apitest.lock.nova`. The next `apitest run --update-lock` records the new usage as usual.

The IR also makes JS and Python exporters cheap later, since the four frontends share the same `cx` semantics.

## Guards

- **Targets.** Only services that `apitest.nova` marks with `explore: true` (or that are passed as `--allow-service`) can be called. Raw paths and `visit`-style links must resolve to one of those hosts.
- **Redirects.** `call` doesn't follow redirects: a `3xx` comes back as the step's result, and the agent decides whether to follow it. `visit` (which follows redirects in tests today) follows them one hop at a time in a session, checks each hop against the allowlist, and stops with an error on a target outside it. Both the allowlist and credential attachment match the service's full origin (scheme, host and port), not just the host. An `https` → `http` hop or a hop to another port is refused, and an actor's credentials are never sent outside its own service's origin.
- **Budget.** Maximum steps per session, maximum response size returned to the agent (bodies truncated, with a `truncated: true` flag and the full body kept for export), and per-call timeouts from `test_timeout`.
- **Tracing.** Every request carries the session's trace (`scope: run`) plus `baggage: apitest.session=<id>`, so service logs can show what an exploration did.

## Agent-aware CLI output

An agent that uses the CLI should find out that `apitest mcp` exists, and it should get output it can read easily. [`detect-coding-agent`](https://crates.io/crates/detect-coding-agent) tells the CLI whether it was started by a coding agent. It reads the `AI_AGENT` convention plus agent-specific signals (Claude Code, Copilot, Codex, Cursor, Gemini CLI and others) and reports `kind` (agent, interactive, hybrid), `confidence` (low, medium, high) and the `signal` it used.

- **Where:** only `apitest-cli` depends on it. The library and the harness don't change behaviour based on who runs them. `detect()` runs once at startup.
- **When to act:** `kind: agent` with `medium` or `high` confidence. `low` confidence, and `interactive` or `hybrid` (an IDE terminal a person is typing into), change nothing, so a human never gets the agent experience by accident.
- **What changes:**
  - *A pointer to the session.* After `apitest run` or `apitest contract check` fails, the CLI prints one line on stderr: ``hint: to explore the live API as an actor with automatic rollback, run `apitest mcp` ``. It is printed at most once per invocation and never on stdout, so JUnit, JSON and lockfile output stay byte-for-byte the same.
  - *Readable failures.* No ANSI colour, spinners or progress bars. miette renders with its no-colour theme, and failures open with the step trail. This stays within the earlier miette decision, since that output is still terminal text.
  - *No prompts.* The CLI has no interactive prompts today, and this proposal adds none. The rule is for any prompt added later: in agent mode, a non-destructive default is taken with a note on stderr, and a destructive one (overwriting a file, replacing the lockfile, deleting a journal) fails the command instead and names the flag that confirms the action. That flag has to come with the prompt.
- **Inside the session:** `apitest mcp` is started by the agent, so the same detection names the client. `session_info` reports it, and requests carry `baggage: apitest.agent=<id>` next to `apitest.session`. `<id>` is the detector's `DetectedAgent::id` (`claude-code`, `codex`, …). The tag is only sent when the agent behaviour is active (medium or high confidence, or forced with `--agent`), never for a low-confidence guess, so service logs can tell agent traffic from human traffic.
- **Overrides:** this proposal adds global flags `--agent` / `--no-agent` and `--color auto|always|never` to every command, plus `APITEST_AGENT=1|0`. Precedence is: command-line flag, then `APITEST_AGENT`, then detection. `--color` and the other explicit output flags win over the agent defaults. It also adds `apitest doctor`, which prints what was detected and from which signal, to explain surprising output.
- **MSRV:** `detect-coding-agent` needs Rust 1.91, which is below the workspace's `rust-version` of 1.95, so it doesn't raise the floor. The 1.95 floor comes from `novafile-value`, which uses `if let` match guards (stable since 1.95) while declaring `rust-version = "1.90"`. The `msrv` CI job runs `cargo hack check --rust-version`, which builds every crate with the toolchain its `rust-version` declares. That keeps the declared floor honest, but it doesn't stop the floor from being raised: raising it is a deliberate edit to `Cargo.toml` that shows up in review.

## Effort

For someone who knows the codebase, with the current level of tests (end to end against `demo_server` and the care-flow services, driven by a scripted MCP client):

| Milestone | Piece | Estimate |
|---|---|---|
| M1 | `Session` API in the library: public scope chain, step log, `${…}` references, redaction | 3–4 days |
| M1 | Undo specs, auto-undo from the spec, conflict check, journal, recovery | 4–5 days |
| M1 | SIGINT/SIGTERM cancellation in the runner (shared with the existing open question) | 1–2 days |
| M1 | MCP server (`rmcp`, stdio, tool schemas, guards, resources) | 3 days |
| M1 | Agent-aware CLI output via `detect-coding-agent` | 1 day |
| M1 | Scenario IR and Rust exporter (`quote` / `prettyplease`) | 3 days |
| M1 | End-to-end tests for all of the above, README section | 2 days |
| **M1** | **the useful core on its own** | **17–20 days, about 3.5–4 weeks** |
| M2 | Rhai ergonomics (`step`, `eventually`, `rollback`) plus Rhai exporter | 4–5 days |
| M2 | `verify` by replay, `undo { to }`, undo verification, leftover probe | 2–3 days |
| M2 | End-to-end tests for M2 | 1 day |
| **M2** | | **7–9 days, about 1.5–2 weeks** |
| | **Total** | **24–29 days, about 5 weeks** |

Later, if wanted: streamable HTTP transport, JS and Python exporters, several sessions per server.

## Open questions

- **How eager auto-undo should be.** Requiring the agent to pick when two delete operations fit is safe but chatty. A per-service `undo:` map in `apitest.nova` (`createCase: deleteCase`) would settle it explicitly.
- **Should export keep reads?** Reads the agent made only to understand the API are noise in a test. Reads whose values fed a later call or a `check` are kept, and everything else is opt-in. Is that the right default?
- **Session actors that the API can't delete.** If an app has no "delete my account", the run leaves a user behind. Should `actor` then require an admin actor and an undo up front, the same as any other write?
- **Shared environments.** With several people exploring the same staging system, should sessions take the same `resources` locks as nextest runs, so an exploration and a CI run don't collide on serial data?

## Appendix A: crash recovery and conflicts

### Conflict check

Restoring previous values races with anyone else writing the same resource at the same time. Right after a successful write, the server records the **observed** values of the fields the call sent: from the write's own response if it returns the resource, otherwise from a `GET` immediately afterwards (fields missing from a partial response are filled from the `GET`). If the `GET` doesn't yet show the values just written (a lagging replica or cache), it is retried with backoff for a few seconds. If it still doesn't, the step is marked `unverified`, and its undo is reported in `left_behind` instead of being run blindly. These are what the server actually stored, after any normalisation (lowercased e-mail, coerced enums, filled defaults), not the literal request values. Before an undo, the server `GET`s the resource again and compares only those fields with the observed values. Server-maintained fields (`updated_at`, `version`, `ETag`) are never compared, and write-only fields that don't come back in a `GET` (a `password`) are left out of the comparison. If a compared field differs, someone else changed it since. The undo is then **not** run, so that change isn't overwritten, and the step goes to `left_behind` as a conflict. Avoiding the race altogether is the *shared environments* open question above.

### Journal contents

- undo specs and resolved ids
- the resolved values of generators used in the request (`unique_email(..)` → the actual address), so a `find` lookup can match on them
- previous values captured **before** sending: the fields a `PATCH` will touch, the body a `PUT` will replace, the body a known-id `DELETE` will remove
- after the write, the observed values used by the conflict check (in the `armed` entry)
- the idempotency key, if the service uses one
- per entry: `trace_id`, `sent_at` (wall-clock time of the first send), the original `request` with references resolved, and `resolved`, the value of every generator and step reference used (including tokens from earlier steps)

Request bodies and captured values are journalled **verbatim**, because recovery replays them: a redacted placeholder would be sent as a password or written into a resource. Redaction applies only to what is shown to the agent. Config actor credentials are never written; they are re-resolved from config. But a session actor's literal password, tokens returned by the API, and personal data can all end up in the journal. Treat it as sensitive and keep it out of version control. `end` deletes the journal once every undo has succeeded.

### Recovering `pending` entries

A `pending` entry with no outcome (the process died mid-flight) is treated as **maybe applied**. There's no `observed` record for it, so recovery decides like this:

- **`PUT`/`PATCH`/known-id `DELETE`** (the undo needs nothing from the response): `GET` the resource. As in the conflict check, only fields that can be read back are compared, and write-only fields (`password`, `api_key`) are left out. If the call touched **only** readable fields and they all still equal the previous values captured before sending, the write never landed, and the entry is closed as `void`. If it also touched a write-only field, equal readable fields prove nothing (the write-only part may have landed), so the entry is never closed as `void`. If the call touched only readable fields and they equal exactly the values it would have written, the write landed and the undo runs. A call that also touched a write-only field is never treated as landed-and-undone, because its undo can't restore a value it never captured. It always goes to `left_behind` as `unverifiable`, naming the fields that couldn't be restored and including the captured body in the report. Recovery runs nothing for it, because a restore could overwrite a concurrent writer. In every other case recovery can't tell this call's write from a concurrent writer's. It then follows the same rule as the conflict check: nothing is overwritten, and the entry goes to `left_behind`. That includes any write the service normalised (the values differ from both), and any pending known-id `DELETE` that finds a `404`, since it could be someone else's delete. So the pre-delete capture is only used for undos in a live session, and in recovery it goes into the report so a person can restore the resource.
- **Creates** (the undo needs `${self…}`, typically the id of a `POST`): the response is lost, and recovery tries these in order:
  1. **Lookup.** If the undo spec has a `find`, recovery runs it with the journalled generator values (for example the `unique_email(..)` address). The lookup runs as the journalled actor. **Exactly one** match gives the id, and the undo runs. Several matches are inconclusive, just like none. This is tried first because it has no side effects. **An empty result is inconclusive** (search indexes lag, results are paginated or scoped by permission), so recovery goes on to the next step rather than concluding nothing was created.
  2. **Replay.** Only for services whose config declares that a repeated idempotency key returns the **cached original response without executing the write again** (`idempotency: { header: "Idempotency-Key", replay: cached, retention: 24h }`). The key is generated and journalled before the first send. The config must also name the response marker that proves a replay (`replayed_header: "Idempotent-Replayed"`, or a status). Within the retention window, measured from the entry's `sent_at`, recovery resends the request with the same key. If the marker is present, it resolves the undo from the cached response. If the marker is absent, the resend was a fresh execution (the key had been evicted, or the first send never arrived). Recovery then undoes that write immediately and reports both the write and its undo in `left_behind`, because side effects such as mails can't be undone. If `sent_at` is missing or later than the current time (a restored or copied journal, a clock that jumped), the window counts as passed. If the resend itself fails (any `4xx`, including a `409` that may mean the original landed, any `5xx`, or a transport error), or the window has passed, recovery stops resending and goes to step 3. When the marker is absent and the first send may also have landed, the report names the original as a possible duplicate, with its trace id. A write marked `none` (irreversible) is never resent. Services without this kind of replay cache are handled by lookup and `left_behind` only.
  3. **Otherwise** the entry is reported in `left_behind` with the request, its trace id and the reason, never skipped silently.

Actor definitions are journalled by name and by reference only. Secrets are re-resolved from the environment on recovery. If that fails (a variable is gone, a password was rotated), the affected undos also go to `left_behind` with that reason.
