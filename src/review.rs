//! Orchestrator: fetch the diff, run the structured AI review, anchor findings
//! to diff lines, render a summary, and (unless dry-run) post the review.

use anyhow::Result;
use serde::Serialize;

use crate::agent::agentic_review;
use crate::backend::{OpenRouterBackend, ReviewBackend, ReviewContext};
use crate::config::Config;
use crate::diff::parse_valid_lines;
use crate::llm::{Finding, Review, ReviewResult, Usage};
use crate::providers::{InlineComment, PrMeta, Provider, ReviewPost};
use crate::repo::Workspace;
use crate::repo_config;

pub struct RunReviewInput {
    pub provider: String,
    /// `owner/repo` (GitHub) or `workspace/repo` (Bitbucket).
    pub repo: String,
    pub pr: u64,
    /// When true, generate the review but do NOT post it.
    pub dry_run: bool,
    /// When true, post a "Reviewing…" placeholder comment before the LLM call so
    /// the PR shows instant feedback (used on the webhook path). Ignored on dry-run.
    pub placeholder: bool,
}

/// Result of one review run (serialized as the HTTP/CLI response).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunReviewOutput {
    pub provider: String,
    pub repo: String,
    pub pr: u64,
    pub model: String,
    pub recommendation: String,
    pub findings: usize,
    pub inline_posted: usize,
    pub posted: bool,
    pub comment_url: Option<String>,
    pub summary_markdown: String,
    pub usage: Option<Usage>,
}

/// Rank a severity for sorting (higher = more urgent). Unknown severities rank 0.
fn severity_rank(sev: &str) -> u8 {
    match sev.to_uppercase().as_str() {
        "BLOCKING" => 3,
        "HIGH" => 2,
        "MEDIUM" => 1,
        "LOW" => 0,
        _ => 0,
    }
}

fn severity_emoji(sev: &str) -> &'static str {
    match sev.to_uppercase().as_str() {
        "BLOCKING" => "🚨",
        "HIGH" => "⚠️",
        "MEDIUM" => "ℹ️",
        "LOW" => "💡",
        _ => "•",
    }
}

/// Body for an inline comment: `<emoji> **SEVERITY** — <problem>. Fix: …`
fn inline_body(f: &Finding) -> String {
    format!(
        "{} **{}** — {}",
        severity_emoji(&f.severity),
        f.severity.to_uppercase(),
        f.body.trim()
    )
}

/// The summary comment: overall + recommendation + any findings that couldn't be
/// anchored to a diff line (line-anchored ones go inline).
fn render_summary(review: &Review, unanchored: &[&Finding], inline_count: usize) -> String {
    let mut s = format!(
        "🤖 **Automated review**\n\n{}\n\n**Recommendation:** {}",
        review.summary.trim(),
        review.recommendation.trim()
    );
    if inline_count > 0 {
        s.push_str(&format!("\n\n_{inline_count} inline comment(s) below._"));
    }
    if unanchored.is_empty() {
        if inline_count == 0 {
            s.push_str("\n\nNo blocking issues found.");
        }
    } else {
        s.push_str("\n\n## Findings");
        for f in unanchored {
            let loc = match f.line {
                Some(l) => format!("`{}` (~{l})", f.file),
                None => format!("`{}`", f.file),
            };
            s.push_str(&format!(
                "\n- {} **{}** — {loc} — {}",
                severity_emoji(&f.severity),
                f.severity.to_uppercase(),
                f.body.trim()
            ));
        }
    }
    s.push_str("\n\n_Automated advisory review — a human still owns the merge decision._");
    s
}

/// Placeholder summary body shown immediately while the review runs.
fn render_pending() -> String {
    "🤖 **Automated review**\n\n⏳ _Reviewing this PR… (this comment will update shortly)_"
        .to_string()
}

