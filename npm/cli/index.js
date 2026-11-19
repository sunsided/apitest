"use strict";
// Programmatic access to the native binary, e.g. from build scripts:
//   const { binaryPath } = require("@apitest/cli");
//   execFileSync(binaryPath(), ["run", "suites/"], { stdio: "inherit" });

const path = require("node:path");
const platforms = require("./platforms.json");

function isMusl() {
  if (process.platform !== "linux") return false;
  try {
    // glibcVersionRuntime is only reported on glibc systems.
    return !process.report.getReport().header.glibcVersionRuntime;
  } catch {
    return false;
  }
}

/** Candidate platform packages for this machine, best match first. */
function candidates() {
  const os = process.platform;
  const cpu = process.arch;
  const libc = isMusl() ? "musl" : "glibc";
  return platforms
    .filter((p) => p.os === os && p.cpu === cpu)
    .sort((a, b) => Number(b.libc === libc) - Number(a.libc === libc));
}

/** Absolute path of the native `apitest` binary. */
function binaryPath() {
  if (process.env.APITEST_BINARY) return process.env.APITEST_BINARY;
  const found = candidates();
  for (const p of found) {
    try {
      return require.resolve(`${p.package}/bin/apitest${p.exe ?? ""}`);
    } catch {
      // not installed; try the next candidate
    }
  }
  const tried = found.map((p) => p.package).join(", ") || "none";
  throw new Error(
    `apitest: no native binary for ${process.platform}-${process.arch} (looked for: ${tried}).\n` +
      "  - Was the install run with --omit=optional / --no-optional? The binary ships as an optional dependency.\n" +
      "  - Set APITEST_BINARY=/path/to/apitest to use your own build,\n" +
      "  - or install it with: cargo install apitest-cli",
  );
}

module.exports = { binaryPath, candidates, typesPath: path.join(__dirname, "types", "index.d.ts") };
