# A GitHub review bot in Node

A working bot on the [`kaniscope`](https://www.npmjs.com/package/kaniscope) npm package. Around eighty lines, of which maybe twenty are the bot.

```sh
npm install
OPENROUTER_API_KEY=... GH_TOKEN=... GITHUB_WEBHOOK_SECRET=... npm start
# then point a GitHub webhook (pull_request events) at http://your-host/webhook
```

## What you are not writing

The single `review()` call covers all of this:

fetching the PR diff · glob-filtering it · ranking and packing it to a token budget · tree-sitter structural context · complexity metrics · blast radius · OSV lockfile scanning · the model call · a self-critique pass · confidence floor and severity cap · anchoring each finding to a line the API will accept · re-anchoring when the model drifts a line or two · committable `suggestion` blocks · **posting** the inline comments and the summary · and, on a second review, reconciling its own earlier comments rather than stacking a duplicate set.

That is the part that takes months.

## What you are writing

Three things, and they are yours because they are where bots differ:

- **An HTTP server.** Any framework; this uses `node:http` to keep the dependency list at one entry.
- **Signature verification.** The engine has `webhook::verify_signature` in Rust, but the binary does not expose it — so `verifySignature` here is the real thing, HMAC-SHA256 over the **raw** body with a timing-safe compare. It is the only security-relevant code in the file.
- **When to review.** `shouldReview` skips drafts and skips `synchronize`, because pushing is the inner loop and a review costs minutes and rate limit. This is the knob most bots tune.

## Testing it without a key

```sh
npm test
```

Nine checks, no API key, no GitHub token, no pull request. `KANISCOPE_BINARY_PATH` points the client at a fake engine that records how it was called and answers with a valid `RunReviewOutput`, so the whole path runs for real:

```
ok   a bad signature is rejected
ok   a missing signature is rejected
ok   a push (synchronize) is skipped
ok   a draft is skipped
ok   a signed payload missing repository is rejected, not crashed on
ok   an oversized body is refused before it can exhaust memory
ok   a real PR is accepted immediately
ok   the engine was called with the payload's repo and PR
```

Two of those exist because the engine's own reviewer found the bugs in this file. It flagged that the body was buffered without a limit *before* the signature could be checked — an unauthenticated memory-exhaustion path — and that a signed-but-malformed payload was dereferenced **after** the 202 was sent, where a throw becomes an unhandled rejection that kills the process. Both are the kind of thing a copied example propagates, so both are fixed and tested here.

That override is worth knowing about generally — it makes the engine substitutable in one environment variable, which is what lets a bot's own tests stay fast and offline.

## Configuration

Everything beyond `provider`/`repo`/`pr` comes from the environment: the model, confidence floor, globs, agentic mode, bot identity. `kaniscope --config-docs` prints the full table, or pass overrides per call:

```js
await review({
  provider: "github", repo, pr,
  env: { MODEL: "anthropic/claude-sonnet-5", MIN_CONFIDENCE: "70", AGENTIC: "true" },
});
```

Those are strings today, and untyped — a typed `config` option generated from the engine's own spec is the intended next step.

## What this bot cannot do

Worth knowing before you build on it:

- **No PR commands.** `/ask`, `/describe` and `/review-file` exist in the engine but are not reachable from the binary, so a Node bot cannot offer them.
- **No custom review backend.** Supplying your own `ReviewBackend` needs callbacks into the host language, which a subprocess cannot do.
- **No concurrency control.** Two webhooks for one PR start two reviews. Real deployments want a per-PR lock; that is deliberately out of scope here.
