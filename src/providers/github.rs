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

/// Percent-encode a URL path, preserving `/` segment separators. Keeps nested
/// paths resolvable while stopping a caller-supplied path from injecting a query
/// or fragment (`?`, `#`, `&`, whitespace, …) into the request URL.
pub(super) fn enc_path(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
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
    // 406 is GitHub refusing the `.diff` media type itself — "the diff exceeded the
    // maximum number of lines (20000)". The PR is fine; only this representation of
    // it is unavailable, so rebuild the diff from the Files API instead of failing.
    if res.status() == reqwest::StatusCode::NOT_ACCEPTABLE {
        tracing::warn!(
            "GitHub refused the .diff media type for {repo}#{pr} (406, diff too large); \
             rebuilding from the Files API"
        );
        return diff_from_files_api(client, cfg, repo, pr).await;
    }
    if !res.status().is_success() {
        let status = res.status();
        anyhow::bail!(
            "GitHub getDiff {status}: {}",
            clip(&res.text().await.unwrap_or_default(), 300)
        );
    }
    Ok(res.text().await?)
}

/// One entry from `GET /repos/{repo}/pulls/{pr}/files`.
#[derive(Debug, Deserialize)]
pub(super) struct FileEntry {
    pub filename: String,
    /// Set only on a rename/copy — the path on the old side.
    #[serde(default)]
    pub previous_filename: Option<String>,
    /// `added` | `removed` | `modified` | `renamed` | `copied` | `changed` | `unchanged`.
    #[serde(default)]
    pub status: Option<String>,
    /// The hunks. GitHub omits this for binaries and for files too large to patch.
    #[serde(default)]
    pub patch: Option<String>,
    #[serde(default)]
    pub additions: u64,
    #[serde(default)]
    pub deletions: u64,
}

/// GitHub's own ceiling on the Files API: 30 pages × 100 files.
const FILES_PER_PAGE: u64 = 100;
const FILES_MAX_PAGES: u64 = 30;

/// Is this path *unambiguously* binary from its extension alone?
///
/// Used only on the Files-API path, where a missing `patch` is ambiguous between "a
/// binary" and "GitHub truncated a large PR". Claiming binary is what feeds
/// [`crate::diff::diff_hygiene`]'s swept-in-binary finding, so the list is an
/// allowlist of formats no one edits as text: a false claim here costs a MEDIUM
/// finding (and, via the recommendation floor, an "approve with changes"), whereas
/// missing one only loses a hygiene bonus on an already-oversized PR.
/// Grouped by kind, in order: archives; executables/objects/libraries; images,
/// fonts and media; documents, databases, model weights and installers.
fn is_binary_ext(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    const BINARY_EXT: &[&str] = &[
        ".zip",
        ".tar",
        ".gz",
        ".tgz",
        ".bz2",
        ".xz",
        ".7z",
        ".rar",
        ".jar",
        ".war",
        ".exe",
        ".dll",
        ".so",
        ".dylib",
        ".bin",
        ".o",
        ".a",
        ".lib",
        ".class",
        ".wasm",
        ".pdb",
        ".obj",
        ".png",
        ".jpg",
        ".jpeg",
        ".gif",
        ".ico",
        ".webp",
        ".avif",
        ".bmp",
        ".tiff",
        ".psd",
        ".woff",
        ".woff2",
        ".ttf",
        ".otf",
        ".eot",
        ".mp3",
        ".mp4",
        ".mov",
        ".avi",
        ".wav",
        ".webm",
        ".ogg",
        ".pdf",
        ".doc",
        ".docx",
        ".xls",
        ".xlsx",
        ".ppt",
        ".pptx",
        ".db",
        ".sqlite",
        ".sqlite3",
        ".mdb",
        ".onnx",
        ".pt",
        ".pth",
        ".h5",
        ".pkl",
        ".npy",
        ".npz",
        ".safetensors",
        ".dmg",
        ".iso",
        ".deb",
        ".rpm",
        ".msi",
        ".apk",
        ".ipa",
        ".keystore",
        ".jks",
        ".p12",
        ".pfx",
    ];
    BINARY_EXT.iter().any(|e| p.ends_with(e))
}

