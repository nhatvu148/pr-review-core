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
    if let Some(ctx) = structural_context {
        if !ctx.trim().is_empty() {
            header.push_str(&format!("\n\n## Structural context\n{ctx}\n"));
        }
    }
    format!("{header}\n\n--- BEGIN DIFF ---\n{diff}\n--- END DIFF ---")
}
