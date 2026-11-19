#!/usr/bin/env node
// Sets the version of @apitest/cli and of all its platform optionalDependencies.
//   node npm/scripts/set-version.mjs 0.3.1      (e.g. from a v0.3.1 git tag)

import { readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const version = (process.argv[2] ?? "").replace(/^v/, "");
if (!/^\d+\.\d+\.\d+(-[\w.]+)?$/.test(version)) {
  console.error("usage: set-version.mjs <semver>");
  process.exit(2);
}
const file = join(dirname(fileURLToPath(import.meta.url)), "..", "cli", "package.json");
const pkg = JSON.parse(readFileSync(file, "utf8"));
pkg.version = version;
for (const dep of Object.keys(pkg.optionalDependencies ?? {})) pkg.optionalDependencies[dep] = version;
writeFileSync(file, JSON.stringify(pkg, null, 2) + "\n");
console.log(`@apitest/cli → ${version}`);
