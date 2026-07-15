//! GitHub provider — REST API with a personal access token (`GH_TOKEN`).
//! Posts a summary issue comment (deduped/updated by marker) plus inline review
//! comments (the bot's prior inline comments are deleted and reposted each run).

use anyhow::{Context, Result};
use base64::Engine;
use reqwest::Client;
use serde::Deserialize;

use super::{
    extract_fp, finding_fingerprint, fp_marker, is_bot_comment, render_resolved, InlineComment,
    PrMeta, ReviewPost,
};
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
        body: Option<String>,
        base: Option<Ref>,
        head: Option<Ref>,
    }

    let res = gh(client.get(pr_url(cfg, repo, pr)), cfg).send().await?;
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
        body: pr_data.body,
    })
}

/// Post a standalone issue comment (NOT deduped) — used for `/ask` answers and
/// `/describe` confirmations. Returns the new comment's URL.
///
/// # Errors
/// If `GH_TOKEN` is missing or the request fails.
pub async fn post_comment(
    client: &Client,
    cfg: &Config,
    repo: &str,
    pr: u64,
    body: &str,
) -> Result<Option<String>> {
    require(&cfg.github_token, "GH_TOKEN")?;
    #[derive(Deserialize)]
    struct Created {
        html_url: Option<String>,
    }
    let marked = format!("{body}\n\n_{}_", cfg.comment_marker);
    let url = format!("{}/repos/{repo}/issues/{pr}/comments", cfg.github_api_base);
    let res = gh(client.post(url), cfg)
        .json(&serde_json::json!({ "body": marked }))
        .send()
        .await?;
    if !res.status().is_success() {
        let status = res.status();
        anyhow::bail!(
            "GitHub postComment {status}: {}",
            clip(&res.text().await.unwrap_or_default(), 300)
        );
    }
    let c: Created = res.json().await?;
    Ok(c.html_url)
}

/// Replace the PR description body (the `/describe` command).
///
/// # Errors
/// If `GH_TOKEN` is missing or the request fails.
pub async fn update_pr_description(
    client: &Client,
    cfg: &Config,
    repo: &str,
    pr: u64,
    description: &str,
) -> Result<()> {
    require(&cfg.github_token, "GH_TOKEN")?;
    let res = gh(client.patch(pr_url(cfg, repo, pr)), cfg)
        .json(&serde_json::json!({ "body": description }))
        .send()
        .await?;
    if !res.status().is_success() {
        let status = res.status();
        anyhow::bail!(
            "GitHub updatePrDescription {status}: {}",
            clip(&res.text().await.unwrap_or_default(), 300)
        );
    }
    Ok(())
}

