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
  will accept (out-of-diff findings fold into the summary). A finding that just
  missed a diff line (model off-by-a-few / drift) is **re-anchored** to a nearby
  diff line when that line's code matches what the finding references, so small
  drift still posts inline instead of the summary (`REANCHOR_FINDINGS`).
- Optional agentic reviewer with a two-tier model split (cheap explore model +
  stronger synthesis model).
- **Structural context**: tree-sitter identifies which functions/symbols each
  change belongs to (Rust/TS/TSX/JS/Python/Go), computed locally without a clone,
  with a git hunk-header fallback.
- **Blast radius** (agentic path): from the clone, precomputes the callers, tests,
  and type uses of each changed symbol and seeds the reviewer with them (plus a
  `references(symbol)` tool), so it doesn't have to rediscover them by hand. For
  TS/TSX it uses tree-sitter, so **JSX renders (`<Comp/>`) and type positions
  (`: T`, `Foo<T>`)** count as references, not just `name(` calls. Fail-open; tune
  with `BLAST_RADIUS` / `BLAST_MAX_SYMBOLS` / `BLAST_MAX_REFS`.
  _Measured note: on typical, well-named repos this showed **no recall improvement**
  in benchmarking (a capable model already infers cross-file breakage from the diff
  via names/types/docs); it may still help on large monorepos or poorly-named code.
  On by default — measure on your repos before relying on it._
- **Complexity metrics**: deterministic cyclomatic + cognitive complexity (with an
  A–F grade) for the functions a change touches, computed with tree-sitter from the
  files already fetched for structural context — no LLM, no extra fetch. Only
  functions at/above `COMPLEXITY_MIN_CYCLOMATIC` are surfaced, as a risk signal.
  Toggle with `COMPLEXITY_METRICS`.
- **Smart diff packing**: on large PRs, whole files are ranked (source > tests >
  docs) and packed to the budget instead of blunt truncation; omitted files are
  named to the model. With **file bundling** (`FILE_BUNDLING`), related files — a
  source and its test, i18n siblings — pack as one unit and stay adjacent so the
  model reviews them together, rather than being scattered by priority.
- **Dependency vulnerability scan**: added lockfile entries (Cargo/npm/yarn/pnpm/
  Go/PyPI/RubyGems/Composer) are checked against [OSV.dev](https://osv.dev) and
  known CVEs are surfaced in the summary with severity + fix version — no local
  resolver, HTTP-only. Python coverage reads `requirements.txt` (`==` pins only),
  `poetry.lock`, `uv.lock` and `pdm.lock`.
- **PR commands**: `/ask <question>` answers questions about the PR from its diff;
  `/describe` (re)generates the PR description idempotently, preserving human edits;
  `/review-file <path>` deep-reviews an entire file at the PR head, beyond just the diff.
- **Per-repo config**: a `.prbot.toml` at the repo root overrides model, globs
  (including `vendored`, which marks third-party source the reviewer must not file
  hygiene findings on or propose edits inside), confidence/caps, `pr_body` and
  `pr_body_max_chars` (how much of the PR description the reviewer reads — a repo
  whose PRs run long descriptions wants a higher cap), `grep_context`, and adds
  free-text review `instructions`.
- **Benchmark harness**: `examples/bench.rs` scores the reviewer against a corpus
  of PRs with known issues (`examples/bench-corpus.example.json`) — reporting
  precision / recall / F1 and token cost, so a feature's effect (blast radius,
  complexity, backend) can be A/B'd by re-running with the flag toggled. Dry-run;
  needs an OpenRouter key. `RunReviewOutput.findings_detail` exposes the structured
  findings for tooling.