/// Clone the repo (off the async runtime) and run the agentic reviewer.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_agentic(
    provider: &Provider,
    client: &reqwest::Client,
    cfg: &Config,
    meta: &PrMeta,
    diff: &str,
    omitted_note: Option<&str>,
    structural_context: Option<&str>,
    repo: &str,
) -> Result<ReviewResult> {
    let url = provider.clone_url(cfg, repo)?;
    let sha = meta.head_sha.clone();
    // git clone is blocking — keep it off the async worker threads.
    let ws = tokio::task::spawn_blocking(move || Workspace::clone(&url, sha.as_deref())).await??;
    agentic_review(
        client,
        cfg,
        meta,
        diff,
        omitted_note,
        structural_context,
        &ws,
    )
    .await
}

/// Load an optional per-repo `.prbot.toml` and merge it over `base`, returning the
/// effective config for this one review.
///
/// Fully fail-open: a missing file, a fetch error, or a parse error all log and
/// return `base.clone()` so a repo config problem can never break the review.
pub(crate) async fn load_repo_config(
    provider: &Provider,
    client: &reqwest::Client,
    base: &Config,
    repo: &str,
    meta: &PrMeta,
) -> Config {
    // Prefer the exact head commit; fall back to the base branch when the provider
    // didn't give us a head SHA (e.g. Bitbucket meta). If neither is available,
    // there's nothing to fetch against — use the base config as-is.
    let git_ref = match (meta.head_sha.as_deref(), meta.base_branch.as_deref()) {
        (Some(sha), _) if !sha.is_empty() => sha,
        (_, Some(branch)) if !branch.is_empty() => branch,
        _ => return base.clone(),
    };

    match provider
        .get_file_contents(client, base, repo, git_ref, ".prbot.toml")
        .await
    {
        Ok(Some(text)) => match repo_config::parse(&text) {
            Ok(rc) => {
                tracing::info!("applied .prbot.toml overrides for {repo}");
                base.with_repo_overrides(&rc)
            }
            Err(e) => {
                tracing::warn!("ignoring invalid .prbot.toml for {repo}: {e:#}");
                base.clone()
            }
        },
        // No file, or any fetch error — proceed with the base config (fail-open).
        Ok(None) => base.clone(),
        Err(e) => {
            tracing::warn!("could not fetch .prbot.toml for {repo}: {e:#}");
            base.clone()
        }
    }
}

/// Post an advisory-only summary when the reviewable diff is empty but a
/// dependency scan found something (e.g. a lockfile-only PR). Skipped on dry-run.
async fn post_advisory_only(
    provider: &Provider,
    client: &reqwest::Client,
    cfg: &Config,
    meta: &PrMeta,
    input: &RunReviewInput,
    advisories: Vec<crate::deps::DepAdvisory>,
) -> Result<RunReviewOutput> {
    let mut summary = String::from(
        "🤖 **Automated review**\n\nNo reviewable source changes (dependency/lockfile-only PR).",
    );
    summary.push_str("\n\n");
    summary.push_str(&crate::deps::render_advisories(&advisories));
    summary.push_str("\n\n_Automated advisory review — a human still owns the merge decision._");

    let post = ReviewPost {
        summary: summary.clone(),
        inline: Vec::new(),
    };
    let mut out = RunReviewOutput {
        provider: provider.name().to_string(),
        repo: input.repo.clone(),
        pr: input.pr,
        model: cfg.openrouter_model.clone(),
        recommendation: "APPROVE WITH CHANGES".to_string(),
        findings: 0,
        inline_posted: 0,
        posted: false,
        comment_url: None,
        summary_markdown: summary,
        usage: None,
    };
    if !input.dry_run {
        out.comment_url = provider.post_review(client, cfg, meta, &post).await?;
        out.posted = true;
    }
    Ok(out)
}

/// Review one pull request end-to-end, using the default [`OpenRouterBackend`]
/// (a Claude model via OpenRouter).
///
/// # Errors
/// On unknown provider, empty diff, or any provider/LLM API failure.
pub async fn run_review(cfg: &Config, input: RunReviewInput) -> Result<RunReviewOutput> {
    run_review_with(cfg, input, &OpenRouterBackend).await
}

