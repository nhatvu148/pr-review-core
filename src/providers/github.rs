//! GitHub provider — REST API with a personal access token (`GH_TOKEN`).
//! Posts a summary issue comment (deduped/updated by marker) plus inline review
//! comments (the bot's prior inline comments are deleted and reposted each run).

use anyhow::Result;
use reqwest::Client;
use serde::Deserialize;

use super::{is_bot_comment, InlineComment, PrMeta, ReviewPost};
use crate::clip;
use crate::config::{require, Config};

fn pr_url(cfg: &Config, repo: &str, pr: u64) -> String {
    format!("{}/repos/{repo}/pulls/{pr}", cfg.github_api_base)
}

/// Apply the common GitHub JSON headers + auth to a request builder.
fn gh(rb: reqwest::RequestBuilder, cfg: &Config) -> reqwest::RequestBuilder {
    rb.bearer_auth(&cfg.github_token)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("User-Agent", &cfg.user_agent)
}

pub async fn get_diff(client: &Client, cfg: &Config, repo: &str, pr: u64) -> Result<String> {
    require(&cfg.github_token, "GH_TOKEN")?;
    let res = client
        .get(pr_url(cfg, repo, pr))
        .bearer_auth(&cfg.github_token)
        .header("Accept", "application/vnd.github.diff")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("User-Agent", &cfg.user_agent)
        .send()
        .await?;
    if !res.status().is_success() {
        let status = res.status();
        anyhow::bail!(
            "GitHub getDiff {status}: {}",
            clip(&res.text().await.unwrap_or_default(), 300)
        );
    }
    Ok(res.text().await?)
}

pub async fn get_meta(client: &Client, cfg: &Config, repo: &str, pr: u64) -> Result<PrMeta> {
    require(&cfg.github_token, "GH_TOKEN")?;

    #[derive(Deserialize)]
    struct Ref {
        #[serde(rename = "ref")]
        ref_: Option<String>,
        sha: Option<String>,
    }
    #[derive(Deserialize)]
    struct Pr {
        title: Option<String>,
        base: Option<Ref>,
        head: Option<Ref>,
    }

    let res = gh(client.get(pr_url(cfg, repo, pr)), cfg)
        .send()
        .await?;
    if !res.status().is_success() {
        let status = res.status();
        anyhow::bail!(
            "GitHub getMeta {status}: {}",
            clip(&res.text().await.unwrap_or_default(), 300)
        );
    }
    let pr_data: Pr = res.json().await?;
    Ok(PrMeta {
        repo: repo.to_string(),
        pr,
        title: pr_data.title,
        base_branch: pr_data.base.and_then(|b| b.ref_),
        head_sha: pr_data.head.and_then(|h| h.sha),
    })
}

/// Authenticated HTTPS clone URL (token as `x-access-token`).
pub fn clone_url(cfg: &Config, repo: &str) -> Result<String> {
    require(&cfg.github_token, "GH_TOKEN")?;
    Ok(format!(
        "https://x-access-token:{}@github.com/{repo}.git",
        cfg.github_token
    ))
}

// ── summary issue comment (upsert by marker) ────────────────────────────────

async fn find_summary_comment(
    client: &Client,
    cfg: &Config,
    repo: &str,
    pr: u64,
) -> Result<Option<u64>> {
    #[derive(Deserialize)]
    struct C {
        id: u64,
        body: Option<String>,
    }
    let mut page = 1u32;
    loop {
        let url = format!(
            "{}/repos/{repo}/issues/{pr}/comments?per_page=100&page={page}",
            cfg.github_api_base
        );
        let res = gh(client.get(url), cfg).send().await?;
        if !res.status().is_success() {
            let status = res.status();
            anyhow::bail!(
                "GitHub listComments {status}: {}",
                clip(&res.text().await.unwrap_or_default(), 300)
            );
        }
        let comments: Vec<C> = res.json().await?;
        let n = comments.len();
        if let Some(c) = comments
            .into_iter()
            .find(|c| c.body.as_deref().is_some_and(|b| is_bot_comment(cfg, b)))
        {
            return Ok(Some(c.id));
        }
        if n < 100 {
            return Ok(None);
        }
        page += 1;
    }
}

