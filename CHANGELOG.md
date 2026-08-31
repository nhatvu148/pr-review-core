# Changelog

## Unreleased

**`/describe` output is shapeable — `DESCRIBE_INSTRUCTIONS`.** The prompt was a
hardcoded Summary / Changes / Notes-for-reviewers layout, and `describe_pr`
passed it *verbatim*: it applied neither `extra_system_prompt` nor a repo's
`.prbot.toml` instructions. Every other prompt in the crate honours the
consumer's injected conventions and this one silently did not, so a deployment
with a whole conventions block baked into its image still got the built-in shape
and had no way to change it short of forking.

`DESCRIBE_INSTRUCTIONS` — or `describe_instructions` in `.prbot.toml` — now
shapes it. The per-repo value **replaces** rather than appends, unlike
`instructions`: a layout is not additive, and a repo asking for release-notes
sections wants those sections rather than those plus the deployment's default.

`extra_system_prompt` deliberately does **not** reach this prompt, though an
earlier cut of this change made it. The argument for that was consistency —
every other prompt honours the consumer's injected block. Looking at what a
consumer actually puts there killed it: the deployed SIMCEL block is a hundred
lines of *review rubric* opening with "weigh these project-specific conventions
and RAISE a finding when the diff violates one". Handing that to a prompt whose
job is to describe a change invites descriptions that read like reviews, and
spends a rubric's tokens on a task with no use for one. Consistency across
prompts is not a virtue when the prompts do different jobs. One input, one job;
a consumer wanting project context in descriptions writes it into
`describe_instructions`, where it is scoped and visible.

**No behaviour change at default settings**, and none for any existing consumer:
with `DESCRIBE_INSTRUCTIONS` unset, `/describe` produces exactly what it did
before.

The layout instruction is appended last and told explicitly that it outranks the
built-in section list. Left to ordering alone, a model handed two section lists
returns both — release notes *and* the default three sections, which is the
failure the feature exists to avoid.

## 0.19.0

**A receiver-call in the change diagram no longer reaches across files.** Three
real reviews on 0.18.0 produced one false arrow, and this is it: an Angular
widget's `this.loadKpis(token)` was drawn as an arrow to a *different* widget's
same-named `loadKpis` in another file. The call was to its own class's method all
along — `kpi-table-widget.ts` calls `kpi-table-widget.ts`.

`x.name()` names a method on a value whose type this pass never resolves, and in
OO code method names collide constantly (`loadKpis`, `refresh`, `update`). Within
one file `this.helper()` is very likely that file's helper; across files it is a
collision as often as a reference. So that edge kind is now same-file only.
Losing true cross-file method calls is worth being rid of the false ones: a
diagram a reader cannot trust is worse than no diagram, which is the premise the
whole rendering rests on.

`EdgeKind::Mention` was doing two jobs with opposite reliability and is replaced
by `Receiver` (the guarded one) and `Type` (`Vec<T>`, `: T` — unambiguous, and it
crosses files freely). **This is why the version is 0.19.0 rather than 0.18.1:**
removing a public enum variant breaks any `match` on it. No known consumer
matches on `EdgeKind` — it landed one day ago — but the crate is public and the
contract is with users nobody has met.

The other two reviews behaved correctly and needed no change. A Python bench PR
drew six solid edges, all real. A NestJS controller PR drew none: sibling
controller methods do not call each other, so there were no edges and the diagram
was suppressed rather than rendered as disconnected boxes.

## 0.18.0

**A derived walkthrough table and change diagram — `WALKTHROUGH` / `DIAGRAM`.**
Both off by default; both change what every review comment looks like, which is
the operator's call rather than this crate's.

*For consumers:* **no API break and no behaviour change at default settings.**
`Config` gains four fields, which only breaks a consumer that builds it with a
struct literal rather than `Config::from_env()` — neither known consumer does, and
both were compiled and tested against this before the version was cut. With
`WALKTHROUGH` and `DIAGRAM` unset, a review's comment body is byte-identical to
0.17.0 and no extra work runs: the change map is not built at all unless something
asks to render it. Turning either on is a deployment decision, made with an env
var, and needs no code change.

The walkthrough is one row per changed file: line counts, the definitions the
change landed in, the worst complexity grade among them, and the findings the
review filed there. The diagram groups changed symbols by file and draws an arrow
wherever one names another inside its own definition.

