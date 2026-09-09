# AGENTS.md

Orientation for an automated contributor. Everything here is also true for a human — there are no separate rules for agents, only the ones you would need anyway. Read [CONTRIBUTING.md](CONTRIBUTING.md) too; it carries the conventions this file points at rather than repeats.

## What this is

A **library**, not a bot. It carries no identity of its own: consumers (bot binaries) depend on it and inject branding and extra prompt through `Config`. The `kaniscope` binary in `packaging/` is a thin CLI over the same library, and its `--json` output is the wire contract every non-Rust consumer reads.

## Before you claim anything works

CI runs exactly these three and they must be clean. `clippy` runs with `-D warnings`, so a warning is a failure:

```sh
cargo fmt --check
cargo clippy --all-features --all-targets -- -D warnings
cargo test --all-features
```

Run them. A change that has not been through all three is not finished, however obviously correct it looks.

## The review pipeline

One call, `review::run_review` → `run_review_with` (`src/review.rs`), runs the whole thing in this order. Each stage names the module that owns it.

| # | stage | module |
|---|---|---|
| 1 | resolve the provider, fetch the diff and PR metadata | `providers` (`from_name`, `get_diff`, `get_meta`) |
| 2 | merge a per-repo `.prbot.toml` from the PR head over the env config | `repo_config` |
| 3 | post an upsertable "Reviewing…" summary so the PR shows progress | `providers::post_comment` |
| 4 | scan added dependency lines for CVEs — **on the raw diff** | `deps::scan` |
| 5 | drop lockfiles, generated, vendored, minified files | `diff::filter_diff_by_globs` |
| 6 | deterministic hygiene findings | `diff::diff_hygiene_with` |
| 7 | rank and pack the diff to the token budget | `diff::pack_diff` / `pack_diff_bundled` |
| 8 | name the enclosing symbol of each changed line (tree-sitter) | `structure::inner` |
| 9 | build the walkthrough table and change diagram, if enabled | `changemap` |
| 10 | compose calibration rules and the prompt | `prompt::injected_rules` |
| 11 | run the model — agentic (clone + tools) or diff-only | `backend`, `agent::agentic_review`, `llm` |
| 12 | self-critique pass that drops false positives | `llm::critique_findings` |
| 13 | anchor each finding to a line the provider will accept | `diff::diff_line_texts`, then re-anchor when `reanchor_findings` |
| 14 | validate and render committable suggestions | `suggest::sanitize`, `suggest::render` |
| 15 | render dependency advisories into the summary | `deps::render_advisories` |
| 16 | post the inline review, reconcile against prior comments | `providers::post_review` |
| 17 | append to the run log | `runlog::append` / `write` |

Two module relationships are not visible from that list, and have surprised people: `complexity` is called by `changemap` and `structure`, not by `review`; `blast` is called by `changemap` and `agent`. Neither is a top-level pipeline stage.

`run_review_local` (same file) is the same pipeline for a diff that is **not** a pull request — a branch, a worktree, staged changes. No host, no PR number, nothing to post to.

Around the single review sit `queue` (ranking many PRs), `command` (PR comment commands), `webhook` (signature verification and payload parsing), and `config` / `config_spec` (every environment variable, and the generated reference for them).

## Rules that are easy to violate

- **Never hand-edit the generated types.** `Finding` and `RunReviewOutput` for the Node and Python clients are generated from the binary's own `--schema` by `packaging/generate-types.mjs` and committed. CI regenerates and fails on a diff. Change the Rust type, then regenerate.
- **Enrichment fails open.** The CVE scan, structural context and change map must never block a review when a service is down or a parse fails. A stage that can turn a working review into no review is a bug, not a strictness improvement.
- **Order matters at step 4.** The dependency scan reads the raw diff because step 5 removes lockfiles. Moving it after the filter silently disables it — the tests will still pass on diffs that have no lockfile.
- **Tests live beside the code**, in the module's `mod tests`, named as a sentence stating the behaviour: `an_empty_diff_is_an_error_not_an_empty_review`, not `test_review`.
- **Comment the why, not the what.** The surprising constraint, the reason the simpler version does not work, the format trap. Not a restatement of the line below.
- **Public items get doc comments**, and the examples in them run as doctests.

## Submitting

You are the author of what you submit, regardless of how it was produced: read it, verify it runs, and be able to say why it works. See [CONTRIBUTING.md](CONTRIBUTING.md#ai-assisted-contributions) — a PR the submitter cannot explain will be closed.

Match the file you are editing rather than any general style guide. `CHANGELOG.md` has its own rules in [CONTRIBUTING.md](CONTRIBUTING.md#the-changelog).
