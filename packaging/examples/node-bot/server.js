"use strict";

// A GitHub review bot, in about eighty lines of which maybe twenty matter.
//
// The point of this file is what is NOT in it. There is no diff fetching, no
// file filtering, no packing to a token budget, no tree-sitter, no anchoring a
// finding to a line the API will accept, no re-anchoring when the model drifts,
// no committable suggestions, no posting, and no reconciling the bot's own
// comments on a second review. All of that is `review()`.
//
// What IS here is the part the engine deliberately does not do for you: an HTTP
// server, a signature check, and a decision about when a review is worth
// spending. Those are yours because they are where bots differ.
//
//   npm install && OPENROUTER_API_KEY=... GH_TOKEN=... \
//     GITHUB_WEBHOOK_SECRET=... npm start

const http = require("node:http");
const crypto = require("node:crypto");
const { review } = require("kaniscope");

const PORT = process.env.PORT || 3000;
const SECRET = process.env.GITHUB_WEBHOOK_SECRET || "";

/**
 * Verify GitHub's `X-Hub-Signature-256` over the raw body.
 *
 * `timingSafeEqual`, not `===`: comparing HMACs with a short-circuiting compare
 * leaks how long a prefix matched, one byte at a time. It also throws on a
 * length mismatch, so the lengths are checked first — a malformed header must be
 * a rejection, not a 500.
 *
 * This is the one piece of security-relevant code you are expected to write
 * yourself. The engine has `webhook::verify_signature` in Rust, but the binary
 * does not expose it, so here it is in full.
 */
function verifySignature(rawBody, header) {
  if (!SECRET || !header) return false;
  const expected = "sha256=" + crypto.createHmac("sha256", SECRET).update(rawBody).digest("hex");
  const a = Buffer.from(header);
  const b = Buffer.from(expected);
  return a.length === b.length && crypto.timingSafeEqual(a, b);
}

/** Which events are worth a review. This is the policy knob most bots tune. */
function shouldReview(event, payload) {
  if (event !== "pull_request") return false;
  // `synchronize` fires on every push. Reviewing all of them spends a
  // rate-limited quota on the changes least worth reviewing — the same reasoning
  // behind the engine's `REVIEW_ON_UPDATE` defaulting to false.
  if (!["opened", "reopened", "ready_for_review"].includes(payload.action)) return false;
  return !payload.pull_request?.draft;
}

const server = http.createServer((req, res) => {
  if (req.method !== "POST" || req.url !== "/webhook") {
    res.writeHead(404).end();
    return;
  }

  const chunks = [];
  req.on("data", (c) => chunks.push(c));
  req.on("end", async () => {
    // The RAW bytes, not a re-serialized object: the signature is over exactly
    // what GitHub sent, and `JSON.stringify(JSON.parse(body))` is not that.
    const raw = Buffer.concat(chunks);

    if (!verifySignature(raw, req.headers["x-hub-signature-256"])) {
      res.writeHead(401).end("bad signature");
      return;
    }

    let payload;
    try {
      payload = JSON.parse(raw.toString());
    } catch {
      res.writeHead(400).end("bad json");
      return;
    }

    const event = req.headers["x-github-event"];
    if (!shouldReview(event, payload)) {
      res.writeHead(204).end();
      return;
    }

    // Acknowledge BEFORE reviewing. A review takes minutes and GitHub gives a
    // webhook ten seconds; holding the connection open earns a delivery failure
    // and a redelivery, which reviews the same PR twice.
    res.writeHead(202).end("reviewing");

    const repo = payload.repository.full_name;
    const pr = payload.pull_request.number;
    try {
      const out = await review({
        provider: "github",
        repo,
        pr,
        onLog: (line) => console.error(`[${repo}#${pr}] ${line}`),
      });
      console.log(
        `[${repo}#${pr}] ${out.recommendation} — ${out.findings} finding(s), ` +
          `${out.inlinePosted} inline${out.commentUrl ? ` — ${out.commentUrl}` : ""}`
      );
    } catch (err) {
      // Never rethrow here: the response has already been sent, so an unhandled
      // rejection would take the process down and lose every other in-flight
      // review with it.
      console.error(`[${repo}#${pr}] review failed: ${err.message}`);
    }
  });
});

if (require.main === module) {
  server.listen(PORT, () => console.log(`listening on :${PORT}/webhook`));
}

module.exports = { server, verifySignature, shouldReview };
