# Design: Gherkin (Cucumber) as a fifth frontend

Implemented in [`crates/apitest/src/cucumber.rs`](https://github.com/sunsided/apitest/blob/main/crates/apitest/src/cucumber.rs) and [`crates/apitest/src/cucumber/`](https://github.com/sunsided/apitest/tree/main/crates/apitest/src/cucumber) behind the `cucumber` feature (on in `full` and in the CLI).

## Summary

**apitest runs `.feature` files itself.** A feature file is one more frontend next to Rust, Rhai, JS and Python, compiled into the same `Suite` and run by the same runner. A team that reviews Gherkin as its specification keeps those files as they are and gets everything apitest does for the other frontends: the pinned contract and the drift check before any scenario runs (live spec or `--against` targets), request and response validation on every call, the lockfile of the used surface, cleanups that run even when a scenario fails, `parallel`/`serial` scheduling with resource locks, `apitest.nova` with `.env` files and redaction, and the JUnit and JSON reports.

The goal is one test setup, not two: Gherkin as the reviewed spec, and Rust step definitions on the same runner for what a generic step library can't express (a staff sign-in with an authenticator code, say).

## Why not the `cucumber` crate

The [`cucumber`](https://crates.io/crates/cucumber) crate is a complete test framework: its own runner, its own `World` type fixed at compile time, its own hooks, its own writers (console, JUnit, JSON). Using it would mean either

- running two schedulers (cucumber's around apitest's), with cleanups, timeouts, drift checks and reports done twice or not at all, or
- giving up apitest's lifecycle inside scenarios.

It also needs a compiled test binary per step library, so `apitest run x.feature` could not load feature files the way it loads `.rhai` and `.js`.

So apitest uses [`gherkin`](https://crates.io/crates/gherkin), the parser the `cucumber` crate itself is built on. The syntax is identical, including the 70+ spoken languages (`# language: de`), `Rule`, `Background`, `Scenario Outline` and tag expressions. What apitest adds is a small step registry and the compilation into a `Suite`.

| | `gherkin` + apitest runner (chosen) | `cucumber` crate |
|---|---|---|
| Contract pinning, drift check, lockfile | yes, unchanged | would need wiring into its hooks |
| Cleanups on failure, timeouts, scheduling | apitest's | cucumber's (no LIFO cleanups across steps) |
| Reports | apitest's JUnit/JSON, one format for all frontends | cucumber's writers, a second format |
| `apitest run x.feature` without compiling | yes | no |
| Custom steps | Rust (`Steps::step`), regex | Rust, regex and Cucumber Expressions, attribute macros |
| Cucumber ecosystem (formatters, `cucumber-messages`) | no | yes |

The last two rows are what we give up. Cucumber Expressions (`{int}`, `{string}`) are a nice-to-have on top of regex. Cucumber's message format matters only for tools such as Cucumber Reports; JUnit covers CI.

## How a feature maps onto apitest

| Gherkin | apitest |
|---|---|
| `Feature` | one `Suite`, named after the feature |
| `Scenario` | one test |
| `Scenario Outline` + `Examples` | one test per row, named `name (col=value, …)`, `<col>` filled in steps, docstrings and tables |
| `Background` (feature and rule) | steps run at the start of every scenario, as Cucumber does |
| `Rule` | its name prefixes the test names (`rule: scenario`) |
| a step | a `dsl::step`: logged, timed, in the step trail of failures, in JUnit `system-out` and the JSON report, with its requests nested in it |
| feature tags `@contract:NAME=PATH`, `@live_spec:NAME=URL`, `@mode:NAME=strict` | `Contract::pinned(..).live_spec(..).mode(..)` (paths relative to the feature file, like `s.contract(..)` in Rhai) |
| `@parallel`, `@serial`, `@resource:NAME`, `@timeout:5s` | `TestOpts`; inherited from feature, rule and `Examples`, the innermost `@parallel`/`@serial` wins |
| other tags | `TestOpts::tags`, selectable with `apitest run --tags "@smoke and not @slow"` |

Every step of every scenario is matched when the file is loaded. Undefined and ambiguous steps fail the load, all of them at once with `file:line`, before anything is sent. A failing step fails the scenario with `step 'Then …' failed: at billing.feature:16: …`; the remaining steps are logged as skipped, and the cleanups registered so far run.

The `--tags` expression is user input: it is limited to 1024 bytes (the parser and the matcher recurse once per nesting level), covered by property tests, and fuzzed together with the feature loader (`fuzz/fuzz_targets/gherkin.rs`).

## The built-in step library

Steps match regardless of keyword (`Given`/`When`/`Then`/`And`/`But`). Names in quotes are free text.

**Actors** (per scenario; there is no implicit lookup):

```gherkin
Given "anon" is an anonymous actor on "billing"
Given "admin" is the configured actor "hub_admin"            # actors: in apitest.nova
Given "patient" logs in on "app" as "{{email}}" with password "${PATIENT_PW}"   # lazy, via the service's login flow
Given "user" is an actor on "billing" with the bearer token "{{token}}"
Given "ops" is an actor on "reports" with basic auth "ops" and password "${OPS_PW}"
Given "user" sends the header "X-Tenant" with "t1"
```

**Requests.** A contract call by `operationId`, or a raw request to a service, optionally with `expecting <status>` and one of three ways to pass data:

```gherkin
When "user" calls "createInvoice"
When "user" calls "createInvoice" expecting 201 with the JSON body:
  """json
  { "amount": 4200, "currency": "EUR" }
  """
When "user" calls "getInvoice" with:
  | in    | name     | value  |        # optional header row
  | path  | id       | {{id}} |
  | query | expand   | lines  |
  | header| X-Trace  | abc    |
  | form  | user     | demo   |
  | json  | amount   | 4200   |        # JSON value: 4200, "EUR", true, {"a":1}
When "mail" sends GET "/messages" expecting 200 with options:
  """json
  { "query": { "to": "{{email}}" } }
  """
```

`with options:` takes the same object as `cx.call(..)` in Rhai and JS (`path`, `query`, `headers`, `json`, `form`, `body`, `as`, `trace`, …).

**Cleanups.** The same request steps prefixed with `afterwards`. The request is built (and `{{…}}` resolved) when the step runs and sent when the scenario ends, in LIFO order, whether it passed or not. Written right after the call that created something, it is the Gherkin form of `.rollback(..)`: a step that failed before it never registers it.

```gherkin
When "user" calls "createInvoice" expecting 201 with the JSON body: …
And afterwards "user" calls "deleteInvoice" expecting 204 with:
  | path | id | {{response:/id}} |
```

**Values:**

```gherkin
And I remember the response at "/id" as "id"
And I remember the response header "Location" as "location"
Given a unique email with prefix "patient" as "email"         # unique_email(..)
Given a unique value with prefix "case" as "case_name"        # unique(..)
```

**Checks** (on the latest response):

```gherkin
Then the status is 201                     # also: the response status is 201
And the response contains:                 # partial JSON match (json_includes), path of the first mismatch in the error
  """json
  { "status": "open", "currency": "EUR" }
  """
And the response at "/currency" is "EUR"   # strict JSON: EUR without quotes is an error, not a string
And the response at "/items" has 2 items
And the response header "Content-Type" is "application/json"
```

**Waiting.** `within <n> seconds|milliseconds|minutes, <step>` retries a step with the backoff of `eventually!` (50 ms growing to 1 s). For a check, the scenario's latest request is sent again before every new attempt. For a request step, the step itself is retried.

```gherkin
When "mail" sends GET "/messages" expecting 200 with:
  | query | to | {{email}} |
Then within 30 seconds, the response at "/0/subject" is "Confirm your e-mail"
```

### Values in steps, and secrets

- `${NAME}` is a run variable (`--var`, the environment, `.env` files, `vars:`), the same lookup as everywhere else, and `$${` is a literal `${`. `{{name}}` is a value remembered in this scenario, `{{response:/pointer}}` a field of the latest response. Anything unresolved is an error, with a byte position rather than the text. There are no defaults. Inserted values are not scanned again, so a response can never name a variable.
- JSON (docstrings, `json` table cells, the expected value of `the response at … is …`) is parsed first, and `${…}` and `{{…}}` are resolved inside its strings afterwards, so an inserted value can never add keys or change the document's structure. A string that is exactly one placeholder takes the value with its JSON type: `"id": "{{id}}"` is a number when the remembered value is one, `"id-{{id}}"` is a string. A check's expected value may also be a bare `{{id}}`.
- Values inserted into the path of a raw request (`sends GET "/invoices/{{id}}"`) are percent-encoded as one path segment each, `.` and `..` included. The path must start with `/`, unless the step itself spells out an absolute `http(s)://` URL. The actor's credentials and headers go only to its own service's origin; a request to any other origin is sent without them.
- Step names in reports are the text as written, with `${…}` and `{{…}}` unresolved, so values never end up there.
- The login and basic-auth steps leave `${NAME}` to the actor's `Auth`, which resolves it when the first request is sent and masks the value in output and reports, exactly as for actors from `apitest.nova`. Bearer tokens and actor header values that come from a variable, a remembered value or a response are registered with the run's redactor in the step that sets them; values written literally in the step (`application/json`) are not.
- Step errors, logs and the step trail reach the console and the reports through the run's `Redactor`, like every other frontend. Variables used elsewhere (a form field, a JSON body) are not masked unless they are registered secrets or match a `redact:` pattern; that is the same rule as for Rhai and JS.

## Custom steps in Rust

Steps the generic library can't express are Rust functions on the same `World`, for example a staff sign-in with an authenticator code:

```rust
use apitest::cucumber::{FeatureOptions, Steps, load_with};

let steps = Steps::builtin()
    .step(r#""([^"]+)" signs in as staff "([^"]+)""#, |w, a| async move {
        let email = w.interpolate(a.get(2)?)?;
        let code = totp(&w.cx().env("STAFF_TOTP_SECRET")?)?;          // your TOTP helper
        let staff = Actor::on("hub").named(&email);
        let token: String = staff
            .call("staffLogin")
            .json(json!({ "email": email, "password": w.cx().env("STAFF_PW")?, "code": code }))
            .expect(200)
            .await?
            .get("/token")?;
        w.set_actor(a.get(1)?, staff.bearer(token));
        Ok(())
    })?;

// tests/specs.rs with `harness = false`: features next to Rust suites, under cargo test / nextest
apitest::harness::main(|| async {
    let cfg = apitest::RunConfig::new(base_url);
    Ok(apitest::harness::Plan::new(cfg).suites(load_with(
        "tests/specs/onboarding.feature".as_ref(),
        &FeatureOptions::default(),
        steps.into(),
    )?))
})
```

`World` gives a step the test's `Ctx` (`w.cx()`), the scenario's actors, remembered values and latest response, and `interpolate`. Patterns are regular expressions, anchored for you. A step text matching two definitions is reported as ambiguous at load time, so custom steps can't silently shadow built-in ones.

## Module layout

| File | Contents |
|---|---|
| `cucumber.rs` | `load`, `load_with`, `load_str`, `FeatureOptions`; compiling a feature into a `Suite`; running a scenario's steps |
| `cucumber/plan.rs` | outline expansion, `<placeholder>` filling, docstrings, `within …,`, matching steps at load time |
| `cucumber/tags.rs` | `TagFilter` (tag expressions), contracts from feature tags, `TestOpts` from scenario tags |
| `cucumber/steps.rs` | `Steps` (the registry), `StepArgs`, `StepFn` |
| `cucumber/builtin.rs` | the built-in step library |
| `cucumber/world.rs` | `World`: per-scenario actors, values, the latest response, interpolation |

`.feature` is dispatched in `script::load_suites`, so the CLI, `apitest.nova`'s `suites:`, the harness and `contract check` pick it up. `apitest run --tags <expr>` sets `LoadOptions::tags`.

## Next steps

| Gap | Plan |
|---|---|
| Discoverability | `apitest steps` lists the step library with examples (and custom Rust steps in harness binaries); "did you mean" for undefined steps |
| Suite-wide setup in features (one login per feature instead of per scenario) | A `@suite`-tagged `Background`, or Rust/configured fixtures readable as `{{fixture:name}}`. Explicit syntax either way |
| Reports | a `location` (`file:line`) on `StepReport`, and the steps after a failure reported as skipped rather than only logged |
| Step patterns | Cucumber Expressions (`{int}`, `{string}`) next to regex; steps defined in Rhai/JS for teams without Rust |
| Diagnostics | miette source spans pointing at the failing step's line in the `.feature` file |

## Open questions

- **Contracts in the feature file or in `apitest.nova`?** Tags keep a feature self-contained, like `s.contract(..)` in Rhai, but long paths in tags are ugly. A `contracts:` section in `apitest.nova` would serve every frontend, and features could then just say `@contract:billing`.
- **Masking variables used in bodies.** Only credentials are masked automatically. Masking every `${…}` used in a step would also mask harmless values (user names, tenant IDs) everywhere. The alternative is a naming rule (`*_PASSWORD`, `*_SECRET`, `*_TOKEN`) or a `secrets:` list in `apitest.nova`. This applies to all frontends.
- **Wording of the generic steps.** Fixed English phrases are easy to match and review but won't fit every team's language. Gherkin's `# language:` changes only the keywords. Translated step libraries, or Cucumber Expressions with alternatives (`calls/requests`), are possible later.
- **Load runs.** Scenarios are ordinary tests, so `apitest load x.feature -t "create and fetch invoice"` works. Whether Gherkin is a sensible way to write load scenarios is a separate question.
