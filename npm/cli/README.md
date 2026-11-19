# @apitest/cli

API tests, load tests and OpenAPI contract pinning: the native `apitest` binary
plus TypeScript types for JavaScript suites.

```bash
npx @apitest/cli run suites/billing.js --base-url http://localhost:8080
npm i -D @apitest/cli    # pin the version in package.json / package-lock.json
```

Only the binary for your platform is installed (via `optionalDependencies`); there
is no postinstall script and no download at runtime. Set `APITEST_BINARY` to use
your own build.

## Types for suites

```js
// @ts-check
/// <reference types="@apitest/cli" />
const s = suite("billing");
s.test("health", (cx) => { cx.http("GET", "/health").expect(200); });
```

**Suites do not run in Node.js.** They run in apitest's embedded QuickJS runtime,
synchronously (like k6): no `require`, no npm packages, no Node built-ins.

## Programmatic use

```js
const { binaryPath } = require("@apitest/cli");
require("node:child_process").execFileSync(binaryPath(), ["run"], { stdio: "inherit" });
```
