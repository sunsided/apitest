# npm distribution

The npm package is a launcher for the native binary, the same pattern esbuild,
Biome, SWC and oxlint use. It is not WebAssembly.

```
@apitest/cli                  bin `apitest` (bin/apitest.js), types/index.d.ts, platforms.json
  optionalDependencies:
    @apitest/cli-linux-x64-gnu     each holds only bin/apitest[.exe] and declares
    @apitest/cli-linux-x64-musl    "os" / "cpu" / "libc", so npm installs just the
    @apitest/cli-linux-arm64-gnu   one matching the machine
    @apitest/cli-darwin-arm64
    @apitest/cli-darwin-x64
    @apitest/cli-win32-x64-msvc
```

- `cli/platforms.json` is the single list of targets (Rust triple → npm package/os/cpu/libc);
  the launcher and the build script both read it.
- `scripts/build-platform-package.mjs` turns a built binary into a platform package.
- `scripts/set-version.mjs` keeps the wrapper and its optional dependencies in lockstep.
- `.github/workflows/release.yml` (draft, runs on `v*` tags) builds all targets and
  publishes the platform packages first, then the wrapper.

The npm name `apitest` belongs to an unrelated package, so this uses the `@apitest` scope;
the `@apitest` npm organisation still needs to be claimed before the first publish.

## Try it locally

```bash
cargo build -p apitest-cli
node npm/scripts/build-platform-package.mjs --target x86_64-unknown-linux-gnu --binary target/debug/apitest
(cd npm/cli && npm pack) && (cd npm/dist/cli-linux-x64-gnu && npm pack)
# in a scratch project:
npm i --omit=optional /path/to/apitest-cli-0.0.1.tgz /path/to/apitest-cli-linux-x64-gnu-0.0.1.tgz
npx apitest --version
```

Type-check the demo JS suite against the declarations:

```bash
npx -p typescript tsc -p demo/suites/jsconfig.json
```
