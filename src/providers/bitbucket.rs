//! Bitbucket Cloud provider — Basic auth with an Atlassian API token.
//! Posts a summary comment (deduped/updated by marker) plus inline comments
//! (the bot's prior inline comments are deleted and reposted each run).
//!
//! `BB_API_TOKEN` scopes: `read:repository:bitbucket` (to fetch the diff) +
//! `read:pullrequest:bitbucket` + `write:pullrequest:bitbucket`.

use anyhow::Result;
use base64::Engine;
use reqwest::Client;
use serde::Deserialize;

use super::{is_bot_comment, InlineComment, PrMeta, ReviewPost};
use crate::clip;
use crate::config::{require, Config};

fn auth_header(cfg: &Config) -> Result<String> {
    require(&cfg.bitbucket_email, "BB_EMAIL")?;
    require(&cfg.bitbucket_token, "BB_API_TOKEN")?;
    let raw = format!("{}:{}", cfg.bitbucket_email, cfg.bitbucket_token);
    Ok(format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(raw)
    ))
}

fn pr_base(cfg: &Config, repo: &str, pr: u64) -> String {
    format!(
        "{}/repositories/{repo}/pullrequests/{pr}",
        cfg.bitbucket_api_base
    )
}

pub async fn get_diff(client: &Client, cfg: &Config, repo: &str, pr: u64) -> Result<String> {
    let res = client
        .get(format!("{}/diff", pr_base(cfg, repo, pr)))
        .header(reqwest::header::AUTHORIZATION, auth_header(cfg)?)
        .send()
        .await?;
    if !res.status().is_success() {
        let status = res.status();
        anyhow::bail!(
            "Bitbucket getDiff {status}: {}",
            clip(&res.text().await.unwrap_or_default(), 300)
        );
    }
    Ok(res.text().await?)
}

pub async fn get_meta(client: &Client, cfg: &Config, repo: &str, pr: u64) -> Result<PrMeta> {
    #[derive(Deserialize)]
    struct Branch {
        name: Option<String>,
    }
    #[derive(Deserialize)]
    struct Destination {
        branch: Option<Branch>,
    }
    #[derive(Deserialize)]
    struct Pr {
        title: Option<String>,
        destination: Option<Destination>,
    }

    let res = client
        .get(pr_base(cfg, repo, pr))
        .header(reqwest::header::AUTHORIZATION, auth_header(cfg)?)
        .header("Accept", "application/json")
        .send()
        .await?;
    if !res.status().is_success() {
        let status = res.status();
        anyhow::bail!(
            "Bitbucket getMeta {status}: {}",
            clip(&res.text().await.unwrap_or_default(), 300)
        );
    }
    let pr_data: Pr = res.json().await?;
    Ok(PrMeta {
        repo: repo.to_string(),
        pr,
        title: pr_data.title,
        base_branch: pr_data
            .destination
            .and_then(|d| d.branch)
            .and_then(|b| b.name),
        head_sha: None, // Bitbucket inline comments don't need a commit id
    })
}

/// Fetch a repo file's text at a git ref via the `src` endpoint.
///
/// Returns `Ok(None)` when the file doesn't exist (404) so the caller can treat
/// a missing `.prbot.toml` as "no overrides" rather than an error.
///
/// # Errors
/// If credentials are missing or the request fails with a non-404 error status.
pub async fn get_file_contents(
    client: &Client,
    cfg: &Config,
    repo: &str,
    r#ref: &str,
    path: &str,
) -> Result<Option<String>> {
    let git_ref = r#ref;
    let res = client
        .get(format!(
            "{}/repositories/{repo}/src/{git_ref}/{path}",
            cfg.bitbucket_api_base
        ))
        .header(reqwest::header::AUTHORIZATION, auth_header(cfg)?)
        .send()
        .await?;
    if res.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !res.status().is_success() {
        let status = res.status();
        anyhow::bail!(
            "Bitbucket getFileContents {status}: {}",
            clip(&res.text().await.unwrap_or_default(), 300)
        );
    }
    Ok(Some(res.text().await?))
}

/// Authenticated HTTPS clone URL.
///
/// Git-over-HTTPS on Bitbucket Cloud authenticates an Atlassian API token with
/// the fixed username `x-bitbucket-api-token-auth` (NOT the account email — the
/// `email:token` form works for the REST API but is rejected by git, which fails
/// the clone and drops the reviewer to diff-only). The token has no `@`, so no
/// percent-encoding is needed.
pub fn clone_url(cfg: &Config, repo: &str) -> Result<String> {
    require(&cfg.bitbucket_email, "BB_EMAIL")?;
    require(&cfg.bitbucket_token, "BB_API_TOKEN")?;
    Ok(format!(
        "https://x-bitbucket-api-token-auth:{}@bitbucket.org/{repo}.git",
        cfg.bitbucket_token
    ))
}