Every column and every arrow already existed. `structural_context` parses each
changed file with tree-sitter to name the enclosing definition of each changed
line, and `changed_fn_complexity_in` grades the functions from that same tree —
then all of it was flattened into a prompt string and discarded. The new
`ChangeMap` keeps it: no extra fetch, no extra parse, no extra token, no model
call.

**Nothing here asks a model anything, and that is the feature.** A diagram a
model draws from a diff cannot be checked by the reader — a plausible arrow that
does not exist in the code is indistinguishable from a real one, and a reader
will believe it. Every arrow drawn here is a call-shaped occurrence of one
changed symbol's name inside another changed symbol's resolved span, so a
suspicious one can be looked up. Four false-arrow classes were found by running
`examples/changemap_demo` against this crate's own diff and are now pinned by
tests:

- `name {` was read as a Rust struct literal. It is also `if cond {`, `-> T {`,
  and `match x {`; the rule is gone, and losing real struct literals is the
  cheaper error.
- A container (`mod`, `impl`, `class`) was an edge source. Everything inside its
  span is referenced by a function nested in it, not by the container.
- `<Name` was labelled a JSX render in Rust, where it is `Vec<T>`. The file's
  language now decides.
- `x.append(` was drawn solid at this crate's `append`. A call through an
  unresolved receiver is now a dotted "names" edge.

Two guards, both learned from that same demo run: symbol cells collapse to
`(+N more)` past `WALKTHROUGH_MAX_SYMBOLS` (a one-line `lib.rs` edit resolved to
thirteen `mod` declarations), and past `DIAGRAM_MAX_NODES` edge linking narrows
to the highest-complexity symbols instead of bailing out — a 71-symbol change is
ordinary, and the all-or-nothing ceiling meant the diagram never appeared on a
real PR. Test scaffolding sorts last in that ranking, including a `mod tests`
inside a source file, which no path-based rule can see.

The walkthrough's complexity column is read from the complexity pass directly,
never joined to a symbol. Tier B resolves a symbol by walking up to the nearest
node `def_label` recognises, and for TS/JS that list has no `arrow_function` or
`function_expression` — while the complexity pass's `is_function` has both,
precisely because `const handleSubmit = () => {}` is the dominant modern
TS/React style. Joining through symbols meant a React PR got a graded function
in the prompt block and a bare `—` in the table describing the same change. A
file's worst complexity is a fact about the file and needs no symbol. The
*Changed symbols* column still reflects Tier B, so an arrow function is named
there only once `def_label` learns to resolve one — a change to the prompt's
structural context, not to this rendering.

*Changed symbols* resolves the lines the diff **added**, not every line it showed.
`parse_valid_lines` rightly includes context — a finding may anchor to a line the
diff merely displayed — but a column with that heading must not. Seen in the real
GitHub UI: a one-line addition to `src/lib.rs` reported seven changed modules,
one per context line, because each context line sat on a different `mod`. New
`diff::parse_added_lines` answers the narrower question; a pure-deletion hunk
adds nothing and falls back to the wider set rather than naming nothing. The
prompt block is untouched.

Three legibility changes to the diagram, all from looking at one rendered on a
real PR. `DIAGRAM_MAX_NODES` drops 25 → 12, which was a cost number and is now a
legibility one: at 25 the graph sprawled past the width of a comment and
mermaid's own pan controls covered a node. `flowchart TD` replaces `LR`, because
a PR comment is a narrow column. And a grade is drawn only at C or worse — most
changed functions are an A, so annotating every box put a line of text on each
one and distinguished nothing; now the risky nodes stand out by contrast.

The diagram is deliberately not themed, and that is a decision rather than an
omission. GitHub renders mermaid with a theme that follows the viewer's
light/dark preference; an injected `%%{init: {'theme':…}}%%` overrides it, so
colors tuned on one theme go unreadable on the other and the author never sees
it.

The map is built only when a caller asks for it, and asks in three sizes rather
than two. `structural_context` and its local sibling take the cheap path and
return an empty map, so a review with both features off pays nothing for either —
the prompt block it produces is byte-identical either way. A walkthrough-only
review resolves symbols and grades but skips edge linking, which is the pairwise
span scan and which only the diagram reads. The complexity pass runs once per
file and feeds both the prompt block and the map, rather than walking every
changed function twice.

Both features work on the diff-first `run_review_local` path too, whose
deliverable *is* its `summary_markdown`.

The diagram is skipped on Bitbucket, which renders no mermaid — the block would
post as a wall of source. It is also skipped whenever there are no edges: a
picture of disconnected boxes restates the table in a form that is harder to
read.

