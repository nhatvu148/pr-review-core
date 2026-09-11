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
        description: Option<String>,
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
        body: mr.description,
        // GitLab pipeline status is not wired up yet — `None` reads as "unknown",
        // so the reviewer simply gets no CI block rather than a wrong one.
        ci_status: None,
    })
}

/// Post a standalone MR note (NOT deduped) — used for `/ask` answers and
/// `/describe` confirmations. Returns the new note's URL.
///
/// # Errors
/// If `GITLAB_TOKEN` is missing or the request fails.
pub async fn post_comment(
    client: &Client,
    cfg: &Config,
    repo: &str,
    pr: u64,
    body: &str,
) -> Result<Option<String>> {
    require(&cfg.gitlab_token, "GITLAB_TOKEN")?;
    #[derive(Deserialize)]
    struct Created {
        #[serde(default)]
        web_url: Option<String>,
    }
    let marked = format!("{body}\n\n_{}_", cfg.comment_marker);
    let res = gl(
        client.post(format!("{}/notes", mr_base(cfg, repo, pr))),
        cfg,
    )
    .json(&serde_json::json!({ "body": marked }))
    .send()
    .await?;
    if !res.status().is_success() {
        let status = res.status();
        anyhow::bail!(
            "GitLab postComment {status}: {}",
            clip(&res.text().await.unwrap_or_default(), 300)
        );
    }
    let c: Created = res.json().await?;
    Ok(c.web_url)
}

/// Replace the MR description (the `/describe` command).
///
/// # Errors
/// If `GITLAB_TOKEN` is missing or the request fails.
pub async fn update_pr_description(
    client: &Client,
    cfg: &Config,
    repo: &str,
    pr: u64,
    description: &str,
) -> Result<()> {
    require(&cfg.gitlab_token, "GITLAB_TOKEN")?;
    let res = gl(client.put(mr_base(cfg, repo, pr)), cfg)
        .json(&serde_json::json!({ "description": description }))
        .send()
        .await?;
    if !res.status().is_success() {
        let status = res.status();
        anyhow::bail!(
            "GitLab updatePrDescription {status}: {}",
            clip(&res.text().await.unwrap_or_default(), 300)
        );
    }
    Ok(())
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
        .find(|n| n.position.is_none() && n.body.as_deref().is_some_and(|b| is_bot_comment(cfg, b)))
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

/// The position object identifying the line a finding anchors to.
///
/// Shared by the draft-note and discussion paths so the two cannot drift: a
/// finding published one way must land on exactly the line it would have landed
/// on the other way, or the fallback below changes where comments appear.
fn inline_position(refs: &DiffRefs, c: &InlineComment) -> serde_json::Value {
    serde_json::json!({
        "position_type": "text",
        "base_sha": refs.base_sha,
        "start_sha": refs.start_sha,
        "head_sha": refs.head_sha,
        "new_path": c.path,
        "new_line": c.line,
    })
}

/// Stage one finding as a draft note, to be published with the rest in one call.
///
/// Errors rather than warning, unlike `post_inline`: a draft note that fails to
/// stage would otherwise be silently missing from the published review, and the
/// caller can still fall back to posting every finding as its own discussion.
async fn stage_draft(
    client: &Client,
    cfg: &Config,
    repo: &str,
    pr: u64,
    refs: &DiffRefs,
    c: &InlineComment,
) -> Result<()> {
    let body = format!("{}\n\n_{}_", c.body, cfg.comment_marker);
    let res = gl(
        client.post(format!("{}/draft_notes", mr_base(cfg, repo, pr))),
        cfg,
    )
    .json(&serde_json::json!({ "note": body, "position": inline_position(refs, c) }))
    .send()
    .await?;
    let status = res.status();
    if !status.is_success() {
        anyhow::bail!(
            "GitLab draft_note {status} on {}:{}: {}",
            c.path,
            c.line,
            clip(&res.text().await.unwrap_or_default(), 200)
        );
    }
    Ok(())
}

/// Publish every staged draft note as ONE review.
///
/// `reviewer_state` is deliberately not sent. The API documents `reviewed` as
/// recording no formal approval, so it would be safe — but this reviewer is
/// advisory and says so in its own summary, and `requested_changes` is the
/// neighbouring value nobody should reach for by habit. Formal approval lives on
/// a separate `/approve` endpoint that this crate never calls.
///
/// No `note` either: that would put the summary in the published review, while
/// `upsert_summary` EDITS one summary note across rounds. Sending both would
/// leave a stale copy per round.
async fn publish_drafts(client: &Client, cfg: &Config, repo: &str, pr: u64) -> Result<()> {
    let res = gl(
        client.post(format!(
            "{}/draft_notes/bulk_publish",
            mr_base(cfg, repo, pr)
        )),
        cfg,
    )
    .send()
    .await?;
    let status = res.status();
    if !status.is_success() {
        anyhow::bail!(
            "GitLab bulk_publish {status}: {}",
            clip(&res.text().await.unwrap_or_default(), 200)
        );
    }
    Ok(())
}

/// Every draft note pending on this merge request, paginated.
///
/// Paginated like `list_discussions` and `list_notes`, and for a sharper reason
/// than consistency: callers treat an unlisted draft as an absent one, so a
/// page-size blind spot here would let `bulk_publish` post someone else's work.
async fn list_drafts(
    client: &Client,
    cfg: &Config,
    repo: &str,
    pr: u64,
) -> Result<Vec<serde_json::Value>> {
    let url = format!("{}/draft_notes", mr_base(cfg, repo, pr));
    let mut all: Vec<serde_json::Value> = Vec::new();
    let mut page = 1u32;
    loop {
        let res = gl(client.get(format!("{url}?per_page=100&page={page}")), cfg)
            .send()
            .await?;
        let status = res.status();
        if !status.is_success() {
            anyhow::bail!(
                "GitLab list draft_notes {status}: {}",
                clip(&res.text().await.unwrap_or_default(), 200)
            );
        }
        let batch: Vec<serde_json::Value> = res.json().await?;
        let n = batch.len();
        all.extend(batch);
        if n < 100 {
            return Ok(all);
        }
        page += 1;
    }
}

/// Split pending drafts into ours (by id) and a count of everyone else's.
///
/// A draft we cannot identify counts as foreign. Guessing the other way risks
/// deleting or publishing a person's unfinished review; guessing this way costs
/// only a batched round.
fn partition_drafts(cfg: &Config, drafts: &[serde_json::Value]) -> (Vec<u64>, usize) {
    let mut ours = Vec::new();
    let mut foreign = 0usize;
    for d in drafts {
        let is_ours = d
            .get("note")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|b| is_bot_comment(cfg, b));
        match (is_ours, d.get("id").and_then(serde_json::Value::as_u64)) {
            (true, Some(id)) => ours.push(id),
            _ => foreign += 1,
        }
    }
    (ours, foreign)
}

