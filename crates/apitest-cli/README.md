# apitest-cli

[![crates.io](https://img.shields.io/crates/v/apitest-cli.svg)](https://crates.io/crates/apitest-cli)
[![CI](https://github.com/sunsided/apitest/actions/workflows/ci.yml/badge.svg)](https://github.com/sunsided/apitest/actions/workflows/ci.yml)
[![MSRV](https://img.shields.io/crates/msrv/apitest-cli)](https://crates.io/crates/apitest-cli)
[![License](https://img.shields.io/crates/l/apitest-cli)](https://github.com/sunsided/apitest#license)
[![unsafe forbidden](https://img.shields.io/badge/unsafe-forbidden-success.svg)](https://github.com/rust-secure-code/safety-dance/)

The `apitest` command: runs [`apitest`](https://crates.io/crates/apitest) suites written in Rhai, JavaScript or Gherkin,
as functional tests or load tests, checks OpenAPI contracts for drift, and sends one-off requests with the
same services and credentials your suites use.

## Install

```bash
cargo install apitest-cli
```

## Commands

| Command | Does |
|---|---|
| `apitest run [suites…]` | Runs suites functionally; without arguments, the suites listed in `./apitest.nova` |
| `apitest load <file> -t <test>` | Runs one test as a load scenario with VUs, stages and thresholds |
| `apitest contract check` | Diffs the pinned specs against the live service or named comparison specs |
| `apitest contract diff <old> <new>` | Compares two OpenAPI documents; exits 1 on breaking changes |
| `apitest call <service> <method> <path>` | Sends one request, no suite or spec needed |

```bash
apitest run --update-lock                                  # suites from ./apitest.nova
apitest load suites/billing.js -t "create and fetch invoice" \
  --stages "10s:20,30s:20,10s:0" --threshold "http.p95 < 50ms"
apitest contract check suites/billing.rhai -u http://127.0.0.1:8080
apitest contract diff specs/billing-1.0.yaml specs/billing-2.0.yaml
apitest contract check --against qa,next                   # drift vs targets from apitest.nova, no server
apitest call hub GET /users/me --as hub_admin              # one request, no suite or spec
apitest run --env-file .env.ci --redact 'cust-[0-9]+'      # variables from a .env file, extra masking
```

## Suites

Suites are synchronous scripts, like k6, or Gherkin `.feature` files with a generic step library. JavaScript
runs in QuickJS with one runtime per worker or virtual user:

```js
const s = suite("billing");
s.contract("billing", "../specs/billing-1.0.yaml", { liveSpec: "/openapi.json" });

s.fixture("token", "vu",
  (cx) => {
    const r = cx.call("billing", "issueToken",
      { form: { user: "demo", pass: "secret" } }).expect(200);
    cx.header("Authorization", `Bearer ${r.body.access_token}`);
    return r.body.access_token;
  },
  (cx) => cx.call("billing", "revokeToken").expect(204));

s.test("create invoice", { parallel: true }, (cx) => {
  const inv = cx.call("billing", "createInvoice",
    { json: { amount: 4200, currency: "EUR" } }).expect(201).body;
  cx.defer(() => cx.call("billing", "deleteInvoice", { path: { id: inv.id } }).expect(204));
});
```

Fixture teardowns and `cx.defer` cleanups run even when a test fails or times out. The `vu` fixture above
runs once per virtual user, so under `apitest load` every simulated user logs in once.

## Configuration

`apitest.nova` describes the project: where the services live, who calls them, and which suites to run.

```text
base_url: http://127.0.0.1:8080
lockfile: apitest.lock.nova
max_parallel: 8
suites:
  - suites/billing.js
reports:
  junit: target/apitest/junit.xml
actors:
  hub_admin:
    service: hub
    auth:
      type: login
      username: "${HUB_ADMIN_EMAIL}"
      password: "${HUB_ADMIN_PASSWORD}"
drift:
  targets:
    qa:
      billing: specs/billing-1.1.yaml
```

Variables for `${NAME}` come from `--var`, the environment, `.env` and `.env.local` next to the config, then
`vars:`; every one the actors use must resolve before a run starts. Credentials, fetched tokens and
`--redact` / `redact:` matches are masked as `***` in all output. See
[Variables, `.env` files and redaction](https://github.com/sunsided/apitest#variables-env-files-and-redaction).

## Output

Contract drift fails a suite before any test runs, reported per operation and field:

```text
suite billing
  contract billing: pinned v1.0.0 vs live v2.0.0 (http://127.0.0.1:8080/openapi.json) → BREAKING
    ✗ breaking createInvoice @ request.amount_cents: field is now required but this suite does not send it
    ✗ breaking createInvoice @ response 201.id: type changed from integer to string
    ! warning  createInvoice @ response 201.status: enum gained values "draft"
    ...
  ✗ setup failed: live API is incompatible with pinned contract(s): billing
```

Load runs report HDR latency percentiles and check thresholds:

```text
load billing-js
  scenario create and fetch invoice (test 'create and fetch invoice', 2.6s)
    http       n=7729  failed=0  rate= 3017.2/s  avg= 1.22ms p50= 1.12ms p90= 1.93ms p95= 2.21ms p99= 2.91ms
    iterations n=2571  failed=0  rate= 1003.7/s  avg= 5.57ms p50= 5.56ms p90= 7.33ms p95= 7.90ms p99= 9.71ms
    ✓ http.p95 < 50ms  (actual 2.207)
    ✓ iterations.error_rate < 1%  (actual 0.000)
```

Failures render as diagnostics with their whole cause chain on a terminal (`NO_COLOR` and friends are
respected); JUnit XML and JSON reports stay plain text for CI.

Exit codes: 0 passed, 1 tests or thresholds failed (for `apitest call`: a status other
than 2xx or `--expect`), 2 when the run could not be carried out: a usage or config
error, and for `apitest call` also a failed login, a connection error, too many
redirects or `--timeout`.

The [repository README](https://github.com/sunsided/apitest#readme) covers authentication, trace propagation,
drift targets, the lockfile and `apitest call` in depth.

## License

Licensed under EUPL-1.2, MIT or Apache-2.0, at your option. See the [repository](https://github.com/sunsided/apitest#license) for details.