- **Walkthrough & change diagram** (opt-in): a per-file table (line counts, changed
  definitions, worst complexity grade, findings filed there) and a mermaid graph of
  how the changed symbols reference each other — both *derived* from the tree-sitter
  parse that already ran, never model-written, so every row and arrow can be checked
  against the code. `WALKTHROUGH` / `DIAGRAM`.
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
| `REANCHOR_FINDINGS` | `true` | Snap a finding that drifted just off a diff line to the nearest diff line sharing a code symbol (else it folds to the summary). |
| `PR_BODY` | `true` | Pass the PR's own description to the reviewer as a statement of intent to check the diff against. It is author-written, so it is rendered inside an untrusted fence and can never direct the review — see `REVIEW_RULES` → *Untrusted content*. `/ask` and `/describe` never receive it. |
| `PR_BODY_MAX_CHARS` | `12000` | Cap on the description handed to the reviewer. A description clipped by this is explicitly marked truncated in the prompt, with an instruction not to infer undeclared scope from what is missing — clipping a *statement of intent* otherwise manufactures "this is out of scope" findings the missing text would refute. |
| `GREP_CONTEXT` | `true` | Let the agentic reviewer's `grep` ask for 1–8 lines of context around each match, so it can judge what a second site *does* without a `read_file` round trip. Off ignores the argument entirely. **OpenRouter agentic path only** — a `ReviewBackend` driving an agent CLI brings its own tools. |
| `EXCLUDE_GLOBS` | lockfiles, generated, vendored, minified | Comma-separated globs skipped before the LLM call. |
| `INCLUDE_GLOBS` | *(empty = all)* | If set, only files matching these globs are reviewed. |
| `VENDORED_GLOBS` | `thirdparty/`, `third_party/`, `vendor/`, `vendored/`, `external/`, `node_modules/` | Globs marking vendored third-party source. Diff-hygiene findings are suppressed inside them and the reviewer is told not to propose edits there — committing vendored code in bulk is the intent, not a defect. Setting this REPLACES the defaults. |
| `LLM_BASE_URL` | `OPENROUTER_BASE_URL` → openrouter | OpenAI-compatible endpoint (e.g. `http://localhost:11434/v1` for Ollama). |
| `LLM_API_KEY` | `OPENROUTER_API_KEY` | API key for the endpoint above. |
| `CI_STATUS` | `true` | Fetch the head commit's CI results (GitHub check runs / Bitbucket build statuses) and show them in the prompt, so the reviewer can't assert a broken build that CI already decided. One extra API call per review; set `false` for tokens near their rate limit. |
| `DESCRIBE_INSTRUCTIONS` | *(empty)* | Free-form instructions shaping `/describe` output (sections, tables, house layout). Outranks the built-in layout. |
| `CVE_SCAN` | `true` | Scan changed lockfiles for known-vulnerable deps via OSV.dev. |
| `CVE_MAX_PACKAGES` | `100` | Max distinct packages queried against OSV per review. |
| `OSV_API_BASE` | `https://api.osv.dev` | OSV API base (override for a mirror/test double). |

## Walkthrough & change diagram (opt-in)

Two renderings appended to the summary comment, both **derived from the parse the
reviewer already ran** — no model call, no extra fetch, no extra token.

`WALKTHROUGH=true` adds a per-file table: line counts, the definitions the change
landed in, the worst complexity grade among them, and the findings filed there.

`DIAGRAM=true` adds a mermaid graph of the changed symbols, with an arrow wherever
one names another inside its own definition.

| Env var | Default | Effect |
| --- | --- | --- |
| `WALKTHROUGH` | `false` | Append the per-file walkthrough table to the summary comment. |
| `WALKTHROUGH_MAX_SYMBOLS` | `4` | Symbols listed per file before the cell collapses to `(+N more)`. A Rust name runs ~40 characters, so six of them overflowed a PR comment. |
| `DIAGRAM` | `false` | Append the mermaid change diagram. Skipped on Bitbucket (no native mermaid) and whenever there are no edges to draw. |
| `DIAGRAM_MAX_NODES` | `12` | Symbols considered for edge linking, and so the diagram's node budget. Past this, linking narrows to the highest-complexity symbols (test scaffolding last) and the diagram says so. |

Both need `STRUCTURAL_CONTEXT` (on by default); the complexity column additionally
needs `COMPLEXITY_METRICS` (also on by default). Cost follows the ask: with both
off nothing is built, and with only `WALKTHROUGH` on the edge-linking scan the
diagram needs is skipped.

The diagram is deliberately **not themed**. GitHub renders mermaid with a theme
that follows the viewer's light/dark preference, and an injected
`%%{init: {'theme':…}}%%` would override it — colors tuned on one theme become
unreadable on the other, and you would never see it. The legibility levers here
are node count, direction (`flowchart TD`, because a PR comment is a narrow
column) and ink: a grade is drawn only at C or worse, since most changed
functions are an A and annotating every box distinguishes nothing.