`structural_context` and `structural_context_local` keep their signatures; the
map comes from new `*_mapped` siblings. Adding to a public type is what broke a
consumer at 0.11.0. Adding a function cannot.

| Env var | Default | Effect |
| --- | --- | --- |
| `WALKTHROUGH` | `false` | Append the per-file walkthrough table to the summary. |
| `WALKTHROUGH_MAX_SYMBOLS` | `6` | Symbols listed per file before `(+N more)`. |
| `DIAGRAM` | `false` | Append the mermaid change diagram (GitHub/GitLab only). |
| `DIAGRAM_MAX_NODES` | `25` | Symbols considered for edge linking, ranked by complexity. |

## 0.17.0

**The run log can write to stdout — `PRBOT_RUN_LOG=-`.** 0.16.0 could only append
to a file, which assumes a disk that outlives the process. Cloud Run has neither:
its filesystem is ephemeral *and* it runs several instances at once, so every
shared-file answer — a mounted volume, a GCS FUSE bucket — puts concurrent
appenders on one file and corrupts it. A log stream has no such problem, and the
platform already captures stdout and routes it onward (Cloud Logging, then a
BigQuery sink) with no credentials, no client library, and no network call on the
review path. The same change makes an ephemeral CI runner loggable, where a file
was simply lost.

Records now carry `"_kind": "prbot_run_log"`. Stdout is shared with the process's
own tracing output, and a query that selects on "is this line JSON" works right
up until anything else emits structured output — including these bots' own
tracing, if its subscriber is ever switched to `.json()`. Additive; the schema
version is unchanged, because adding a key is not a break for a JSONL reader.

**Fly stays on a file.** Fly retains logs only briefly and has no archive, so
stdout there would need a log shipper to be durable — more moving parts than the
$0.15/month volume it replaces. The sink is a deployment choice, not a better
way; pick the one the platform can actually keep.

### Migration

| before (0.16.0) | after (0.17.0) |
| --- | --- |
| `Config::run_log_path: Option<PathBuf>` | `Config::run_log: Option<RunLogSink>` |
| `runlog::append(&path, &rec)` | `runlog::write(&sink, &rec)` |

A consumer that only calls `Config::from_env()` needs **no change** — verified
against `pr-review-bot`, `simcel-pr-bot` and `kaniscope-action`. Only code that
sets or reads the field directly is affected; construct it with
`RunLogSink::File(path)` or `RunLogSink::Stdout`, or parse a raw env value with
`RunLogSink::from_env_value`.

The field is an enum rather than an `Option<PathBuf>` holding a magic `-`,
because the two sinks have genuinely different mechanics — one creates
directories and appends, the other locks a shared stream — and a path-shaped type
that is sometimes not a path invites precisely one bug: `create_dir_all("-")`.

## 0.16.0

**A local, opt-in run log — `PRBOT_RUN_LOG`.** Set it to a path and every review
appends one JSON line: the per-stage funnel of findings, the findings themselves,
usage, and wall time. Unset (the default), nothing is written.

The bench corpus scores the reviewer against *planted* bugs, where the ground
truth is known. It is silent on the runs that actually happen, and those were
being discarded the moment the comment posted — so questions with no answer
anywhere included: how often does the `MAX_FINDINGS` cap truncate a real review,
how often does `SELF_CRITIQUE` drop a finding, and how often does a review get
salvaged from a response the model cut off. 0.15.0–0.15.3 were three consecutive
releases about review-JSON salvage, shipped without a way to observe the rate the
salvage fires at in production.

The funnel is collected inside `finish_review`, because that is the only place it
exists: each stage consumes its predecessor's vector, so a caller holding the
posted findings cannot reconstruct what the critique, the confidence floor, or
the cap removed. Each finding's resolved anchor comes out of the same place and
for the same reason: `reanchor` posts a comment on a line the finding itself
never records, so a record carries both `line` (what the model said) and
`anchored_line` (where the comment went). The second is the join key for any
later outcome pass — the first would find nothing whenever `REANCHOR_FINDINGS`
moved a finding, which is the default and the common case.

Two limits, stated because a run log invites both mistakes:

- **It cannot measure recall.** No record knows whether a finding was correct, and
  a real PR never reveals what the reviewer missed. This measures behaviour.
- **It is not telemetry.** Records carry finding text — review commentary on
  someone's source. Off by default, written only to the local path you name, and
  no code path in this crate uploads it.

`truncated_salvage` reports the truncation case only, read off the marker the
salvage leaves in the summary. A plain JSON repair (malformed but complete) stays
tracing-only: surfacing it would change the signature of the public
`parse_review_with_repair`, which downstream backends call.

