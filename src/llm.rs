//! OpenRouter chat-completions client. Sends the review prompt to a Claude model
//! via OpenRouter and parses the structured review back.

use anyhow::Result;
use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::clip;
use crate::config::{require, Config};
use crate::prompt::{build_user_prompt, SYSTEM_PROMPT};
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
#[derive(Debug, Clone, Deserialize)]
pub struct Finding {
    pub severity: String,
    pub file: String,
    #[serde(default)]
    pub line: Option<u64>,
    pub body: String,
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
pub struct ReviewResult {
    pub review: Review,
    pub model: String,
    pub usage: Option<Usage>,
}

/// Pull the JSON object out of a model response that may be wrapped in prose or
/// ```json fences — take the first `{` through the last `}`.
pub(crate) fn extract_json(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if end > start {
        Some(&text[start..=end])
    } else {
        None
    }
}

/// Clamp the diff to the size cap, call OpenRouter, parse the structured review.
///
/// # Errors
/// If `OPENROUTER_API_KEY` is missing, OpenRouter returns an error status, or the
/// response can't be parsed as the expected review JSON.
pub async fn review_diff(
    client: &Client,
    cfg: &Config,
    meta: &PrMeta,
    diff: &str,
) -> Result<ReviewResult> {
    require(&cfg.openrouter_api_key, "OPENROUTER_API_KEY")?;

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
                content: build_user_prompt(meta, &clipped, truncated),
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
