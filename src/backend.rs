//! Pluggable review backend: the seam that decides *how* a `Review` is produced
//! from a PR's diff and repo context.
//!
//! The orchestrator ([`crate::review::run_review_with`]) owns everything around
//! the model call — fetching the diff, glob filtering, packing, structural
//! context, dependency scan, finding post-processing, anchoring, and posting.
//! A [`ReviewBackend`] owns only the one step in the middle: turn the prepared
//! context into a [`ReviewResult`].
//!
//! The default [`OpenRouterBackend`] reviews via a Claude model over OpenRouter
//! (agentic tool-loop when enabled, diff-only otherwise). Consumers can supply
//! their own backend — e.g. an AI agent CLI (Claude Code, Codex, Gemini) driven
//! as a subprocess over a clone of the repo — by implementing this trait and
//! calling [`crate::review::run_review_with`].

use anyhow::Result;
use async_trait::async_trait;

use crate::config::Config;
use crate::llm::{review_diff, ReviewResult};
use crate::providers::{PrMeta, Provider};
use crate::review::run_agentic;

/// Everything a [`ReviewBackend`] needs to produce a review. The diff is already
/// glob-filtered and packed to fit the size budget; `omitted_note` and
/// `structural_context` are optional hints the backend may fold into its prompt.
///
/// A backend that wants a working tree (e.g. to point an agent CLI's `cwd` at a
/// checkout) can clone one itself via [`Provider::clone_url`] + `Workspace::clone`
/// using `provider` and `repo`.
pub struct ReviewContext<'a> {
    pub client: &'a reqwest::Client,
    pub cfg: &'a Config,
    pub provider: &'a Provider,
    /// `owner/repo` (GitHub) or `workspace/repo` (Bitbucket).
    pub repo: &'a str,
    pub meta: &'a PrMeta,
    /// The filtered, packed diff the model should review.
    pub diff: &'a str,
    /// Note about files omitted to fit the size budget (NOT reviewed), if any.
    pub omitted_note: Option<&'a str>,
    /// Enclosing-symbol context for changed lines, if computed.
    pub structural_context: Option<&'a str>,
}

/// Produces a structured [`ReviewResult`] from a prepared [`ReviewContext`].
///
/// Implementations decide *how* the review is generated (HTTP model call, a
/// subprocess agent CLI, a local model, …); the orchestrator handles everything
/// before and after. Object-safe: used as `&dyn ReviewBackend`.
#[async_trait]
pub trait ReviewBackend: Send + Sync {
    async fn review(&self, ctx: &ReviewContext<'_>) -> Result<ReviewResult>;
}

/// Default backend: reviews with a Claude model via OpenRouter.
///
/// Uses the agentic tool-loop (clone + read-only `grep`/`read_file`/`list_dir`)
/// when `cfg.agentic` is set, falling back to a diff-only call if the clone or
/// agent loop fails — so a hiccup never drops the review. With `cfg.agentic`
/// off, goes straight to the diff-only path.
pub struct OpenRouterBackend;

#[async_trait]
impl ReviewBackend for OpenRouterBackend {
    async fn review(&self, ctx: &ReviewContext<'_>) -> Result<ReviewResult> {
        if ctx.cfg.agentic {
            match run_agentic(
                ctx.provider,
                ctx.client,
                ctx.cfg,
                ctx.meta,
                ctx.diff,
                ctx.omitted_note,
                ctx.structural_context,
                ctx.repo,
            )
            .await
            {
                Ok(r) => Ok(r),
                Err(e) => {
                    tracing::warn!(
                        "agentic review failed for {}#{} ({e:#}); falling back to diff-only",
                        ctx.repo,
                        ctx.meta.pr,
                    );
                    review_diff(
                        ctx.client,
                        ctx.cfg,
                        ctx.meta,
                        ctx.diff,
                        ctx.omitted_note.map(str::to_string),
                        ctx.structural_context,
                    )
                    .await
                }
            }
        } else {
            review_diff(
                ctx.client,
                ctx.cfg,
                ctx.meta,
                ctx.diff,
                ctx.omitted_note.map(str::to_string),
                ctx.structural_context,
            )
            .await
        }
    }
}