Additive: one new `Config` field, one new module. No behaviour change to any
existing path.


## 0.15.3

**Doc fix on `post_review_failure`.** 0.15.2 claimed it "never creates a comment
where the engine did not already leave one". It does: the upsert underneath
(`github::upsert_summary`) creates the summary comment when no marker comment
exists. Called on a PR the engine never commented on, it would post a failure
notice into silence. The contract is now stated correctly — only call it for a
review that actually posted a placeholder. No behaviour change.


## 0.15.2

**A dead review no longer looks like a finished one.** The engine posts a
"⏳ Reviewing this PR…" placeholder before the slow call, and upserts the real
review over it. If the review then dies — turn cap, OOM, timeout, a crash — the
placeholder stays. Two consequences, both bad:

- The PR promises a review that will never arrive, permanently.
- A consumer's boot reconciliation asks "has this PR been reviewed at its current
  head?" and answers it from the newest bot comment. The placeholder is *newer*
  than the head, so the PR reads as covered — and the one case reconciliation
  exists to catch becomes the one case it cannot see.

Observed on `nhatvu148/VinaText#49`: head at 08:12:56Z, placeholder at 08:13:34Z,
review dead at 08:18:45Z, and the next boot sweep reported "nothing missing".

`is_incomplete_review(body)` answers it properly, matching hidden
`REVIEW_PENDING_MARKER` / `REVIEW_FAILED_MARKER` markers rather than prose — which
would break the moment anyone rewords it. It also matches the legacy placeholder
text, so comments already sitting on open PRs are recognised instead of being
grandfathered into the bug.

`post_review_failure(...)` replaces a placeholder with the error and a `/review`
retry hint. It upserts the same marker comment, so it never adds noise where the
engine did not already leave a comment, and it is best-effort — failing to report
a failure must not mask the original one.


## 0.15.1

**`parse_review_with_repair` and `add_usage` are public.** 0.15.0 shipped them
`pub(crate)`, which quietly falsified that release's own claim that downstream
backends "get the salvage on this version bump instead of another hand-port".
They got it for `review_diff`, `review_file` and `agentic_review` — every path
*inside* core — but a consumer implementing `ReviewBackend::review` and parsing its
own model output could not reach the function at all, which is precisely the case
the change was written for. No behaviour change; visibility only.


## 0.15.0

**Review JSON salvage, in core rather than in one backend.** A single unescaped
`"` in a finding's `body` discarded an entire review: `serde_json::from_str` failed
at the *syntax* layer, before `lenient_findings` — which only drops findings that
fail to *deserialize* — ever saw one. On `SIMCEL/simcel-saas#3` that threw away 265
seconds of agent work and left the PR sitting on a stale placeholder with nothing
to retry it.

On a syntax error, `parse_review_with_repair` hands the broken object back to the
model with no tools and asks for the same data as valid JSON. One attempt; if the
repair also fails, the **original** error is reported. Deliberately not a
sanitizer: whether a bare `"` closes a string or belongs to the prose needs the
surrounding meaning, and guessing wrong silently mangles text a human is about to
read.

Alongside it: `json_error_context` quotes a window around the flagged column
instead of clipping the head (which, for a pretty-printed review, is always the
`summary` — never the break), converting serde's **byte** column to a char index
first so em dashes and curly quotes don't slide the window off the very spot it
exists to show. Truncation (`Category::Eof`) is split from a stray character and
noted in the review's own `summary`, because the repair prompt closes an open value
— keeping what arrived and silently dropping the rest — and a partial review must
not read as a complete one to whoever is deciding the merge. Finding counts are
compared across the pass and warn on divergence; the before-count is a substring
count over text that by definition does not parse, so it can never fail a review.

The repair is attempted **only** on `Category::Syntax` or `Eof` — the failures the
prompt is written for. A `Data` failure (valid JSON of the wrong shape, e.g. a
missing `recommendation`) returns its error directly rather than paying for a call
that would come back unchanged. The prompt is deliberately *not* broadened to
backfill missing fields: the absent field there is the review's verdict, and a
second model told to supply one would invent an `APPROVE` or a `BLOCK` that no
reviewer ever reached. Losing a judgement is better than manufacturing one.

**Why this is a core change and not a backend one.** It was fixed in one consumer
backend by hand while every other path kept the bug — `review_diff`, `review_file`,
`agent::agentic_review` (this crate's own default agentic path, not a downstream
consumer), and every downstream agent backend. All of them turn model output into a
`Review` through the same two lines. **All three in-core sites now salvage**, and the
agentic one folds its repair tokens into the review's.

