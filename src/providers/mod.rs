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

// ── review-lifecycle reconciliation (fingerprints) ──────────────────────────

/// A stable-ish fingerprint of a finding, from its file path + body text
/// (normalized to alphanumerics + lowercase to absorb minor rephrasing and the
/// severity decoration). Embedded as a hidden marker on each inline comment so a
/// later review can tell which findings are the **same** (leave in place), which
/// are **new** (post), and which are **gone** (resolve the thread).
///
/// Best-effort: an LLM that heavily rewords the same finding may read as a new
/// one (old resolved + new posted). Genuinely-fixed findings resolve cleanly.
pub(crate) fn finding_fingerprint(path: &str, body: &str) -> String {
    use sha2::{Digest, Sha256};
    fn norm(s: &str) -> String {
        s.chars()
            .filter(|c| c.is_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect()
    }
    let mut h = Sha256::new();
    h.update(norm(path).as_bytes());
    h.update(b"|");
    h.update(norm(body).as_bytes());
    hex::encode(&h.finalize()[..6]) // 12 hex chars — plenty for per-PR uniqueness
}

/// The hidden marker embedded in a posted comment to carry its fingerprint.
/// GitHub/GitLab hide HTML comments; Bitbucket renders them literally, so the
/// reconcile flow is GitHub/GitLab-only for now.
pub(crate) fn fp_marker(fp: &str) -> String {
    format!("<!-- prbot-fp:{fp} -->")
}

/// Extract the fingerprint from a comment body previously posted by the bot.
pub(crate) fn extract_fp(body: &str) -> Option<String> {
    const PREFIX: &str = "<!-- prbot-fp:";
    let start = body.find(PREFIX)? + PREFIX.len();
    let rest = &body[start..];
    let end = rest.find("-->")?;
    let fp = rest[..end].trim();
    (!fp.is_empty()).then(|| fp.to_string())
}

/// Render a "Resolved since last review" summary section for findings that were
/// flagged in a prior review and are no longer present. Empty string if none.
pub(crate) fn render_resolved(resolved: &[String]) -> String {
    if resolved.is_empty() {
        return String::new();
    }
    let mut s = format!(
        "\n\n## ✅ Resolved since last review\n\n_{} previously-flagged finding(s) no longer present:_\n",
        resolved.len()
    );
    for r in resolved {
        s.push_str(&format!("- {r}\n"));
    }
    s
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

#[cfg(test)]
mod tests {
    use super::{extract_fp, finding_fingerprint, fp_marker, render_resolved};

    #[test]
    fn fingerprint_is_stable_and_normalizes() {
        // Same finding, minor formatting differences → same fingerprint.
        let a = finding_fingerprint("src/lib.rs", "Missing null check. Fix: guard it.");
        let b = finding_fingerprint("src/lib.rs", "missing null   check.  fix: guard it.");
        assert_eq!(a, b);
        // Different file or body → different fingerprint.
        assert_ne!(a, finding_fingerprint("src/other.rs", "Missing null check. Fix: guard it."));
        assert_ne!(a, finding_fingerprint("src/lib.rs", "A totally different problem."));
        assert_eq!(a.len(), 12); // 6 bytes hex-encoded
    }

    #[test]
    fn fp_marker_round_trips() {
        let fp = finding_fingerprint("a.rs", "some finding");
        let body = format!("⚠️ **HIGH** — some finding\n\n_bot_\n{}", fp_marker(&fp));
        assert_eq!(extract_fp(&body).as_deref(), Some(fp.as_str()));
    }

    #[test]
    fn extract_fp_absent() {
        assert_eq!(extract_fp("a plain comment with no marker"), None);
    }

    #[test]
    fn render_resolved_empty_and_nonempty() {
        assert_eq!(render_resolved(&[]), "");
        let s = render_resolved(&["`src/a.rs`".to_string(), "`src/b.rs`".to_string()]);
        assert!(s.contains("Resolved since last review"));
        assert!(s.contains("`src/a.rs`") && s.contains("`src/b.rs`"));
        assert!(s.contains("2 previously-flagged"));
    }
}