Two asymmetries worth knowing. The *Worst complexity* column comes from the
complexity pass directly, so it grades a TS/JS arrow function
(`const handleSubmit = () => {}`) like anything else. The *Changed symbols*
column comes from the structural pass, which resolves a symbol by walking up to
the nearest declaration it recognises — and that list has no `arrow_function`, so
such a file can show a real grade beside an empty symbol cell. That is the
structural context's existing shape, and it is the same in the prompt the model
sees.

And the two columns count different lines. *Changed symbols* resolves only the
lines the diff **added**, so a one-line edit names one definition rather than
every neighbour the hunk happened to show (a pure-deletion hunk adds nothing, and
falls back to the wider set rather than naming nothing). *Worst complexity* keeps
the wider set, because "the function you are editing near is a D" is a useful
signal even when the change itself sits beside it.

**Why nothing here is model-written.** A diagram a model draws from a diff cannot
be checked by the reader: a plausible arrow that doesn't exist in the code is
indistinguishable from a real one, and the reader will believe it. Every arrow
here is a call-shaped occurrence of one changed symbol's name inside another
changed symbol's tree-sitter span, so a suspicious one can be looked up. It is a
narrow claim, and it is the claim the caption makes — candidate references, only
between symbols this PR changed. A bare call to a name several modules define is
not disambiguated. A call through an unresolved receiver (`x.append(`) draws
dotted rather than solid, and only within a single file: across files a method
name is a collision as often as a reference, which is how a real review drew an
Angular widget's `this.loadKpis()` as an arrow to a different widget's
same-named method.

See it before it posts on anyone's PR:

```console
$ git diff main... > /tmp/pr.diff
$ cargo run --example changemap_demo -- . /tmp/pr.diff
```

The example also prints every edge with the span it came from, so an arrow can be
checked against the file rather than argued about.

## Run log (opt-in)

Set `PRBOT_RUN_LOG` and every review emits one JSON line describing the run: the
funnel of findings through each post-processing stage, the findings themselves,
token usage, and wall time.

| `PRBOT_RUN_LOG` | Sink |
| --- | --- |
| *unset or empty* | **off** — the default, nothing is written |
| `-` | **stdout**, one line per review |
| any other value | that **file**, appended to; parent directories created |

Every record carries `"_kind": "prbot_run_log"`, which is how you find them on
stdout — a channel shared with the process's own tracing output. Filter on that
key rather than on "is this line JSON", which works until anything else emits
structured output.

### Choosing a sink

A file needs a disk that outlives the process; a stream needs something capturing
stdout. Which one your platform can actually keep is the whole decision:

| platform | sink | why |
| --- | --- | --- |
| a workstation | file | it has a disk |
| Fly.io | file on a `[mounts]` volume | logs are retained only briefly and there is no archive, so stdout would need a shipper to be durable |
| Cloud Run | `-` (stdout) | the filesystem is ephemeral **and** several instances run at once, so any shared file has concurrent appenders corrupting it. Cloud Logging captures stdout; a sink to BigQuery makes it permanent and queryable |
| an ephemeral CI runner | `-` (stdout) | a file is discarded with the runner; the job log is kept |

On a hosted platform the stdout sink puts records in that platform's logging
system, under its retention and access control — worth weighing when the code
under review is not yours.

It exists to answer the questions a bug-corpus benchmark structurally cannot,
because they are about the runs that actually happen rather than about planted
defects: how often the `MAX_FINDINGS` cap truncates, how often `SELF_CRITIQUE`
drops a finding, how often a review is salvaged from a response the model cut
off, what the severity mix looks like on a given repo.

```console
$ export PRBOT_RUN_LOG=~/.local/share/pr-review/runs.jsonl
$ jq -s 'map(.funnel) | {
    runs: length,
    proposed: (map(.model_raw) | add),
    posted: (map(.posted_findings) | add),
    capped: (map(.after_collapse - .posted_findings) | add),
    unanchored: (map(.unanchored) | add),
  }' "$PRBOT_RUN_LOG"
```

Two things it is not:

- **It is not a recall measurement.** Nothing in a record knows whether a finding
  was correct, and a real PR never reveals what the reviewer missed. These
  records measure the reviewer's *behaviour*; no aggregate over them turns into a
  hit rate.
