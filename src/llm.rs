//! OpenRouter chat-completions client. Sends the review prompt to a Claude model
//! via OpenRouter and parses the structured review back.

use anyhow::{Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::clip;
use crate::config::{require, Config};
use crate::prompt::{
    build_user_prompt, ASK_SYSTEM_PROMPT, CRITIQUE_SYSTEM_PROMPT, DESCRIBE_SYSTEM_PROMPT,
    FILE_REVIEW_SYSTEM_PROMPT,
};
use crate::providers::PrMeta;

#[derive(Serialize)]
struct Msg {
    role: String,
    content: String,
}

#[derive(Serialize)]
struct ChatReq {
    model: String,
    max_tokens: u32,
    temperature: f32,
    messages: Vec<Msg>,
}

/// Token accounting echoed back by OpenRouter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: Option<u32>,
    pub completion_tokens: Option<u32>,
    pub total_tokens: Option<u32>,
}

/// Severity assumed when the model omits the field. Deliberately not `LOW`:
/// `severity_rank` ranks LOW and unknown equally at 0, so an unlabelled finding
/// would sort last and be first out under `max_findings`. MEDIUM keeps a real
/// finding visible without inventing urgency.
fn default_severity() -> String {
    "MEDIUM".to_string()
}

/// One review finding from the model.
///
/// Only `body` is required. `severity` and `file` are defaulted rather than
/// demanded because a model that drops one field must not cost the whole review:
/// an unlabelled severity becomes MEDIUM, and an empty `file` simply fails to
/// anchor and folds into the summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finding {
    #[serde(default = "default_severity")]
    pub severity: String,
    #[serde(default)]
    pub file: String,
    #[serde(default)]
    pub line: Option<u64>,
    pub body: String,
    /// Model's confidence (0–100) that this is a real, actionable issue a senior
    /// reviewer would flag. Absent on older responses; treated as full confidence.
    #[serde(default)]
    pub confidence: Option<u8>,
}

/// Parse a findings array element-by-element, dropping (with a warning) any element
/// that still can't be understood. One malformed finding must never invalidate the
/// review it sits in — that trades a whole expensive review for a formatting slip.
///
/// Returns `(kept, dropped)`.
pub(crate) fn findings_from_values(raw: Vec<serde_json::Value>) -> (Vec<Finding>, usize) {
    let mut kept = Vec::with_capacity(raw.len());
    let mut dropped = 0usize;
    for v in raw {
        match serde_json::from_value::<Finding>(v) {
            Ok(f) => kept.push(f),
            Err(e) => {
                dropped += 1;
                tracing::warn!("dropping malformed finding ({e})");
            }
        }
    }
    (kept, dropped)
}

fn lenient_findings<'de, D>(d: D) -> std::result::Result<Vec<Finding>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Vec::<serde_json::Value>::deserialize(d)?;
    Ok(findings_from_values(raw).0)
}

/// The structured review the model returns.
#[derive(Debug, Clone, Deserialize)]
pub struct Review {
    pub summary: String,
    pub recommendation: String,
    #[serde(default, deserialize_with = "lenient_findings")]
    pub findings: Vec<Finding>,
}

#[derive(Deserialize)]
struct ChoiceMsg {
    content: Option<String>,
}
#[derive(Deserialize)]
struct Choice {
    message: Option<ChoiceMsg>,
}
#[derive(Deserialize)]
struct ApiErr {
    message: Option<String>,
}
#[derive(Deserialize)]
struct ChatRes {
    choices: Option<Vec<Choice>>,
    usage: Option<Usage>,
    error: Option<ApiErr>,
}

/// The outcome of one review call.
#[derive(Debug)]
pub struct ReviewResult {
    pub review: Review,
    pub model: String,
    pub usage: Option<Usage>,
}

/// The outcome of one free-form completion — everything the call reported, not
/// just its text.
///
/// [`ReviewBackend::complete`] returns bare `String`, which is enough for `/ask`
/// but throws away `usage`. That matters once a completion is billed as part of
/// something larger: the JSON repair pass in [`parse_review_with_repair`] is a
/// second call on the same review, and a review that reports only the first
/// call's tokens under-states what it actually cost.
///
/// [`ReviewBackend::complete`]: crate::backend::ReviewBackend::complete
#[derive(Debug, Default)]
pub struct Completion {
    pub text: String,
    /// Model the backend reported using, when it says.
    pub model: Option<String>,
    pub usage: Option<Usage>,
}

/// Pull the JSON object out of a model response that may be wrapped in prose or
/// ```json fences — take the first `{` through the last `}`. Exposed so custom
/// [`crate::backend::ReviewBackend`]s can parse a model's text into a [`Review`].
pub fn extract_json(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if end > start {
        Some(&text[start..=end])
    } else {
        None
    }
}

/// Pull the first JSON array out of a model response that may be wrapped in prose
/// or ```json fences — take the first `[` through the last `]`.
pub(crate) fn extract_json_array(text: &str) -> Option<&str> {
    let start = text.find('[')?;
    let end = text.rfind(']')?;
    if end > start {
        Some(&text[start..=end])
    } else {
        None
    }
}

