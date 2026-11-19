# apitest-cli

[![crates.io](https://img.shields.io/crates/v/apitest-cli.svg)](https://crates.io/crates/apitest-cli)
[![CI](https://github.com/sunsided/apitest/actions/workflows/ci.yml/badge.svg)](https://github.com/sunsided/apitest/actions/workflows/ci.yml)
[![MSRV](https://img.shields.io/crates/msrv/apitest-cli)](https://crates.io/crates/apitest-cli)
[![License](https://img.shields.io/crates/l/apitest-cli)](https://github.com/sunsided/apitest#license)
[![unsafe forbidden](https://img.shields.io/badge/unsafe-forbidden-success.svg)](https://github.com/rust-secure-code/safety-dance/)

The `apitest` command: runs [`apitest`](https://crates.io/crates/apitest) suites written in Rhai or JavaScript,
as functional tests or load tests, and checks OpenAPI contracts for drift.

## Install

```bash
cargo install apitest-cli          # or: npx @apitest/cli
```

Prebuilt binaries for Linux, macOS and Windows are attached to each
[GitHub release](https://github.com/sunsided/apitest/releases).

## Usage

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

Variables for `${NAME}` in `apitest.nova` come from `--var`, the environment, `.env` and
`.env.local` next to the config, then `vars:`; every one the actors use must resolve before
a run starts. Credentials, fetched tokens and `--redact` / `redact:` matches are masked as
`***` in all output. See
[Variables, `.env` files and redaction](https://github.com/sunsided/apitest#variables-env-files-and-redaction).

Exit codes: 0 passed, 1 tests or thresholds failed (for `apitest call`: a status other
than 2xx or `--expect`), 2 when the run could not be carried out: a usage or config
error, and for `apitest call` also a failed login, a connection error, too many
redirects or `--timeout`.

## License

Licensed under EUPL-1.2, MIT or Apache-2.0, at your option. See the [repository](https://github.com/sunsided/apitest#license) for details.