/// All of the bot's prior comments, split into (summary id, inline ids).
async fn find_bot_comments(
    client: &Client,
    cfg: &Config,
    repo: &str,
    pr: u64,
) -> Result<(Option<u64>, Vec<u64>)> {
    #[derive(Deserialize)]
    struct Content {
        raw: Option<String>,
    }
    #[derive(Deserialize)]
    struct Inline {}
    #[derive(Deserialize)]
    struct Comment {
        id: u64,
        content: Option<Content>,
        inline: Option<Inline>,
        deleted: Option<bool>,
    }
    #[derive(Deserialize)]
    struct Page {
        values: Vec<Comment>,
        next: Option<String>,
    }

    let mut url = format!("{}/comments?pagelen=100", pr_base(cfg, repo, pr));
    let mut summary: Option<u64> = None;
    let mut inline: Vec<u64> = Vec::new();
    loop {
        let res = client
            .get(&url)
            .header(reqwest::header::AUTHORIZATION, auth_header(cfg)?)
            .send()
            .await?;
        if !res.status().is_success() {
            let status = res.status();
            anyhow::bail!(
                "Bitbucket listComments {status}: {}",
                clip(&res.text().await.unwrap_or_default(), 300)
            );
        }
        let page: Page = res.json().await?;
        for c in page.values {
            if c.deleted == Some(true) {
                continue;
            }
            let is_ours = c
                .content
                .as_ref()
                .and_then(|x| x.raw.as_deref())
                .is_some_and(|b| is_bot_comment(cfg, b));
            if !is_ours {
                continue;
            }
            if c.inline.is_some() {
                inline.push(c.id);
            } else if summary.is_none() {
                summary = Some(c.id);
            }
        }
        match page.next {
            Some(n) => url = n,
            None => return Ok((summary, inline)),
        }
    }
}

async fn upsert_summary(
    client: &Client,
    cfg: &Config,
    repo: &str,
    pr: u64,
    body: &str,
    existing: Option<u64>,
) -> Result<Option<String>> {
    #[derive(Deserialize)]
    struct Html {
        href: Option<String>,
    }
    #[derive(Deserialize)]
    struct Links {
        html: Option<Html>,
    }
    #[derive(Deserialize)]
    struct Created {
        links: Option<Links>,
    }
    let marked = format!("{body}\n\n_{}_", cfg.comment_marker);
    let (req, action) = match existing {
        Some(id) => (
            client.put(format!("{}/comments/{id}", pr_base(cfg, repo, pr))),
            "updateComment",
        ),
        None => (
            client.post(format!("{}/comments", pr_base(cfg, repo, pr))),
            "postComment",
        ),
    };
    let res = req
        .header(reqwest::header::AUTHORIZATION, auth_header(cfg)?)
        .json(&serde_json::json!({ "content": { "raw": marked } }))
        .send()
        .await?;
    if !res.status().is_success() {
        let status = res.status();
        anyhow::bail!(
            "Bitbucket {action} {status}: {}",
            clip(&res.text().await.unwrap_or_default(), 300)
        );
    }
    let c: Created = res.json().await?;
    Ok(c.links.and_then(|l| l.html).and_then(|h| h.href))
}

async fn post_inline(
    client: &Client,
    cfg: &Config,
    repo: &str,
    pr: u64,
    c: &InlineComment,
) -> Result<()> {
    let body = format!("{}\n\n_{}_", c.body, cfg.comment_marker);
    let res = client
        .post(format!("{}/comments", pr_base(cfg, repo, pr)))
        .header(reqwest::header::AUTHORIZATION, auth_header(cfg)?)
        .json(&serde_json::json!({ "content": { "raw": body }, "inline": { "path": c.path, "to": c.line } }))
        .send()
        .await?;
    if !res.status().is_success() {
        let status = res.status();
        tracing::warn!(
            "Bitbucket inline comment failed ({status}) on {}:{}: {}",
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
    let (summary_id, inline_ids) = find_bot_comments(client, cfg, &meta.repo, meta.pr).await?;

    // Refresh inline comments: delete the bot's prior ones, post the new set.
    if !review.inline.is_empty() || !inline_ids.is_empty() {
        for id in inline_ids {
            let url = format!("{}/comments/{id}", pr_base(cfg, &meta.repo, meta.pr));
            let _ = client
                .delete(url)
                .header(reqwest::header::AUTHORIZATION, auth_header(cfg)?)
                .send()
                .await;
        }
        for c in &review.inline {
            post_inline(client, cfg, &meta.repo, meta.pr, c).await?;
        }
    }

    upsert_summary(
        client,
        cfg,
        &meta.repo,
        meta.pr,
        &review.summary,
        summary_id,
    )
    .await
}