/// Delete our own pending drafts, and report whether anyone else's remain.
///
/// `bulk_publish` has no filter: it publishes **every** pending draft the token
/// owns, and that token is very likely a person's rather than a robot's. So a
/// draft that is not ours is untouchable in both directions — deleting it would
/// destroy a review someone is still writing, publishing it would post their
/// unfinished words inside our round — and the safe answer is not to batch. The
/// per-discussion path touches no drafts.
///
/// `Ok(false)` means someone else has drafts pending. `Err` means the state could
/// not be established, which is not a licence to proceed.
async fn clear_our_drafts(client: &Client, cfg: &Config, repo: &str, pr: u64) -> Result<bool> {
    let url = format!("{}/draft_notes", mr_base(cfg, repo, pr));
    let (ours, foreign) = partition_drafts(cfg, &list_drafts(client, cfg, repo, pr).await?);
    for id in ours {
        let del = gl(client.delete(format!("{url}/{id}")), cfg).send().await?;
        if !del.status().is_success() {
            anyhow::bail!("GitLab delete draft_note {id}: {}", del.status());
        }
    }
    if foreign > 0 {
        tracing::info!(
            "{foreign} draft note(s) on {repo}!{pr} are not ours; posting individually so \
             bulk_publish cannot publish them"
        );
    }
    Ok(foreign == 0)
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
    .json(&serde_json::json!({ "body": body, "position": inline_position(refs, c) }))
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

/// Stage every finding as a draft note, then publish them as one review.
///
/// Unlike the GitHub provider, there is nothing to reconcile here: `post_review`
/// deletes the bot's prior inline notes and reposts the whole set each round, so
/// this changes how the set is delivered and not which comments exist.
///
/// Any failure discards the drafts before returning. Leaving them staged would
/// be worse than not batching at all — the next run's `bulk_publish` publishes
/// every pending draft this token owns, so an abandoned review's findings would
/// surface later attached to a round that never produced them.
async fn publish_as_one_review(
    client: &Client,
    cfg: &Config,
    repo: &str,
    pr: u64,
    refs: &DiffRefs,
    inline: &[InlineComment],
) -> Result<()> {
    if !clear_our_drafts(client, cfg, repo, pr).await? {
        anyhow::bail!("draft notes from another author are pending");
    }

    for c in inline {
        if let Err(e) = stage_draft(client, cfg, repo, pr, refs, c).await {
            let _ = clear_our_drafts(client, cfg, repo, pr).await;
            return Err(e);
        }
    }

    // Check again. Staging is many requests, and a colleague can open a review
    // during them. This narrows the window rather than closing it — the API
    // offers no way to publish only named drafts — but it shrinks the exposure
    // from the whole staging run to the gap before the next call.
    let (_, foreign) = partition_drafts(cfg, &list_drafts(client, cfg, repo, pr).await?);
    if foreign > 0 {
        let _ = clear_our_drafts(client, cfg, repo, pr).await;
        anyhow::bail!("draft notes from another author appeared while staging");
    }

    if let Err(e) = publish_drafts(client, cfg, repo, pr).await {
        // The call failed, but GitLab may have processed it and lost the
        // response. Our drafts being gone is the only evidence separating the
        // two, and it is worth asking for: returning an error here makes the
        // caller repost every finding on top of a review that already published.
        match list_drafts(client, cfg, repo, pr).await {
            Ok(remaining) if partition_drafts(cfg, &remaining).0.is_empty() => {
                tracing::warn!(
                    "GitLab bulk_publish for {repo}!{pr} reported an error ({e:#}) but our \
                     drafts are gone; treating the review as published"
                );
                return Ok(());
            }
            _ => {
                let _ = clear_our_drafts(client, cfg, repo, pr).await;
                return Err(e);
            }
        }
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
                if let Err(e) =
                    publish_as_one_review(client, cfg, repo, pr, &refs, &review.inline).await
                {
                    // Fall back to one discussion per finding — the path this
                    // provider used before draft notes existed here. Safe to
                    // retry from scratch because `publish_as_one_review`
                    // discards its own drafts on failure, so nothing it staged
                    // can be published later alongside these.
                    tracing::warn!(
                        "GitLab batched review failed for {repo}!{pr} ({e:#}); posting {} \
                         comment(s) individually",
                        review.inline.len()
                    );
                    for c in &review.inline {
                        post_inline(client, cfg, repo, pr, &refs, c).await?;
                    }
                }
            }
            Err(e) => {
                tracing::warn!("no diff_refs for {repo}!{pr} ({e:#}); skipping inline comments");
            }
        }
    }

    upsert_summary(client, cfg, repo, pr, &review.summary).await
}

#[cfg(test)]
mod tests {
    use super::inline_position;
    use crate::providers::types::InlineComment;

    fn refs() -> super::DiffRefs {
        super::DiffRefs {
            base_sha: Some("base".into()),
            start_sha: Some("start".into()),
            head_sha: Some("head".into()),
        }
    }

    /// The two creation paths must anchor identically.
    ///
    /// A finding published as a draft note and the same finding posted as a
    /// discussion have to land on the same line, or the fallback silently moves
    /// comments when it fires — and the fallback only fires when something has
    /// already gone wrong, which is the worst moment to also change where
    /// comments appear.
    #[test]
    fn a_draft_note_anchors_exactly_where_a_discussion_would() {
        let c = InlineComment {
            path: "src/a.py".into(),
            line: 42,
            body: "x".into(),
        };
        let pos = inline_position(&refs(), &c);
        assert_eq!(pos["position_type"], "text");
        assert_eq!(pos["new_path"], "src/a.py");
        assert_eq!(pos["new_line"], 42);
        assert_eq!(pos["base_sha"], "base");
        assert_eq!(pos["start_sha"], "start");
        assert_eq!(pos["head_sha"], "head");
    }
}