/// Render Files-API entries as a unified diff equivalent to what the `.diff` media
/// type would have returned.
///
/// The synthesized `diff --git` / `new file mode` / `--- `/`+++ ` headers are what
/// every downstream consumer keys on — [`crate::diff::split_diff_sections`],
/// [`crate::diff::parse_valid_lines`], and [`crate::diff::diff_hygiene`] — so the
/// rebuilt diff anchors inline comments and triggers hygiene findings exactly like a
/// native one.
///
/// Two things the Files API cannot give us are stated in the output rather than
/// silently dropped, so neither the model nor a reader mistakes a partial diff for a
/// small change: files whose `patch` GitHub withheld, and a file list truncated at
/// the 3000-file ceiling.
pub(super) fn render_files_diff(files: &[FileEntry], truncated: bool) -> String {
    let mut out = String::new();
    if truncated {
        out.push_str(
            "[note: this PR exceeds GitHub's 3000-file API ceiling — the files below \
             are the first 3000; later files are NOT shown]\n",
        );
    }
    // A patch-less file is the *norm* on this path, not an anomaly: GitHub withholds
    // patches wholesale once a PR's diff passes its size limits. Lead with the count
    // so a reader sees "N files I could not inspect" rather than inferring something
    // about the handful that later survive the findings cap.
    let no_patch = files.iter().filter(|f| f.patch.is_none()).count();
    if no_patch > 0 {
        out.push_str(&format!(
            "[note: GitHub returned no patch for {no_patch} of {} files (its per-PR diff \
             size limits) — those files are listed below but their contents are NOT shown \
             and could not be reviewed]\n",
            files.len()
        ));
    }
    for f in files {
        let status = f.status.as_deref().unwrap_or("modified");
        let old = f.previous_filename.as_deref().unwrap_or(&f.filename);
        let new = f.filename.as_str();

        out.push_str(&format!("diff --git a/{old} b/{new}\n"));
        match status {
            // `new file mode` is the marker diff_hygiene uses to judge *added* files.
            "added" => out.push_str("new file mode 100644\n"),
            "removed" => out.push_str("deleted file mode 100644\n"),
            "renamed" | "copied" => {
                out.push_str(&format!("rename from {old}\nrename to {new}\n"));
            }
            _ => {}
        }

        let (a, b) = match status {
            "added" => ("/dev/null".to_string(), format!("b/{new}")),
            "removed" => (format!("a/{old}"), "/dev/null".to_string()),
            _ => (format!("a/{old}"), format!("b/{new}")),
        };

        match &f.patch {
            Some(patch) => {
                out.push_str(&format!("--- {a}\n+++ {b}\n"));
                out.push_str(patch);
                if !patch.ends_with('\n') {
                    out.push('\n');
                }
            }
            // No `patch`. A missing patch is *unknown*, not *binary*: GitHub omits
            // `patch` AND reports `additions: 0` for ordinary text files whenever the
            // PR-level diff exceeds its limits — which is exactly the situation that
            // put us on this path. Counts therefore cannot distinguish the two, so
            // only the path itself may claim binary, and only for extensions that are
            // unambiguously binary. Everything else says it does not know.
            //
            // Deliberately no `+++` line in either arm — there are no hunks to anchor.
            None if is_binary_ext(new) => {
                out.push_str(&format!("Binary files {a} and {b} differ\n"));
            }
            None => {
                out.push_str(&format!(
                    "[no diff available from GitHub for this file (+{} -{}) — contents not \
                     reviewable; do NOT infer that it is binary or unchanged]\n",
                    f.additions, f.deletions
                ));
            }
        }
    }
    out
}

