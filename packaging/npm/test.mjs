// Regression tests for the Node client.
//
// Written against a fake binary rather than the real engine, so they need no API
// key, no network and no PR — which is what makes them cheap enough to run on
// every push. Each case is a bug one of these clients actually shipped.
//
//   node packaging/npm/test.mjs

import { execFileSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
const client = require("./index.js");

const failures = [];
function check(name, ok, detail = "") {
  console.log(`${ok ? "ok  " : "FAIL"} ${name}${detail ? ` — ${detail}` : ""}`);
  if (!ok) failures.push(name);
}

/** A throwaway executable standing in for the engine. */
function fakeBinary(script) {
  const file = path.join(fs.mkdtempSync(path.join(os.tmpdir(), "kaniscope-test-")), "fake.sh");
  fs.writeFileSync(file, `#!/bin/sh\n${script}\n`, { mode: 0o755 });
  return file;
}

// An unknown option must throw rather than be dropped. `dry_run` is the natural
// typo when porting from the Python client, where that IS the spelling; dropped
// silently it posts a live review while the caller believes they asked for a dry
// run. The Python client shipped exactly this hole on its async path.
await client
  .review({ dry_run: true })
  .then(() => check("an unknown option is rejected", false, "it was accepted"))
  .catch((err) => check("an unknown option is rejected", err instanceof TypeError, err.message));

// stdout is one JSON line of unbounded length. `execFile` would cap it at 1 MB by
// default and fail on exactly the large PRs most worth reviewing, so this client
// accumulates chunks instead.
{
  const payload = JSON.stringify({
    model: "m",
    findings: 0,
    inlinePosted: 0,
    posted: false,
    pr: 0,
    provider: "p",
    recommendation: "r",
    repo: "r",
    summaryMarkdown: "y".repeat(2_000_000),
  });
  const binary = fakeBinary(`cat <<'EOF'\n${payload}\nEOF`);
  const out = await client.review({ binary });
  check(
    "a review past the 1 MB buffer default parses",
    out.summaryMarkdown.length === 2_000_000,
    `${Math.round(payload.length / 1024)} KiB single line`
  );
}

// A failed review must carry why it failed, not just that it did.
{
  const binary = fakeBinary("echo 'the cause' >&2\nexit 3");
  await client
    .review({ binary })
    .then(() => check("a non-zero exit rejects with its stderr", false, "no error"))
    .catch((err) =>
      check(
        "a non-zero exit rejects with its stderr",
        err.exitCode === 3 && err.stderr.includes("the cause"),
        `exit=${err.exitCode}`
      )
    );
}

// A binary that exits 0 with output the client cannot parse is a DIFFERENT
// failure from a review that failed, and needs a different fix. The message has
// to say which.
{
  const binary = fakeBinary("echo 'not json at all'");
  await client
    .review({ binary })
    .then(() => check("unparseable stdout is reported as such", false, "no error"))
    .catch((err) =>
      check("unparseable stdout is reported as such", err.message.includes("was not JSON"))
    );
}

// The override is the documented escape hatch for an unsupported platform, so it
// has to be read BEFORE the supported-platform check that names it.
{
  const previous = process.env.KANISCOPE_BINARY_PATH;
  process.env.KANISCOPE_BINARY_PATH = "/nonexistent/kaniscope";
  check(
    "KANISCOPE_BINARY_PATH wins over the platform check",
    client.binaryPath() === "/nonexistent/kaniscope"
  );
  if (previous === undefined) delete process.env.KANISCOPE_BINARY_PATH;
  else process.env.KANISCOPE_BINARY_PATH = previous;
}

// The CLI shim must pass the child's exit code through: a review that failed
// must not look like a pass because a wrapper swallowed it.
{
  const binary = fakeBinary("exit 7");
  let code = 0;
  try {
    execFileSync(process.execPath, [path.join(import.meta.dirname, "bin", "kaniscope.js")], {
      env: { ...process.env, KANISCOPE_BINARY_PATH: binary },
      stdio: "ignore",
    });
  } catch (err) {
    code = err.status;
  }
  check("the CLI shim propagates the exit code", code === 7, `exit=${code}`);
}

console.log();
if (failures.length) {
  console.error(`${failures.length} failure(s): ${failures.join(", ")}`);
  process.exit(1);
}
console.log("node client: all checks passed");