async fn upsert_summary(
    client: &Client,
    cfg: &Config,
    repo: &str,
    pr: u64,
    body: &str,
) -> Result<Option<String>> {
    #[derive(Deserialize)]
    struct Created {
        html_url: Option<String>,
    }
    let marked = format!("{body}\n\n_{}_", cfg.comment_marker);
    let (req, action) = match find_summary_comment(client, cfg, repo, pr).await? {
        Some(id) => (
            client.patch(format!(
                "{}/repos/{repo}/issues/comments/{id}",
                cfg.github_api_base
            )),
            "updateComment",
        ),
        None => (
            client.post(format!(
                "{}/repos/{repo}/issues/{pr}/comments",
                cfg.github_api_base
            )),
            "postComment",
        ),
    };
    let res = gh(req, cfg)
        .json(&serde_json::json!({ "body": marked }))
        .send()
        .await?;
    if !res.status().is_success() {
        let status = res.status();
        anyhow::bail!(
            "GitHub {action} {status}: {}",
            clip(&res.text().await.unwrap_or_default(), 300)
        );
    }
    let c: Created = res.json().await?;
    Ok(c.html_url)
}

// ── inline review comments (delete prior, post new) ─────────────────────────

async fn delete_prior_inline(client: &Client, cfg: &Config, repo: &str, pr: u64) -> Result<()> {
    #[derive(Deserialize)]
    struct C {
        id: u64,
        body: Option<String>,
    }
    let mut page = 1u32;
    let mut ids: Vec<u64> = Vec::new();
    loop {
        let url = format!(
            "{}/repos/{repo}/pulls/{pr}/comments?per_page=100&page={page}",
            cfg.github_api_base
        );
        let res = gh(client.get(url), cfg).send().await?;
        if !res.status().is_success() {
            let status = res.status();
            anyhow::bail!(
                "GitHub listReviewComments {status}: {}",
                clip(&res.text().await.unwrap_or_default(), 300)
            );
        }
        let comments: Vec<C> = res.json().await?;
        let n = comments.len();
        for c in comments {
            if c.body.as_deref().is_some_and(|b| is_bot_comment(cfg, b)) {
                ids.push(c.id);
            }
        }
        if n < 100 {
            break;
        }
        page += 1;
    }
    for id in ids {
        let url = format!("{}/repos/{repo}/pulls/comments/{id}", cfg.github_api_base);
        let _ = gh(client.delete(url), cfg).send().await; // best-effort
    }
    Ok(())
}

async fn post_inline(
    client: &Client,
    cfg: &Config,
    repo: &str,
    pr: u64,
    commit_id: &str,
    c: &InlineComment,
) -> Result<()> {
    let url = format!("{}/repos/{repo}/pulls/{pr}/comments", cfg.github_api_base);
    let body = format!("{}\n\n_{}_", c.body, cfg.comment_marker);
    let res = gh(client.post(url), cfg)
        .json(&serde_json::json!({
            "body": body, "commit_id": commit_id, "path": c.path, "line": c.line, "side": "RIGHT"
        }))
        .send()
        .await?;
    if !res.status().is_success() {
        let status = res.status();
        // Don't abort the whole run on one bad anchor — log and move on.
        tracing::warn!(
            "GitHub inline comment failed ({status}) on {}:{}: {}",
            c.path,
            c.line,
            clip(&res.text().await.unwrap_or_default(), 200)
        );
    }
    Ok(())
}

pub async fn post_review(
    client: &Client,
    cfg: &Config,
    meta: &PrMeta,
    review: &ReviewPost,
) -> Result<Option<String>> {
    require(&cfg.github_token, "GH_TOKEN")?;

    // Inline comments first (delete stale, post fresh) so the summary lands last.
    if !review.inline.is_empty() {
        if let Some(sha) = &meta.head_sha {
            delete_prior_inline(client, cfg, &meta.repo, meta.pr).await?;
            for c in &review.inline {
                post_inline(client, cfg, &meta.repo, meta.pr, sha, c).await?;
            }
        } else {
            tracing::warn!(
                "no head SHA for {}#{}; skipping inline comments",
                meta.repo,
                meta.pr
            );
        }
    }

    upsert_summary(client, cfg, &meta.repo, meta.pr, &review.summary).await
}
