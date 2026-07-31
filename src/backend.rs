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
    /// Calibration/verification rules + the consumer's `extra_system_prompt`,
    /// composed by the orchestrator (see [`crate::prompt::injected_rules`]).
    ///
    /// A backend MUST fold this into whatever system prompt it sends — use
    /// [`ReviewContext::system_prompt`] rather than composing it by hand, and
    /// never reach for [`crate::prompt::REVIEW_RULES`] directly. A backend that
    /// skips these rules produces a review that looks normal and is
    /// systematically miscalibrated; that failure shipped undetected for months.
    pub injected_rules: &'a str,
}

impl ReviewContext<'_> {
    /// The system prompt to send: this backend's own rubric, followed by the
    /// orchestrator-injected rules. `rubric` is the backend's task description
    /// (output shape, what to look for); everything about *calibration* comes
    /// from the orchestrator so every backend is judged on the same scale.
    pub fn system_prompt(&self, rubric: &str) -> String {
        crate::prompt::with_injected_rules(rubric, self.injected_rules)
    }
}

/// Produces a structured [`ReviewResult`] from a prepared [`ReviewContext`].
///
/// Implementations decide *how* the review is generated (HTTP model call, a
/// subprocess agent CLI, a local model, …); the orchestrator handles everything
/// before and after. Object-safe: used as `&dyn ReviewBackend`.
#[async_trait]
pub trait ReviewBackend: Send + Sync {
    async fn review(&self, ctx: &ReviewContext<'_>) -> Result<ReviewResult>;

    /// Free-form text completion, given a system prompt and a user prompt (which
    /// already contains the diff/context). Powers the `/ask` and `/describe`
    /// commands, so they run on the same backend as reviews instead of always
    /// OpenRouter. Default: the OpenRouter chat path.
    async fn complete(&self, cfg: &Config, system: &str, user: &str) -> Result<String> {
        let client = reqwest::Client::new();
        crate::llm::chat_text(&client, cfg, system, user).await
    }
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
        // Both paths take their system prompt from the context: the rubric differs
        // (the agentic one describes tools), the injected rules never do.
        let diff_only = ctx.system_prompt(crate::prompt::SYSTEM_PROMPT);
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
                &ctx.system_prompt(crate::agent::AGENT_SYSTEM_PROMPT),
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
                        &diff_only,
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
                &diff_only,
            )
            .await
        }
    }
}
