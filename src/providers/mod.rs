//! Provider abstraction so the same review flow works against GitHub or
//! Bitbucket. Enum dispatch (rather than `dyn Trait`) keeps the async methods
//! object-safe without an `async_trait` dependency.

use anyhow::Result;
use reqwest::Client;

use crate::config::Config;

mod bitbucket;
mod github;
mod gitlab;
mod types;

pub use types::{InlineComment, PrMeta, ProviderName, ReviewPost};

/// Whether a comment body was authored by this bot — matches the configured
/// comment marker (the signature appended to every comment the bot posts).
///
/// The marker is rendered as a small italic footer (see callers) rather than a
/// hidden HTML comment: GitHub hides HTML comments, but Bitbucket shows them as
/// literal `<!-- ... -->` text, which looked like junk. A visible attribution
/// line reads as intentional and still serves as the dedupe key.
pub fn is_bot_comment(cfg: &crate::config::Config, body: &str) -> bool {
    body.contains(&cfg.comment_marker)
}

/// Which host the PR lives on.
#[derive(Clone, Copy)]
pub enum Provider {
    Github,
    Bitbucket,
    Gitlab,
}

impl Provider {
    /// Resolve a provider from its name.
    pub fn from_name(name: &str) -> Result<Self> {
        match name {
            "github" => Ok(Provider::Github),
            "bitbucket" => Ok(Provider::Bitbucket),
            "gitlab" => Ok(Provider::Gitlab),
            other => anyhow::bail!(
                "Unknown provider \"{other}\" (expected: github | bitbucket | gitlab)"
            ),
        }
    }

    pub fn name(&self) -> ProviderName {
        match self {
            Provider::Github => "github",
            Provider::Bitbucket => "bitbucket",
            Provider::Gitlab => "gitlab",
        }
    }

    /// Fetch the unified diff text for the PR.
    pub async fn get_diff(
        &self,
        client: &Client,
        cfg: &Config,
        repo: &str,
        pr: u64,
    ) -> Result<String> {
        match self {
            Provider::Github => github::get_diff(client, cfg, repo, pr).await,
            Provider::Bitbucket => bitbucket::get_diff(client, cfg, repo, pr).await,
            Provider::Gitlab => gitlab::get_diff(client, cfg, repo, pr).await,
        }
    }

    /// Fetch lightweight PR metadata (title, base branch, head SHA).
    pub async fn get_meta(
        &self,
        client: &Client,
        cfg: &Config,
        repo: &str,
        pr: u64,
    ) -> Result<PrMeta> {
        match self {
            Provider::Github => github::get_meta(client, cfg, repo, pr).await,
            Provider::Bitbucket => bitbucket::get_meta(client, cfg, repo, pr).await,
            Provider::Gitlab => gitlab::get_meta(client, cfg, repo, pr).await,
        }
    }

    /// Fetch a repo file's text at a git ref. `Ok(None)` if the file doesn't
    /// exist (404). Used to load an optional per-repo `.prbot.toml`.
    pub async fn get_file_contents(
        &self,
        client: &Client,
        cfg: &Config,
        repo: &str,
        r#ref: &str,
        path: &str,
    ) -> Result<Option<String>> {
        match self {
            Provider::Github => github::get_file_contents(client, cfg, repo, r#ref, path).await,
            Provider::Bitbucket => {
                bitbucket::get_file_contents(client, cfg, repo, r#ref, path).await
            }
            Provider::Gitlab => gitlab::get_file_contents(client, cfg, repo, r#ref, path).await,
        }
    }

    /// Build an authenticated HTTPS clone URL for the agentic reviewer.
    pub fn clone_url(&self, cfg: &Config, repo: &str) -> Result<String> {
        match self {
            Provider::Github => github::clone_url(cfg, repo),
            Provider::Bitbucket => bitbucket::clone_url(cfg, repo),
            Provider::Gitlab => gitlab::clone_url(cfg, repo),
        }
    }

    /// Post a standalone comment on the PR (NOT deduped) — used for `/ask`
    /// answers and `/describe` confirmations. Returns the new comment's URL.
    pub async fn post_comment(
        &self,
        client: &Client,
        cfg: &Config,
        repo: &str,
        pr: u64,
        body: &str,
    ) -> Result<Option<String>> {
        match self {
            Provider::Github => github::post_comment(client, cfg, repo, pr, body).await,
            Provider::Bitbucket => bitbucket::post_comment(client, cfg, repo, pr, body).await,
            Provider::Gitlab => gitlab::post_comment(client, cfg, repo, pr, body).await,
        }
    }

    /// Replace the PR/MR description body (the `/describe` command). `meta`
    /// supplies the title Bitbucket requires alongside the description.
    pub async fn update_pr_description(
        &self,
        client: &Client,
        cfg: &Config,
        meta: &PrMeta,
        description: &str,
    ) -> Result<()> {
        match self {
            Provider::Github => {
                github::update_pr_description(client, cfg, &meta.repo, meta.pr, description).await
            }
            Provider::Bitbucket => {
                bitbucket::update_pr_description(client, cfg, meta, description).await
            }
            Provider::Gitlab => {
                gitlab::update_pr_description(client, cfg, &meta.repo, meta.pr, description).await
            }
        }
    }

    /// Post the review: upsert the summary comment, and refresh the inline
    /// comments (delete the bot's prior inline comments, post the new set).
    /// Returns the summary comment URL when available.
    pub async fn post_review(
        &self,
        client: &Client,
        cfg: &Config,
        meta: &PrMeta,
        review: &ReviewPost,
    ) -> Result<Option<String>> {
        match self {
            Provider::Github => github::post_review(client, cfg, meta, review).await,
            Provider::Bitbucket => bitbucket::post_review(client, cfg, meta, review).await,
            Provider::Gitlab => gitlab::post_review(client, cfg, meta, review).await,
        }
    }
}
