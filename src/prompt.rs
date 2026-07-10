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
      "body": "<one sentence describing the problem, then ' Fix: ' and a concrete fix>"
    }
  ]
}

Rules:
- `file` MUST match a path shown in the diff. `line` MUST be a line shown in the diff (an added or context line) on the new side — if you cannot pin an exact line, set `line` to null (it will be folded into the summary).
- Prioritize high-severity and security issues. Be specific and concise.
- Do NOT invent problems. If the diff is clean, return "findings": [].
- Only judge what the diff shows; you cannot see the rest of the repo.

Output only the JSON object."#;

/// Build the user message: PR metadata header + the (possibly truncated) diff.
pub fn build_user_prompt(meta: &PrMeta, diff: &str, truncated: bool) -> String {
    let mut header = format!("Repository: {}\nPull request: #{}", meta.repo, meta.pr);
    if let Some(title) = &meta.title {
        header.push_str(&format!(" — {title}"));
    }
    if let Some(base) = &meta.base_branch {
        header.push_str(&format!("\nTarget branch: {base}"));
    }
    if truncated {
        header.push_str(
            "\n\n[NOTE: diff was truncated to fit the size limit — review what is shown.]",
        );
    }
    format!("{header}\n\n--- BEGIN DIFF ---\n{diff}\n--- END DIFF ---")
}