**`ReviewBackend::complete_detailed`.** The repair is a second billed call, and
`complete()` returns bare `String`, so its `usage` was unreachable — a repaired
review would report only the first call's tokens. Added as a **defaulted** method
returning `Completion { text, model, usage }` rather than by widening `complete()`,
so no existing implementation breaks.

The default **delegates to `complete`**, which matters more than it looks: routing
it to OpenRouter instead would mean a consumer running an agent CLI has its repair
answered by a different service and model, and fails outright without an
`OPENROUTER_API_KEY` — a review discarded by the very mechanism meant to salvage it.
So a backend overriding only `complete` keeps every call, repair included, on its
own backend, and simply reports no usage. `OpenRouterBackend` overrides
`complete_detailed` to report real figures, and `chat_completion` stops discarding
the `usage` it already parsed out of the response.


## 0.14.0

Three changes that together let a consumer review a **local diff** — a branch, a
worktree, staged changes — on its own backend, with the same calibration and the
same post-processing the PR path gets. Written for vexar's convergence loop, which
reviews a worktree with no PR behind it.

**Orchestrator-injected rules.** `prompt::REVIEW_RULES` was a const each
`ReviewBackend` was trusted to append, and the deployed claude-code backend ran for
months without it — invisibly, because a review with no calibration rules still
reads like a review. (The incident is written up in the private `pr-review-docs`
repo; `docs/` is gitignored here on purpose, so it is not a path in this tree.)
`run_review_with` now composes the rules plus the consumer's `extra_system_prompt`
once and hands them over on `ReviewContext.injected_rules`;
`ReviewContext::system_prompt(rubric)` joins them to whatever rubric the backend
uses. What the model receives is byte-identical on both OpenRouter paths, pinned by
a test against the old formula.

**Critique behind the seam.** `llm::critique_findings` posted to OpenRouter
directly, so a consumer with its own backend lost the noise filter unless it also
held an OpenRouter key — and lost it silently, since the caller fails open. It now
runs on `ReviewBackend::complete()`. The default backend still resolves that to the
same OpenRouter call, so the bot is unchanged.

**`review::run_review_local`.** Takes a `LocalReviewInput { diff, repo_root, label }`
and runs the same pipeline as `run_review_with` — glob filtering, diff hygiene,
packing, structural context, self-critique, confidence floor, burst collapse,
recommendation floor, cap, line anchoring — minus the three stages that only mean
something for a PR: no provider fetch, no posting, and **no CVE scan** (the OSV scan
reads added dependency lines out of a lockfile diff, and a local diff carries no
guarantee of lockfile semantics). The stages are extracted into `prepare_diff` and
`finish_review` and shared, not forked: the local path is what the convergence loop
scores itself on, and a filter that applied to only one path would make those
numbers incomparable with the bot's. `structure::structural_context_local` reads
new-side files from the checkout instead of the provider.

**Breaking (API):**

- `ReviewContext` gains `injected_rules` and `local_root`, and its `provider` is now
  `Option<&Provider>` — `None` on the local path, where inventing a host would make
  an agentic backend try to clone a repo that isn't there.
- `llm::review_diff` and `agent::agentic_review` take the prepared `system_prompt`.
  Callers driving them directly can build it with `prompt::review_system_prompt`.
- `llm::critique_findings` takes `(cfg, backend, ...)` instead of `(client, cfg, ...)`.

**Consumer migration.** All three known consumers were compiled against this
release before it was cut:

| consumer | change needed |
|---|---|
| `pr-review-bot` | `ctx.provider` is an `Option`; `review_diff` takes a system prompt; the claude-code backend now takes its rules from `ctx.system_prompt(..)` instead of importing `REVIEW_RULES` |
| `simcel-pr-bot` | same two, via `try_clone` and its `review_system_prompt` helper |
| `kaniscope-action` | none — it only uses `Config` / `run_review` / `RunReviewInput` |

**Behaviour change, latent rather than live:** a backend overriding
`ReviewBackend::complete` (both bots do) now runs the self-critique pass on *itself*
rather than on OpenRouter. Both bots deploy with `SELF_CRITIQUE=false`, so upgrading
changes nothing for them — the flag was set because the pass could never succeed on
a non-OpenRouter backend, not as a preference.

What changes is that enabling it is now *possible* there. It should be enabled as
its own decision, A/B'd on the bench corpus: the pass prunes findings, and it has
never been measured on the claude-code path.

