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
    ///
    /// There is deliberately no `posted` or `comment_url` here. This type carried
    /// both, hardcoded to `false` and `None`, with a doc comment claiming
    /// `crate::command` set them — it never did; it reads `summary_markdown`,
    /// posts, and returns its own `CommandOutcome` with the URL on it. Two dead
    /// fields in a published wire contract are worse than absent ones, because a
    /// consumer reads `posted: false` as a fact rather than as a field nobody
    /// fills in. A file review does not post; that is the operation, not a state.
    pub summary_markdown: String,
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

/// Why a path was refused, or the file it resolved to.
///
/// An enum rather than `Option<PathBuf>` because the caller has to tell "this
/// repository does not allow that file to be read" from "that file is not here" —
/// the first is a refusal to report, the second may be a typo.
pub(crate) enum ResolvedPath {
    /// Safe to read: inside the repository, allowed by its filters, a real file.
    File(std::path::PathBuf),
    Excluded(String),
    NotFound(String),
}

/// Resolve a repository-relative path to a real file, refusing anything that
/// escapes the checkout or that the repository's filters exclude.
///
/// Every local entry point goes through this, because each check below was a
/// real hole in something that looked correct:
///
/// - **Absolute paths.** `Path::join` *discards the base* when the argument is
///   absolute, so `repo_root.join("/etc/passwd")` is `/etc/passwd`. A caller who
///   thought joining a root confined the read was wrong, and the glob filter does
///   not help — the default config excludes lockfiles and build output, not the
///   filesystem.
/// - **`..` traversal**, for the same reason in a different spelling.
/// - **Symlinks out of the tree.** The two checks above are *lexical*: they read
///   the string, and a symlink is not in the string. `repo/notes.md -> /etc/passwd`
///   passes both and `read_to_string` follows it. Only canonicalizing and
///   comparing catches that.
/// - **Symlinks to an excluded file inside the tree**, which is the same bypass
///   aimed at `.env` rather than at `/etc`. The filters are re-applied to the
///   resolved path, not just the requested one.
/// - **Anything that is not a regular file.** `read_to_string` on a fifo blocks
///   forever, which turns a review into a hung process.
///
/// This matters because the file's contents are sent to a model — a read here is
/// a disclosure, not just a read.
pub(crate) fn resolve_repo_file(
    cfg: &Config,
    repo_root: &std::path::Path,
    path: &str,
) -> ResolvedPath {
    if path.split(['/', '\\']).any(|seg| seg == "..") || std::path::Path::new(path).is_absolute() {
        return ResolvedPath::Excluded(format!(
            "`{path}` must be a path inside the repository, relative to its root."
        ));
    }
    if !path_is_reviewable(cfg, path) {
        return ResolvedPath::Excluded(excluded_reason(path));
    }

    // Canonicalize both sides: comparing a canonical target against a
    // non-canonical root would reject every checkout reached through a symlink
    // (`/tmp` on macOS is one), which is a false refusal rather than a safe one.
    let Ok(root) = repo_root.canonicalize() else {
        return ResolvedPath::NotFound(format!("no repository at {}", repo_root.display()));
    };
    let full = root.join(path);
    let resolved = match full.canonicalize() {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return ResolvedPath::NotFound(format!("no file at {}", full.display()))
        }
        Err(e) => return ResolvedPath::NotFound(format!("cannot read {}: {e}", full.display())),
    };

    if !resolved.starts_with(&root) {
        return ResolvedPath::Excluded(format!(
            "`{path}` resolves outside the repository and was not read."
        ));
    }
    // Re-check the filters against what the path actually resolved to, so a
    // symlink cannot be used to reach an excluded file inside the tree.
    if let Ok(rel) = resolved.strip_prefix(&root) {
        let rel = rel.to_string_lossy().replace('\\', "/");
        if rel != path && !path_is_reviewable(cfg, &rel) {
            return ResolvedPath::Excluded(excluded_reason(&rel));
        }
    }
    match std::fs::metadata(&resolved) {
        Ok(m) if m.is_file() => ResolvedPath::File(resolved),
        Ok(_) => ResolvedPath::Excluded(format!("`{path}` is not a regular file.")),
        Err(e) => ResolvedPath::NotFound(format!("cannot stat {}: {e}", resolved.display())),
    }
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
    })
}