- **It is not telemetry.** A record contains the finding text, which is review
  commentary on someone's source. The log is off by default, is written only to
  the local path you name, and nothing in this crate uploads it. Point it
  somewhere private — and if that is inside a repo, gitignore it.

## PR commands

Wire a comment webhook (see the bot binaries) and the reviewer answers these
commands posted as PR comments:

| Command | Effect |
| --- | --- |
| `/review` | (Re)run the full review. |
| `/ask <question>` | Answer a question about the PR, grounded in its diff. |
| `/describe` | (Re)generate the PR description, merged idempotently into the body. Shape it with `DESCRIBE_INSTRUCTIONS` (below). |
| `/review-file <path>` | Deep-review an entire file at the PR head (not just the diff); findings post as a summary comment. |

Route them from a bot binary with [`command::parse_command`] + [`command::run_command`].

### Shaping the PR description

`/describe` writes a Summary / Changes / Notes-for-reviewers layout by default.
`DESCRIBE_INSTRUCTIONS` (or `describe_instructions` in `.prbot.toml`, which
**replaces** rather than appends — a layout is not additive) changes that:

```sh
DESCRIBE_INSTRUCTIONS='Use exactly these sections, omitting any that are empty:
Breaking Changes, New Features, Bug Fixes, Deprecations, Migration Notes.'
```

It is appended last and declared to outrank the built-in section list, because a
model handed two section lists without being told which governs returns both.

`EXTRA_SYSTEM_PROMPT` deliberately does **not** reach this prompt. In practice
that variable holds a *review rubric* — the deployed SIMCEL block opens with
"RAISE a finding when the diff violates one" — and handing that to a prompt whose
job is to describe a change invites descriptions that read like reviews. If you
want project context in your descriptions, put it here, where it is scoped to the
job.

**Placement.** The generated block is wrapped in
`<!-- prbot:describe:start -->` / `<!-- prbot:describe:end -->` and rewritten in
place on each run, so human edits outside it survive. Put that marker pair
anywhere in the description yourself and the block lands there instead of at the
top.

[`command::parse_command`]: src/command.rs
[`command::run_command`]: src/command.rs

## Releasing

**Compile every consumer against the release candidate before cutting a version.**

This is a manual step on purpose. CI's `downstream compiles (public consumers)` job
can only reach the public ones — the private consumers are invisible to it, and one
lives in a client org that is deliberately not named in a public workflow. A check
that skips the consumers most likely to break and reports green anyway is worse than
no check.

The failure this catches is specific and has happened: **0.11.0 added a field to the
public `PrMeta` struct. Every test here passed, and a consumer then failed to
compile.** A breaking change to a published crate is invisible to its own test
suite — only a consumer build sees it.

```sh
# In each consumer: repoint the dep at this checkout, then build and test it.
# Repoint rather than [patch.crates-io] — a patch must satisfy the consumer's
# version requirement, so every version-bump PR would fail for the wrong reason.
#
# `perl -i` rather than `sed -i`: in-place editing is the one place the two seds
# disagree. BSD/macOS needs `sed -i ''`, GNU/Linux rejects it; GNU takes `sed -i`,
# BSD then eats the next argument as the backup suffix. This runs on both.
perl -i -pe 's|^pr-review-core = .*|pr-review-core = { path = "../pr-review-core" }|' Cargo.toml
cargo check --all-targets     # add --features claude-code where the consumer has it
cargo test
```

Then:

1. Bump `version` in `Cargo.toml`; promote `## Unreleased` in `CHANGELOG.md`.
2. **Record each breaking change and which consumer it touches.** The changelog's
   migration table is what a consumer reads to find out what broke.
3. **Flag behaviour changes separately from API breaks.** An API break fails the
   build and announces itself; a behaviour change ships silently. 0.14.0 is the
   example — routing self-critique through the backend seam broke no consumer's
   compile, but moved what runs at review time.
4. `cargo publish --dry-run`, then publish, then tag `vX.Y.Z`.
5. Bump each consumer's dep to the published version and commit.

## License

Licensed under either of

- MIT license ([LICENSE-MIT](LICENSE-MIT))
- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))

at your option.

## Contribution

See [CONTRIBUTING.md](CONTRIBUTING.md) for how to claim an issue, the checks CI
runs, and what a reviewable PR looks like here. Please comment on an issue before
starting work on it.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.

[`Config`]: src/config.rs
