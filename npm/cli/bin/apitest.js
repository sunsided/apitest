#!/usr/bin/env node
"use strict";
// Thin launcher: finds the platform binary from the matching optional
// dependency and execs it with the same arguments, stdio and exit code.

const { spawnSync } = require("node:child_process");
const { binaryPath } = require("../index.js");

let bin;
try {
  bin = binaryPath();
} catch (e) {
  console.error(e.message);
  process.exit(2);
}

const result = spawnSync(bin, process.argv.slice(2), { stdio: "inherit", windowsHide: true });
if (result.error) {
  console.error(`apitest: failed to start ${bin}: ${result.error.message}`);
  process.exit(2);
}
if (result.signal) {
  // Re-raise the signal so callers see the same termination reason.
  process.kill(process.pid, result.signal);
}
process.exit(result.status ?? 1);