/// Rebuild a PR's diff from `GET /repos/{repo}/pulls/{pr}/files`, which has no
/// 20000-line limit. Used when the `.diff` media type 406s.
async fn diff_from_files_api(client: &Client, cfg: &Config, repo: &str, pr: u64) -> Result<String> {
    let mut files: Vec<FileEntry> = Vec::new();
    let mut truncated = false;

    for page in 1..=FILES_MAX_PAGES {
        let url = format!(
            "{}/repos/{repo}/pulls/{pr}/files?per_page={FILES_PER_PAGE}&page={page}",
            cfg.github_api_base
        );
        let res = gh(client.get(url), cfg).send().await?;
        if !res.status().is_success() {
            let status = res.status();
            anyhow::bail!(
                "GitHub getDiff(files page {page}) {status}: {}",
                clip(&res.text().await.unwrap_or_default(), 300)
            );
        }
        let batch: Vec<FileEntry> = res
            .json()
            .await
            .context("GitHub getDiff(files): unexpected response shape")?;
        let full_page = batch.len() as u64 == FILES_PER_PAGE;
        files.extend(batch);
        if !full_page {
            break;
        }
        // A full last page means there is very likely more we cannot reach.
        truncated = page == FILES_MAX_PAGES;
    }

    if files.is_empty() {
        anyhow::bail!("GitHub getDiff(files): PR {repo}#{pr} reported no files");
    }
    tracing::info!(
        "rebuilt {repo}#{pr} diff from the Files API: {} file(s){}",
        files.len(),
        if truncated { " (truncated)" } else { "" }
    );
    Ok(render_files_diff(&files, truncated))
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
    let head_sha = pr_data.head.and_then(|h| h.sha);
    // Fail-open: CI status is context, never a precondition for reviewing.
    let ci_status = match head_sha.as_ref().filter(|_| cfg.ci_status) {
        Some(sha) => get_check_status(client, cfg, repo, sha)
            .await
            .unwrap_or_else(|e| {
                tracing::warn!("could not fetch check runs for {repo}@{sha}: {e:#}");
                None
            }),
        None => None,
    };
    Ok(PrMeta {
        repo: repo.to_string(),
        pr,
        title: pr_data.title,
        base_branch: pr_data.base.and_then(|b| b.ref_),
        head_sha,
        body: pr_data.body,
        ci_status,
    })
}

