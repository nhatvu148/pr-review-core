//! The review prompt. Asks the model for a STRUCTURED JSON review so findings
//! can be posted as inline comments anchored to file + line.

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

## One claim, one finding

If the same observation applies to many files, raise it ONCE, name the pattern, and say how many files it covers. Do not emit one finding per file."#;

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
        ] {
            assert!(
                REVIEW_RULES.contains(required),
                "REVIEW_RULES lost the rule containing {required:?}"
            );
        }
    }
}