/// Review a file in a local checkout. No host, nothing posted.
///
/// # Errors
/// If the file cannot be read for a reason other than absence or refusal.
pub async fn review_local(
    cfg: &Config,
    backend: &dyn ReviewBackend,
    repo_root: &std::path::Path,
    path: &str,
) -> Result<FileReviewOutput> {
    let source = FileSource::Local {
        repo_root: repo_root.display().to_string(),
    };

    // The repository's own rules govern what may be read, so they are loaded
    // BEFORE the filter check — exactly as `review_pr_file` does below.
    //
    // This was missing, and the asymmetry is the bug: the PR path honoured a
    // repository's `exclude_globs` and the local path silently did not, so
    // `review-file` locally could read a file the same repository refuses to have
    // reviewed on a pull request. It also meant `min_confidence`, `max_findings`
    // and `model` came from the deployment rather than the repository, which is
    // the parity Phase 1 established for diff reviews and this path never got.
    let effective = crate::review::local_repo_config_source(cfg, Some(repo_root)).0;
    let cfg = &effective;

    let full = match resolve_repo_file(cfg, repo_root, path) {
        ResolvedPath::File(p) => p,
        ResolvedPath::Excluded(reason) => {
            return Ok(refused(
                path,
                source,
                FileReviewOutcome::Excluded { reason },
            ))
        }
        ResolvedPath::NotFound(reason) => {
            return Ok(refused(
                path,
                source,
                FileReviewOutcome::NotFound { reason },
            ))
        }
    };
    let content = std::fs::read_to_string(&full)
        .map_err(|e| anyhow::anyhow!("reading {}: {e}", full.display()))?;
    review_content(cfg, backend, path, &content, source).await
}

/// Review a file at a pull request's head. Reads only — nothing is posted.
///
/// Returns the resolved provider, client and config too, because the caller that
/// posts needs all three and re-deriving them would mean a second `.prbot.toml`
/// answer and a second connection pool for one comment.
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
) -> Result<(FileReviewOutput, PrFileContext)> {
    let provider = crate::providers::Provider::from_name(provider_name)?;
    let client = reqwest::Client::new();
    let meta = provider.get_meta(&client, cfg, repo, pr).await?;
    // Everything the posting adapter would otherwise rebuild.
    let ctx = |cfg: Config| PrFileContext {
        provider,
        client: client.clone(),
        cfg,
    };
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
            ctx(effective),
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
                ctx(effective),
            ))
        }
    };
    let out = review_content(&effective, backend, path, &content, source).await?;
    Ok((out, ctx(effective)))
}

/// What [`review_pr_file`] already resolved, for a caller that goes on to post.
///
/// Handed back rather than rebuilt: a second `Provider::from_name` is harmless,
/// but a second `reqwest::Client` is a second connection pool and TLS handshake
/// for one comment, and a second `load_repo_config` is a second answer to a
/// question that must only have one.
pub struct PrFileContext {
    pub provider: crate::providers::Provider,
    pub client: reqwest::Client,
    pub cfg: Config,
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

#[cfg(test)]
mod path_resolution_tests {
    //! Every case here was a way to make `review-file` read something it should
    //! not have. The file's contents go to a model, so a read is a disclosure.

    use super::{resolve_repo_file, ResolvedPath};
    use crate::config::Config;

    fn cfg() -> Config {
        let mut c = Config::from_env();
        c.include_globs = Vec::new();
        c.exclude_globs = vec!["**/.env".to_string()];
        c
    }

    fn excluded(r: &ResolvedPath) -> &str {
        match r {
            ResolvedPath::Excluded(reason) => reason,
            ResolvedPath::File(p) => panic!("must not resolve, got {}", p.display()),
            ResolvedPath::NotFound(r) => panic!("expected a refusal, got not-found: {r}"),
        }
    }