/// Call OpenRouter and parse the structured review.
///
/// The diff is expected to be pre-packed to fit the size budget (whole files
/// dropped, lowest-priority first) by [`crate::diff::pack_diff`]; `omitted_note`
/// carries the human-readable list of those dropped files so the model is told
/// they were NOT reviewed. A SAFETY clamp (`take(max_diff_chars)`) still applies
/// so a single un-packable oversized file can't blow the budget.
///
/// `system_prompt` is supplied by the caller: the orchestrator composes it from the
/// rubric and the injected rules, and hands it to the backend on
/// [`crate::backend::ReviewContext`]. Callers driving this function directly can
/// build the same string with [`crate::prompt::review_system_prompt`].
///
/// # Errors
/// If `OPENROUTER_API_KEY` is missing, OpenRouter returns an error status, or the
/// response can't be parsed as the expected review JSON.
pub async fn review_diff(
    client: &Client,
    cfg: &Config,
    meta: &PrMeta,
    diff: &str,
    omitted_note: Option<String>,
    structural_context: Option<&str>,
    system_prompt: &str,
) -> Result<ReviewResult> {
    require(&cfg.openrouter_api_key, "OPENROUTER_API_KEY")?;

    // Safety clamp: the diff is already packed to fit, but a lone giant file can
    // still exceed the cap — hard-trim it and flag the truncation.
    let truncated = diff.chars().count() > cfg.max_diff_chars;
    let clipped: String = if truncated {
        diff.chars().take(cfg.max_diff_chars).collect()
    } else {
        diff.to_string()
    };

    let req = ChatReq {
        model: cfg.openrouter_model.clone(),
        max_tokens: cfg.openrouter_max_tokens,
        temperature: cfg.openrouter_temperature,
        messages: vec![
            Msg {
                role: "system".into(),
                content: system_prompt.to_string(),
            },
            Msg {
                role: "user".into(),
                content: build_user_prompt(
                    meta,
                    &clipped,
                    truncated,
                    omitted_note.as_deref(),
                    structural_context,
                ),
            },
        ],
    };

    let res = client
        .post(format!("{}/chat/completions", cfg.openrouter_base_url))
        .bearer_auth(&cfg.openrouter_api_key)
        .header("HTTP-Referer", &cfg.http_referer)
        .header("X-Title", &cfg.x_title)
        .json(&req)
        .send()
        .await?;

    let status = res.status();
    let text = res.text().await?;
    let data: ChatRes = serde_json::from_str(&text).map_err(|e| {
        anyhow::anyhow!(
            "OpenRouter {status}: non-JSON response ({e}): {}",
            clip(&text, 300)
        )
    })?;

    if !status.is_success() || data.error.is_some() {
        let msg = data
            .error
            .and_then(|e| e.message)
            .unwrap_or_else(|| clip(&text, 500));
        anyhow::bail!("OpenRouter {status}: {msg}");
    }

    let content = data
        .choices
        .and_then(|c| c.into_iter().next())
        .and_then(|c| c.message)
        .and_then(|m| m.content)
        .ok_or_else(|| anyhow::anyhow!("OpenRouter returned an empty response."))?;

    let json = extract_json(&content).ok_or_else(|| {
        anyhow::anyhow!(
            "Model did not return a JSON object: {}",
            clip(&content, 300)
        )
    })?;
    // One repair pass rather than discarding the review over a stray character.
    let (review, repair_usage) =
        parse_review_with_repair(json, "openrouter review", |system, broken| async move {
            chat_completion(client, cfg, system, &broken).await
        })
        .await?;

    Ok(ReviewResult {
        review,
        model: cfg.openrouter_model.clone(),
        // The repair is a second billed call; its tokens still count.
        usage: add_usage(data.usage, repair_usage),
    })
}