/// Fetch a repo file's text at a git ref via the Contents API.
///
/// Returns `Ok(None)` when the file doesn't exist (404) so the caller can treat
/// a missing `.prbot.toml` as "no overrides" rather than an error.
///
/// # Errors
/// If `GH_TOKEN` is missing, the request fails, or the response can't be decoded.
pub async fn get_file_contents(
    client: &Client,
    cfg: &Config,
    repo: &str,
    r#ref: &str,
    path: &str,
) -> Result<Option<String>> {
    require(&cfg.github_token, "GH_TOKEN")?;

    #[derive(Deserialize)]
    struct Contents {
        content: Option<String>,
        encoding: Option<String>,
    }

    let git_ref = r#ref;
    let url = format!(
        "{}/repos/{repo}/contents/{path}?ref={git_ref}",
        cfg.github_api_base
    );
    let res = gh(client.get(url), cfg).send().await?;
    if res.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !res.status().is_success() {
        let status = res.status();
        anyhow::bail!(
            "GitHub getFileContents {status}: {}",
            clip(&res.text().await.unwrap_or_default(), 300)
        );
    }
    let data: Contents = res.json().await?;
    match data.encoding.as_deref() {
        Some("base64") => {
            let content = data.content.unwrap_or_default();
            // GitHub wraps the base64 payload at 60 cols; strip whitespace first.
            let cleaned: String = content.split_whitespace().collect();
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(cleaned.as_bytes())
                .map_err(|e| anyhow::anyhow!("GitHub getFileContents: bad base64 ({e})"))?;
            Ok(Some(String::from_utf8(bytes).map_err(|e| {
                anyhow::anyhow!("GitHub getFileContents: non-UTF8 content ({e})")
            })?))
        }
        // Small files can occasionally come back already decoded / other encodings.
        _ => Ok(data.content),
    }
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

// ── inline review comments (reconcile: keep, add, resolve) ──────────────────

/// POST one inline review comment, embedding the finding's fingerprint as a
/// hidden marker so a later review can match it (keep / resolve).
async fn post_inline(
    client: &Client,
    cfg: &Config,
    repo: &str,
    pr: u64,
    commit_id: &str,
    c: &InlineComment,
    fp: &str,
) -> Result<()> {
    let url = format!("{}/repos/{repo}/pulls/{pr}/comments", cfg.github_api_base);
    let body = format!("{}\n\n_{}_\n{}", c.body, cfg.comment_marker, fp_marker(fp));
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

/// POST a GraphQL query/mutation; error on transport failure or GraphQL `errors`.
async fn graphql(
    client: &Client,
    cfg: &Config,
    query: &str,
    variables: serde_json::Value,
) -> Result<serde_json::Value> {
    let url = format!("{}/graphql", cfg.github_api_base);
    let res = gh(client.post(url), cfg)
        .json(&serde_json::json!({ "query": query, "variables": variables }))
        .send()
        .await?;
    let status = res.status();
    let v: serde_json::Value = res.json().await.unwrap_or(serde_json::Value::Null);
    if !status.is_success() || v.get("errors").is_some() {
        anyhow::bail!("GitHub GraphQL {status}: {}", clip(&v.to_string(), 300));
    }
    Ok(v)
}

const THREADS_QUERY: &str = r#"
query($owner:String!,$name:String!,$pr:Int!,$cursor:String){
  repository(owner:$owner,name:$name){
    pullRequest(number:$pr){
      reviewThreads(first:100, after:$cursor){
        pageInfo{ hasNextPage endCursor }
        nodes{ id isResolved path line comments(first:1){ nodes{ databaseId body } } }
      }
    }
  }
}"#;

const RESOLVE_MUTATION: &str =
    "mutation($tid:ID!){ resolveReviewThread(input:{threadId:$tid}){ thread{ id } } }";

const REPLY_MUTATION: &str = "mutation($tid:ID!,$body:String!){ addPullRequestReviewThreadReply(input:{pullRequestReviewThreadId:$tid, body:$body}){ comment{ id } } }";

/// One of the bot's existing review-comment threads on the PR.
struct BotThread {
    id: String,
    /// REST id of the thread's first (top) comment — for the delete fallback.
    comment_id: u64,
    is_resolved: bool,
    /// `None` for a legacy (pre-0.6) comment: it carries the bot marker but no
    /// fingerprint, so it never matches a finding and is cleaned up on first sight.
    fp: Option<String>,
    path: String,
    /// Current line of the thread (tracked by GitHub across commits), used as a
    /// secondary match key so a reworded finding on the same line still matches.
    line: Option<u64>,
}

/// List the bot's prior inline-comment threads (whose first comment carries the
/// fingerprint marker), paginating GraphQL `reviewThreads`.
async fn bot_threads(
    client: &Client,
    cfg: &Config,
    owner: &str,
    name: &str,
    pr: u64,
) -> Result<Vec<BotThread>> {
    let mut out = Vec::new();
    let mut cursor = serde_json::Value::Null;
    loop {
        let data = graphql(
            client,
            cfg,
            THREADS_QUERY,
            serde_json::json!({ "owner": owner, "name": name, "pr": pr, "cursor": cursor }),
        )
        .await?;
        let rt = &data["data"]["repository"]["pullRequest"]["reviewThreads"];
        if let Some(nodes) = rt["nodes"].as_array() {
            for node in nodes {
                let first = &node["comments"]["nodes"][0];
                let body = first["body"].as_str().unwrap_or("");
                if !is_bot_comment(cfg, body) {
                    continue;
                }
                // Include ALL bot threads. One with the marker but no fingerprint
                // is a legacy (pre-0.6) comment — it never matches a finding, so
                // reconcile cleans it up (resolve/delete) on first sight.
                out.push(BotThread {
                    id: node["id"].as_str().unwrap_or_default().to_string(),
                    comment_id: first["databaseId"].as_u64().unwrap_or(0),
                    is_resolved: node["isResolved"].as_bool().unwrap_or(false),
                    fp: extract_fp(body),
                    path: node["path"].as_str().unwrap_or_default().to_string(),
                    line: node["line"].as_u64(),
                });
            }
        }
        if rt["pageInfo"]["hasNextPage"].as_bool().unwrap_or(false) {
            cursor = rt["pageInfo"]["endCursor"].clone();
        } else {
            break;
        }
    }
    Ok(out)
}

/// Reconcile the bot's inline comments against a fresh set of findings:
/// - a finding already present (same fingerprint) is **left in place** (no
///   repost → no notification churn, thread history preserved);
/// - a new finding is **posted**;
/// - a prior finding no longer present has its thread **resolved** (with a
///   "✅ resolved" reply).
///
/// Returns the paths of resolved findings for the summary.
async fn reconcile_inline(
    client: &Client,
    cfg: &Config,
    meta: &PrMeta,
    commit_id: &str,
    inline: &[InlineComment],
) -> Result<Vec<String>> {
    let (owner, name) = meta
        .repo
        .split_once('/')
        .context("GitHub repo must be owner/name")?;

    let threads = bot_threads(client, cfg, owner, name, meta.pr).await?;

    // Pair each new finding to AT MOST ONE existing thread, claiming it so two
    // findings can't both match the same thread (and a stale thread isn't kept
    // alive by an unrelated finding on its line). Match by fingerprint first, then
    // by (file, line) — the line key keeps a *reworded* still-present finding
    // matched, since LLM text isn't stable across runs. Legacy threads (no
    // fingerprint) never match and are cleaned up below.
    let mut claimed = vec![false; threads.len()];
    for c in inline {
        let fp = finding_fingerprint(&c.path, &c.body);
        let mut hit = None;
        for (i, t) in threads.iter().enumerate() {
            if !claimed[i] && t.fp.as_deref() == Some(fp.as_str()) {
                hit = Some(i);
                break;
            }
        }
        if hit.is_none() {
            for (i, t) in threads.iter().enumerate() {
                if !claimed[i] && t.fp.is_some() && t.path == c.path && t.line == Some(c.line) {
                    hit = Some(i);
                    break;
                }
            }
        }
        match hit {
            Some(i) => claimed[i] = true, // already present → leave the thread as-is
            None => post_inline(client, cfg, &meta.repo, meta.pr, commit_id, c, &fp).await?,
        }
    }

    // Any unclaimed thread is a finding that's gone (or a legacy comment): clean it
    // up. Prefer resolving the thread (keeps history + leaves a ✅ note); if the
    // token can't resolve threads (a common PAT limitation — "Resource not
    // accessible by personal access token"), fall back to DELETING the comment so
    // nothing accumulates. Report real (fingerprinted) findings in the summary;
    // legacy comments are silent migration cleanup.
    let short = &commit_id[..commit_id.len().min(7)];
    let mut resolved = Vec::new();
    for (i, t) in threads.iter().enumerate() {
        if claimed[i] || t.is_resolved {
            continue;
        }
        if t.fp.is_some() {
            resolved.push(format!("`{}`", t.path));
        }
        match graphql(client, cfg, RESOLVE_MUTATION, serde_json::json!({ "tid": t.id })).await {
            Ok(_) => {
                let reply = format!(
                    "✅ Resolved — no longer flagged as of `{short}`.\n\n_{}_",
                    cfg.comment_marker
                );
                let _ = graphql(
                    client,
                    cfg,
                    REPLY_MUTATION,
                    serde_json::json!({ "tid": t.id, "body": reply }),
                )
                .await;
            }
            Err(e) => {
                tracing::debug!(
                    "resolve thread failed on {} ({e:#}); deleting the comment instead",
                    t.path
                );
                delete_comment(client, cfg, &meta.repo, t.comment_id).await;
            }
        }
    }
    Ok(resolved)
}

/// Best-effort delete of a review comment by its REST id (the fallback when the
/// token can't resolve threads).
async fn delete_comment(client: &Client, cfg: &Config, repo: &str, comment_id: u64) {
    if comment_id == 0 {
        return;
    }
    let url = format!("{}/repos/{repo}/pulls/comments/{comment_id}", cfg.github_api_base);
    let _ = gh(client.delete(url), cfg).send().await;
}

pub async fn post_review(
    client: &Client,
    cfg: &Config,
    meta: &PrMeta,
    review: &ReviewPost,
) -> Result<Option<String>> {
    require(&cfg.github_token, "GH_TOKEN")?;

    // Reconcile inline findings (add new, resolve fixed) — runs even with no new
    // findings so prior ones get resolved. Needs the head SHA to anchor new
    // comments; without it, skip inline (fail-open). Fail-soft on any hiccup so
    // the summary still posts.
    let mut resolved = Vec::new();
    match &meta.head_sha {
        Some(sha) => {
            resolved = reconcile_inline(client, cfg, meta, sha, &review.inline)
                .await
                .unwrap_or_else(|e| {
                    tracing::warn!("inline reconcile failed for {}#{}: {e:#}", meta.repo, meta.pr);
                    Vec::new()
                });
        }
        None if !review.inline.is_empty() => {
            tracing::warn!(
                "no head SHA for {}#{}; skipping inline comments",
                meta.repo,
                meta.pr
            );
        }
        None => {}
    }

    // Append a "Resolved since last review" section to the summary, then upsert.
    let mut summary = review.summary.clone();
    summary.push_str(&render_resolved(&resolved));
    upsert_summary(client, cfg, &meta.repo, meta.pr, &summary).await
}