Note also that `tree-sitter` moved to 0.26 (it carries `links = "tree-sitter"`, so
exactly one version may exist in a dependency graph — this line is a compatibility
contract with every consumer, not a private choice).

## 0.13.0

Minor: **a build claim CI has already falsified is capped at LOW** — enforced in
code, after the prompt was given two chances and did not take.

**The history this comes from**

- `VinaText#10`: two **BLOCKING** findings claiming a broken MFC build on a commit
  whose CI was green. Via the recommendation floor, that posted a `BLOCK`.
- 0.11.0 shipped the CI-status block plus an explicit rule — *a passing check
  FALSIFIES any claim that this change breaks that build*.
- `pr-review-core#28` filed the same shape anyway: BLOCKING, then MEDIUM on a re-run
  once the rules actually reached the claude-code backend. Never dropped, never
  restated at LOW. The model lowered its confidence **without re-examining the
  claim**, and its second attempt supplied *more argument* for the same wrong
  conclusion.

**What now happens**

`review::demote_falsified_build_claims` caps such a finding at LOW when every check
on the reviewed commit reports success, and says so in the body.

- **Demote, not delete.** The observation underneath is usually true — the line
  really is 118 characters. It is the inference to "the check will fail" that CI
  refutes. LOW keeps the observation while removing what it costs, since via the
  recommendation floor a MEDIUM or above turns the verdict into "approve with
  changes" or "block".
- **Two-tier matching.** Unambiguous phrases (`breaks the build`, `fails to
  compile`, `fails ci`) stand alone. Ambiguous ones (`will fail`, `would fail`)
  only count when the body also names a check — `cargo fmt/test/clippy`, `--check`,
  or a whole word like `ci`/`build`/`compile`/`clippy`/`lint`. Whole words, not
  substrings: `ci` lives inside *specific*, *decision*, *efficient*. Without this,
  "this will fail at runtime when the list is empty" — a real HIGH that green CI
  says nothing about — would be silently capped.
- **Conservative on the CI side.** No CI block, an empty one, any non-success
  state, or a *truncated* list (where a hidden failure is exactly the risk) all mean
  "not known green", and nothing is demoted.

**Also**

- `BENCH_SHOW_FINDINGS=1` now works in `examples/bench_local.rs` too, so all three
  harnesses can print finding text. Recall alone only says a finding hit the right
  line, not that it made the right claim — that flag is how the 0.12.0 class-A
  procedure was verified (`auth-mount-401`, recall 1.00 ± 0.00 at n=3).

No API change.

## 0.12.0

Minor: the **added-guard procedure** (coverage class A) plus a `VERSION` const for
consumers.

**Class A — caller-visible behaviour change**

`REVIEW_RULES` now carries a procedure for the highest-value miss in the corpus.
From nomnaviet#94: `app.use('/api/ocr', optionalAuth, …)` was added to routes that
had mounted no auth middleware. `optionalAuth` delegates to `verifyAuth`, which
returns 401 on a malformed or expired token, so every caller holding a stale token
went from working to broken — and **nothing in the diff says "401"**.

> When a middleware, guard, decorator, interceptor or wrapper is ADDED to a route,
> handler or function that did not have one: read its implementation rather than
> assuming from its name, enumerate every response it can now produce (401, 403,
> 429, a redirect, a thrown error, a timeout), and say which existing callers that
> breaks. Severity at least MEDIUM.

The capability was always there — the reviewer can `read_file` the middleware. What
was missing was a procedure for *behavioural* breakage; the prompt only ever
described structural breakage (signatures and types).

**`pub const VERSION`**

Consumers can now report which engine they run. A bot binary knows its own version
but not its engine's, and "is the deployed image current?" was otherwise answerable
only by reading deploy logs — which cost real time three separate times while
shipping 0.11.0.

Prompt-only behaviour change plus one additive const; no breaking API change.

## 0.11.0

Minor: four reviewer-quality changes, each written from a recorded production
failure in the feedback corpus rather than from theory, plus the tooling to tell
whether any of it worked.

**⚠️ Breaking (source-level).** `PrMeta` gained `ci_status`, and `Config` gained
`vendored_globs` and `ci_status`. Code constructing either with a struct literal
must add the fields; code using `Config::from_env()` and provider `get_meta()` is
unaffected. This is why it's 0.11.0 and not 0.10.5.

**Vendored paths (class D)**

