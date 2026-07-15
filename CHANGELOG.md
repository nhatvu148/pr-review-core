# Changelog

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
