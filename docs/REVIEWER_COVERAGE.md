# Reviewer coverage — requirements and gaps

**Purpose:** a specification for what `pr-review-core` should catch, derived from
measured behaviour on a real repository, so improvements target *classes* of
defect rather than the last thing that slipped through.

**How to use this in a fresh session:** read this file, then
`src/prompt.rs`, `src/agent.rs`, `src/blast.rs`, `src/complexity.rs`. Pick a
class from §3 that is marked **IN REACH** and implement it against the
acceptance test given for that class. §6 is the regression corpus — do not
declare a class fixed without replaying it.

---

## 1. Evidence base

Measured 2026-07-26 against `nomnaviet/nomnaviet` (TypeScript monorepo: Express
API, Next.js web, Capacitor mobile). Six pull requests, ~11 findings, reviewed
in agentic mode by the deployed bot.

**This is one repo, one language, one session.** Treat the *classes* below as
the durable output and the counts as indicative, not statistical.

### What it caught (do not regress these)

| Finding | Class | Why it was good |
|---|---|---|
| `flushUsageBuffer()` early-returns on a `flushing` guard, so an awaited shutdown flush silently drops buffered rows | concurrency / lifecycle | Required reasoning about a window that only opens during shutdown. Confirmed real by reverting the fix and watching a regression test fail. |
| `array_agg` over a filtered `readings × meanings` join truncates a character's readings to only the matched one | SQL semantics | Required tracing which rows survive `WHERE` *before* aggregation. Real impact was worse than reported: 1 of 20 rows returned. |
| `(fanqie as any).reconstructed` reads a field that does not exist on the type | type-level lie | The cast is exactly what defeats the compiler; a reviewer catching it is real value. |
| A comment asserting an endpoint is "not exposed on the UI" directly above a new comment saying it is | doc/code contradiction | Cheap to find, genuinely misleading if left. |
| Code citing `docs/SERVER_CPU_DIAGNOSIS.md`, which was not in the repo | dangling reference | Correct, and under-stated — the file did not exist anywhere. |
| `@hono/node-server@1.19.15` GHSA-frvp-7c67-39w9 | dependency advisory | Accurate, correctly scoped, named the fixed version. Already implemented in `src/deps.rs`. |

**Precision was high.** Every finding checked against the code or the database
was real. There were no false positives worth the name across six PRs.

### What it missed

| Miss | PR | Class | Cost if unnoticed |
|---|---|---|---|
| Swapping in `optionalAuth` made previously-unauthenticated routes return **401 for stale tokens** | #94 | **A — caller-visible behaviour change** | Silent break for every existing client holding an expired token |
| The change metered `/api/ocr/document` while the product's own UI calls `/api/recognition/page` — an endpoint ~13× cheaper in CPU | #94 | **B — intent conformance** | A 30-day measurement window spent on the wrong endpoint; every derived cost figure an order of magnitude wrong |
| `bytes_out` records 0 on every `/mcp` row while other routes populate it | #99 | **C — runtime-only** | Quietly wrong data in a table meant to inform a pricing decision |
| A **15 MB binary** (`soan-nom-ime.zip`) swept into a version-bump commit | #97 | **D — diff hygiene** | Permanent repo bloat; invisible in a normal diff view |

---

## 2. The framing that matters

The reviewer is **already agentic**: `src/agent.rs` gives it `grep`,
`read_file`, `list_dir` and `references` over a clone at the PR head, plus a
precomputed `## Blast radius` block from `src/blast.rs`.

So most misses above are **not** structural limits of diff review. The
capability is present and unused. The gap is in what the prompt *asks for* and
what the pipeline *feeds in* — which is a much better problem to have.

Only class **C** is genuinely out of reach: no static reviewer observes a value
that is wrong only at runtime. That is CI's job, not the bot's, and the correct
outcome for class C is to stop trying.

---

## 3. Defect classes

Each class states what it is, whether it is reachable today, what capability it
needs, and a concrete acceptance test.

### Class A — Caller-visible behaviour change · **IN REACH** · highest value

**Definition.** The diff changes the observable contract of an endpoint,
function, or module in a way that breaks existing callers, without changing any
line those callers can see. Includes: new auth/permission requirements on a
previously open path, a widened error condition, a narrowed accepted input, a
changed status code, a nullable return becoming non-null.

**The #94 instance.** `app.use('/api/ocr', optionalAuth, …)` on routes that had
mounted no auth middleware. `optionalAuth` delegates to `verifyAuth`, which
returns 401 on a malformed or expired token. Callers with stale tokens went from
working to broken. Nothing in the diff says "401".

**Why it was missed.** Detecting it needs three hops: (1) the route previously
had no auth middleware, (2) the added middleware's failure mode, (3) the
inference that pre-existing callers may hold bad tokens. The prompt asks for
"a change that breaks a caller" but gives no procedure for *behavioural*
breakage, only structural (signature/type) breakage.