/// Fetch the check runs for a commit and render them one per line.
///
/// Returns `None` when no checks have reported — which must read as "unknown", not
/// "nothing ran": a repo with no CI and a repo whose CI has not started yet are
/// indistinguishable here, and neither licenses a claim about the build.
///
/// # Errors
/// If the request fails or the response can't be parsed.
pub async fn get_check_status(
    client: &Client,
    cfg: &Config,
    repo: &str,
    sha: &str,
) -> Result<Option<String>> {
    #[derive(Deserialize)]
    struct Run {
        name: Option<String>,
        /// `queued` | `in_progress` | `completed`.
        status: Option<String>,
        /// `success` | `failure` | `neutral` | `cancelled` | `timed_out` | …
        conclusion: Option<String>,
    }
    #[derive(Deserialize)]
    struct Runs {
        check_runs: Option<Vec<Run>>,
        /// How many runs the commit actually has — the only way to know the page
        /// we got is partial.
        #[serde(default)]
        total_count: Option<u64>,
    }

    // A CI matrix routinely produces dozens of runs. Paging matters here more than
    // it looks: the prompt tells the model a passing check falsifies a build-break
    // claim, so a *failing* run that fell off the end would license exactly the
    // wrong conclusion. Page to a bound, then say plainly what was not fetched.
    const PER_PAGE: u64 = 100;
    const MAX_PAGES: u64 = 3;

    let mut runs: Vec<Run> = Vec::new();
    let mut total: Option<u64> = None;
    for page in 1..=MAX_PAGES {
        let url = format!(
            "{}/repos/{repo}/commits/{}/check-runs?per_page={PER_PAGE}&page={page}",
            cfg.github_api_base,
            enc_path(sha)
        );
        let res = gh(client.get(url), cfg).send().await?;
        if !res.status().is_success() {
            let status = res.status();
            anyhow::bail!(
                "GitHub getCheckRuns {status}: {}",
                clip(&res.text().await.unwrap_or_default(), 200)
            );
        }
        let body: Runs = res
            .json()
            .await
            .context("GitHub getCheckRuns: unexpected response shape")?;
        total = body.total_count.or(total);
        let batch = body.check_runs.unwrap_or_default();
        let full_page = batch.len() as u64 == PER_PAGE;
        runs.extend(batch);
        if !full_page || total.is_some_and(|t| runs.len() as u64 >= t) {
            break;
        }
    }
    if runs.is_empty() {
        return Ok(None);
    }

    let mut rendered: Vec<String> = runs
        .iter()
        .map(|r| {
            let name = r.name.as_deref().unwrap_or("(unnamed check)");
            // A completed run reports its conclusion; a running one has none yet,
            // and saying "in_progress" is more honest than reporting no result.
            let state = r
                .conclusion
                .as_deref()
                .or(r.status.as_deref())
                .unwrap_or("unknown");
            format!("- {name}: {state}")
        })
        .collect();
    // An incomplete list must never read as a complete one — a hidden failure is
    // the case this whole block exists to prevent the reviewer getting wrong.
    if let Some(t) = total.filter(|t| *t > runs.len() as u64) {
        let missing = t - runs.len() as u64;
        tracing::warn!(
            "check runs for {repo}@{sha} truncated: showed {} of {t}",
            runs.len()
        );
        rendered.push(format!(
            "- [{missing} further check(s) NOT shown — this commit has {t}. \
             The list above is incomplete: it cannot show that every check passed.]"
        ));
    }
    Ok(Some(rendered.join("\n")))
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

    // Encode the (possibly caller-supplied) path so it can't inject a query/
    // fragment and override `ref`; pass `ref` as a real query param. Slashes are
    // preserved so nested paths still resolve.
    let url = format!(
        "{}/repos/{repo}/contents/{}",
        cfg.github_api_base,
        enc_path(path)
    );
    let res = gh(client.get(url).query(&[("ref", r#ref)]), cfg)
        .send()
        .await?;
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
        match graphql(
            client,
            cfg,
            RESOLVE_MUTATION,
            serde_json::json!({ "tid": t.id }),
        )
        .await
        {
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
    let url = format!(
        "{}/repos/{repo}/pulls/comments/{comment_id}",
        cfg.github_api_base
    );
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
                    tracing::warn!(
                        "inline reconcile failed for {}#{}: {e:#}",
                        meta.repo,
                        meta.pr
                    );
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

#[cfg(test)]
mod tests {
    use super::{enc_path, render_files_diff, FileEntry};

    fn entry(filename: &str, status: &str, patch: Option<&str>) -> FileEntry {
        let (additions, deletions) = patch.map_or((0, 0), |_| (1, 0));
        FileEntry {
            filename: filename.to_string(),
            previous_filename: None,
            status: Some(status.to_string()),
            patch: patch.map(str::to_string),
            additions,
            deletions,
        }
    }

    /// The whole point of the rebuild: a diff assembled from the Files API must be
    /// parseable by the same machinery as a native one, or findings can't anchor.
    #[test]
    fn rebuilt_diff_anchors_lines_like_a_native_one() {
        let files = vec![entry(
            "src/a.rs",
            "modified",
            Some("@@ -1,2 +1,3 @@\n ctx\n+added\n ctx2\n"),
        )];
        let diff = render_files_diff(&files, false);

        let valid = crate::diff::parse_valid_lines(&diff);
        assert!(
            valid["src/a.rs"].contains(&2),
            "the +added line is new-side 2"
        );
        let sections = crate::diff::split_diff_sections(&diff);
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].0, "src/a.rs");
    }

    #[test]
    fn added_and_removed_files_get_dev_null_sides() {
        let diff = render_files_diff(
            &[
                entry("new.rs", "added", Some("@@ -0,0 +1 @@\n+hi\n")),
                entry("gone.rs", "removed", Some("@@ -1 +0,0 @@\n-bye\n")),
            ],
            false,
        );
        assert!(diff.contains("new file mode 100644\n--- /dev/null\n+++ b/new.rs"));
        assert!(diff.contains("deleted file mode 100644\n--- a/gone.rs\n+++ /dev/null"));
        // A deleted file must not claim new-side lines.
        assert!(!crate::diff::parse_valid_lines(&diff).contains_key("gone.rs"));
    }

    #[test]
    fn a_rename_keeps_both_paths() {
        let mut f = entry("new/path.rs", "renamed", Some("@@ -1 +1 @@\n-a\n+b\n"));
        f.previous_filename = Some("old/path.rs".to_string());
        let diff = render_files_diff(&[f], false);
        assert!(diff.starts_with("diff --git a/old/path.rs b/new/path.rs\n"));
        assert!(diff.contains("rename from old/path.rs\nrename to new/path.rs\n"));
    }

    /// A patch-less entry with no additions/deletions is GitHub's binary signature;
    /// rendering the marker keeps `diff_hygiene`'s swept-in-binary check working
    /// through the fallback path.
    #[test]
    fn a_binary_file_still_trips_diff_hygiene() {
        let diff = render_files_diff(&[entry("assets/ime.zip", "added", None)], false);
        assert!(diff.contains("Binary files /dev/null and b/assets/ime.zip differ"));
        let issues = crate::diff::diff_hygiene(&diff);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].file, "assets/ime.zip");
    }

    /// A patch withheld for size is NOT a binary — say so rather than emit a marker
    /// claiming the file is unchanged.
    #[test]
    fn a_withheld_patch_is_reported_not_silently_dropped() {
        let mut f = entry("huge.sql", "modified", None);
        f.additions = 40_000;
        let diff = render_files_diff(&[f], false);
        assert!(diff.contains("[no diff available from GitHub for this file (+40000 -0)"));
        assert!(!diff.contains("Binary files"));
    }

    /// Acceptance test for VinaText#20. GitHub omits `patch` **and reports
    /// `additions: 0`** for ordinary text files once a PR's diff passes its size
    /// limits — 111 of 267 files on that PR. Treating zero counts as "binary" turned
    /// vendored `.cxx` sources into five MEDIUM "you committed a binary" findings.
    /// Counts cannot distinguish the cases; only the extension may.
    #[test]
    fn truncated_text_files_are_never_called_binary() {
        let files: Vec<FileEntry> = ["lexers/LexD.cxx", "src/app.ts", "tools/gen.py", "Makefile"]
            .iter()
            .map(|p| entry(p, "added", None)) // patch: None, additions 0, deletions 0
            .collect();
        let diff = render_files_diff(&files, false);

        assert!(!diff.contains("Binary files"), "no binary claim: {diff}");
        assert!(
            crate::diff::diff_hygiene(&diff).is_empty(),
            "and therefore no hygiene finding"
        );
        assert!(diff.contains("[no diff available from GitHub for this file"));
    }

    /// The counterpart: the swept-in-archive signal must survive on this path, since
    /// suppressing it everywhere would trade one false-positive class for a real miss.
    #[test]
    fn a_patchless_archive_is_still_called_binary() {
        let diff = render_files_diff(&[entry("assets/ime.zip", "added", None)], false);
        assert!(diff.contains("Binary files /dev/null and b/assets/ime.zip differ"));
        assert_eq!(crate::diff::diff_hygiene(&diff).len(), 1);
    }

    /// §4.6, no silent caps: the count of un-inspectable files leads the diff, so the
    /// review reads as "111 files I could not see", not "5 binaries you committed".
    #[test]
    fn the_count_of_patchless_files_is_stated_up_front() {
        let files = vec![
            entry("a.cxx", "added", None),
            entry("b.cxx", "added", None),
            entry("c.rs", "modified", Some("@@ -1 +1 @@\n+x\n")),
        ];
        let diff = render_files_diff(&files, false);
        assert!(diff.starts_with("[note: GitHub returned no patch for 2 of 3 files"));
        // The note must not break parsing of the file that does have a patch.
        assert!(crate::diff::parse_valid_lines(&diff).contains_key("c.rs"));
    }

    #[test]
    fn truncation_is_stated_in_the_diff() {
        let diff = render_files_diff(
            &[entry("a.rs", "modified", Some("@@ -1 +1 @@\n+x\n"))],
            true,
        );
        assert!(diff.starts_with("[note: this PR exceeds GitHub's 3000-file API ceiling"));
        // The note must not corrupt parsing of the files that follow.
        assert_eq!(crate::diff::split_diff_sections(&diff).len(), 2); // preamble + file
        assert!(crate::diff::parse_valid_lines(&diff).contains_key("a.rs"));
    }

    #[test]
    fn enc_path_preserves_slashes_but_escapes_query_injection() {
        // Normal nested paths pass through untouched.
        assert_eq!(
            enc_path("src/providers/github.rs"),
            "src/providers/github.rs"
        );
        // A path trying to smuggle a query param gets its `?`/`=` escaped.
        assert_eq!(enc_path("foo.rs?ref=evil"), "foo.rs%3Fref%3Devil");
        // Spaces and other unsafe bytes are escaped too.
        assert_eq!(enc_path("a b#c"), "a%20b%23c");
    }
}
