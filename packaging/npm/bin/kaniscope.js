#!/usr/bin/env node
"use strict";

// The CLI entry point: hand this process straight to the native binary.
//
// `spawnSync` with stdio inherited rather than `execFileSync`, so stdin still
// pipes (`git diff | kaniscope --local`) and the child's exit code is this
// process's exit code — a review that fails must not exit 0 through a wrapper.

const { spawnSync } = require("node:child_process");
const { binaryPath } = require("../binary.js");

let bin;
try {
  bin = binaryPath();
} catch (err) {
  console.error(err.message);
  process.exit(1);
}

const result = spawnSync(bin, process.argv.slice(2), { stdio: "inherit" });

// A child killed by a signal has a null status. Reporting that as 0 would tell a
// CI job that a review which was OOM-killed had passed.
if (result.error) {
  console.error(`kaniscope: could not run ${bin}: ${result.error.message}`);
  process.exit(1);
}
process.exit(result.status === null ? 1 : result.status);
