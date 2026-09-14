//! The effective review rules, as data a caller can read.
//!
//! "Why did the reviewer do that?" has always been answerable only by reading the
//! deployment's environment and the repository's `.prbot.toml` side by side and
//! merging them in your head. This module answers it directly: what scope was
//! reviewed, which `.prbot.toml` was found (or why none was), what it overrode,
//! what the merged settings came out as, and the exact calibration text injected
//! into the system prompt.
//!
//! It performs **no model call**, so it needs no `OPENROUTER_API_KEY`. The local
//! scope needs no credentials at all; the PR scope needs only the provider token
//! it takes to fetch a file.

use serde::Serialize;

use crate::config::Config;
use crate::repo_config::RepoConfig;

/// What the rules were resolved for.
#[derive(Debug, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", tag = "kind")]
#[schemars(rename_all = "camelCase")]
pub enum RulesScope {
    /// A checkout on disk — the pre-PR path.
    // `rename_all` on the enum renames the VARIANTS; fields inside a struct
    // variant need their own, or `repo_root` ships snake_case into an otherwise
    // camelCase document.
    #[serde(rename_all = "camelCase")]
    #[schemars(rename_all = "camelCase")]
    Local {
        /// The directory whose `.prbot.toml` was consulted.
        repo_root: String,
    },
    /// A pull request on a host.
    Pr {
        provider: String,
        repo: String,
        pr: u64,
    },
}

/// Where the per-repo configuration came from, and what it said.
///
/// Four outcomes rather than an `Option`, because "there is no file" and "there is
/// a file and it is broken" lead to the same effective settings by design
/// (repo config is fail-open) and must not therefore look the same to a reader.
/// A repository whose `.prbot.toml` has a typo'd key gets the *deployment's*
/// rules and no indication of it anywhere in the review; this is where that
/// becomes visible.
#[derive(Debug, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", tag = "status")]
#[schemars(rename_all = "camelCase")]
pub enum RepoConfigSource {
    /// Nothing to read against — no checkout, or a PR with no ref to fetch from.
    Unavailable { reason: String },
    /// Looked, and the repository ships no `.prbot.toml`.
    Absent { location: String },
    /// Read, parsed, and merged over the environment config.
    Applied {
        location: String,
        /// Exactly the keys the file set. Absent keys are omitted rather than
        /// rendered as null, so this reads as "what this repo chose to change".
        overrides: Box<RepoConfig>,
    },
    /// Read, but rejected — invalid TOML or an unknown key. The review ran on the
    /// environment config alone.
    Invalid { location: String, error: String },
}

/// The review-relevant configuration, after merging repo overrides.
///
/// **An allowlist, and that is the point.** Serializing [`Config`] would expose
/// `openrouter_api_key`, `github_token`, `gitlab_token`, `bitbucket_token` and
/// three webhook secrets, and would expose the *next* secret too — silently, on
/// the day it was added, in an output an agent is encouraged to print. Naming
/// each field here means a new `Config` field appears in this output only when
/// somebody decides it should.
///
/// Operational settings are left out for a second reason: they are not rules.
/// Ports, API base URLs, timeouts, retry counts and the run-log sink do not
/// change what the reviewer finds, and a reader scanning for "why was this
/// flagged" should not have to skip past them.
#[derive(Debug, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(rename_all = "camelCase")]
pub struct ReviewSettings {
    pub model: String,
    pub model_explore: String,
    pub max_tokens: u32,
    pub temperature: f32,
    pub max_diff_chars: usize,
    pub include_globs: Vec<String>,
    pub exclude_globs: Vec<String>,
    pub vendored_globs: Vec<String>,
    pub file_bundling: bool,
    pub self_critique: bool,
    pub min_confidence: u8,
    pub max_findings: usize,
    pub review_samples: usize,
    pub sample_min_agreement: usize,
    pub sample_line_tolerance: u64,
    pub reanchor_findings: bool,
    pub suggestions: bool,
    pub pr_body: bool,
    pub pr_body_max_chars: usize,
    pub change_intent_max_chars: usize,
    pub grep_context: bool,
    pub agentic: bool,
    pub max_turns: usize,
    pub max_history_chars: usize,
    pub structural_context: bool,
    pub structural_max_files: usize,
    pub complexity_metrics: bool,
    pub complexity_min_cyclomatic: u32,
    pub walkthrough: bool,
    pub walkthrough_max_symbols: usize,
    pub diagram: bool,
    pub diagram_max_nodes: usize,
    pub blast_radius: bool,
    pub blast_max_symbols: usize,
    pub blast_max_refs: usize,
    pub ci_status: bool,
    pub cve_scan: bool,
    pub cve_max_packages: usize,
    /// The signature appended to every comment this deployment posts. Not a
    /// secret, and the key a caller needs to tell this bot's findings from
    /// another tool's — which is exactly what `get-findings` does with it.
    pub comment_marker: String,
}

