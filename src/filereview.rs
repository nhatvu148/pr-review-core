//! Deep-review one complete file, as reusable stages.
//!
//! `/review-file <path>` has existed as a PR comment command since before there
//! was a toolbox, but only as one function that fetched, reviewed, rendered and
//! **posted** — so the only way to get a file reviewed was to have a pull request
//! and to be willing to write a comment on it. A coding agent wants the finding,
//! not the comment, and usually has the file on disk rather than on a host.
//!
//! The stages here are the same ones that function performed, in the same order,
//! with the posting removed to its own caller ([`crate::command`]):
//!
//! 1. authorize the path against the repository's include/exclude globs,
//! 2. read the file — from a checkout, or from the PR head,
//! 3. review it through the selected [`ReviewBackend`],
//! 4. apply the review path's confidence floor, severity sort and cap,
//! 5. render.
//!
//! Step 1 is not a formality. It is what stops `/review-file .env` from printing a
//! repository's secrets into a PR comment, and it must keep applying to every new
//! caller — which is why it lives at the front of this shared path rather than in
//! the command adapter that happens to have needed it first.

use anyhow::Result;
use serde::Serialize;

use crate::backend::ReviewBackend;
use crate::config::Config;
use crate::llm::Finding;

/// Where the reviewed file was read from.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", tag = "kind")]
#[schemars(rename_all = "camelCase")]
pub enum FileSource {
    /// A checkout on disk.
    // Per-variant, for the reason noted on `rules::RulesScope::Local`.
    #[serde(rename_all = "camelCase")]
    #[schemars(rename_all = "camelCase")]
    Local { repo_root: String },
    /// A host, at a specific ref — named, because "the file" is meaningless
    /// without saying which revision of it was read.
    #[serde(rename_all = "camelCase")]
    #[schemars(rename_all = "camelCase")]
    Pr {
        provider: String,
        repo: String,
        pr: u64,
        git_ref: String,
    },
}

/// What came of the request.
///
/// Refusals are outcomes, not errors. A caller needs to tell "this path is
/// excluded by the repository's own rules" from "the reviewer crashed", and an
/// agent that receives the first as an `Err` will retry it forever.
#[derive(Debug, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", tag = "status")]
#[schemars(rename_all = "camelCase")]
pub enum FileReviewOutcome {
    /// The file was read and reviewed.
    Reviewed {
        summary: String,
        recommendation: String,
        /// After the confidence floor, severity sort and cap — the same policy a
        /// diff review applies, so a finding here means what it means there.
        findings: Vec<Finding>,
    },
    /// The path is excluded by this repository's review file filters.
    Excluded { reason: String },
    /// No such file at the resolved source.
    NotFound { reason: String },
}

/// One file review.
#[derive(Debug, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(rename_all = "camelCase")]
pub struct FileReviewOutput {
    pub path: String,
    pub source: FileSource,
    pub outcome: FileReviewOutcome,
    /// The result as a comment body — what the PR command posts, and what a CLI
    /// caller prints. Rendered even when nothing is posted, so `--json` and a
    /// posted comment cannot say different things.
    pub summary_markdown: String,
    /// Always false here. Posting belongs to [`crate::command`], which sets it.
    pub posted: bool,
    pub comment_url: Option<String>,
}

/// Whether this repository's filters allow the path to be reviewed at all.
///
/// Separated out because it is the security-relevant step and every entry point
/// has to take it. Vendored paths are allowed through: a deliberate request to
/// read third-party source is not the same as sweeping it into a diff, and the
/// prompt already tells the reviewer not to propose edits there.
#[must_use]
pub fn path_is_reviewable(cfg: &Config, path: &str) -> bool {
    crate::diff::path_matches_globs(path, &cfg.include_globs, &cfg.exclude_globs)
}

/// The message shown when a path is refused, shared so the PR comment and the
/// CLI refuse in the same words.
#[must_use]
fn excluded_reason(path: &str) -> String {
    format!("`{path}` is excluded by this repository's review file filters, so it was not read.")
}

/// Review a file already in hand.
///
/// The core of the operation, with no I/O of its own beyond the model call: both
/// the local and the PR entry points converge here, so the review policy cannot
/// differ between them.
pub async fn review_content(
    cfg: &Config,
    backend: &dyn ReviewBackend,
    path: &str,
    content: &str,
    source: FileSource,
) -> Result<FileReviewOutput> {
    let review = crate::llm::review_file(cfg, backend, path, content).await?;

    // Same post-processing as a diff review: confidence floor, severity sort, cap.
    let mut findings = review.findings.clone();
    findings.retain(|f| f.confidence.unwrap_or(100) >= cfg.min_confidence);
    findings.sort_by(|a, b| {
        crate::review::severity_rank(&b.severity)
            .cmp(&crate::review::severity_rank(&a.severity))
            // Secondary key: higher confidence first — matches the `/review` path.
            .then(b.confidence.unwrap_or(0).cmp(&a.confidence.unwrap_or(0)))
    });
    findings.truncate(cfg.max_findings);

    let summary_markdown = render(path, &review, &findings);
    Ok(FileReviewOutput {
        path: path.to_string(),
        source,
        outcome: FileReviewOutcome::Reviewed {
            summary: review.summary,
            recommendation: review.recommendation,
            findings,
        },
        summary_markdown,
        posted: false,
        comment_url: None,
    })
}

