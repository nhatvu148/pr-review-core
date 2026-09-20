# pr-review-core roadmap

Positioning: the **embeddable** AI review engine — a library with a backend
seam (`Config` injects identity and prompt, `ReviewBackend` lets a consumer run
its own reviewer) and a read-only toolbox a coding agent can drive over MCP.
Each tier ships as a crate minor version; both bots upgrade by bumping
`pr-review-core = "0.x"`.

*Positioning note, 2026-09-19.* This line used to read "the no-Python-runtime
alternative to PR-Agent". PR-Agent has since been handed to a community-owned
org by Qodo and is no longer the thing to be an alternative to, and "fast,
lightweight, single-binary, self-hostable" now describes Alibaba's Apache-2.0
`open-code-review` — a Go binary with 37.3k stars — at least as well as it
describes us. Speed and self-hosting are table stakes; the seam is not. Rust
strengths (tiny footprint, tree-sitter structural analysis, deterministic
derived artifacts) still justify the implementation, they just no longer carry
the pitch.

## Tier 1 — trust & signal (v0.2) — cheap, high-impact
1. **Noise reduction** — self-critique pass + per-finding confidence + severity/
   confidence ranking + per-PR finding cap. Turns "annoying bot" into "trusted
   bot". Config: `SELF_CRITIQUE`, `MIN_CONFIDENCE`, `MAX_FINDINGS`.
2. **File include/exclude globs** — skip lockfiles, generated, vendored, minified
   before the LLM call. Config: `INCLUDE_GLOBS`, `EXCLUDE_GLOBS` (sensible
   defaults). Saves tokens + noise.
3. **Any OpenAI-compatible endpoint** — `LLM_BASE_URL` / `LLM_API_KEY` aliases so
   Ollama / vLLM / Together / Groq / local work out of the box (fully offline).

## Tier 2 — differentiate (v0.3)
4. **Config file** (`.prbot.toml`) — per-repo rules, excludes, model, in-repo,
   merged over env. Enables "custom rules in plain language".
5. **Tree-sitter structural context** — identify changed symbols and include their
   enclosing scope cheaply/locally, without the agentic clone. The Rust moat.
6. **Smart large-diff handling** — rank + pack files instead of blunt truncation.
7. **GitLab provider** — biggest missing platform by market share.

## Tier 3 — bigger bets (v0.4) — SHIPPED
8. ✅ **CVE / dependency scan** — OSV.dev API (HTTP-only, no embeddings) on changed
   lockfiles; surfaces severity + advisory + fix version in the summary.
9. ✅ **`/ask` and `/describe` commands** — Q&A on the PR; idempotent PR description.

## Non-goals (for now)
Full-repo embedding index, learning-loop rule synthesis — heavy; revisit only if
there's real traction. Keep the binary small and fast.