impl ReviewSettings {
    /// Read the review-relevant settings out of a merged [`Config`].
    #[must_use]
    pub fn from_config(cfg: &Config) -> Self {
        Self {
            model: cfg.openrouter_model.clone(),
            model_explore: cfg.openrouter_model_explore.clone(),
            max_tokens: cfg.openrouter_max_tokens,
            temperature: cfg.openrouter_temperature,
            max_diff_chars: cfg.max_diff_chars,
            include_globs: cfg.include_globs.clone(),
            exclude_globs: cfg.exclude_globs.clone(),
            vendored_globs: cfg.vendored_globs.clone(),
            file_bundling: cfg.file_bundling,
            self_critique: cfg.self_critique,
            min_confidence: cfg.min_confidence,
            max_findings: cfg.max_findings,
            review_samples: cfg.review_samples,
            sample_min_agreement: cfg.sample_min_agreement,
            sample_line_tolerance: cfg.sample_line_tolerance,
            reanchor_findings: cfg.reanchor_findings,
            suggestions: cfg.suggestions,
            pr_body: cfg.pr_body,
            pr_body_max_chars: cfg.pr_body_max_chars,
            change_intent_max_chars: cfg.change_intent_max_chars,
            grep_context: cfg.grep_context,
            agentic: cfg.agentic,
            max_turns: cfg.max_turns,
            max_history_chars: cfg.max_history_chars,
            structural_context: cfg.structural_context,
            structural_max_files: cfg.structural_max_files,
            complexity_metrics: cfg.complexity_metrics,
            complexity_min_cyclomatic: cfg.complexity_min_cyclomatic,
            walkthrough: cfg.walkthrough,
            walkthrough_max_symbols: cfg.walkthrough_max_symbols,
            diagram: cfg.diagram,
            diagram_max_nodes: cfg.diagram_max_nodes,
            blast_radius: cfg.blast_radius,
            blast_max_symbols: cfg.blast_max_symbols,
            blast_max_refs: cfg.blast_max_refs,
            ci_status: cfg.ci_status,
            cve_scan: cfg.cve_scan,
            cve_max_packages: cfg.cve_max_packages,
            comment_marker: cfg.comment_marker.clone(),
        }
    }
}

/// Everything that shapes one review, resolved and redacted.
#[derive(Debug, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(rename_all = "camelCase")]
pub struct EffectiveRules {
    pub scope: RulesScope,
    pub repo_config: RepoConfigSource,
    pub settings: ReviewSettings,
    /// The exact text appended to the backend's system prompt — calibration rules,
    /// the suggestion rules when enabled, and the consumer's `EXTRA_PROMPT` with
    /// any repository `instructions` already merged in.
    ///
    /// The whole string, not a summary. A reviewer behaving oddly is usually
    /// behaving exactly as some line in here tells it to, and a caller that has to
    /// guess at the wording cannot find that line.
    pub injected_rules: String,
    /// Anything that failed open on the way here. Empty is the normal case.
    ///
    /// A fail-open stage keeps the review running, which is right, and leaves the
    /// caller with no way to know it happened, which is not. This is that way.
    pub warnings: Vec<String>,
}

/// Resolve the effective rules for a local checkout.
///
/// Needs no network and no credentials of any kind: it reads one file and merges
/// it. `repo_root` of `None` reports [`RepoConfigSource::Unavailable`] and returns
/// the environment config unchanged — the same thing a local review would do.
///
/// # Examples
/// ```
/// # use pr_review_core::{config::Config, rules};
/// let rules = rules::local(&Config::from_env(), Some(std::path::Path::new(".")));
/// // Reading rules never requires a model key.
/// assert!(!rules.injected_rules.is_empty());
/// ```
#[must_use]
pub fn local(base: &Config, repo_root: Option<&std::path::Path>) -> EffectiveRules {
    let (effective, source) = crate::review::local_repo_config_source(base, repo_root);
    finish(
        RulesScope::Local {
            repo_root: repo_root
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
        },
        source,
        &effective,
    )
}

/// Resolve the effective rules for a pull request.
///
/// Fetches the PR's metadata and its head `.prbot.toml`, so it needs the provider
/// token — but still no model key, because nothing here calls a model.
///
/// # Errors
/// If the provider name is unknown, or the PR metadata cannot be fetched.
pub async fn remote(
    base: &Config,
    provider_name: &str,
    repo: &str,
    pr: u64,
) -> anyhow::Result<EffectiveRules> {
    let provider = crate::providers::Provider::from_name(provider_name)?;
    let client = reqwest::Client::new();
    let meta = provider.get_meta(&client, base, repo, pr).await?;
    let (effective, source) =
        crate::review::remote_repo_config_source(&provider, &client, base, repo, &meta).await;
    Ok(finish(
        RulesScope::Pr {
            provider: provider.name().to_string(),
            repo: repo.to_string(),
            pr,
        },
        source,
        &effective,
    ))
}