- `diff::is_vendored` + `diff::diff_hygiene_with` skip third-party trees, defaulting
  to `thirdparty/`, `third_party/`, `vendor/`, `vendored/`, `external/` and
  `node_modules/`, matched as **whole path segments at any depth** (so
  `frontend/node_modules/x` counts and `src/vendor_client.rs` does not). Override
  with `VENDORED_GLOBS` or `vendored` in `.prbot.toml`; setting either replaces the
  defaults. `diff_hygiene` keeps its signature and delegates.
- On `VinaText#20`, a PR vendoring 265 files of Scintilla/Lexilla drew binary
  findings on its `.cxx` sources and then seven "adds N lines — `.gitignore` or
  exclude it" findings on the same tree. Every line count was exact and every
  conclusion was wrong: you cannot gitignore source you must compile.

**Burst collapse**

- Findings making the same claim about different files collapse into one that states
  the count (`review::collapse_bursts`). Two failures had this signature — 111 files
  reported as added binaries, then seven reported as oversized — where the review
  read as "N separate problems" instead of "one claim about N files".
- Bounded deliberately: a group containing a **HIGH or BLOCKING** finding is never
  collapsed (collapsing is lossy — only the representative keeps its anchor and its
  `Fix:` text), and severity is part of the group key, so a shared phrase at two
  severities is two claims. Both recorded bursts were uniform MEDIUM / uniform LOW.

**CI status**

- `PrMeta::ci_status` carries the head commit's GitHub check runs or Bitbucket build
  statuses, fetched with the metadata so every backend gets it without a signature
  change, and rendered by `build_user_prompt` **before** the diff with the
  consequence spelled out: a passing check falsifies a build-break claim.
- Paged (3 × 100) with `total_count` honoured; when a commit has more runs than were
  fetched, the block says the list is incomplete and cannot show that every check
  passed — an incomplete list must never read as a complete one.
- Gated by `CI_STATUS` (default on) and `ci_status` in `.prbot.toml`. Fail-open
  throughout: a failed status fetch costs a log line, never the review. GitLab
  pipelines are not wired up; `None` reads as "unknown", so there is simply no block.
- From `VinaText#10`, where two **BLOCKING** findings asserted a broken MFC build on
  a commit whose CI was green — the cheapest-to-falsify claim a reviewer can make,
  arriving at the one severity that stops a merge.

**Shared review rules**

- `prompt::REVIEW_RULES`, appended to `SYSTEM_PROMPT` and the agent system prompt and
  public so agent-CLI backends outside this crate can append it too, keeping all
  three from drifting. It encodes: the severity rubric (a bug that **throws** is
  never LOW; a wrong BLOCKING is the most expensive error available), verify a cited
  rule against the config that *enforces* it rather than a prose doc, don't assert
  build outcomes, don't propose edits inside vendored code, and raise a repeated
  claim once.

**Measurement**

- `scripts/feedback_scoreboard.py` turns the hand-written feedback entries into a
  precision scoreboard — per PR-round, plus the worst severity any false positive was
  filed at. Entries it cannot parse are reported, never silently dropped.
- `scripts/swe_to_corpus.py` gains `--max-issues` / `--max-diff-chars`. A
  reverse-patched refactor turns every changed line into a "known issue" — measured
  on SWE-bench_Multilingual: median 2 per case but mean 25.8, max 1617, and one
  205 KB diff — and those outliers dominate both recall and the token bill.

## 0.10.4

Patch: **a missing patch is *unknown*, not *binary*** — fixes a false-positive class
0.10.3 introduced. On `nhatvu148/VinaText#20` (265 vendored files) the Files-API
fallback rendered **111 text files** as `Binary files … differ`, which fed
`diff_hygiene` 111 fake candidates and posted 5 MEDIUM "you committed a binary"
findings — enough, via the recommendation floor, to turn an `APPROVE` into
`APPROVE WITH CHANGES`.

0.10.3 assumed `additions + deletions == 0` identifies a binary. GitHub reports
exactly that, and omits `patch`, for **ordinary text files** whenever the PR-level
diff exceeds its size limits — i.e. precisely the condition that puts us on this path.
Counts cannot distinguish the two cases.

- Only an unambiguously binary **extension** may now synthesize the `Binary files …
  differ` marker (74-entry allowlist: archives, executables, media, fonts, documents,
  databases, model weights, installers). Verified against the real PR: 0 of its 111
  patch-less files match. A swept-in `.zip` still fires, so the class-D signal holds.
- Every other patch-less file renders as `[no diff available from GitHub for this file
  (+N -M) — contents not reviewable; do NOT infer that it is binary or unchanged]`.
