# Changelog

## 0.9.0

Five additions this cycle — deterministic **complexity metrics**, **smart diff
bundling**, a **benchmark harness**, **finding re-anchoring**, and a full-file
**`/review-file`** command.

**Complexity metrics**

- Deterministic **cyclomatic (McCabe) + cognitive** complexity with an A–F grade
  for the functions a change touches, computed with tree-sitter from the files
  already fetched for structural context — no LLM, no extra fetch. Named arrow
  functions are measured (name recovered from the binding), and the parse is
  shared with structural context so each file is parsed once. Only functions
  at/above `COMPLEXITY_MIN_CYCLOMATIC` are surfaced. Toggle with
  `COMPLEXITY_METRICS`. Fully fail-open.

**Smart diff bundling**

- On large PRs, related changed files — a source and its test, i18n siblings —
  now pack as one unit and stay adjacent so the model reviews them together
  instead of having them scattered by priority. Toggle with `FILE_BUNDLING`;
  per-repo overridable via `.prbot.toml`.

**Re-anchor drifted findings**

- A finding whose line drifted just off a real diff line is snapped to the
  nearest diff line (within a ±3 window) whose code shares a significant symbol
  with the finding body — otherwise it folds to the summary as before. The match
  requires a distinctive shared symbol (filtered both sides, length ≥4) and
  breaks ties deterministically, so a wrong inline anchor is never posted. Toggle
  with `REANCHOR_FINDINGS`; per-repo overridable.

**`/review-file <path>` command**

- Deep-review an entire file at the PR head, beyond just the diff — findings may
  sit on any line and post as a summary comment. Honors the repo's include/exclude
  globs (an excluded path is politely refused). User-supplied paths are
  percent-encoded before reaching the provider APIs so they can't override the
  fetched ref.

**Benchmark harness** (`examples/bench.rs`)

- Scores the reviewer against a corpus of PRs with known issues, reporting
  labeled **precision / recall / F1** and token cost, with a replicates mode
  (mean ± spread) to average out agentic non-determinism — so a feature's effect
  can be A/B'd by toggling its flag. `RunReviewOutput.findings_detail` exposes the
  structured findings for tooling. _Note: blast radius (0.8.0) was measured with
  this harness and showed no recall improvement on typical well-named repos; it's
  kept on but the docs no longer claim it "catches more bugs."_

## 0.8.0

**Blast radius** — the agentic reviewer is now seeded with each changed symbol's
callers, tests, and type uses, computed locally from the clone, so it catches
cross-file breakage without hand-rolling greps to rediscover them.

- New `blast` module: for every changed symbol, a clone-wide search finds its
  **callers**, **tests**, and (TS/TSX) **type uses**, rendered as a `## Blast
  radius` block injected into the agentic prompt. Also exposes a
  `references(symbol)` tool. Fully fail-open; tune with `BLAST_RADIUS`,
  `BLAST_MAX_SYMBOLS`, and `BLAST_MAX_REFS`.
- **TS/TSX precision**: references are classified with tree-sitter, so JSX renders
  (`<Comp/>`, `<Ns.Comp/>`) count as callers and type positions (`: T`, `Foo<T>`,
  return types) populate a dedicated "type uses" bucket — references a `name(`
  call grep can't see. Rust/Go/Python keep the grep call path.
- Non-JS caller discovery uses a narrow `NAME(` grep (undiluted fetch budget);
  JS-family files are located by a broad grep and AST-classified. Buckets are
  de-duplicated by `(path, line)`, and a capped clone-wide search is flagged so an
  empty bucket is never misread as "no callers".

## 0.7.0

