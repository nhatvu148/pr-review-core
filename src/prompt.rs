//! The review prompt. Asks the model for a STRUCTURED JSON review so findings
//! can be posted as inline comments anchored to file + line.

use crate::config::Config;
use crate::providers::PrMeta;

/// System prompt: instructs the model to return a single JSON object with an
/// overall summary, a merge recommendation, and a list of line-anchored findings.
pub const SYSTEM_PROMPT: &str = r#"You are an expert software engineer reviewing a pull request, given ONLY its unified diff.

Analyze for:
- Security vulnerabilities and missing protections (authz, input validation, injection, secrets)
- Correctness bugs, and anything that could cause data loss or unauthorized access
- Code quality / tech debt
- Production-readiness (error handling, logging, edge cases)
- Obvious convention violations

Return ONLY a JSON object — no markdown fences, no prose around it — with exactly this shape:
{
  "summary": "<1-2 sentence overall summary of the PR>",
  "recommendation": "BLOCK" | "APPROVE WITH CHANGES" | "APPROVE",
  "findings": [
    {
      "severity": "BLOCKING" | "HIGH" | "MEDIUM" | "LOW",
      "file": "<path EXACTLY as it appears in the diff, new side>",
      "line": <integer line number in the NEW version of the file, or null if not line-specific>,
      "body": "<one sentence describing the problem, then ' Fix: ' and a concrete fix>",
      "confidence": <integer 0-100 — your confidence a senior reviewer would flag this>
    }
  ]
}

Rules:
- `file` MUST match a path shown in the diff. `line` MUST be a line shown in the diff (an added or context line) on the new side — if you cannot pin an exact line, set `line` to null (it will be folded into the summary).
- Prioritize high-severity and security issues. Be specific and concise.
- Assign confidence honestly; reserve 90+ for clear correctness/security issues. Do NOT report style nits or speculative concerns.
- Do NOT invent problems. If the diff is clean, return "findings": [].
- Only judge what the diff shows; you cannot see the rest of the repo.

Output only the JSON object."#;

/// Calibration and verification rules shared by every review path — the plain
/// OpenRouter review, the agentic review, and the agent-CLI backends that live
/// outside this crate. Appended to whichever system prompt is in play so the three
/// cannot drift.
///
/// Every rule here was written from a recorded production failure, not from theory:
///
/// - **Severity.** Two repos and two stacks filed bugs that *throw* as `LOW`, while
///   another filed two non-defects as `BLOCKING`. Severity was tracking how the
///   finding felt rather than what it costs.
/// - **Cited rules.** A finding asserted an ESLint violation from a prose rules doc
///   while the actual config had that rule `off`.
/// - **Build claims.** Two `BLOCKING` findings asserted a broken build on a commit
///   whose CI was green — a claim one status query falsifies.
/// - **Vendored code.** Three separate findings proposed edits to, or hygiene fixes
///   for, third-party source the repo's own rules forbid touching.
/// - **Repetition.** One claim about 111 files read as 111 problems.
pub const REVIEW_RULES: &str = r#"
## Severity

Severity is blast radius × how silently it fails — NOT how confident or annoyed you are.
- BLOCKING: merging causes data loss, a security breach, or a production break, AND you have direct evidence. A wrong BLOCKING is the most expensive error you can make: it stops a merge. When unsure, it is not BLOCKING.
- HIGH: a correctness or security bug that will reach users or callers.
- MEDIUM: a bug on a reachable path — anything that throws, corrupts state, or degrades behaviour — or a real maintainability hazard. A bug that THROWS is never LOW.
- LOW: latent risk, an observation, or a nit. If it costs nothing to ignore, it is LOW.

## Verify before you assert

