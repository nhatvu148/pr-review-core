//! OpenRouter chat-completions client. Sends the review prompt to a Claude model
//! via OpenRouter and parses the structured review back.

use anyhow::Result;
use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::clip;
use crate::config::{require, Config};
use crate::prompt::{
    build_user_prompt, ASK_SYSTEM_PROMPT, CRITIQUE_SYSTEM_PROMPT, DESCRIBE_SYSTEM_PROMPT,
    SYSTEM_PROMPT,
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

/// One review finding from the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finding {
    pub severity: String,
    pub file: String,
    #[serde(default)]
    pub line: Option<u64>,
    pub body: String,
    /// Model's confidence (0–100) that this is a real, actionable issue a senior
    /// reviewer would flag. Absent on older responses; treated as full confidence.
    #[serde(default)]
    pub confidence: Option<u8>,
}

/// The structured review the model returns.
#[derive(Debug, Clone, Deserialize)]
pub struct Review {
    pub summary: String,
    pub recommendation: String,
    #[serde(default)]
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

    let system_prompt = if cfg.extra_system_prompt.is_empty() {
        SYSTEM_PROMPT.to_string()
    } else {
        format!("{SYSTEM_PROMPT}\n{}", cfg.extra_system_prompt)
    };

    let req = ChatReq {
        model: cfg.openrouter_model.clone(),
        max_tokens: cfg.openrouter_max_tokens,
        temperature: cfg.openrouter_temperature,
        messages: vec![
            Msg {
                role: "system".into(),
                content: system_prompt,
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
    let review: Review = serde_json::from_str(json)
        .map_err(|e| anyhow::anyhow!("Could not parse review JSON ({e}): {}", clip(json, 300)))?;

    Ok(ReviewResult {
        review,
        model: cfg.openrouter_model.clone(),
        usage: data.usage,
    })
}

/// Second-pass "self-critique": ask the model to prune false positives, duplicates,
/// and out-of-scope nits from a set of proposed findings, and to assign an honest
/// confidence to each survivor. Uses the same OpenRouter call pattern as
/// [`review_diff`] (same headers, base URL, and synthesis model).
///
/// The caller MUST treat any error as fail-open (keep the original findings) — a
/// critique hiccup must never drop the review.
///
/// # Errors
/// If `OPENROUTER_API_KEY` is missing, OpenRouter returns an error status, or the
/// response can't be parsed as a JSON array of findings.
pub async fn critique_findings(
    client: &Client,
    cfg: &Config,
    meta: &PrMeta,
    diff: &str,
    findings: &[Finding],
) -> Result<Vec<Finding>> {
    require(&cfg.openrouter_api_key, "OPENROUTER_API_KEY")?;

    let clipped: String = diff.chars().take(cfg.max_diff_chars).collect();
    let findings_json = serde_json::to_string_pretty(findings)
        .map_err(|e| anyhow::anyhow!("could not serialize findings for critique: {e}"))?;
    let user = format!(
        "Repository: {}\nPull request: #{}\n\n--- BEGIN DIFF ---\n{clipped}\n--- END DIFF ---\n\n--- PROPOSED FINDINGS (JSON) ---\n{findings_json}",
        meta.repo, meta.pr,
    );

    let req = ChatReq {
        model: cfg.openrouter_model.clone(),
        max_tokens: cfg.openrouter_max_tokens,
        temperature: cfg.openrouter_temperature,
        messages: vec![
            Msg {
                role: "system".into(),
                content: CRITIQUE_SYSTEM_PROMPT.to_string(),
            },
            Msg {
                role: "user".into(),
                content: user,
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
        .ok_or_else(|| anyhow::anyhow!("OpenRouter returned an empty critique response."))?;

    let json = extract_json_array(&content).ok_or_else(|| {
        anyhow::anyhow!(
            "Critique did not return a JSON array: {}",
            clip(&content, 300)
        )
    })?;
    let kept: Vec<Finding> = serde_json::from_str(json)
        .map_err(|e| anyhow::anyhow!("Could not parse critique JSON ({e}): {}", clip(json, 300)))?;

    Ok(kept)
}

/// One-shot chat completion returning the raw assistant text. Shares the same
/// OpenRouter call pattern (headers, base URL, synthesis model) as [`review_diff`].
///
/// # Errors
/// If `OPENROUTER_API_KEY` is missing, OpenRouter returns an error status, or the
/// response has no content.
pub(crate) async fn chat_text(
    client: &Client,
    cfg: &Config,
    system: &str,
    user: &str,
) -> Result<String> {
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

    data.choices
        .and_then(|c| c.into_iter().next())
        .and_then(|c| c.message)
        .and_then(|m| m.content)
        .map(|c| c.trim().to_string())
        .filter(|c| !c.is_empty())
        .ok_or_else(|| anyhow::anyhow!("OpenRouter returned an empty response."))
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