/// Second-pass "self-critique": ask the model to prune false positives, duplicates,
/// and out-of-scope nits from a set of proposed findings, and to assign an honest
/// confidence to each survivor.
///
/// Runs on `backend` — the same backend that produced the review — via
/// [`crate::backend::ReviewBackend::complete`]. It used to post to OpenRouter
/// directly, which meant any consumer with its own backend (an agent CLI, a local
/// model) lost the noise filter entirely unless it also held an OpenRouter key. The
/// default backend still resolves `complete()` to the OpenRouter chat path, so
/// nothing changes for the bot.
///
/// The caller MUST treat any error as fail-open (keep the original findings) — a
/// critique hiccup must never drop the review.
///
/// # Errors
/// If the backend call fails, or the response can't be parsed as a JSON array of
/// findings.
pub async fn critique_findings(
    cfg: &Config,
    backend: &dyn crate::backend::ReviewBackend,
    meta: &PrMeta,
    diff: &str,
    findings: &[Finding],
) -> Result<Vec<Finding>> {
    let clipped: String = diff.chars().take(cfg.max_diff_chars).collect();
    let findings_json = serde_json::to_string_pretty(findings)
        .map_err(|e| anyhow::anyhow!("could not serialize findings for critique: {e}"))?;
    let user = format!(
        "Repository: {}\nPull request: #{}\n\n--- BEGIN DIFF ---\n{clipped}\n--- END DIFF ---\n\n--- PROPOSED FINDINGS (JSON) ---\n{findings_json}",
        meta.repo, meta.pr,
    );

    let content = backend.complete(cfg, CRITIQUE_SYSTEM_PROMPT, &user).await?;

    let json = extract_json_array(&content).ok_or_else(|| {
        anyhow::anyhow!(
            "Critique did not return a JSON array: {}",
            clip(&content, 300)
        )
    })?;
    let raw: Vec<serde_json::Value> = serde_json::from_str(json)
        .map_err(|e| anyhow::anyhow!("Could not parse critique JSON ({e}): {}", clip(json, 300)))?;
    let (kept, dropped) = findings_from_values(raw);
    // If the critique returned elements but none survived, its shape is wrong — error
    // so the caller fails open and keeps the original findings. Silently returning an
    // empty list here would delete every finding in the review.
    if kept.is_empty() && dropped > 0 {
        anyhow::bail!(
            "Critique returned {dropped} unparseable finding(s): {}",
            clip(json, 300)
        );
    }

    Ok(kept)
}

/// One-shot chat completion returning the raw assistant text. Shares the same
/// OpenRouter call pattern (headers, base URL, synthesis model) as [`review_diff`].
///
/// # Errors
/// If `OPENROUTER_API_KEY` is missing, OpenRouter returns an error status, or the
/// response has no content.
pub(crate) async fn chat_completion(
    client: &Client,
    cfg: &Config,
    system: &str,
    user: &str,
) -> Result<Completion> {
    require(&cfg.openrouter_api_key, "OPENROUTER_API_KEY")?;

    let req = ChatReq {
        model: cfg.openrouter_model.clone(),
        max_tokens: cfg.openrouter_max_tokens,
        temperature: cfg.openrouter_temperature,
        messages: vec![
            Msg {
                role: "system".into(),
                content: system.to_string(),
            },
            Msg {
                role: "user".into(),
                content: user.to_string(),
            },
        ],
    };

    let res = client
        .post(format!("{}/chat/completions", cfg.openrouter_base_url))
        .bearer_auth(&cfg.openrouter_api_key)
        .header("HTTP-Referer", &cfg.http_referer)
        .header("X-Title", &cfg.x_title)
        .json(&req)
        .send()
        .await?;

    let status = res.status();
    let text = res.text().await?;
    let data: ChatRes = serde_json::from_str(&text).map_err(|e| {
        anyhow::anyhow!(
            "OpenRouter {status}: non-JSON response ({e}): {}",
            clip(&text, 300)
        )
    })?;

    if !status.is_success() || data.error.is_some() {
        let msg = data
            .error
            .and_then(|e| e.message)
            .unwrap_or_else(|| clip(&text, 500));
        anyhow::bail!("OpenRouter {status}: {msg}");
    }

    let text = data
        .choices
        .and_then(|c| c.into_iter().next())
        .and_then(|c| c.message)
        .and_then(|m| m.content)
        .map(|c| c.trim().to_string())
        .filter(|c| !c.is_empty())
        .ok_or_else(|| anyhow::anyhow!("OpenRouter returned an empty response."))?;
    // `usage` was already parsed out of the response; returning it costs nothing
    // and is the difference between a repaired review reporting its real bill and
    // reporting only half of it.
    Ok(Completion {
        text,
        model: Some(cfg.openrouter_model.clone()),
        usage: data.usage,
    })
}

/// Answer a free-form question about a PR (the `/ask` command), grounded in its
/// diff. Returns the answer as markdown.
///
/// # Errors
/// If `OPENROUTER_API_KEY` is missing or the OpenRouter call fails.
pub async fn answer_question(
    cfg: &Config,
    backend: &dyn crate::backend::ReviewBackend,
    meta: &PrMeta,
    diff: &str,
    question: &str,
    structural_context: Option<&str>,
) -> Result<String> {
    let clipped: String = diff.chars().take(cfg.max_diff_chars).collect();
    let truncated = diff.chars().count() > cfg.max_diff_chars;
    let context = build_user_prompt(meta, &clipped, truncated, None, structural_context);
    let user = format!("{context}\n\n--- QUESTION ---\n{}", question.trim());
    let system = if cfg.extra_system_prompt.is_empty() {
        ASK_SYSTEM_PROMPT.to_string()
    } else {
        format!("{ASK_SYSTEM_PROMPT}\n{}", cfg.extra_system_prompt)
    };
    backend.complete(cfg, &system, &user).await
}

