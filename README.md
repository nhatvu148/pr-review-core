# pr-review-core

[![crates.io](https://img.shields.io/crates/v/pr-review-core.svg)](https://crates.io/crates/pr-review-core)
[![docs.rs](https://img.shields.io/docsrs/pr-review-core)](https://docs.rs/pr-review-core)
[![License: MIT OR Apache-2.0](https://img.shields.io/crates/l/pr-review-core.svg)](#license)

Core engine behind self-hosted, advisory AI pull-request reviewers.

`pr-review-core` fetches a pull request's unified diff, reviews it with a Claude
model via [OpenRouter](https://openrouter.ai), and posts line-anchored inline
comments plus a summary comment. It works with **GitHub**, **GitLab**, and
**Bitbucket**, and optionally runs an *agentic* pass that clones the repo and lets
the model investigate cross-file context (grep / read_file / list_dir) before
writing its findings.

This crate is a **library** — it carries no bot identity of its own. Consumers
(the actual bot binaries) depend on it and inject their branding and any extra
prompt through [`Config`].

## Used by

- **🦀 Kaniscope** — built entirely on this crate:
  - a hosted **[playground](https://kaniscope.nvnv.app)** (paste a diff or a GitHub PR URL → get a review), and
  - a **[GitHub Action](https://github.com/marketplace/actions/kaniscope-ai-code-review)** on the Marketplace (`uses: nhatvu148/kaniscope-action@v1`).

## What's in the box

- Provider-agnostic review flow (`review::run_review`) across GitHub, GitLab, and
  Bitbucket.
- Structured JSON review from the model, anchored to diff lines that the provider
  will accept (out-of-diff findings fold into the summary).
- Optional agentic reviewer with a two-tier model split (cheap explore model +
  stronger synthesis model).
- **Structural context**: tree-sitter identifies which functions/symbols each
  change belongs to (Rust/TS/TSX/JS/Python/Go), computed locally without a clone,
  with a git hunk-header fallback.
- **Blast radius** (agentic path): from the clone, precomputes the callers and
  tests of each changed symbol and seeds the reviewer with them (plus a
  `references(symbol)` tool), so it catches cross-file breakage — a changed
  function whose caller or test still expects the old shape — without hand-rolling
  greps. Fail-open; tune with `BLAST_RADIUS` / `BLAST_MAX_SYMBOLS` / `BLAST_MAX_REFS`.
- **Smart diff packing**: on large PRs, whole files are ranked (source > tests >
  docs) and packed to the budget instead of blunt truncation; omitted files are
  named to the model.
- **Dependency vulnerability scan**: added lockfile entries (Cargo/npm/yarn/pnpm/
  Go/PyPI/RubyGems/Composer) are checked against [OSV.dev](https://osv.dev) and
  known CVEs are surfaced in the summary with severity + fix version — no local
  resolver, HTTP-only.
- **PR commands**: `/ask <question>` answers questions about the PR from its diff;
  `/describe` (re)generates the PR description idempotently, preserving human edits.
- **Per-repo config**: a `.prbot.toml` at the repo root overrides model, globs,
  confidence/caps, and adds free-text review `instructions`.
- **Noise control**: an optional self-critique pass drops false positives / nits,
  a per-finding confidence score drives ranking, and a per-PR cap keeps reviews
  focused.
- **File globs**: lockfiles, generated, vendored, and minified files are excluded
  from the diff before the model ever sees them (saves tokens and noise).
- **Any OpenAI-compatible endpoint**: point it at OpenRouter, or Ollama / vLLM /
  Together / Groq / a local server via `LLM_BASE_URL` + `LLM_API_KEY`.
- Webhook signature verification and payload parsing helpers.
- Dedupe: the bot updates its own prior comments on re-review instead of stacking.

## Injecting identity and prompt

Nothing about the bot's identity is hardcoded. `Config::from_env()` reads:

| Field | Env var | Default |
| --- | --- | --- |
| `comment_marker` | `COMMENT_MARKER` | `🤖 ai-pr-review` |
| `user_agent` | `USER_AGENT` | `pr-review-core` |
| `http_referer` | `OPENROUTER_HTTP_REFERER` | `https://github.com/nhatvu148/pr-review-core` |
| `x_title` | `OPENROUTER_X_TITLE` | `pr-review` |
| `extra_system_prompt` | `EXTRA_SYSTEM_PROMPT` / `EXTRA_SYSTEM_PROMPT_FILE` | *(empty)* |

- `comment_marker` is the signature appended to every comment and the dedupe key
  used to find/update the bot's own comments.
- `extra_system_prompt` is appended to the built-in system prompts. Set it inline
  via `EXTRA_SYSTEM_PROMPT`, or point `EXTRA_SYSTEM_PROMPT_FILE` at a file baked
  into your Docker image to inject a large conventions block without touching the
  library.

Other operational settings (OpenRouter key/models, provider tokens, agentic mode,
size caps) are also read from the environment — see `src/config.rs`.

## Review quality & cost controls

| Env var | Default | Effect |
| --- | --- | --- |
| `SELF_CRITIQUE` | `true` | Second skeptical pass that removes false positives / low-value nits. |
| `MIN_CONFIDENCE` | `0` | Drop findings below this confidence (0–100). |
| `MAX_FINDINGS` | `20` | Cap findings per PR (ranked by severity then confidence). |
| `EXCLUDE_GLOBS` | lockfiles, generated, vendored, minified | Comma-separated globs skipped before the LLM call. |
| `INCLUDE_GLOBS` | *(empty = all)* | If set, only files matching these globs are reviewed. |
| `LLM_BASE_URL` | `OPENROUTER_BASE_URL` → openrouter | OpenAI-compatible endpoint (e.g. `http://localhost:11434/v1` for Ollama). |
| `LLM_API_KEY` | `OPENROUTER_API_KEY` | API key for the endpoint above. |
| `CVE_SCAN` | `true` | Scan changed lockfiles for known-vulnerable deps via OSV.dev. |
| `CVE_MAX_PACKAGES` | `100` | Max distinct packages queried against OSV per review. |
| `OSV_API_BASE` | `https://api.osv.dev` | OSV API base (override for a mirror/test double). |

## PR commands

Wire a comment webhook (see the bot binaries) and the reviewer answers these
commands posted as PR comments:

| Command | Effect |
| --- | --- |
| `/review` | (Re)run the full review. |
| `/ask <question>` | Answer a question about the PR, grounded in its diff. |
| `/describe` | (Re)generate the PR description, merged idempotently into the body. |

Route them from a bot binary with [`command::parse_command`] + [`command::run_command`].

[`command::parse_command`]: src/command.rs
[`command::run_command`]: src/command.rs

## License

Licensed under either of

- MIT license ([LICENSE-MIT](LICENSE-MIT))
- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))

at your option.

## Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.

[`Config`]: src/config.rs
