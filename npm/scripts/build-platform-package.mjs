#!/usr/bin/env node
// Creates the npm package for one platform from a built binary.
//
//   node npm/scripts/build-platform-package.mjs --target x86_64-unknown-linux-gnu \
//        --binary target/x86_64-unknown-linux-gnu/release/apitest --out npm/dist
//
// The version is taken from npm/cli/package.json, so wrapper and platform
// packages are always published in lockstep.

import { chmodSync, copyFileSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs } from "node:util";

const here = dirname(fileURLToPath(import.meta.url));
const cliDir = join(here, "..", "cli");
const { values } = parseArgs({
  options: {
    target: { type: "string" },
    binary: { type: "string" },
    out: { type: "string", default: join(here, "..", "dist") },
  },
});
if (!values.target || !values.binary) {
  console.error("usage: build-platform-package.mjs --target <rust triple> --binary <path> [--out <dir>]");
  process.exit(2);
}

const wrapper = JSON.parse(readFileSync(join(cliDir, "package.json"), "utf8"));
const platforms = JSON.parse(readFileSync(join(cliDir, "platforms.json"), "utf8"));
const p = platforms.find((x) => x.target === values.target);
if (!p) {
  console.error(`unknown target ${values.target}; known: ${platforms.map((x) => x.target).join(", ")}`);
  process.exit(2);
}

const dir = join(values.out, p.package.replace("@apitest/", ""));
mkdirSync(join(dir, "bin"), { recursive: true });
const exe = `apitest${p.exe ?? ""}`;
copyFileSync(values.binary, join(dir, "bin", exe));
chmodSync(join(dir, "bin", exe), 0o755);

const pkg = {
  name: p.package,
  version: wrapper.version,
  description: `apitest native binary for ${values.target}`,
  license: wrapper.license,
  repository: wrapper.repository,
  os: [p.os],
  cpu: [p.cpu],
  ...(p.libc ? { libc: [p.libc] } : {}),
  files: ["bin/"],
  preferUnplugged: true,
};
writeFileSync(join(dir, "package.json"), JSON.stringify(pkg, null, 2) + "\n");
writeFileSync(
  join(dir, "README.md"),
  `# ${p.package}\n\nThe \`apitest\` binary for \`${values.target}\`. Install [\`@apitest/cli\`](https://www.npmjs.com/package/@apitest/cli) instead; it picks the right platform package automatically.\n`,
);
console.log(dir);