/// Generate a PR description from its diff (the `/describe` command). Returns
/// markdown (no title header — the PR already has a title).
///
/// # Errors
/// If `OPENROUTER_API_KEY` is missing or the OpenRouter call fails.
pub async fn describe_pr(
    cfg: &Config,
    backend: &dyn crate::backend::ReviewBackend,
    meta: &PrMeta,
    diff: &str,
    structural_context: Option<&str>,
) -> Result<String> {
    let clipped: String = diff.chars().take(cfg.max_diff_chars).collect();
    let truncated = diff.chars().count() > cfg.max_diff_chars;
    let user = build_user_prompt(meta, &clipped, truncated, None, structural_context);
    backend.complete(cfg, DESCRIBE_SYSTEM_PROMPT, &user).await
}

/// Deep-review an ENTIRE file (the `/review-file` command): number the file's
/// lines, ask the model for the structured review JSON, and parse it. Findings may
/// anchor to any line in the file (not just diff lines).
///
/// # Errors
/// On a backend failure, or if the model doesn't return a parseable review.
pub async fn review_file(
    cfg: &Config,
    backend: &dyn crate::backend::ReviewBackend,
    path: &str,
    content: &str,
) -> Result<Review> {
    // 1-index the lines so the model can anchor findings to real line numbers.
    let numbered: String = content
        .lines()
        .enumerate()
        .map(|(i, l)| format!("{}: {l}", i + 1))
        .collect::<Vec<_>>()
        .join("\n");
    let clipped: String = numbered.chars().take(cfg.max_diff_chars).collect();
    let truncated = numbered.chars().count() > cfg.max_diff_chars;
    let user = format!(
        "File: {path}{}\n\n{clipped}",
        if truncated {
            "\n[NOTE: file truncated to the size limit — review what is shown]"
        } else {
            ""
        }
    );
    let system = if cfg.extra_system_prompt.is_empty() {
        FILE_REVIEW_SYSTEM_PROMPT.to_string()
    } else {
        format!("{FILE_REVIEW_SYSTEM_PROMPT}\n{}", cfg.extra_system_prompt)
    };
    let raw = backend.complete(cfg, &system, &user).await?;
    let json = extract_json(&raw)
        .ok_or_else(|| anyhow::anyhow!("file review returned no JSON: {}", clip(&raw, 300)))?;
    // Same salvage as the review path: /review-file emits the same shape of JSON
    // and had the same one-stray-character failure.
    let (review, _repair_usage) =
        parse_review_with_repair(json, "file review", |system, broken| async move {
            backend.complete_detailed(cfg, system, &broken).await
        })
        .await?;
    Ok(review)
}

#[cfg(test)]
mod tests {
    //! Deserialization tolerance. A model that drops a field on one finding used to
    //! cost the entire review (seen in production: `missing field \`severity\``
    //! after a ~5-minute agent run). These pin the salvage behaviour.

    use super::{findings_from_values, Review};

