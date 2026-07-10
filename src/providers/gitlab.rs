//! GitLab provider — REST API v4 with a personal/project access token
//! (`GITLAB_TOKEN`, sent as the `PRIVATE-TOKEN` header). Posts a summary note
//! (deduped/updated by marker) plus inline discussion comments (the bot's prior
//! inline discussions are deleted and reposted each run).
//!
//! A GitLab merge request is addressed by the URL-encoded project path plus the
//! MR `iid` (the `pr` argument throughout is the MR iid).

use anyhow::Result;
use reqwest::Client;
use serde::Deserialize;

use super::{is_bot_comment, InlineComment, PrMeta, ReviewPost};
use crate::clip;
use crate::config::{require, Config};

/// Percent-encode a single path segment, leaving RFC 3986 unreserved characters
/// (`A-Z a-z 0-9 - . _ ~`) intact and encoding everything else — including `/`.
///
/// Used both for the project id (a namespace path, e.g. `group/sub/project`) and
/// for repository file paths, which GitLab expects fully URL-encoded.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// URL-encode a project path (`group/sub/project` → `group%2Fsub%2Fproject`) for
/// use as the `:id` in the GitLab API.
fn enc(repo: &str) -> String {
    percent_encode(repo)
}

fn mr_base(cfg: &Config, repo: &str, pr: u64) -> String {
    format!(
        "{}/projects/{}/merge_requests/{pr}",
        cfg.gitlab_api_base,
        enc(repo)
    )
}

/// Apply the GitLab auth header to a request builder.
fn gl(rb: reqwest::RequestBuilder, cfg: &Config) -> reqwest::RequestBuilder {
    rb.header("PRIVATE-TOKEN", &cfg.gitlab_token)
        .header("User-Agent", &cfg.user_agent)
}

pub async fn get_meta(client: &Client, cfg: &Config, repo: &str, pr: u64) -> Result<PrMeta> {
    require(&cfg.gitlab_token, "GITLAB_TOKEN")?;

    #[derive(Deserialize)]
    struct Mr {
        title: Option<String>,
        target_branch: Option<String>,
        sha: Option<String>,
    }

    let res = gl(client.get(mr_base(cfg, repo, pr)), cfg).send().await?;
    if !res.status().is_success() {
        let status = res.status();
        anyhow::bail!(
            "GitLab getMeta {status}: {}",
            clip(&res.text().await.unwrap_or_default(), 300)
        );
    }
    let mr: Mr = res.json().await?;
    Ok(PrMeta {
        repo: repo.to_string(),
        pr,
        title: mr.title,
        base_branch: mr.target_branch,
        head_sha: mr.sha,
    })
}

pub async fn get_diff(client: &Client, cfg: &Config, repo: &str, pr: u64) -> Result<String> {
    require(&cfg.gitlab_token, "GITLAB_TOKEN")?;

    #[derive(Deserialize)]
    struct DiffEntry {
        old_path: Option<String>,
        new_path: Option<String>,
        diff: Option<String>,
        #[serde(default)]
        new_file: bool,
        #[serde(default)]
        deleted_file: bool,
        #[serde(default)]
        renamed_file: bool,
    }

    // One page of up to 100 files. GitLab paginates the diffs endpoint; a single
    // page is sufficient for typical MRs (see note in the module/PR).
    let url = format!("{}/diffs?per_page=100", mr_base(cfg, repo, pr));
    let res = gl(client.get(url), cfg).send().await?;
    if !res.status().is_success() {
        let status = res.status();
        anyhow::bail!(
            "GitLab getDiff {status}: {}",
            clip(&res.text().await.unwrap_or_default(), 300)
        );
    }
    let entries: Vec<DiffEntry> = res.json().await?;

    // Reconstruct a standard unified diff so the rest of the pipeline
    // (parse_valid_lines, glob filter, structural, pack) works unchanged.
    let mut out = String::new();
    for e in entries {
        let old_path = e.old_path.as_deref().unwrap_or_default();
        let new_path = e.new_path.as_deref().unwrap_or_default();
        out.push_str(&format!("diff --git a/{old_path} b/{new_path}\n"));
        let _ = e.renamed_file; // parsed for completeness; header above suffices
        if e.deleted_file {
            out.push_str(&format!("--- a/{old_path}\n+++ /dev/null\n"));
        } else if e.new_file {
            out.push_str(&format!("--- /dev/null\n+++ b/{new_path}\n"));
        } else {
            out.push_str(&format!("--- a/{old_path}\n+++ b/{new_path}\n"));
        }
        if let Some(d) = &e.diff {
            out.push_str(d);
        }
    }
    Ok(out)
}