/// Review a file in a local checkout. No host, nothing posted.
///
/// # Errors
/// If the file cannot be read for a reason other than absence.
pub async fn review_local(
    cfg: &Config,
    backend: &dyn ReviewBackend,
    repo_root: &std::path::Path,
    path: &str,
) -> Result<FileReviewOutput> {
    let source = FileSource::Local {
        repo_root: repo_root.display().to_string(),
    };

    // Before touching the filesystem: the filters decide what may be read, and a
    // check placed after the read has already read it.
    if !path_is_reviewable(cfg, path) {
        return Ok(refused(
            path,
            source,
            FileReviewOutcome::Excluded {
                reason: excluded_reason(path),
            },
        ));
    }

    // `path` is repository-relative by contract, and a caller that sends
    // `../../etc/passwd` must not get it. Rejecting traversal outright is clearer
    // than canonicalizing and comparing, and does not depend on the file existing.
    if path.split(['/', '\\']).any(|seg| seg == "..") || std::path::Path::new(path).is_absolute() {
        return Ok(refused(
            path,
            source,
            FileReviewOutcome::Excluded {
                reason: format!(
                    "`{path}` must be a path inside the repository, relative to its root."
                ),
            },
        ));
    }

    let full = repo_root.join(path);
    let content = match std::fs::read_to_string(&full) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(refused(
                path,
                source,
                FileReviewOutcome::NotFound {
                    reason: format!("no file at {}", full.display()),
                },
            ))
        }
        Err(e) => return Err(anyhow::anyhow!("reading {}: {e}", full.display())),
    };
    review_content(cfg, backend, path, &content, source).await
}

/// Review a file at a pull request's head. Reads only — nothing is posted.
///
/// Returns the resolved config too, because the caller that posts needs the same
/// merged `.prbot.toml` this used and re-deriving it would be a second answer.
///
/// # Errors
/// If the provider is unknown, the PR metadata cannot be fetched, or the file
/// fetch fails for a reason other than absence.
pub async fn review_pr_file(
    cfg: &Config,
    backend: &dyn ReviewBackend,
    provider_name: &str,
    repo: &str,
    pr: u64,
    path: &str,
) -> Result<(FileReviewOutput, Config)> {
    let provider = crate::providers::Provider::from_name(provider_name)?;
    let client = reqwest::Client::new();
    let meta = provider.get_meta(&client, cfg, repo, pr).await?;
    // The repository's own rules govern what may be read — so they are loaded
    // before the filter check, not after it.
    let effective = crate::review::load_repo_config(&provider, &client, cfg, repo, &meta).await;

    let git_ref = match (meta.head_sha.as_deref(), meta.base_branch.as_deref()) {
        (Some(s), _) if !s.is_empty() => s.to_string(),
        (_, Some(b)) if !b.is_empty() => b.to_string(),
        _ => anyhow::bail!("no git ref to fetch `{path}` against"),
    };
    let source = FileSource::Pr {
        provider: provider.name().to_string(),
        repo: repo.to_string(),
        pr,
        git_ref: git_ref.clone(),
    };

    if !path_is_reviewable(&effective, path) {
        return Ok((
            refused(
                path,
                source,
                FileReviewOutcome::Excluded {
                    reason: excluded_reason(path),
                },
            ),
            effective,
        ));
    }

    let content = match provider
        .get_file_contents(&client, &effective, repo, &git_ref, path)
        .await?
    {
        Some(c) => c,
        None => {
            return Ok((
                refused(
                    path,
                    source,
                    FileReviewOutcome::NotFound {
                        reason: format!("no `{path}` at {repo}@{git_ref}"),
                    },
                ),
                effective,
            ))
        }
    };
    let out = review_content(&effective, backend, path, &content, source).await?;
    Ok((out, effective))
}

/// Build the output for a request that was refused before any model call.
fn refused(path: &str, source: FileSource, outcome: FileReviewOutcome) -> FileReviewOutput {
    let summary_markdown = match &outcome {
        FileReviewOutcome::Excluded { reason } | FileReviewOutcome::NotFound { reason } => {
            format!("🔍 **File review — `{path}`**\n\n{reason}")
        }
        FileReviewOutcome::Reviewed { .. } => unreachable!("a refusal is not a review"),
    };
    FileReviewOutput {
        path: path.to_string(),
        source,
        outcome,
        summary_markdown,
        posted: false,
        comment_url: None,
    }
}

/// Render a file review as a comment body.
pub(crate) fn render(
    path: &str,
    review: &crate::llm::Review,
    findings: &[crate::llm::Finding],
) -> String {
    let mut s = format!(
        "🔍 **File review — `{path}`**\n\n{}\n\n**Recommendation:** {}",
        review.summary.trim(),
        review.recommendation.trim()
    );
    if findings.is_empty() {
        s.push_str("\n\nNo issues found.");
    } else {
        s.push_str("\n\n## Findings");
        for f in findings {
            let loc = f.line.map(|l| format!(" (line {l})")).unwrap_or_default();
            s.push_str(&format!(
                "\n- {} **{}** — `{path}`{loc} — {}",
                crate::review::severity_emoji(&f.severity),
                f.severity.to_uppercase(),
                f.body.trim()
            ));
        }
    }
    s.push_str("\n\n_Automated advisory review — a human still owns the merge decision._");
    s
}