- The rebuilt diff now **leads with the count** of un-inspectable files, so a review
  reads as "111 files I could not inspect" rather than "5 binaries you committed"
  (`REVIEWER_COVERAGE.md` §4.6, no silent caps).

The 0.10.3 test could not catch this: it set `additions = 40_000`, while real PR-level
truncation zeroes the counts. Replaced with the zero-count case.

## 0.10.3

Patch: **a PR too big for GitHub's `.diff` media type is now reviewable.** Seen in
production on `nhatvu148/VinaText#20` — the review died with
`GitHub getDiff 406 Not Acceptable: "the diff exceeded the maximum number of lines
(20000)"`. The PR was fine; only that *representation* of it was unavailable.

- `providers::github::get_diff` now falls back to `GET /pulls/{n}/files` on 406 and
  rebuilds an equivalent unified diff from the per-file patches. The Files API has no
  20000-line limit.
- The synthesized `diff --git` / `new file mode` / `---`/`+++` headers are the ones
  `split_diff_sections`, `parse_valid_lines`, and `diff_hygiene` key on, so a rebuilt
  diff anchors inline comments and trips hygiene findings exactly like a native one
  (pinned by tests).
- What the Files API can't provide is stated in the diff instead of silently dropped:
  a withheld `patch` renders as `[diff omitted by GitHub — file too large to patch]`,
  a binary as the standard `Binary files … differ` marker, and a file list past the
  3000-file API ceiling as a leading truncation note. A partial diff never reads as a
  small change.

Only affects PRs that already failed outright. No API change.

## 0.10.2

Patch: **a model dropping one field no longer costs the whole review.** Seen in
production on a large PR — after a ~5-minute agent run the review died with
`could not parse review JSON: missing field \`severity\``, because `Finding`
demanded every field.

- `Finding::severity` now defaults to `MEDIUM` when absent (not `LOW`: LOW and
  unknown both rank 0, so an unlabelled finding would sort last and be first out
  under `max_findings`). `Finding::file` defaults to empty — it then simply fails to
  anchor and folds into the summary. Only `body` is still required.
- `Review::findings` deserializes **element-by-element**: an element that still can't
  be parsed is dropped with a `warn!` instead of failing the review.
- `critique_findings` parses leniently too, but errors if the array was non-empty and
  *nothing* survived — a wrong-shaped critique must fail open (keep the original
  findings) rather than silently return zero.

Applies to every backend that returns `Review`, including agent-CLI backends that
deserialize these types out-of-crate. No API change.

## 0.10.1

Patch: **calibrate the diff-hygiene binary check.** As first shipped it flagged
*any* added binary and produced a false positive on a routine image asset (a 7 KB
`.webp` avatar in `public/`). `diff::diff_hygiene()` now suppresses common
image/font extensions (`.png/.jpg/.webp/.ico/.woff…`) — archives, executables,
databases, and unknown binaries still fire, so the swept-in-`.zip` signal is intact.
Precision fix; no API change.

## 0.10.0

Two review-facing additions — deterministic **diff-hygiene** findings and a
**recommendation floor** — plus benchmark-harness improvements.

**Diff hygiene (coverage class D)**

- `diff::diff_hygiene()` flags change-set hazards a normal diff view hides, with no
  LLM call (zero tokens, zero variance, can't hallucinate): an **added binary file**
  (MEDIUM) and an **oversized added file** (LOW, ≥1000 added lines). Runs on the raw
  diff — *before* the empty-diff short-circuit — so a swept-in binary or vendored
  tree is caught even when every file was filtered out of the LLM review. Merged into
  both the normal and advisory-only paths and ranked by severity like any finding.

**Recommendation floor**

- The posted recommendation is now the **stronger** of the model's own verdict and
  the floor implied by the merged findings' max severity — only ever upgrading, never
  softening. A deterministic MEDIUM hygiene finding (a swept-in binary) can no longer
  sit under an "APPROVE". Computed from the pre-truncation finding set so a capped
  finding can't leave the recommendation understating a real problem.

**Benchmark harness**

- New `examples/bench_local.rs` + `scripts/swe_to_corpus.py`: score the reviewer
  against raw-diff corpora built from public bug-fix datasets (SWE-bench, reverse-
  patched so a fix commit is a free ground-truth annotation), with per-language F1.
- `hits()` now scores **file-level** issues and **summary findings** (`line: null`),
  matching like-with-like — so unanchored-but-correct findings are measurable instead
  of always reading as a miss.

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
