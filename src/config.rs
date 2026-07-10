//! Environment configuration, loaded once at process start.
//!
//! Everything has a default except secrets; values that are required only for a
//! specific path (a provider token, the OpenRouter key) are validated at the
//! point of use via [`require`].

use std::env;

/// Resolved runtime configuration.
#[derive(Clone)]
pub struct Config {
    pub port: u16,
    pub worker_token: String,
    /// Shared secret GitHub signs webhook deliveries with (X-Hub-Signature-256).
    pub github_webhook_secret: String,
    /// Shared secret Bitbucket signs webhook deliveries with (X-Hub-Signature).
    pub bitbucket_webhook_secret: String,

    pub openrouter_api_key: String,
    /// Synthesis model: writes the final review findings (quality matters here).
    pub openrouter_model: String,
    /// Exploration model: drives the agentic tool loop (grep/read_file/list_dir)
    /// to gather context. Cheaper — it navigates files, it doesn't judge.
    pub openrouter_model_explore: String,
    pub openrouter_base_url: String,
    pub openrouter_max_tokens: u32,
    pub openrouter_temperature: f32,

    pub max_diff_chars: usize,

    /// Use the agentic reviewer: clone the repo and let the model investigate
    /// cross-file context with tools, instead of a single diff-only call.
    pub agentic: bool,
    /// Max tool-loop turns for the agentic reviewer (cost guard).
    pub max_turns: usize,
    /// Char budget for accumulated tool results in the agent conversation;
    /// older results are elided once newer ones fill it (bounds per-turn tokens).
    pub max_history_chars: usize,

    /// Re-review when new commits are pushed to an open PR (GitHub `synchronize`
    /// / Bitbucket `pullrequest:updated`). Off by default so iterating on a PR
    /// doesn't trigger a fresh (expensive) review per push — `opened`/`reopened`/
    /// `ready_for_review` always review; use the manual `POST /review` endpoint
    /// to re-review on demand.
    pub review_on_update: bool,

    pub github_token: String,
    pub github_api_base: String,

    pub bitbucket_email: String,
    pub bitbucket_token: String,
    pub bitbucket_api_base: String,

    /// Signature appended to every comment the bot posts and used as the dedupe
    /// key to find/update its own comments. Injected so the library carries no
    /// hardcoded bot identity.
    pub comment_marker: String,
    /// `User-Agent` header sent on provider (GitHub) API requests.
    pub user_agent: String,
    /// `HTTP-Referer` header sent to OpenRouter (attribution).
    pub http_referer: String,
    /// `X-Title` header sent to OpenRouter (attribution).
    pub x_title: String,
    /// Extra system-prompt text appended to the built-in prompts. Lets a consumer
    /// inject a large conventions block (e.g. via a file baked into its image)
    /// without changing the library.
    pub extra_system_prompt: String,
}

impl Config {
    /// Build the config from environment variables, applying defaults.
    pub fn from_env() -> Self {
        Self {
            // Local default avoids the common 8080 clash with Docker Desktop.
            // Production pins PORT=8080 via the Dockerfile, matching fly.toml.
            port: env_or("PORT", "8088").parse().unwrap_or(8088),
            worker_token: env::var("WORKER_TOKEN").unwrap_or_default(),
            github_webhook_secret: env::var("GITHUB_WEBHOOK_SECRET").unwrap_or_default(),
            bitbucket_webhook_secret: env::var("BITBUCKET_WEBHOOK_SECRET").unwrap_or_default(),

            openrouter_api_key: env::var("OPENROUTER_API_KEY").unwrap_or_default(),
            openrouter_model: env_or("OPENROUTER_MODEL", "anthropic/claude-sonnet-4.5"),
            openrouter_model_explore: env_or("OPENROUTER_MODEL_EXPLORE", "moonshotai/kimi-k2-0905"),
            openrouter_base_url: env_or("OPENROUTER_BASE_URL", "https://openrouter.ai/api/v1"),
            openrouter_max_tokens: env_or("OPENROUTER_MAX_TOKENS", "4000")
                .parse()
                .unwrap_or(4000),
            openrouter_temperature: env_or("OPENROUTER_TEMPERATURE", "0.2")
                .parse()
                .unwrap_or(0.2),

            max_diff_chars: env_or("MAX_DIFF_CHARS", "200000")
                .parse()
                .unwrap_or(200_000),

            agentic: env_or("AGENTIC", "false").parse().unwrap_or(false),
            max_turns: env_or("MAX_TURNS", "6").parse().unwrap_or(6),
            max_history_chars: env_or("MAX_HISTORY_CHARS", "45000")
                .parse()
                .unwrap_or(45_000),

            review_on_update: env_or("REVIEW_ON_UPDATE", "false").parse().unwrap_or(false),

            github_token: env::var("GH_TOKEN").unwrap_or_default(),
            github_api_base: env_or("GH_API_BASE", "https://api.github.com"),

            bitbucket_email: env::var("BB_EMAIL").unwrap_or_default(),
            bitbucket_token: env::var("BB_API_TOKEN").unwrap_or_default(),
            bitbucket_api_base: "https://api.bitbucket.org/2.0".to_string(),

            comment_marker: env_or("COMMENT_MARKER", "🤖 ai-pr-review"),
            user_agent: env_or("USER_AGENT", "pr-review-core"),
            http_referer: env_or(
                "OPENROUTER_HTTP_REFERER",
                "https://github.com/nhatvu148/pr-review-core",
            ),
            x_title: env_or("OPENROUTER_X_TITLE", "pr-review"),
            extra_system_prompt: resolve_extra_system_prompt(),
        }
    }
}

/// Resolve the extra system prompt: prefer the inline `EXTRA_SYSTEM_PROMPT` env
/// var, fall back to the contents of the file named by `EXTRA_SYSTEM_PROMPT_FILE`,
/// else empty.
fn resolve_extra_system_prompt() -> String {
    match env::var("EXTRA_SYSTEM_PROMPT") {
        Ok(v) if !v.is_empty() => v,
        _ => match env::var("EXTRA_SYSTEM_PROMPT_FILE") {
            Ok(path) if !path.is_empty() => std::fs::read_to_string(path).unwrap_or_default(),
            _ => String::new(),
        },
    }
}

fn env_or(name: &str, default: &str) -> String {
    env::var(name).unwrap_or_else(|_| default.to_string())
}

/// Ensure a required value is present, with a clear error naming the env var.
///
/// # Examples
/// ```
/// # use pr_review_core::config::require;
/// assert!(require("", "OPENROUTER_API_KEY").is_err());
/// assert!(require("sk-or-x", "OPENROUTER_API_KEY").is_ok());
/// ```
pub fn require(value: &str, name: &str) -> anyhow::Result<()> {
    if value.is_empty() {
        anyhow::bail!("Missing required env var: {name}");
    }
    Ok(())
}