    /// `Path::join` DISCARDS the base when the argument is absolute, so
    /// `repo_root.join("/etc/passwd")` is `/etc/passwd`. Verified against the
    /// real API, not assumed — a caller who thought joining a root confined the
    /// read was simply wrong, and the glob filter does not help because the
    /// default config excludes lockfiles, not the filesystem.
    #[test]
    // Demonstrating the hazard IS the test. Clippy has a lint for this exact
    // footgun, which is the best evidence that the check above earns its keep.
    #[allow(clippy::join_absolute_paths)]
    fn an_absolute_path_cannot_escape_the_repository() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(
            dir.path().join("/etc/passwd"),
            std::path::Path::new("/etc/passwd"),
            "join() discards the base — this is why the check exists"
        );
        let r = resolve_repo_file(&cfg(), dir.path(), "/etc/passwd");
        assert!(excluded(&r).contains("inside the repository"));
    }

    #[test]
    fn dot_dot_traversal_cannot_escape_the_repository() {
        let dir = tempfile::tempdir().expect("tempdir");
        let r = resolve_repo_file(&cfg(), dir.path(), "../../../etc/passwd");
        assert!(excluded(&r).contains("inside the repository"));
    }

    /// The lexical checks above read the STRING, and a symlink is not in the
    /// string. `repo/notes.md -> /etc/passwd` passes both of them, and
    /// `read_to_string` follows it. Only canonicalizing and comparing catches it.
    #[test]
    #[cfg(unix)]
    fn a_symlink_out_of_the_tree_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outside = dir.path().join("outside.txt");
        std::fs::write(&outside, "a secret\n").expect("write");

        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).expect("mkdir");
        std::os::unix::fs::symlink(&outside, repo.join("notes.md")).expect("symlink");

        let r = resolve_repo_file(&cfg(), &repo, "notes.md");
        assert!(
            excluded(&r).contains("outside the repository"),
            "{}",
            excluded(&r)
        );
    }

    /// The same bypass aimed at an excluded file *inside* the tree rather than
    /// at `/etc`: `allowed.md -> .env` passes the filters on its own name.
    #[test]
    #[cfg(unix)]
    fn a_symlink_to_an_excluded_file_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(".env"), "TOKEN=shhh\n").expect("write");
        std::os::unix::fs::symlink(dir.path().join(".env"), dir.path().join("allowed.md"))
            .expect("symlink");

        let r = resolve_repo_file(&cfg(), dir.path(), "allowed.md");
        assert!(excluded(&r).contains("excluded"), "{}", excluded(&r));
    }

    /// `read_to_string` on a fifo blocks forever, turning a review into a hung
    /// process rather than a failed one.
    #[test]
    fn a_directory_is_not_a_reviewable_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join("src")).expect("mkdir");
        let r = resolve_repo_file(&cfg(), dir.path(), "src");
        assert!(
            excluded(&r).contains("not a regular file"),
            "{}",
            excluded(&r)
        );
    }

    /// A checkout reached THROUGH a symlink (`/tmp` on macOS is one) must still
    /// work — comparing a canonical target against a non-canonical root would
    /// refuse every such repository, which is a false refusal, not a safe one.
    #[test]
    fn an_ordinary_file_in_the_repository_resolves() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join("src")).expect("mkdir");
        std::fs::write(dir.path().join("src/a.rs"), "fn main() {}\n").expect("write");

        match resolve_repo_file(&cfg(), dir.path(), "src/a.rs") {
            ResolvedPath::File(p) => assert!(p.ends_with("src/a.rs"), "{}", p.display()),
            ResolvedPath::Excluded(r) | ResolvedPath::NotFound(r) => {
                panic!("an ordinary file must resolve: {r}")
            }
        }
    }

    /// An excluded path is refused before the filesystem is touched at all.
    #[test]
    fn an_excluded_path_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(".env"), "TOKEN=shhh\n").expect("write");
        let r = resolve_repo_file(&cfg(), dir.path(), ".env");
        assert!(excluded(&r).contains("excluded"));
    }

    /// A missing file is not a refusal — it may be a typo, and the caller has to
    /// be able to tell the two apart.
    #[test]
    fn a_missing_file_is_reported_as_missing_not_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        match resolve_repo_file(&cfg(), dir.path(), "src/nope.rs") {
            ResolvedPath::NotFound(r) => assert!(r.contains("no file at"), "{r}"),
            ResolvedPath::File(_) => panic!("there is no such file"),
            ResolvedPath::Excluded(r) => panic!("a missing file is not a refusal: {r}"),
        }
    }
}