/// Assemble the answer once the config has been resolved, so the local and remote
/// paths cannot describe the same merged config differently.
fn finish(scope: RulesScope, repo_config: RepoConfigSource, effective: &Config) -> EffectiveRules {
    // Derived from the MERGED config, not the base: `instructions` in a
    // `.prbot.toml` is appended to `extra_system_prompt` during the merge, so
    // reading the base here would omit exactly the repo-authored rules a caller
    // asked this question to see.
    let injected_rules = crate::prompt::injected_rules(effective);
    let warnings = match &repo_config {
        RepoConfigSource::Invalid { location, error } => {
            vec![format!(
                "{location} was ignored ({error}) — this review ran on the environment \
                 configuration alone, without this repository's own rules."
            )]
        }
        _ => Vec::new(),
    };
    EffectiveRules {
        scope,
        repo_config,
        settings: ReviewSettings::from_config(effective),
        injected_rules,
        warnings,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one property this module exists to guarantee.
    ///
    /// Every credential is set to a distinctive sentinel and the whole output is
    /// searched for it. This catches the field added to [`ReviewSettings`] by
    /// reflex on the day a new secret lands in [`Config`] — which is the only way
    /// this leak ever happens, since nobody sets out to publish a token.
    #[test]
    fn no_credential_of_any_kind_reaches_the_output() {
        let mut cfg = Config::from_env();
        cfg.openrouter_api_key = "SENTINEL-openrouter-key".into();
        cfg.github_token = "SENTINEL-github-token".into();
        cfg.gitlab_token = "SENTINEL-gitlab-token".into();
        cfg.bitbucket_token = "SENTINEL-bitbucket-token".into();
        cfg.bitbucket_email = "SENTINEL-bitbucket-email".into();
        cfg.github_webhook_secret = "SENTINEL-github-webhook".into();
        cfg.gitlab_webhook_secret = "SENTINEL-gitlab-webhook".into();
        cfg.bitbucket_webhook_secret = "SENTINEL-bitbucket-webhook".into();
        cfg.worker_token = "SENTINEL-worker-token".into();

        let rules = finish(
            RulesScope::Local {
                repo_root: ".".into(),
            },
            RepoConfigSource::Absent {
                location: "./.prbot.toml".into(),
            },
            &cfg,
        );
        let json = serde_json::to_string(&rules).expect("serializes");

        assert!(
            !json.contains("SENTINEL"),
            "a credential reached the rules output: {json}"
        );
    }

    /// The settings that actually shape a review are present — a redaction that
    /// removed the answer along with the secrets would be useless.
    #[test]
    fn the_settings_that_shape_a_review_are_present() {
        let cfg = Config::from_env();
        let json = serde_json::to_value(ReviewSettings::from_config(&cfg)).expect("serializes");
        for key in [
            "minConfidence",
            "maxFindings",
            "excludeGlobs",
            "includeGlobs",
            "agentic",
            "selfCritique",
            "suggestions",
            "commentMarker",
        ] {
            assert!(
                json.get(key).is_some(),
                "missing {key} from the rules output"
            );
        }
    }

    /// An invalid `.prbot.toml` is fail-open by design, so the settings look
    /// completely normal. Without a warning the caller cannot tell that the
    /// repository's own rules were silently dropped.
    #[test]
    fn an_invalid_repo_config_is_reported_as_a_warning() {
        let cfg = Config::from_env();
        let rules = finish(
            RulesScope::Local {
                repo_root: ".".into(),
            },
            RepoConfigSource::Invalid {
                location: "./.prbot.toml".into(),
                error: "unknown field `not_a_real_key`".into(),
            },
            &cfg,
        );
        assert_eq!(rules.warnings.len(), 1);
        assert!(
            rules.warnings[0].contains("not_a_real_key"),
            "{:?}",
            rules.warnings
        );
        assert!(rules.warnings[0].contains("environment configuration alone"));
    }

    /// A repository that ships no config is not a problem, and must not be
    /// reported as one.
    #[test]
    fn an_absent_repo_config_is_not_a_warning() {
        let cfg = Config::from_env();
        let rules = finish(
            RulesScope::Local {
                repo_root: ".".into(),
            },
            RepoConfigSource::Absent {
                location: "./.prbot.toml".into(),
            },
            &cfg,
        );
        assert!(rules.warnings.is_empty());
    }
}