**Required capability.** Already present. Needs prompt work: when a middleware,
guard, decorator or wrapper is **added** to an existing route or handler, the
reviewer must `read_file` that middleware's implementation and enumerate every
response it can produce that the route could not produce before.

**Acceptance test.**
> Given a diff that adds an auth middleware to a route mount that previously had
> none, the review must contain a finding naming the new failure response (401)
> and the caller population affected. Severity ≥ MEDIUM.

### Class B — Intent conformance · **IN REACH** · highest value

**Definition.** The change is internally correct but does not accomplish its
stated purpose. The defect is the gap between the PR's declared intent and what
the code actually does — never visible in the changed lines alone.

**The #94 instance.** The PR's stated goal was measuring OCR cost per page. It
instrumented `/api/ocr/document`. The product's own UI (`DocumentOCRModal.tsx`,
`recognize/page.tsx`) posts to `/api/recognition/page`, a different and ~13×
cheaper path. The instrumentation was correct; the target was wrong.

**Why it was missed.** The reviewer reads the diff, not the PR body, and not the
design document the PR body links to. It had no statement of intent to check
against, so there was nothing to find a gap *from*.

**Required capability — this is the one real pipeline change.**
1. Feed the **PR title and body** into the review context. They are already
   fetched by the provider layer; they are not currently in the prompt.
2. When the body links a doc in the repo (`docs/**.md`), `read_file` it.
3. Add an explicit instruction: *state what the PR claims to do, then verify the
   code does that — for any endpoint, table, flag or path the change targets,
   grep for who actually uses it.*

**Acceptance test.**
> Given a PR whose body says it instruments endpoint X, where a `grep` of the
> frontend shows the product calls endpoint Y for that feature, the review must
> raise that mismatch. Severity ≥ MEDIUM.

This class subsumes a family worth naming: metering/logging/feature-flagging the
wrong target, migrating the wrong table, adding an index to the wrong column,
caching a path that is never hit.

### Class C — Runtime-only defect · **OUT OF REACH** · do not pursue

**Definition.** Code that compiles, type-checks, reads correctly, and produces
wrong values only when executed.

**The #99 instance.** `bytes_out` recorded 0 for every `/mcp` row because the
SDK's response path bypassed the `res.write`/`res.end` chunk accounting the
metering middleware wraps. Every individual line is correct.

**Position.** Do not add heuristics for this. A reviewer that guesses at runtime
values will generate false positives and erode trust in the classes it *can*
decide. The correct mitigation is CI running tests, which is a repo problem.

**The one thing worth doing:** when a PR adds a field/column/metric that is
written in one code path and read in another, note that it is untested if no
test asserts it. That is class F, not a runtime claim.

### Class D — Diff hygiene · **PARTIALLY IMPLEMENTED** · cheap, deterministic, no LLM needed