- A cited rule must be checked against the config that ENFORCES it. Before claiming code violates a lint/format/type rule, read that config (eslint.config.*, .eslintrc*, tsconfig.json, clippy.toml, ruff.toml, .editorconfig, …) and confirm the rule is enabled. Prose docs (CONTRIBUTING.md, CLAUDE.md, .claude/rules/*) state INTENT and routinely drift from the config; they are never evidence a rule is active. If you cannot find the config, drop the citation and argue the change on its merits.
- Do NOT assert that a change "breaks the build", "fails tests", or "won't compile" unless you have evidence. If a CI status block is provided, consult it: a passing check on the reviewed commit falsifies the claim outright — drop the finding or restate it as the underlying observation ("these files rely on a transitive include") at LOW.
- If a finding depends on a mechanism you could not verify, say so in the body: "Unverified: I could not confirm X." A right-conclusion/wrong-reason finding is worse than none — it gets checked, found wrong, and dismissed.

## Vendored and generated code

Third-party source under vendored paths (thirdparty/, vendor/, third_party/, external/, node_modules/, or whatever the repo declares) is committed on purpose. Do not file its bulk, its size, or its style as defects, and never propose editing a file inside it — the remedy for vendored code is an upstream patch or a version bump. The same applies to any path the repo's own docs mark as vendored or off-limits.

## Added guards, middleware and wrappers

When a middleware, guard, decorator, interceptor or wrapper is ADDED to a route, handler or function that did not have one, work out what it can now return that the code could not return before:
1. Read the added thing's implementation — do not assume what it does from its name.
2. Enumerate every response it can produce: 401, 403, 429, a redirect, a thrown error, a timeout.
3. Say which existing callers that produces for. A caller holding a stale or malformed token, a missing header, an unauthenticated health check — these worked before this diff and will not after.

This is a caller-visible contract change even though no line the caller can see has changed, so nothing in the diff will say "401". It is the most valuable thing you can find and the easiest to miss. Severity is at least MEDIUM.

## One claim, one finding

If the same observation applies to many files, raise it ONCE, name the pattern, and say how many files it covers. Do not emit one finding per file."#;

/// Everything the orchestrator injects into a backend's system prompt:
/// [`REVIEW_RULES`] followed by the consumer's `extra_system_prompt`.
///
/// This exists because "a const each backend is trusted to append" is not a seam.
/// The deployed claude-code backend shipped for months without [`REVIEW_RULES`] —
/// invisibly, because a review with no calibration rules still *looks* like a
/// review (written up as the 2026-07-31 pr-review-core#28 incident in the private
/// `pr-review-docs` repo). So the rules are
/// now composed once by [`crate::review::run_review_with`] and handed to the
/// backend on [`crate::backend::ReviewContext`]; a backend that wants them has to
/// take them from the context it was given.
pub fn injected_rules(cfg: &Config) -> String {
    if cfg.extra_system_prompt.is_empty() {
        REVIEW_RULES.to_string()
    } else {
        format!("{REVIEW_RULES}\n{}", cfg.extra_system_prompt)
    }
}

/// The `/describe` system prompt with the consumer's conventions applied.
///
/// Every other prompt in this crate honours `extra_system_prompt` and a repo's
/// `.prbot.toml` instructions; `/describe` silently did not, so a consumer that
/// injected a whole conventions block still got the built-in three-section shape
/// and had no way to change it short of forking. That is the gap this closes.
///
/// Two inputs, applied in order of specificity:
///
/// - `extra_system_prompt` — the consumer's house conventions, the same block the
///   review prompts get.
/// - `describe_instructions` — free-form text about *this output's shape*
///   (`DESCRIBE_INSTRUCTIONS`, or `describe_instructions` in `.prbot.toml`). It
///   comes last and is told it wins, so a repo can ask for release-notes sections
///   or a contributor table and actually get them rather than a blend.
pub fn describe_system_prompt(cfg: &Config) -> String {
    let mut s = DESCRIBE_SYSTEM_PROMPT.to_string();
    if !cfg.extra_system_prompt.trim().is_empty() {
        s.push('\n');
        s.push_str(cfg.extra_system_prompt.trim());
    }
    if !cfg.describe_instructions.trim().is_empty() {
        // Stated explicitly rather than left to ordering. A model handed two
        // section lists without being told which governs will merge them, and the
        // repo asking for release notes gets release notes *plus* the default
        // three sections — which is the failure this whole feature exists to avoid.
        s.push_str(
            "\n\nThe following instructions describe the required shape of this \
             description. Where they conflict with the section layout above, FOLLOW \
             THESE AND IGNORE THE LAYOUT ABOVE — including replacing the section \
             headings entirely:\n",
        );
        s.push_str(cfg.describe_instructions.trim());
    }
    s
}

/// Join a backend's own rubric to the orchestrator-injected rules. The single
/// place that decides how the two are separated, so the diff-only path, the
/// agentic path, and any out-of-crate backend cannot drift apart.
pub fn with_injected_rules(rubric: &str, injected_rules: &str) -> String {
    if injected_rules.trim().is_empty() {
        rubric.to_string()
    } else {
        format!("{rubric}\n{injected_rules}")
    }
}

/// The complete system prompt for a plain diff-only review: the [`SYSTEM_PROMPT`]
/// rubric plus the injected rules. Reviews normally get this through
/// [`crate::backend::ReviewContext::system_prompt`]; this is the same string for
/// callers that drive [`crate::llm::review_diff`] directly (the bench harness).
pub fn review_system_prompt(cfg: &Config) -> String {
    with_injected_rules(SYSTEM_PROMPT, &injected_rules(cfg))
}

/// System prompt for the optional second-pass self-critique. Given the diff and a
/// JSON array of proposed findings, the model prunes noise and re-scores what it
/// keeps, returning ONLY a JSON array of the surviving findings.
pub const CRITIQUE_SYSTEM_PROMPT: &str = r#"You are a skeptical senior reviewer doing a second pass. Given the diff and a JSON array of proposed findings, REMOVE false positives, duplicates, out-of-scope nits, and anything not clearly actionable. For each finding you KEEP, set an honest `confidence` 0–100. Return ONLY a JSON array of the kept findings, each with the same shape {severity, file, line, body, confidence}. If all should be dropped, return []."#;

/// System prompt for the `/ask` command: answer a free-form question about the
/// PR, grounded strictly in its diff.
pub const ASK_SYSTEM_PROMPT: &str = r#"You are an expert software engineer answering a question about a pull request, given its unified diff and any structural context. Answer the question directly and concisely in GitHub-flavored markdown. Ground every claim in what the diff actually shows — if the diff doesn't contain enough information to answer, say so plainly rather than guessing. Do not invent code, files, or behavior that isn't present. Keep it focused; no preamble like "Great question"."#;

/// System prompt for the `/describe` command: write a PR description from the diff.
pub const DESCRIBE_SYSTEM_PROMPT: &str = r#"You are writing a clear, factual pull request description from its unified diff. Return GitHub-flavored markdown with these sections:

## Summary
One short paragraph on what this PR does and why.

## Changes
A bullet list of the notable changes (group related files; skip trivia like lockfile churn).

## Notes for reviewers
(Optional) Anything a reviewer should focus on — risky areas, follow-ups, or things intentionally out of scope. Omit this section if there's nothing useful to say.

Describe ONLY what the diff shows — do not speculate about intent you can't see or invent testing that isn't present. Do not add a top-level title header (the PR already has a title). Be concise."#;

/// System prompt for the `/review-file` command: deep-review an ENTIRE file (not a
/// diff), so findings may sit on any line. Same JSON contract as the diff review.
pub const FILE_REVIEW_SYSTEM_PROMPT: &str = r#"You are an expert software engineer deep-reviewing an ENTIRE source file on request (not a diff). You are given the full file with 1-indexed line numbers.

Analyze the whole file for security vulnerabilities, correctness bugs, error/resource handling, and clear code-quality problems. Findings may be on ANY line.

Return ONLY a JSON object — no markdown fences, no prose around it — with exactly this shape:
{
  "summary": "<1-2 sentence overall assessment of the file>",
  "recommendation": "BLOCK" | "APPROVE WITH CHANGES" | "APPROVE",
  "findings": [
    { "severity": "BLOCKING" | "HIGH" | "MEDIUM" | "LOW",
      "file": "<the file path>",
      "line": <1-indexed line number in this file, or null>,
      "body": "<one sentence describing the problem, then ' Fix: ' and a concrete fix>",
      "confidence": <integer 0-100 — your confidence a senior reviewer would flag this> }
  ]
}
Rules: `line` is a line number shown in this file. Prioritize real security/correctness issues; be specific and concise; do NOT report speculative concerns or style nits. If the file is clean, return "findings": []. Output only the JSON object."#;

/// Build the user message: PR metadata header + the (possibly truncated) diff.
///
/// `omitted_note`, when `Some`, describes whole files that were dropped to fit the
/// size budget (packed out before this call) and is surfaced to the model so it
/// knows those files were NOT reviewed. This is distinct from `truncated`, which
/// flags a hard character clamp of a single oversized file.
///
/// `structural_context`, when `Some` and non-empty, names the enclosing
/// function/symbol of each changed line (see [`crate::structure`]); it's inserted
/// as a `## Structural context` block BEFORE the diff so the model knows each
/// change's scope.
pub fn build_user_prompt(
    meta: &PrMeta,
    diff: &str,
    truncated: bool,
    omitted_note: Option<&str>,
    structural_context: Option<&str>,
) -> String {
    let mut header = format!("Repository: {}\nPull request: #{}", meta.repo, meta.pr);
    if let Some(title) = &meta.title {
        header.push_str(&format!(" — {title}"));
    }
    if let Some(base) = &meta.base_branch {
        header.push_str(&format!("\nTarget branch: {base}"));
    }
    if let Some(note) = omitted_note {
        header.push_str(&format!("\n\n[NOTE: {note}]"));
    }
    if truncated {
        header.push_str(
            "\n\n[NOTE: diff was truncated to fit the size limit — review what is shown.]",
        );
    }
    // CI results for the reviewed commit, when the provider reported any. Placed
    // before the diff so it frames everything read after it: a green check here
    // settles "does this build" without the model reasoning about it at all.
    if let Some(ci) = &meta.ci_status {
        if !ci.trim().is_empty() {
            header.push_str(&format!(
                "\n\n## CI status for the reviewed commit\n{ci}\n\
                 (These checks ran on the exact commit under review. A passing check \
                 FALSIFIES any claim that this change breaks that build — do not \
                 assert otherwise.)\n"
            ));
        }
    }
    if let Some(ctx) = structural_context {
        if !ctx.trim().is_empty() {
            header.push_str(&format!("\n\n## Structural context\n{ctx}\n"));
        }
    }
    format!("{header}\n\n--- BEGIN DIFF ---\n{diff}\n--- END DIFF ---")
}

#[cfg(test)]
mod describe_prompt_tests {
    use super::*;
    use crate::config::Config;

    fn cfg() -> Config {
        let mut c = Config::from_env();
        c.extra_system_prompt = String::new();
        c.describe_instructions = String::new();
        c
    }

    /// The default shape is unchanged for anyone who configures nothing.
    #[test]
    fn a_bare_config_yields_the_builtin_prompt_verbatim() {
        assert_eq!(describe_system_prompt(&cfg()), DESCRIBE_SYSTEM_PROMPT);
    }

    /// The gap this closes: a consumer injecting house conventions got them on
    /// every prompt in the crate except this one.
    #[test]
    fn the_consumers_conventions_reach_the_describe_prompt() {
        let mut c = cfg();
        c.extra_system_prompt = "Never mention lockfiles.".to_string();
        let p = describe_system_prompt(&c);
        assert!(p.starts_with(DESCRIBE_SYSTEM_PROMPT), "{p}");
        assert!(p.contains("Never mention lockfiles."), "{p}");
    }

    /// A layout instruction has to be told it outranks the built-in section list,
    /// or the model returns both — release notes *and* the default three sections.
    #[test]
    fn layout_instructions_are_declared_to_outrank_the_default_sections() {
        let mut c = cfg();
        c.describe_instructions = "Use: Breaking Changes, New Features, Bug Fixes.".to_string();
        let p = describe_system_prompt(&c);
        assert!(
            p.contains("Breaking Changes, New Features, Bug Fixes."),
            "{p}"
        );
        assert!(p.contains("IGNORE THE LAYOUT ABOVE"), "{p}");
        // ...and it comes last, so nothing after it can walk it back.
        assert!(
            p.rfind("Breaking Changes").unwrap() > p.rfind("Notes for reviewers").unwrap(),
            "{p}"
        );
    }

    /// Both at once: conventions first, then the shape that governs.
    #[test]
    fn conventions_and_layout_compose_in_order_of_specificity() {
        let mut c = cfg();
        c.extra_system_prompt = "House voice.".to_string();
        c.describe_instructions = "Release notes only.".to_string();
        let p = describe_system_prompt(&c);
        assert!(
            p.find("House voice.").unwrap() < p.find("Release notes only.").unwrap(),
            "{p}"
        );
    }

    /// Whitespace-only is not an instruction — it must not bolt an empty
    /// "follow these" clause onto the prompt with nothing after it.
    #[test]
    fn whitespace_only_instructions_change_nothing() {
        let mut c = cfg();
        c.describe_instructions = "   \n  ".to_string();
        assert_eq!(describe_system_prompt(&c), DESCRIBE_SYSTEM_PROMPT);
    }
}

#[cfg(test)]
mod tests {
    use super::{build_user_prompt, REVIEW_RULES};
    use crate::providers::PrMeta;

    fn meta(ci: Option<&str>) -> PrMeta {
        PrMeta {
            repo: "o/r".to_string(),
            pr: 1,
            title: None,
            base_branch: None,
            head_sha: None,
            body: None,
            ci_status: ci.map(str::to_string),
        }
    }

    /// VinaText#10: two BLOCKING findings asserted a broken build on a commit whose
    /// CI was green. The reviewer cannot query CI, so the answer has to arrive in the
    /// prompt — before the diff, with the consequence spelled out.
    #[test]
    fn ci_status_is_rendered_before_the_diff_with_its_consequence() {
        let p = build_user_prompt(
            &meta(Some("- MFC build (Release|x64): success")),
            "diff body",
            false,
            None,
            None,
        );
        assert!(p.contains("## CI status for the reviewed commit"));
        assert!(p.contains("- MFC build (Release|x64): success"));
        assert!(p.contains("FALSIFIES"));
        assert!(
            p.find("## CI status").unwrap() < p.find("--- BEGIN DIFF ---").unwrap(),
            "the CI block must frame the diff, not trail it"
        );
    }

    /// No CI block at all when nothing reported — an absent block reads as "unknown",
    /// whereas an empty one would imply "nothing ran", which is a different claim.
    #[test]
    fn no_ci_block_when_the_provider_reported_nothing() {
        let p = build_user_prompt(&meta(None), "diff body", false, None, None);
        assert!(!p.contains("CI status"));
        let empty = build_user_prompt(&meta(Some("  ")), "diff body", false, None, None);
        assert!(!empty.contains("CI status"));
    }

    /// The refactor that moved rule composition from each backend to the
    /// orchestrator must not change one byte of what the model receives — a
    /// prompt change and a plumbing change in the same commit are impossible to
    /// tell apart in the bench numbers afterwards.
    #[test]
    fn composition_is_byte_identical_to_the_per_backend_formula() {
        use super::{injected_rules, with_injected_rules, SYSTEM_PROMPT};
        let mut cfg = crate::config::Config::from_env();

        cfg.extra_system_prompt = String::new();
        let old = format!("{SYSTEM_PROMPT}\n{REVIEW_RULES}");
        assert_eq!(
            with_injected_rules(SYSTEM_PROMPT, &injected_rules(&cfg)),
            old
        );

        cfg.extra_system_prompt = "House conventions.".to_string();
        let old = format!("{SYSTEM_PROMPT}\n{REVIEW_RULES}\nHouse conventions.");
        assert_eq!(
            with_injected_rules(SYSTEM_PROMPT, &injected_rules(&cfg)),
            old
        );
    }

    #[test]
    fn the_shared_rules_cover_each_recorded_failure() {
        // Cheap guard against a well-meaning edit quietly dropping one.
        for required in [
            "BLOCKING", // severity rubric
            "THROWS is never LOW",
            "config that ENFORCES it",
            "breaks the build",
            "vendored",
            "raise it ONCE",
            "middleware, guard, decorator", // class A: added-guard procedure
            "401",
        ] {
            assert!(
                REVIEW_RULES.contains(required),
                "REVIEW_RULES lost the rule containing {required:?}"
            );
        }
    }
}