    #[test]
    fn finding_missing_severity_defaults_to_medium() {
        let review: Review = serde_json::from_str(
            r#"{"summary":"s","recommendation":"APPROVE WITH CHANGES",
                "findings":[{"file":"a.rs","line":11,"body":"b"}]}"#,
        )
        .expect("a missing severity must not fail the whole review");
        assert_eq!(review.findings.len(), 1);
        assert_eq!(review.findings[0].severity, "MEDIUM");
    }

    #[test]
    fn finding_missing_file_still_parses_and_folds_to_summary() {
        let review: Review = serde_json::from_str(
            r#"{"summary":"s","recommendation":"APPROVE",
                "findings":[{"severity":"HIGH","body":"b"}]}"#,
        )
        .expect("a missing file must not fail the whole review");
        assert_eq!(review.findings[0].file, "");
        assert_eq!(review.findings[0].line, None);
    }

    #[test]
    fn one_malformed_finding_is_dropped_not_fatal() {
        // Element 2 has no `body` — nothing to post — so it goes, and the two real
        // findings survive.
        let review: Review = serde_json::from_str(
            r#"{"summary":"s","recommendation":"BLOCK","findings":[
                {"severity":"HIGH","file":"a.rs","line":3,"body":"real"},
                {"severity":"LOW","file":"b.rs"},
                {"file":"c.rs","body":"also real"}]}"#,
        )
        .expect("one bad element must not fail the whole review");
        assert_eq!(review.findings.len(), 2);
        assert_eq!(review.findings[1].severity, "MEDIUM");
    }

    #[test]
    fn required_fields_of_the_review_itself_still_hard_fail() {
        // `summary`/`recommendation` stay mandatory: without them there is no review
        // to post, so failing loudly is right.
        assert!(serde_json::from_str::<Review>(r#"{"summary":"s","findings":[]}"#).is_err());
    }

    #[test]
    fn findings_from_values_reports_what_it_dropped() {
        let raw = vec![
            serde_json::json!({"file": "a.rs", "body": "real"}),
            serde_json::json!({"file": "b.rs"}),
            serde_json::json!("not even an object"),
        ];
        let (kept, dropped) = findings_from_values(raw);
        assert_eq!(kept.len(), 1);
        assert_eq!(dropped, 2);
    }
}

/// [`chat_completion`] when only the text is wanted.
pub(crate) async fn chat_text(
    client: &Client,
    cfg: &Config,
    system: &str,
    user: &str,
) -> Result<String> {
    Ok(chat_completion(client, cfg, system, user).await?.text)
}

// ─── Review JSON salvage ────────────────────────────────────────────────────
//
// A single unescaped `"` in a finding's `body` used to discard an entire review.
// On `SIMCEL/simcel-saas#3` that threw away 265s of agent work and left the PR on
// a stale "Reviewing this PR…" placeholder with nothing to retry it.
//
// [`lenient_findings`] does not cover this: it drops findings that fail to
// *deserialize*, which only happens once the text parses as JSON at all. A
// **syntax** error takes the whole review down before serde sees a finding.
//
// This lives in core rather than in a backend because every path that turns model
// output into a [`Review`] has the same exposure — the OpenRouter review,
// `/review-file`, and every downstream agent backend. It was fixed in one of them
// first, by hand; the copy that stayed behind kept the bug, which is the argument
// for it being here.

/// System prompt for the repair pass.
const JSON_REPAIR_SYSTEM: &str = r#"You repair malformed JSON. The user message is a JSON object that failed to parse. Return the SAME data as a single valid JSON object and nothing else — no markdown fences, no prose, no commentary.

Rules:
- Preserve every field and all text content EXACTLY as given. Do NOT summarize, shorten, reword, reorder, add, or drop anything.
- Fix syntax only: escape double quotes inside strings as \", escape raw newlines as \n, escape stray backslashes as \\, add missing commas, remove trailing commas.
- If the input is truncated mid-value, close the open string/array/object so the result parses, keeping everything already present.

Output only the JSON object."#;

/// Appended to the summary of a review whose JSON was **truncated** rather than
/// merely malformed.
///
/// Closing the open value keeps the findings that arrived, but whatever the model
/// never emitted is gone — and a partial review is indistinguishable from a
/// complete one once posted. A log line does not fix that: the person deciding
/// whether to merge reads the PR comment, not the logs. So the comment itself has
/// to say so, or the salvage trades a visible failure for an invisible one.
pub const TRUNCATED_NOTE: &str = "\n\n⚠️ _The model's response was cut off mid-output. This review was salvaged from the part that arrived and is **incomplete** — any findings after the cut are missing. Re-run `/review` for a full pass._";

/// Whether a parse failure means the JSON *ran out* rather than being malformed.
///
/// This decides whether the repair is cosmetic (re-escape a stray quote, losing
/// nothing) or lossy (close an open value, dropping everything after the cut).
fn is_truncation(err: &serde_json::Error) -> bool {
    matches!(err.classify(), serde_json::error::Category::Eof)
}

/// How many findings the model *appears* to have written, counted from raw text
/// before it is repaired.
///
/// Necessarily a heuristic: the text does not parse — that is why it is being
/// repaired — so the count cannot come from serde. `"severity"` is the one key
/// every finding carries, but a `body` containing that literal inflates it. So it
/// can justify a warning and never a failure.
fn count_findings_hint(raw: &str) -> usize {
    raw.matches("\"severity\"").count()
}

/// Add the repair pass's tokens to the review's, so a repaired review reports what
/// it actually cost rather than only the part that happened to parse.
///
/// Every count is independently optional, so a missing figure on one side must not
/// erase a present one on the other — absent means "not reported", not zero.
pub(crate) fn add_usage(review: Option<Usage>, repair: Option<Usage>) -> Option<Usage> {
    fn add(a: Option<u32>, b: Option<u32>) -> Option<u32> {
        match (a, b) {
            (Some(a), Some(b)) => Some(a.saturating_add(b)),
            (only, None) | (None, only) => only,
        }
    }
    match (review, repair) {
        (Some(r), Some(p)) => Some(Usage {
            prompt_tokens: add(r.prompt_tokens, p.prompt_tokens),
            completion_tokens: add(r.completion_tokens, p.completion_tokens),
            total_tokens: add(r.total_tokens, p.total_tokens),
        }),
        (only, None) | (None, only) => only,
    }
}

/// Quote the text around the spot serde flagged, for an error a human can act on.
///
/// Clipping the head of the object instead shows, for a pretty-printed review, the
/// *summary* — never the break. The original failure reported "line 16 column 483"
/// while showing line 2, so the only way to see the malformed character was to
/// reproduce the run.
fn json_error_context(json: &str, err: &serde_json::Error) -> String {
    /// Characters of the offending line to show either side of the column.
    const WINDOW: usize = 90;

    let (line_no, column) = (err.line(), err.column());
    // Line 0 means the error carries no position (an I/O or data error, not syntax).
    let Some(line) = line_no.checked_sub(1).and_then(|i| json.lines().nth(i)) else {
        return format!("near: {}", clip(json, 200));
    };
    // `column` is 1-based and counts **bytes** since the newline, not characters,
    // and can point one past the end of the line (EOF). Convert to a char index
    // before indexing chars: review prose is full of em dashes and curly quotes,
    // and using the byte offset directly slides the window right by one per
    // multi-byte character — off the very spot this exists to show.
    let at_byte = column.saturating_sub(1).min(line.len());
    let at_byte = (0..=at_byte)
        .rev()
        .find(|&i| line.is_char_boundary(i))
        .unwrap_or(0);
    let at = line[..at_byte].chars().count();

    let chars: Vec<char> = line.chars().collect();
    let (start, end) = (at.saturating_sub(WINDOW), (at + WINDOW).min(chars.len()));
    let window: String = chars[start..end].iter().collect();
    let lead = if start > 0 { "…" } else { "" };
    let tail = if end < chars.len() { "…" } else { "" };
    format!("at line {line_no} column {column}: {lead}{window}{tail}")
}

/// Parse a review, salvaging it with one model-driven repair pass when the JSON
/// does not parse.
///
/// `repair` is supplied by the caller because the two call sites reach a model
/// differently — [`review_diff`] holds an HTTP client, [`review_file`] holds a
/// [`ReviewBackend`]. Bounded to **one** attempt; if the repair also fails, the
/// **original** error is reported, not the repair's.
///
/// Deliberately not a hand-written sanitizer: deciding whether a bare `"` closes a
/// string or belongs to the prose needs the surrounding meaning, and a heuristic
/// that guesses wrong silently mangles review text a human is about to read. A
/// visible failure beats a quiet corruption.
///
/// Truncation is detected separately and marked in the review's own `summary`, so
/// a partial review cannot be mistaken for a complete one.
///
/// Returns the repair's own usage, `None` when the JSON parsed first time.
///
/// [`ReviewBackend`]: crate::backend::ReviewBackend
pub(crate) async fn parse_review_with_repair<F, Fut>(
    json: &str,
    what: &str,
    repair: F,
) -> Result<(Review, Option<Usage>)>
where
    F: FnOnce(&'static str, String) -> Fut,
    Fut: std::future::Future<Output = Result<Completion>>,
{
    let err = match serde_json::from_str::<Review>(json) {
        Ok(review) => return Ok((review, None)),
        Err(e) => e,
    };
    tracing::warn!(
        "{what}: review JSON did not parse ({err}) {}; asking the model to repair it",
        json_error_context(json, &err)
    );

    let repaired = repair(JSON_REPAIR_SYSTEM, json.to_string())
        .await
        .with_context(|| {
            format!(
                "could not parse review JSON ({err}) {}; the repair pass could not run",
                json_error_context(json, &err)
            )
        })?;
    let repaired_json = extract_json(&repaired.text).ok_or_else(|| {
        anyhow::anyhow!(
            "could not parse review JSON ({err}) {}; the repair pass returned no JSON object: {}",
            json_error_context(json, &err),
            clip(&repaired.text, 300)
        )
    })?;
    let mut review: Review = serde_json::from_str(repaired_json).map_err(|repair_err| {
        anyhow::anyhow!(
            "could not parse review JSON ({err}) {}; the repair pass also failed ({repair_err}) {}",
            json_error_context(json, &err),
            json_error_context(repaired_json, &repair_err)
        )
    })?;

    let spent = repaired.usage.as_ref().and_then(|u| u.total_tokens);
    let on = repaired.model.as_deref().unwrap_or("an unreported model");
    if is_truncation(&err) {
        tracing::warn!(
            "{what}: review JSON was TRUNCATED, not just malformed — salvaged {} finding(s) \
             from what arrived, anything after the cut is lost ({spent:?} extra token(s) on {on})",
            review.findings.len(),
        );
        review.summary.push_str(TRUNCATED_NOTE);
    } else {
        tracing::info!("{what}: repaired the review JSON using {spent:?} extra token(s) on {on}");
        // Nothing *enforces* the repair prompt's "preserve everything exactly" — it
        // is a second model asked to be faithful, and one that quietly dropped a
        // finding would look identical to a clean repair. Leave a trace. Only off
        // the truncation path, where a smaller count is the expected outcome.
        let (before, after) = (count_findings_hint(json), review.findings.len());
        if before != after {
            tracing::warn!(
                "{what}: the repair pass changed the finding count — {before} before, {after} \
                 after. Two things do this: the repair model dropped or invented one despite \
                 being told to preserve content, or lenient_findings discarded one that would \
                 not deserialize. The before-count is a heuristic over unparseable text, so \
                 treat a small divergence as a prompt to go read the review, not as proof."
            );
        }
    }
    Ok((review, repaired.usage))
}

#[cfg(test)]
mod salvage_tests {
    use super::*;

    /// The shape a reviewer actually emits: pretty-printed, one field per line,
    /// with the long prose in `summary` (line 2) and each finding's `body`.
    fn review_json(body: &str) -> String {
        format!(
            r#"{{
  "summary": "{}",
  "recommendation": "APPROVE WITH CHANGES",
  "findings": [
    {{
      "severity": "HIGH",
      "file": "scripts/local-db.sh",
      "line": 12,
      "body": "{body}",
      "confidence": 85
    }}
  ]
}}"#,
            "This PR repairs the local dev tooling. ".repeat(12)
        )
    }

    fn usage(prompt: Option<u32>, completion: Option<u32>, total: Option<u32>) -> Usage {
        Usage {
            prompt_tokens: prompt,
            completion_tokens: completion,
            total_tokens: total,
        }
    }

    /// What a repair pass would have returned. Paired with an inline
    /// `|_, _| async move { Ok(completion(...)) }` closure, this drives the whole
    /// salvage path without a model — the thing that was untestable while the
    /// salvage lived in a backend, where repairing meant spawning a real CLI.
    fn completion(text: &str, u: Option<Usage>) -> Completion {
        Completion {
            text: text.to_string(),
            model: Some("test-model".into()),
            usage: u,
        }
    }

    #[tokio::test]
    async fn valid_json_never_reaches_the_repair_pass() {
        let good = review_json("Quotes the URI. Fix: quote it.");
        let (review, repair_usage) = parse_review_with_repair(&good, "t", |_, _| async {
            panic!("repair must not run when the JSON already parses")
        })
        .await
        .expect("parses");
        assert_eq!(review.findings.len(), 1);
        assert!(
            repair_usage.is_none(),
            "nothing was repaired, nothing billed"
        );
    }

    /// The failure this exists for: one unescaped `"` used to discard the review.
    #[tokio::test]
    async fn a_stray_quote_is_repaired_rather_than_discarded() {
        let broken = review_json(r#"passes --uri="$REMOTE" unquoted. Fix: quote it."#);
        assert!(
            serde_json::from_str::<Review>(&broken).is_err(),
            "precondition"
        );
        let fixed = review_json(r#"passes --uri=\"$REMOTE\" unquoted. Fix: quote it."#);

        let (review, repair_usage) = parse_review_with_repair(&broken, "t", |_s, _b| async move {
            Ok(completion(
                &fixed,
                Some(usage(Some(300), Some(80), Some(380))),
            ))
        })
        .await
        .expect("salvaged");

        assert_eq!(review.findings.len(), 1);
        assert!(review.findings[0].body.contains("$REMOTE"));
        // Not truncation, so the review must NOT be stamped incomplete.
        assert!(
            !review.summary.contains("incomplete"),
            "a stray quote is not a cut-off response"
        );
        assert_eq!(repair_usage.and_then(|u| u.total_tokens), Some(380));
    }

    /// The one that must not regress: a truncated review is salvageable but
    /// **lossy**, and has to say so where the merge decision is made.
    #[tokio::test]
    async fn a_truncated_review_is_marked_incomplete_in_its_summary() {
        let cut = r#"{"summary": "half a summ"#;
        let err = serde_json::from_str::<Review>(cut).expect_err("truncated");
        assert!(is_truncation(&err), "precondition: reads as truncation");

        let repaired = review_json("fine");
        let (review, _) =
            parse_review_with_repair(
                cut,
                "t",
                |_s, _b| async move { Ok(completion(&repaired, None)) },
            )
            .await
            .expect("salvaged");
        assert!(review.summary.contains("incomplete"));
        assert!(
            review.summary.contains("/review"),
            "says how to get a full pass"
        );
    }

    /// One attempt only, and the error a human sees is the **original** parse
    /// failure — the repair's own error is context, not the headline.
    #[tokio::test]
    async fn a_failed_repair_reports_the_original_error() {
        let broken = review_json(r#"bad " quote"#);
        let err = parse_review_with_repair(&broken, "t", |_s, _b| async move {
            Ok(completion("still {not] json", None))
        })
        .await
        .expect_err("repair could not fix it");
        let msg = format!("{err:#}");
        assert!(msg.contains("could not parse review JSON"), "{msg}");
        assert!(msg.contains("the repair pass"), "{msg}");
    }

    #[tokio::test]
    async fn a_repair_that_returns_no_json_is_reported_as_such() {
        let broken = review_json(r#"bad " quote"#);
        let err = parse_review_with_repair(&broken, "t", |_s, _b| async move {
            Ok(completion("I'm sorry, I can't.", None))
        })
        .await
        .expect_err("no JSON object");
        assert!(format!("{err:#}").contains("returned no JSON object"));
    }

    /// serde_json reports `column()` in **bytes** since the newline, so using it as
    /// a char index drifts right by one per multi-byte character — and review prose
    /// is full of em dashes and curly quotes. Filler on *both* sides makes the drift
    /// visible; filler on one side would still overlap the marker by accident.
    #[test]
    fn a_multibyte_line_windows_on_the_break_not_on_a_byte_offset() {
        let body = format!("{}NEEDLE bad \" quote{}", "é".repeat(200), "x".repeat(200));
        let json = review_json(&body);
        let err = serde_json::from_str::<Review>(&json).expect_err("unescaped quote");
        let ctx = json_error_context(&json, &err);
        assert!(
            ctx.contains("NEEDLE"),
            "window drifted off the break: {ctx}"
        );
    }

    /// The window must land on the break, not on the summary that opens the object.
    #[test]
    fn the_context_quotes_the_offending_spot_not_the_summary() {
        let json = review_json(r#"The mongodump call passes --uri="$REMOTE" unquoted."#);
        let err = serde_json::from_str::<Review>(&json).expect_err("unescaped quote");
        let ctx = json_error_context(&json, &err);
        assert!(ctx.contains("mongodump"), "{ctx}");
        assert!(!ctx.contains("This PR repairs"), "{ctx}");
    }

    /// A truncated object errors at the very end of the line, so the column can
    /// point one past the final character — slicing there must not panic.
    #[test]
    fn a_column_past_the_end_of_the_line_is_clamped() {
        let json = "{\n  \"summary\": \"cut off";
        let err = serde_json::from_str::<Review>(json).expect_err("truncated");
        assert!(json_error_context(json, &err).contains("cut off"));
    }

    /// Errors raised away from the parser carry no position at all — line 0, which
    /// is not a line. Fall back to the head rather than indexing it.
    #[test]
    fn an_error_without_a_position_falls_back_to_the_head() {
        let err = serde_json::from_value::<Review>(serde_json::json!([])).expect_err("wrong type");
        assert_eq!(err.line(), 0, "precondition: a positionless error");
        let ctx = json_error_context("{\"summary\": \"x\"}", &err);
        assert!(ctx.starts_with("near: "), "{ctx}");
    }

    /// Getting this backwards stamps "incomplete" on whole reviews that are fine.
    #[test]
    fn a_cut_off_response_is_told_apart_from_a_stray_character() {
        for cut in [
            r#"{"summary": "half a sum"#,
            r#"{"summary": "s", "recommendation": "APPROVE","#,
            r#"{"summary": "s", "recommendation": "A", "findings": [{"severity": "LOW""#,
        ] {
            let err = serde_json::from_str::<Review>(cut).expect_err("truncated");
            assert!(is_truncation(&err), "should read as truncated: {cut}");
        }
        let err = serde_json::from_str::<Review>(&review_json(r#"bad " quote"#))
            .expect_err("unescaped quote");
        assert!(
            !is_truncation(&err),
            "a stray quote is not truncation — it would wrongly flag a review incomplete"
        );
    }

    #[test]
    fn findings_are_counted_from_text_that_does_not_parse() {
        let broken = review_json(r#"bad " quote"#);
        assert!(
            serde_json::from_str::<Review>(&broken).is_err(),
            "precondition"
        );
        assert_eq!(count_findings_hint(&broken), 1);
        assert_eq!(count_findings_hint(r#"{"findings": []}"#), 0);
    }

    /// Pin the known limitation rather than let a later reader trust the number. An
    /// unescaped mention of the key inflates it — and an unescaped quote is exactly
    /// the breakage the repair exists for, so the two co-occur when it matters.
    /// This is why divergence only ever warns.
    #[test]
    fn the_finding_count_is_a_heuristic_not_a_measurement() {
        let self_referential = review_json(r#"the "severity" field is unvalidated"#);
        assert!(serde_json::from_str::<Review>(&self_referential).is_err());
        assert_eq!(
            count_findings_hint(&self_referential),
            2,
            "one real finding, miscounted as two — must never be able to fail a review"
        );
        assert_eq!(
            count_findings_hint(&review_json(r#"the \"severity\" field is unvalidated"#)),
            1
        );
    }

    #[test]
    fn repair_tokens_are_added_to_the_reviews() {
        let sum = add_usage(
            Some(usage(Some(1000), Some(200), Some(1200))),
            Some(usage(Some(300), Some(80), Some(380))),
        )
        .expect("both present");
        assert_eq!(sum.prompt_tokens, Some(1300));
        assert_eq!(sum.completion_tokens, Some(280));
        assert_eq!(sum.total_tokens, Some(1580));
    }

    /// Counts are independently optional. Absent means "not reported", not zero.
    #[test]
    fn a_missing_count_on_one_side_keeps_the_other() {
        let sum = add_usage(
            Some(usage(Some(1000), None, None)),
            Some(usage(None, Some(80), Some(380))),
        )
        .expect("both present");
        assert_eq!(sum.prompt_tokens, Some(1000));
        assert_eq!(sum.completion_tokens, Some(80));
        assert_eq!(sum.total_tokens, Some(380));

        // Absent on *both* sides stays absent. Without this the assertions above all
        // hold against a naive `a.unwrap_or(0) + b.unwrap_or(0)`, which reports zero
        // tokens where none were reported at all — every field above has a figure on
        // at least one side, so none can tell the two apart.
        let neither = add_usage(
            Some(usage(Some(1000), None, None)),
            Some(usage(None, Some(80), None)),
        )
        .expect("both present");
        assert_eq!(neither.total_tokens, None);
        assert!(add_usage(None, None).is_none(), "nothing reported at all");
    }
}