**Status.** `diff::diff_hygiene()` ships the two highest-signal, lowest-false-
positive checks: **added binary file** (MEDIUM — the #97 miss) and **oversized
added file** (LOW, ≥1000 added lines). Wired into `run_review_with` after
self-critique so they can't be pruned, and ranked with every other finding by
severity. The remaining checks below (lockfile-without-manifest, `.gitignore`
match, high-entropy strings) are **not yet implemented** — they carry more false-
positive risk and should only land with a regression case that proves the value.

**Definition.** Properties of the *change set* rather than the code: large
binaries, vendored trees, lockfile churn unrelated to a dependency change,
generated output, secrets-shaped strings, files matching `.gitignore` intent,
license headers.

**The #97 instance.** A 15 MB `.zip` swept into a version-bump commit by an
over-broad `git add`. Trivially detectable; nothing was looking.

**Required capability.** None from the model. Implement in Rust in `src/diff.rs`
as deterministic pre-checks emitted as findings:

| check | threshold | status |
|---|---|---|
| added binary file | git's `Binary files … differ` marker on a `new file mode` | **done** (MEDIUM) |
| oversized added file | ≥ `LARGE_ADDED_LINES` (1000) added lines | **done** (LOW) |
| lockfile changed with no manifest change | exact | deferred |
| added path matching a `.gitignore` pattern | exact | deferred |
| high-entropy string in an added line | tuned; expect false positives, gate behind config | deferred |

The two `done` checks run on the **raw** diff (before glob filtering and before the
empty-diff short-circuit), so an added binary or oversized file is flagged even when
it is the PR's *only* change and every file was filtered out of the LLM review. The
`deferred` rows carry more false-positive risk and should land only with a matching
regression case, per §4.1.

Deterministic checks are strictly better than model-derived ones here: zero
token cost, zero variance, and they cannot hallucinate.

**Acceptance test.**
> Given a diff adding a binary file (git's `Binary files … differ` marker with a
> `new file mode` header), the review contains a finding naming the file, without
> an LLM call. **Implemented** — see `diff::tests::hygiene_flags_an_added_binary_file`.
> (Size in bytes isn't in a unified diff; the agentic clone would be needed to add a
> byte-size threshold — deferred.)

### Class E — Cross-file consistency · **PARTIALLY IN REACH** · already decent

Already caught: stale comments contradicting new ones, dangling doc references.

**Extend to** parallel structures that must move together. In this repo:
`apps/web/messages/*.json` and `apps/mobile/src/i18n/locales/*.json` are
duplicate locale files that must be edited together; `apps/web/src/lib/*` has
mobile overrides aliased in `apps/mobile/vite.config.ts` that shadow them.

**Required capability.** Present. This is naturally per-repo, so it belongs in
`.prbot.toml` (`src/repo_config.rs`) rather than the global prompt:

```toml
[[consistency]]
name = "locale files are duplicated, not shared"
when_changed = "apps/web/messages/*.json"
must_also_change = "apps/mobile/src/i18n/locales/*.json"
```

**Acceptance test.**
> With that rule configured, a PR touching only `apps/web/messages/en.json`
> raises a finding naming the mobile counterpart.

### Class F — Test coverage of the changed behaviour · **IN REACH** · needs de-genericising

**Current behaviour is a liability.** "This ships with no unit tests" appeared
on 2 of 6 PRs with no specificity. Generic coverage complaints train the reader
to skim findings, which costs precision on findings that matter.

**Required change.** Only raise this when *both* hold:
1. The diff adds or changes a branch, error path, or aggregation, **and**
2. `references(symbol)` finds no test file referencing the changed symbol.

Then name the specific untested path, not the file. `src/complexity.rs` already
computes per-function complexity — gate on it.

**Acceptance test.**
> A PR adding a function with cyclomatic complexity ≥ 5 and no referencing test
> yields a finding naming that function. A PR adding a pure rename yields none.

### Class G — Severity calibration · **IN REACH** · affects every finding

**Observed miscalibration**, consistently downward:

| finding | assigned | warranted | why |
|---|---|---|---|
| readings fanout returning 1 of 20 rows | MEDIUM | HIGH | Silent, wrong data returned to callers |
| shutdown flush dropping buffered rows | LOW | MEDIUM | Silent data loss on a normal code path |
| dangling doc reference | LOW | LOW | Correct |

**Rule to encode.** Severity is a function of *blast radius × silence*, not of
how hard the fix is:

- **HIGH** — wrong data returned to a caller, or data lost, with no error raised.
- **MEDIUM** — behaviour change a caller can observe, or a correctness bug that
  surfaces loudly (throws, fails a request).
- **LOW** — cosmetic, documentation, or a nit with no runtime effect.

"Silent" is the amplifier. A bug that throws is *less* severe than one that
returns a plausible wrong answer, and the current calibration has this backwards.

### Class H — Diagnosis accuracy · **IN REACH** · trust-critical

**The instance.** On the `fastlane match` Matchfile change, the bot flagged the
right risk (the certs repo reference could break CI) with the wrong mechanism
("the new repo may not exist or was not seeded"). The repo existed and was
correctly seeded; the actual cause was the CI token's *resource owner* not
covering the new org after a repo transfer. CI broke exactly as predicted, for a
reason the finding did not name.

**Why it matters.** A right-conclusion/wrong-reason finding is worse than none:
it was verified against the stated mechanism, found sound, and dismissed. The
review reader cannot tell the difference without redoing the work.

**Required change.** When a finding asserts a mechanism the reviewer has not
verified with a tool call, it must say so. Add a field or a required phrasing:

> `Fix:` … *Unverified: I could not confirm X from the repo; the mechanism may
> differ.*

**Acceptance test.**
> A finding whose body asserts a fact not derivable from any tool call made in
> that run must carry the unverified marker.

**A second instance — reading glyphs instead of bytes.** On nomnaviet#102 the
reviewer produced its first false positives (two, both HIGH, both on the
safety-critical anti-hallucination gate): it claimed a CJK regex character class
ended in a stray unescaped `-`, and separately that the class was missing the BMP
Private Use Area range `U+E000` to `U+F8FF`. Both were wrong for the same reason —
the class *did* contain that range, written as **literal code units** (not escape
sequences), and PUA codepoints render as nothing, so in the diff the range visually
collapsed to a bare `-` immediately before the `]`. From what the model could see,
both findings were reasonable readings; they just weren't what the file contained.
Acting on either would have broken a working grounding gate.

**Required change (verify by codepoint, not glyph).** Before asserting a defect in
a character class, regex, or string literal, the reviewer must confirm it by
examining **codepoints** — `read_file` and explicit reasoning about the bytes, not
the rendered glyphs. If it cannot, it marks the finding unverified rather than
asserting a mechanism. This generalises beyond PUA to zero-width characters,
combining marks, RTL/LTR marks, homoglyphs, and non-breaking spaces: **any source
where what is displayed differs from what is stored.** The tell: the finding is
*about the exact characters* — escaping, ranges, delimiters, whitespace — which is
precisely where rendering lies.

**Acceptance test.**
> A correct character class containing the literal `U+E000`–`U+F8FF` code units
> (PUA, rendering as nothing) produces **no** finding. Any finding asserting a
> defect inside a regex, character class, or string literal that was not confirmed
> at the codepoint level must carry the unverified marker.

---

## 4. Anti-requirements

Things that would *reduce* value. Ordered by how tempting they are.

1. **Do not raise findings you cannot ground.** Precision is this bot's strongest
   asset. Every class above is a *targeted* addition, not a lowered bar.
2. **Do not add a "possible runtime issue" heuristic.** See class C.
3. **Do not emit generic coverage or style findings.** See class F.
4. **Do not exceed the existing `max_findings` budget** to fit new classes. If
   the new checks crowd out logic findings, rank by severity and drop LOW.
5. **Do not make deterministic checks (class D) go through the model.** They are
   cheaper, more reliable and more explainable in Rust.
6. **Do not review the PR description as prose.** Class B uses the body as a
   *statement of intent to verify against*, not as something to critique.

---

## 5. Suggested implementation order

Ranked by value ÷ effort, given the capability that already exists.

| # | Class | Change | Effort |
|---|---|---|---|
| 1 | D | Deterministic diff-hygiene checks in `src/diff.rs` | Low, no LLM · **binary + oversized-file done** |
| 2 | G | Severity rubric into `SYSTEM_PROMPT` + `AGENT_SYSTEM_PROMPT` | Low |
| 3 | B | PR title/body into the prompt; `read_file` linked repo docs | **Medium — highest value** |
| 4 | A | "Added middleware/guard ⇒ enumerate new failure responses" procedure | Medium |
| 5 | F | Gate coverage findings on `complexity.rs` + `references()` | Medium |
| 6 | H | Unverified-mechanism marker | Low |
| 7 | E | `[[consistency]]` rules in `.prbot.toml` | Medium |

Items 3 and 4 are the ones that would have caught the two misses that actually
mattered.

---

## 6. Regression corpus

**A class is not fixed until it is replayed.** These are real, public-shaped
cases with known-correct answers. Build them as fixtures — a stored diff plus,
where the class needs it, a repo snapshot for the tool loop.

| id | source | expected finding | class |
|---|---|---|---|
| `auth-mount-401` | nomnaviet#94 | Adding `optionalAuth` to a previously-unauthenticated route mount introduces 401 for stale tokens | A |
| `wrong-endpoint-metered` | nomnaviet#94 | PR claims to measure page OCR cost; the UI calls a different endpoint than the one instrumented | B |
| `binary-in-diff` | nomnaviet#97 | A 15 MB `.zip` added in a version-bump commit | D |
| `sql-fanout` | nomnaviet#99 | Aggregating over a filtered join truncates the aggregate — **must be HIGH, not MEDIUM** | G |
| `as-any-field` | nomnaviet#99 | `(x as any).field` reads a property absent from the type | (regression — already caught) |
| `flush-guard` | nomnaviet#94 | Early return on an in-flight guard drops buffered data on shutdown | (regression — already caught) |
| `dangling-doc-ref` | nomnaviet#100 | Code cites a doc that is not in the repo | (regression — already caught) |
| `rename-no-tests` | synthetic | A pure rename **must produce no coverage finding** | F, negative case |
| `pua-range-in-regex` | nomnaviet#102 | A correct character class containing the literal `U+E000`–`U+F8FF` code units (which render as nothing) **must produce no finding** | H, negative case |

Include the negative cases. A change that raises findings where none are
warranted is the failure mode this bot has so far avoided, and the fastest way
to lose that is to add classes without testing for silence.

---

## 7. Open questions for the implementer

- **Cost.** Class B adds document reads to every run. Bound it: read linked docs
  only when the body links a path inside the repo, cap at N files / M KB, and
  count it against `max_turns`.
- **`max_turns` pressure.** Classes A and B both want extra tool calls. Measure
  whether the current default starves them before raising it.
- **Where does intent live when the body is empty?** Many PRs have no
  description. Fall back to the branch name and commit messages, or skip class B
  rather than inventing an intent to check against.
- **Per-repo vs global.** Class E is inherently per-repo. Consider whether A and
  B also want per-repo hints (`.prbot.toml`), e.g. "the frontend lives in
  `apps/web`" to make the class-B grep targeted rather than repo-wide.