/// Fetch a repo file's text at a git ref via the raw files endpoint.
///
/// Returns `Ok(None)` when the file doesn't exist (404) so the caller can treat
/// a missing `.prbot.toml` as "no overrides" rather than an error.
///
/// # Errors
/// If `GITLAB_TOKEN` is missing, the request fails, or the response can't be read.
pub async fn get_file_contents(
    client: &Client,
    cfg: &Config,
    repo: &str,
    r#ref: &str,
    path: &str,
) -> Result<Option<String>> {
    require(&cfg.gitlab_token, "GITLAB_TOKEN")?;

    let git_ref = r#ref;
    let url = format!(
        "{}/projects/{}/repository/files/{}/raw?ref={}",
        cfg.gitlab_api_base,
        enc(repo),
        percent_encode(path),
        percent_encode(git_ref)
    );
    let res = gl(client.get(url), cfg).send().await?;
    if res.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !res.status().is_success() {
        let status = res.status();
        anyhow::bail!(
            "GitLab getFileContents {status}: {}",
            clip(&res.text().await.unwrap_or_default(), 300)
        );
    }
    Ok(Some(res.text().await?))
}

/// Authenticated HTTPS clone URL (`oauth2:<token>` basic form).
///
/// The host is derived from `gitlab_api_base` by stripping the scheme and the
/// trailing `/api/v4`, so self-managed instances clone from the right host —
/// e.g. `https://gitlab.com/api/v4` → `gitlab.com`.
pub fn clone_url(cfg: &Config, repo: &str) -> Result<String> {
    require(&cfg.gitlab_token, "GITLAB_TOKEN")?;
    let host = cfg
        .gitlab_api_base
        .trim_end_matches('/')
        .trim_end_matches("/api/v4")
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/');
    Ok(format!(
        "https://oauth2:{}@{host}/{repo}.git",
        cfg.gitlab_token
    ))
}

// ── summary note (upsert by marker) ──────────────────────────────────────────

/// A merge request note (comment). Inline discussion comments carry a `position`.
#[derive(Deserialize)]
struct Note {
    id: u64,
    body: Option<String>,
    #[serde(default)]
    position: Option<serde_json::Value>,
}

/// One discussion (thread) — a wrapper around one or more notes.
#[derive(Deserialize)]
struct Discussion {
    id: String,
    notes: Option<Vec<Note>>,
}

async fn list_notes(client: &Client, cfg: &Config, repo: &str, pr: u64) -> Result<Vec<Note>> {
    let mut page = 1u32;
    let mut all: Vec<Note> = Vec::new();
    loop {
        let url = format!("{}/notes?per_page=100&page={page}", mr_base(cfg, repo, pr));
        let res = gl(client.get(url), cfg).send().await?;
        if !res.status().is_success() {
            let status = res.status();
            anyhow::bail!(
                "GitLab listNotes {status}: {}",
                clip(&res.text().await.unwrap_or_default(), 300)
            );
        }
        let notes: Vec<Note> = res.json().await?;
        let n = notes.len();
        all.extend(notes);
        if n < 100 {
            return Ok(all);
        }
        page += 1;
    }
}

async fn find_summary_note(
    client: &Client,
    cfg: &Config,
    repo: &str,
    pr: u64,
) -> Result<Option<u64>> {
    let notes = list_notes(client, cfg, repo, pr).await?;
    Ok(notes
        .into_iter()
        // A summary note is a plain (non-inline) note authored by the bot.
        .find(|n| {
            n.position.is_none() && n.body.as_deref().is_some_and(|b| is_bot_comment(cfg, b))
        })
        .map(|n| n.id))
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
        #[serde(default)]
        web_url: Option<String>,
    }
    let marked = format!("{body}\n\n_{}_", cfg.comment_marker);
    let (req, action) = match find_summary_note(client, cfg, repo, pr).await? {
        Some(id) => (
            client.put(format!("{}/notes/{id}", mr_base(cfg, repo, pr))),
            "updateNote",
        ),
        None => (
            client.post(format!("{}/notes", mr_base(cfg, repo, pr))),
            "postNote",
        ),
    };
    let res = gl(req, cfg)
        .json(&serde_json::json!({ "body": marked }))
        .send()
        .await?;
    if !res.status().is_success() {
        let status = res.status();
        anyhow::bail!(
            "GitLab {action} {status}: {}",
            clip(&res.text().await.unwrap_or_default(), 300)
        );
    }
    let c: Created = res.json().await?;
    Ok(c.web_url)
}

