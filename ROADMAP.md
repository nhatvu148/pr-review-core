# pr-review-core roadmap

Positioning: the **fast, lightweight, single-binary, self-hostable** AI PR
reviewer — the no-Python-runtime alternative to PR-Agent / ai-review / mira.
Lean into Rust-native strengths (speed, tiny footprint, tree-sitter structural
analysis). Each tier ships as a crate minor version; both bots upgrade by
bumping `pr-review-core = "0.x"`.

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