/// Review one pull request end-to-end with a caller-supplied [`ReviewBackend`].
///
/// Identical to [`run_review`] except the model step is delegated to `backend`,
/// letting a consumer plug in a different reviewer (e.g. an AI agent CLI) while
/// reusing all of the diff preparation, finding post-processing, anchoring, and
/// posting logic. [`run_review`] is just this with [`OpenRouterBackend`].
///
/// # Errors
/// On unknown provider, empty diff, or any provider/backend failure.
pub async fn run_review_with(
    cfg: &Config,
    input: RunReviewInput,
    backend: &dyn ReviewBackend,
) -> Result<RunReviewOutput> {
    let provider = Provider::from_name(&input.provider)?;
    let client = reqwest::Client::new();

    let meta = provider
        .get_meta(&client, cfg, &input.repo, input.pr)
        .await?;

    // Merge an optional per-repo `.prbot.toml` (fetched from the PR head) over the
    // env config; shadow `cfg` so every step below — glob filter, model choice,
    // agentic decision, self-critique, caps, and prompt — honors the overrides.
    let effective = load_repo_config(&provider, &client, cfg, &input.repo, &meta).await;
    let cfg = &effective;

    // Instant feedback: drop a "Reviewing…" summary comment before the slow LLM
    // call. It's upserted, so the real review updates this same comment.
    if input.placeholder && !input.dry_run {
        let pending = ReviewPost {
            summary: render_pending(),
            inline: Vec::new(),
        };
        if let Err(e) = provider.post_review(&client, cfg, &meta, &pending).await {
            tracing::warn!(
                "placeholder comment failed for {}#{}: {e:#}",
                input.repo,
                input.pr
            );
        }
    }

    let raw_diff = provider
        .get_diff(&client, cfg, &input.repo, input.pr)
        .await?;

    // Dependency vulnerability scan runs on the RAW diff: lockfiles are dropped
    // by the glob filter below (and never reach the LLM), so we must read added
    // dependency lines before that. Fully fail-open — returns [] on any error.
    let advisories = crate::deps::scan(&client, cfg, &raw_diff).await;
    if !advisories.is_empty() {
        tracing::info!(
            "OSV: {} dependency advisor(y/ies) for {}#{}",
            advisories.len(),
            input.repo,
            input.pr
        );
    }

    // Drop noisy files (lockfiles, generated, vendored, minified) before the LLM
    // sees the diff — saves tokens and noise. Fail-open: never loses the review.
    let (diff, dropped) =
        crate::diff::filter_diff_by_globs(&raw_diff, &cfg.include_globs, &cfg.exclude_globs);
    if !dropped.is_empty() {
        tracing::info!("skipped {} file(s) by glob: {:?}", dropped.len(), dropped);
    }

    // If every changed file was filtered out (e.g. a lockfile-only PR) there's
    // nothing for the LLM to review — but a dependency advisory found on those
    // lockfiles still deserves a comment. Post an advisory-only summary and return.
    if diff.trim().is_empty() {
        if !advisories.is_empty() {
            return post_advisory_only(&provider, &client, cfg, &meta, &input, advisories).await;
        }
        anyhow::bail!(
            "PR diff is empty (all files excluded by globs, or no changes) — nothing to review."
        );
    }

    // Smart size handling: keep whole files, dropping the lowest-priority ones
    // first, until the diff fits `max_diff_chars` — instead of a blunt mid-file
    // char cut. Applied ONCE here so both review paths get the same packed diff.
    let (diff, packed_dropped) = crate::diff::pack_diff(&diff, cfg.max_diff_chars);
    if !packed_dropped.is_empty() {
        tracing::info!(
            "packed diff: omitted {} lower-priority file(s) to fit budget: {:?}",
            packed_dropped.len(),
            packed_dropped
        );
    }
    // Surfaced to the model so it knows these files were NOT reviewed.
    let omitted_note = (!packed_dropped.is_empty()).then(|| {
        format!(
            "{} file(s) were omitted to fit the size limit and were NOT reviewed: {}",
            packed_dropped.len(),
            packed_dropped.join(", ")
        )
    });

    // Structural context: name the enclosing function/symbol of each changed line
    // so the model knows every change's scope. Tier B (tree-sitter over fetched
    // files) with a Tier A (hunk-header) fallback — fully fail-open, so a hiccup
    // just yields an empty string and the review proceeds without it.
    let structural = if cfg.structural_context {
        crate::structure::structural_context(&provider, &client, cfg, &input.repo, &meta, &diff)
            .await
    } else {
        String::new()
    };
    if !structural.is_empty() {
        tracing::info!(
            "structural context for {}#{}: {} line(s)",
            input.repo,
            input.pr,
            structural.lines().count()
        );
    }
    let structural_opt = (!structural.is_empty()).then_some(structural.as_str());

    // Delegate the model step to the backend. The default OpenRouterBackend runs
    // the agentic path (clone + tools) when enabled and falls back to diff-only
    // on failure; a custom backend (e.g. an agent CLI) decides its own strategy.
    let ctx = ReviewContext {
        client: &client,
        cfg,
        provider: &provider,
        repo: &input.repo,
        meta: &meta,
        diff: &diff,
        omitted_note: omitted_note.as_deref(),
        structural_context: structural_opt,
    };
    let result = backend.review(&ctx).await?;
    // Post-process findings before anchoring: optional self-critique pass, then a
    // confidence floor, severity sort, and a hard cap — cuts noise before posting.
    let mut findings = result.review.findings.clone();
    if cfg.self_critique && !findings.is_empty() {
        findings = match crate::llm::critique_findings(&client, cfg, &meta, &diff, &findings).await
        {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!("self-critique failed ({e:#}); keeping original findings");
                findings
            }
        };
    }
    findings.retain(|f| f.confidence.unwrap_or(100) >= cfg.min_confidence);
    findings.sort_by(|a, b| {
        severity_rank(&b.severity)
            .cmp(&severity_rank(&a.severity))
            .then(b.confidence.unwrap_or(0).cmp(&a.confidence.unwrap_or(0)))
    });
    findings.truncate(cfg.max_findings);

    let valid = parse_valid_lines(&diff);

    // Anchor findings whose (file, line) is actually in the diff; the rest fold
    // into the summary so the provider never rejects an out-of-diff anchor.
    let mut inline: Vec<InlineComment> = Vec::new();
    let mut unanchored: Vec<&Finding> = Vec::new();
    for f in &findings {
        let anchored = f
            .line
            .is_some_and(|l| valid.get(&f.file).is_some_and(|s| s.contains(&l)));
        match (anchored, f.line) {
            (true, Some(line)) => inline.push(InlineComment {
                path: f.file.clone(),
                line,
                body: inline_body(f),
            }),
            _ => unanchored.push(f),
        }
    }

    let mut summary = render_summary(&result.review, &unanchored, inline.len());
    if !advisories.is_empty() {
        summary.push_str("\n\n");
        summary.push_str(&crate::deps::render_advisories(&advisories));
    }
    let inline_count = inline.len();
    let post = ReviewPost {
        summary: summary.clone(),
        inline,
    };

    let mut out = RunReviewOutput {
        provider: provider.name().to_string(),
        repo: input.repo.clone(),
        pr: input.pr,
        model: result.model,
        recommendation: result.review.recommendation.clone(),
        findings: findings.len(),
        inline_posted: inline_count,
        posted: false,
        comment_url: None,
        summary_markdown: summary,
        usage: result.usage,
    };

    if !input.dry_run {
        out.comment_url = provider.post_review(&client, cfg, &meta, &post).await?;
        out.posted = true;
    }

    Ok(out)
}
