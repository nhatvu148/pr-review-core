//! Orchestrator: fetch the diff, run the structured AI review, anchor findings
//! to diff lines, render a summary, and (unless dry-run) post the review.

use anyhow::Result;
use serde::Serialize;

use crate::agent::agentic_review;
use crate::config::Config;
use crate::diff::parse_valid_lines;
use crate::llm::{review_diff, Finding, Review, ReviewResult, Usage};
use crate::providers::{InlineComment, PrMeta, Provider, ReviewPost};
use crate::repo::Workspace;

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
async fn run_agentic(
    provider: &Provider,
    client: &reqwest::Client,
    cfg: &Config,
    meta: &PrMeta,
    diff: &str,
    repo: &str,
) -> Result<ReviewResult> {
    let url = provider.clone_url(cfg, repo)?;
    let sha = meta.head_sha.clone();
    // git clone is blocking — keep it off the async worker threads.
    let ws = tokio::task::spawn_blocking(move || Workspace::clone(&url, sha.as_deref())).await??;
    agentic_review(client, cfg, meta, diff, &ws).await
}

/// Review one pull request end-to-end.
///
/// # Errors
/// On unknown provider, empty diff, or any provider/LLM API failure.
pub async fn run_review(cfg: &Config, input: RunReviewInput) -> Result<RunReviewOutput> {
    let provider = Provider::from_name(&input.provider)?;
    let client = reqwest::Client::new();

    let meta = provider
        .get_meta(&client, cfg, &input.repo, input.pr)
        .await?;

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

    let diff = provider
        .get_diff(&client, cfg, &input.repo, input.pr)
        .await?;
    if diff.trim().is_empty() {
        anyhow::bail!("PR diff is empty — nothing to review.");
    }

    // Agentic path (clone + tools) when enabled; falls back to diff-only on any
    // failure so a clone/agent hiccup never drops the review entirely.
    let result = if cfg.agentic {
        match run_agentic(&provider, &client, cfg, &meta, &diff, &input.repo).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(
                    "agentic review failed for {}#{} ({e:#}); falling back to diff-only",
                    input.repo,
                    input.pr
                );
                review_diff(&client, cfg, &meta, &diff).await?
            }
        }
    } else {
        review_diff(&client, cfg, &meta, &diff).await?
    };
    let valid = parse_valid_lines(&diff);

    // Anchor findings whose (file, line) is actually in the diff; the rest fold
    // into the summary so the provider never rejects an out-of-diff anchor.
    let mut inline: Vec<InlineComment> = Vec::new();
    let mut unanchored: Vec<&Finding> = Vec::new();
    for f in &result.review.findings {
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

    let summary = render_summary(&result.review, &unanchored, inline.len());
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
        findings: result.review.findings.len(),
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
