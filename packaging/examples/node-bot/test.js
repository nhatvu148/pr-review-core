"use strict";

// Exercises the bot end to end with a FAKE engine binary, so it needs no API
// key, no GitHub token and no pull request. `KANISCOPE_BINARY_PATH` is the
// documented override the client checks before anything else, which makes the
// whole review path substitutable in one environment variable.
//
// What is actually being tested is the part the engine does not supply: does a
// forged signature get rejected, does a draft get skipped, and when a review IS
// warranted, is the binary invoked with the arguments the payload implies.
//
//   npm test

const crypto = require("node:crypto");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");

const SECRET = "test-secret";
process.env.GITHUB_WEBHOOK_SECRET = SECRET;
process.env.PORT = "0"; // let the OS pick a free port

const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "kaniscope-bot-test-"));
const ARGV_LOG = path.join(tmp, "argv.txt");

// A stand-in for the engine: records how it was called, then answers with a
// valid RunReviewOutput so the bot's success path runs for real.
const fake = path.join(tmp, "fake-kaniscope.sh");
fs.writeFileSync(
  fake,
  `#!/bin/sh
printf '%s\\n' "$*" >> ${JSON.stringify(ARGV_LOG)}
cat <<'EOF'
{"model":"fake","findings":2,"inlinePosted":2,"posted":true,"pr":42,
 "provider":"github","recommendation":"APPROVE WITH CHANGES","repo":"me/app",
 "summaryMarkdown":"","commentUrl":"https://example.test/c/1"}
EOF
`,
  { mode: 0o755 }
);
process.env.KANISCOPE_BINARY_PATH = fake;

const { server } = require("./server.js");

const failures = [];
function check(name, ok, detail = "") {
  console.log(`${ok ? "ok  " : "FAIL"} ${name}${detail ? ` — ${detail}` : ""}`);
  if (!ok) failures.push(name);
}

function sign(body) {
  return "sha256=" + crypto.createHmac("sha256", SECRET).update(body).digest("hex");
}

async function post(port, payload, { signature, event = "pull_request" } = {}) {
  const body = JSON.stringify(payload);
  const res = await fetch(`http://127.0.0.1:${port}/webhook`, {
    method: "POST",
    headers: {
      "content-type": "application/json",
      "x-github-event": event,
      "x-hub-signature-256": signature === undefined ? sign(body) : signature,
    },
    body,
  });
  return res.status;
}

const prEvent = (action, extra = {}) => ({
  action,
  repository: { full_name: "me/app" },
  pull_request: { number: 42, draft: false, ...extra },
});

/** Wait for the fake binary to be invoked, or give up. */
async function waitForInvocation(ms = 3000) {
  const until = Date.now() + ms;
  while (Date.now() < until) {
    if (fs.existsSync(ARGV_LOG)) return fs.readFileSync(ARGV_LOG, "utf8").trim();
    await new Promise((r) => setTimeout(r, 50));
  }
  return "";
}

(async () => {
  await new Promise((r) => server.listen(0, r));
  const { port } = server.address();

  // A forged signature must never reach the engine. This is the one piece of
  // security-relevant code the bot author writes, so it is the one most worth a
  // test.
  check("a bad signature is rejected", (await post(port, prEvent("opened"), { signature: "sha256=deadbeef" })) === 401);
  check("a missing signature is rejected", (await post(port, prEvent("opened"), { signature: null })) === 401);

  // Policy: reviews cost minutes and rate limit, so most events are not worth one.
  check("a push (synchronize) is skipped", (await post(port, prEvent("synchronize"))) === 204);
  check("a draft is skipped", (await post(port, prEvent("opened", { draft: true }))) === 204);
  check("a non-PR event is skipped", (await post(port, prEvent("opened"), { event: "issues" })) === 204);

  // A signed payload is authentic, not well-formed. These fields are read
  // BEFORE the 202, because reading them after it throws inside an async
  // listener with nobody left to catch it — an unhandled rejection that takes
  // the process and every in-flight review with it.
  check(
    "a signed payload missing repository is rejected, not crashed on",
    (await post(port, { action: "opened", pull_request: { number: 42, draft: false } })) === 400
  );
  check(
    "a signed payload missing pull_request.number is rejected",
    (await post(port, { action: "opened", repository: { full_name: "me/app" }, pull_request: { draft: false } })) === 400
  );

  // The body has to be bounded BEFORE the signature can be checked, since the
  // check needs the body. Without a cap, any unauthenticated client can make the
  // process buffer until it dies.
  {
    const huge = JSON.stringify({ action: "opened", pad: "x".repeat(3 * 1024 * 1024) });
    let status = 0;
    try {
      const res = await fetch(`http://127.0.0.1:${port}/webhook`, {
        method: "POST",
        headers: { "content-type": "application/json", "x-github-event": "pull_request",
                   "x-hub-signature-256": sign(huge) },
        body: huge,
      });
      status = res.status;
    } catch {
      // A destroyed socket can surface as a fetch error rather than a 413; both
      // mean the server refused to buffer it, which is the property under test.
      status = 413;
    }
    check("an oversized body is refused before it can exhaust memory", status === 413, `status=${status}`);
  }

  check("nothing has reached the engine yet", (await waitForInvocation(300)) === "");

  // The real path.
  check("a real PR is accepted immediately", (await post(port, prEvent("opened"))) === 202);
  const argv = await waitForInvocation();
  check(
    "the engine was called with the payload's repo and PR",
    argv.includes("--provider github") && argv.includes("--repo me/app") && argv.includes("--pr 42"),
    argv || "(never invoked)"
  );
  check("and asked for JSON", argv.includes("--json"), argv);

  server.close();
  fs.rmSync(tmp, { recursive: true, force: true });

  console.log();
  if (failures.length) {
    console.error(`${failures.length} failure(s): ${failures.join(", ")}`);
    process.exit(1);
  }
  console.log("node bot example: all checks passed");
})();