Migrated the agentic reviewer onto the shared [`agent-loop-core`](https://crates.io/crates/agent-loop-core) crate, and **fixed a live bug**: the agentic path built its HTTP client with no timeout, so a stalled provider hung the entire review and a single 429 discarded it.

**Resilient transport (the fix)**

- The agentic review loop now runs on `agent-loop-core`, whose chat transport
  carries a per-request **timeout** and **429/5xx retry** with backoff — both
  configurable via `OPENROUTER_TIMEOUT_SECS` (default 120) and
  `OPENROUTER_MAX_RETRIES` (default 3). Previously a hung OpenRouter connection
  had no upper bound.

**Loop internals moved to `agent-loop-core`**

- The tool-calling loop, history compaction, and the two-model explore/synthesize
  split now live in the shared crate (also consumed by other reviewers). The three
  repo tools (`grep` / `read_file` / `list_dir`) became typed tools — the schema
  is derived from the argument type, so the advertised schema and the arguments a
  tool actually reads can no longer drift.
- Malformed tool arguments are now **rejected** (reported to the model) instead of
  silently running the tool with defaults — previously a malformed `grep` became a
  repo-wide empty-regex match.

**Dependency note**

- Adds `agent-loop-core = "0.1"`. Its transport uses `rustls` with bundled
  `webpki-roots` (via reqwest 0.12), so it needs **no cmake to build and no
  `ca-certificates` at runtime** — consumers on minimal images are unaffected.

The public API (`run_review`, `ReviewBackend`, `Review`, the command/webhook
surface) is unchanged; this is an internals + resilience release.

## 0.6.0

Review lifecycle + a backend seam for `/ask` and `/describe`.

**Command backend seam**

- **`ReviewBackend::complete`**: a free-form text-completion method (default: the
  OpenRouter chat path) that powers `/ask` and `/describe`, so those commands run
  on the **same backend as reviews** instead of always OpenRouter. A consumer
  using an agent-CLI backend gets `/ask` and `/describe` on that backend too.
- **`command::run_command_with(..., &dyn ReviewBackend)`**: like `run_command`
  but with a caller-supplied backend; `run_command` is that with the default
  OpenRouter backend, and `/review` now honors the supplied backend as well.

**Review lifecycle — reconcile instead of delete-and-repost (GitHub)**

- **Fingerprinted findings**: each inline comment carries a hidden fingerprint
  (file + normalized body). On re-review, GitHub now **reconciles** rather than
  deleting all prior comments and reposting: a finding still present is **left in
  place** (no notification churn, thread history preserved), a new finding is
  **posted**, and a finding that's gone is **cleaned up**.
- **Robust finding matching**: findings are paired to existing comments **1:1**
  (each thread claimed once) by fingerprint **or** `(file, line)` — the line key
  keeps a *reworded* but still-present finding matched (LLM output isn't stable),
  and the 1:1 pairing prevents two findings sharing a line from both matching one
  thread (which could drop a genuinely-new finding or pin a stale thread open).
- **Upgrade migration**: legacy comments from 0.5.0 (bot marker, no fingerprint)
  are recognized and cleaned up (resolved/deleted) on the first 0.6 review rather
  than left as orphaned, undedupable threads.
- **Resolve, with a delete fallback**: a gone finding's **review thread is
  resolved** (GraphQL `resolveReviewThread`) with a "✅ Resolved" reply. If the
  token can't resolve threads (a common PAT limitation — "Resource not accessible
  by personal access token"), the comment is **deleted** instead, so gone
  findings never pile up as stale open threads.
- **"Resolved since last review"** summary section; the reconcile runs even when
  a re-review finds nothing, so prior findings are cleaned up and reported.
- Fully fail-soft: a GraphQL hiccup logs and degrades to posting the summary.
- GitLab and Bitbucket keep the prior delete-and-repost behavior for now
  (GitLab port to follow; Bitbucket renders HTML markers literally).

## 0.5.0

Pluggable review backend.

- **`ReviewBackend` seam**: the model step of a review is now a trait
  (`backend::ReviewBackend`) fed a `backend::ReviewContext` (client, config,
  provider, repo, PR meta, prepared diff, omitted-files note, structural
  context). `review::run_review_with(cfg, input, &dyn ReviewBackend)` runs the
  full pipeline — diff fetch, glob filter, packing, structural context,
  dependency scan, finding post-processing, anchoring, posting — and delegates
  only the model call. Lets a consumer plug in a different reviewer (e.g. an AI
  agent CLI driven over a repo clone) while reusing everything around it.
- **`OpenRouterBackend`**: the default backend (Claude via OpenRouter, agentic
  loop + diff-only fallback). `run_review` is now `run_review_with` with this
  backend, so existing behavior and API are unchanged.

## 0.4.0

Tier 3 — bigger bets.

- **Dependency vulnerability scan** (OSV.dev): parses the versions added by a PR
  from changed lockfiles (`Cargo.lock`, `package-lock.json`, `yarn.lock`,
  `pnpm-lock.yaml`, `go.sum`, `requirements.txt`, `Gemfile.lock`, `composer.lock`)
  and appends a known-CVE advisory block (severity, summary, fix version, link) to
  the review — even on a lockfile-only PR. HTTP-only, no local resolver.
  `CVE_SCAN`, `CVE_MAX_PACKAGES`, `OSV_API_BASE`; per-repo `cve_scan`.
- **`/ask` and `/describe` commands**: `/ask <question>` answers a question about
  the PR grounded in its diff; `/describe` (re)generates the PR description and
  merges it into the body idempotently, preserving human-written content. New
  provider capabilities (`post_comment`, `update_pr_description`) across GitHub,
  GitLab, and Bitbucket, driven by `command::run_command`.

## 0.3.0

Tier 2 — differentiate.

- **GitLab provider**: merge-request review (diff, inline discussions + summary,
  file fetch, clone, webhook helpers) alongside GitHub + Bitbucket.
- **Structural context**: tree-sitter resolves which functions/symbols each change
  touches (Rust/TS/TSX/JS/Python/Go), no clone needed, with a git hunk-header
  fallback. `STRUCTURAL_CONTEXT`, `STRUCTURAL_MAX_FILES`.
- **Smart large-diff packing**: rank files (source > tests > docs) and pack whole
  sections to the budget instead of truncating; omitted files named to the model.
- **Per-repo `.prbot.toml`**: override model, globs, confidence/caps, agentic, and
  add free-text review `instructions` — fetched from the PR head, merged over env.

## 0.2.0

Tier 1 — trust & signal.

- **Noise reduction**: optional self-critique pass (`SELF_CRITIQUE`, default on)
  removes false positives / nits; per-finding `confidence` drives ranking;
  `MIN_CONFIDENCE` floor and `MAX_FINDINGS` cap keep reviews focused.
- **File globs**: `EXCLUDE_GLOBS` (defaults skip lockfiles, generated, vendored,
  minified) and `INCLUDE_GLOBS` filter the diff before the LLM call — big token
  savings (e.g. a package-lock.json PR dropped from ~73k to ~1k tokens).
- **Any OpenAI-compatible endpoint**: `LLM_BASE_URL` / `LLM_API_KEY` aliases so
  Ollama, vLLM, Together, Groq, or a local server work out of the box.

## 0.1.0

Initial release: diff fetch, OpenRouter review, GitHub + Bitbucket providers,
inline + summary comments, agentic mode, webhook helpers. Bot identity and extra
prompt injected via `Config`.
