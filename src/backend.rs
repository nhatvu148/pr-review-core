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
/// checkout) should prefer `local_root` when it is set — the caller already has the
/// code on disk — and otherwise clone one via [`Provider::clone_url`] +
/// `Workspace::clone` using `provider` and `repo`.
pub struct ReviewContext<'a> {
    pub client: &'a reqwest::Client,
    pub cfg: &'a Config,
    /// The host this change lives on, or `None` on the local diff-first path
    /// ([`crate::review::run_review_local`]) where there is no PR and no host.
    /// A backend that needs one must handle its absence rather than assume it.
    pub provider: Option<&'a Provider>,
    /// A checkout of the code under review, when the caller has one. Set on the
    /// local path; `None` for a PR, where the code is only reachable by cloning.
    pub local_root: Option<&'a std::path::Path>,
    /// `owner/repo` (GitHub) or `workspace/repo` (Bitbucket). On the local path
    /// this is the caller's label for the change, not a host coordinate.
    pub repo: &'a str,
    /// PR metadata, synthesized on the local path (`pr: 0`, no head SHA, no CI).
    pub meta: &'a PrMeta,
    /// The filtered, packed diff the model should review.
    pub diff: &'a str,
    /// Note about files omitted to fit the size budget (NOT reviewed), if any.
    pub omitted_note: Option<&'a str>,
    /// Enclosing-symbol context for changed lines, if computed.
    pub structural_context: Option<&'a str>,
    /// The PR's own description, **already wrapped in its untrusted fence** by
    /// [`crate::prompt::pr_body_block`], or `None` when `PR_BODY` is off, the
    /// description is blank, or there is no PR. Append it verbatim; do not read
    /// `meta.body` yourself.
    ///
    /// Composed by the orchestrator for the same reason as `injected_rules`, and
    /// after the same failure. `PR_BODY` first shipped derived inside the two
    /// in-crate prompt builders, so it reached the diff-only path, then the
    /// agentic one — and no agent-CLI backend at all, because each of those builds
    /// its own prompt. A backend that splices `meta.body` in by hand also loses the
    /// fence, which is the part that makes author-written prose safe to show a
    /// reviewer. One composition site, handed over ready to use, is what stops both.
    pub pr_body: Option<&'a str>,
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

    /// [`Self::complete`] plus what the call reported about itself — the model used
    /// and the tokens spent. Powers the JSON repair pass, which is a second billed
    /// call on the same review: usage that omits it invents a smaller bill.
    ///
    /// **The default delegates to [`Self::complete`]**, so a backend that overrides
    /// only `complete` keeps every call — including the repair — on its own
    /// backend, and simply reports no usage. It deliberately does *not* fall back to
    /// the OpenRouter path: a consumer running an agent CLI would then have its
    /// repair silently answered by a different service and model, and would fail
    /// outright if it has no `OPENROUTER_API_KEY` — a review discarded by the very
    /// mechanism meant to salvage it.
    ///
    /// Override it to report usage. Overriding it does not replace `complete`;
    /// backends that want both should implement this and have `complete` return its
    /// `.text`.
    async fn complete_detailed(
        &self,
        cfg: &Config,
        system: &str,
        user: &str,
    ) -> Result<crate::llm::Completion> {
        Ok(crate::llm::Completion {
            text: self.complete(cfg, system, user).await?,
            model: None,
            usage: None,
        })
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
    /// Reports usage, which the trait default cannot: it goes through `complete`,
    /// whose `String` return has nowhere to put it. This is the backend that
    /// actually makes the OpenRouter call, so it has the figures to hand.
    async fn complete_detailed(
        &self,
        cfg: &Config,
        system: &str,
        user: &str,
    ) -> Result<crate::llm::Completion> {
        let client = reqwest::Client::new();
        crate::llm::chat_completion(&client, cfg, system, user).await
    }

    async fn review(&self, ctx: &ReviewContext<'_>) -> Result<ReviewResult> {
        // Both paths take their system prompt from the context: the rubric differs
        // (the agentic one describes tools), the injected rules never do.
        let diff_only = ctx.system_prompt(crate::prompt::SYSTEM_PROMPT);
        // The agentic path reads the repo from a clone, which needs a host to clone
        // from. On the local path there is none, so it reviews the diff alone rather
        // than pretending it explored the tree.
        if let (true, Some(provider)) = (ctx.cfg.agentic, ctx.provider) {
            match run_agentic(
                provider,
                ctx.client,
                ctx.cfg,
                ctx.meta,
                ctx.diff,
                ctx.omitted_note,
                ctx.structural_context,
                ctx.pr_body,
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
                        ctx.pr_body,
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
                ctx.pr_body,
                &diff_only,
            )
            .await
        }
    }
}

#[cfg(test)]
mod complete_seam_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A consumer backend of the shape the trait docs invite: it overrides
    /// `complete` and nothing else.
    struct OverridesCompleteOnly(AtomicUsize);

    #[async_trait]
    impl ReviewBackend for OverridesCompleteOnly {
        async fn review(&self, _ctx: &ReviewContext<'_>) -> Result<ReviewResult> {
            unimplemented!("not exercised")
        }
        async fn complete(&self, _cfg: &Config, _system: &str, user: &str) -> Result<String> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Ok(format!("answered by the consumer backend: {user}"))
        }
    }

    /// The repair pass runs through `complete_detailed`. If its default reached for
    /// OpenRouter instead of `self.complete`, a consumer running an agent CLI would
    /// have its repair answered by a different service and model — and would fail
    /// outright with no `OPENROUTER_API_KEY`, discarding the review by the very
    /// mechanism meant to salvage it.
    ///
    /// No network is configured here, so if this ever regresses to the OpenRouter
    /// path it fails rather than silently passing.
    #[tokio::test]
    async fn the_default_complete_detailed_stays_on_the_consumer_backend() {
        let backend = OverridesCompleteOnly(AtomicUsize::new(0));
        // from_env with no OPENROUTER_API_KEY set: if this ever regresses to the
        // OpenRouter path it errors on the missing key rather than passing quietly.
        let cfg = Config::from_env();

        let out = backend
            .complete_detailed(&cfg, "sys", "repair this")
            .await
            .expect("must not reach for OpenRouter");

        assert!(out.text.contains("answered by the consumer backend"));
        assert_eq!(
            backend.0.load(Ordering::Relaxed),
            1,
            "complete_detailed must delegate to the backend's own complete"
        );
        assert!(
            out.usage.is_none(),
            "no usage is honest; a wrong figure is not"
        );
    }
}