// ── inline discussions (delete prior, post new) ──────────────────────────────

/// The three shas GitLab needs to anchor an inline discussion to the diff.
#[derive(Deserialize, Clone)]
struct DiffRefs {
    base_sha: Option<String>,
    start_sha: Option<String>,
    head_sha: Option<String>,
}

async fn get_diff_refs(client: &Client, cfg: &Config, repo: &str, pr: u64) -> Result<DiffRefs> {
    #[derive(Deserialize)]
    struct Mr {
        diff_refs: Option<DiffRefs>,
    }
    let res = gl(client.get(mr_base(cfg, repo, pr)), cfg).send().await?;
    if !res.status().is_success() {
        let status = res.status();
        anyhow::bail!(
            "GitLab getDiffRefs {status}: {}",
            clip(&res.text().await.unwrap_or_default(), 300)
        );
    }
    let mr: Mr = res.json().await?;
    mr.diff_refs
        .ok_or_else(|| anyhow::anyhow!("GitLab MR has no diff_refs"))
}

async fn list_discussions(
    client: &Client,
    cfg: &Config,
    repo: &str,
    pr: u64,
) -> Result<Vec<Discussion>> {
    let mut page = 1u32;
    let mut all: Vec<Discussion> = Vec::new();
    loop {
        let url = format!(
            "{}/discussions?per_page=100&page={page}",
            mr_base(cfg, repo, pr)
        );
        let res = gl(client.get(url), cfg).send().await?;
        if !res.status().is_success() {
            let status = res.status();
            anyhow::bail!(
                "GitLab listDiscussions {status}: {}",
                clip(&res.text().await.unwrap_or_default(), 300)
            );
        }
        let discussions: Vec<Discussion> = res.json().await?;
        let n = discussions.len();
        all.extend(discussions);
        if n < 100 {
            return Ok(all);
        }
        page += 1;
    }
}

/// Delete the bot's prior inline discussion notes (best-effort).
async fn delete_prior_inline(client: &Client, cfg: &Config, repo: &str, pr: u64) -> Result<()> {
    let discussions = list_discussions(client, cfg, repo, pr).await?;
    for d in discussions {
        for note in d.notes.into_iter().flatten() {
            // Only our own inline notes (those carrying a position).
            let ours = note.position.is_some()
                && note.body.as_deref().is_some_and(|b| is_bot_comment(cfg, b));
            if !ours {
                continue;
            }
            let url = format!(
                "{}/discussions/{}/notes/{}",
                mr_base(cfg, repo, pr),
                d.id,
                note.id
            );
            let _ = gl(client.delete(url), cfg).send().await; // best-effort
        }
    }
    Ok(())
}

async fn post_inline(
    client: &Client,
    cfg: &Config,
    repo: &str,
    pr: u64,
    refs: &DiffRefs,
    c: &InlineComment,
) -> Result<()> {
    let body = format!("{}\n\n_{}_", c.body, cfg.comment_marker);
    let res = gl(
        client.post(format!("{}/discussions", mr_base(cfg, repo, pr))),
        cfg,
    )
    .json(&serde_json::json!({
        "body": body,
        "position": {
            "position_type": "text",
            "base_sha": refs.base_sha,
            "start_sha": refs.start_sha,
            "head_sha": refs.head_sha,
            "new_path": c.path,
            "new_line": c.line,
        }
    }))
    .send()
    .await?;
    if !res.status().is_success() {
        let status = res.status();
        // Don't abort the whole run on one bad anchor — log and move on.
        tracing::warn!(
            "GitLab inline comment failed ({status}) on {}:{}: {}",
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
    require(&cfg.gitlab_token, "GITLAB_TOKEN")?;
    let repo = &meta.repo;
    let pr = meta.pr;

    // Refresh inline comments: delete the bot's prior ones, post the new set.
    if !review.inline.is_empty() {
        match get_diff_refs(client, cfg, repo, pr).await {
            Ok(refs) => {
                delete_prior_inline(client, cfg, repo, pr).await?;
                for c in &review.inline {
                    post_inline(client, cfg, repo, pr, &refs, c).await?;
                }
            }
            Err(e) => {
                tracing::warn!(
                    "no diff_refs for {repo}!{pr} ({e:#}); skipping inline comments"
                );
            }
        }
    }

    upsert_summary(client, cfg, repo, pr, &review.summary).await
}
