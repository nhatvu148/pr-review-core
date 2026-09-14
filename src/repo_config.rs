//! Per-repo config file (`.prbot.toml`) support.
//!
//! A repository being reviewed can ship a `.prbot.toml` at its root to override
//! review settings for its own PRs — the "custom rules in plain language"
//! capability. The file is fetched from the PR's head commit at review time and
//! merged over the env-derived [`Config`](crate::config::Config) for that one
//! review. Parsing is fail-open at the call site: any fetch/parse error logs a
//! warning and the base config is used unchanged.

/// Per-repo review overrides parsed from a `.prbot.toml`.
///
/// Every field is optional; only the ones present in the file override the base
/// config (see [`Config::with_repo_overrides`](crate::config::Config::with_repo_overrides)).
/// Unknown keys are rejected so typos surface as a warning instead of silently
/// doing nothing.
/// `Serialize` alongside `Deserialize` so `kaniscope get-rules` can show a caller
/// exactly which keys a repository set. `skip_serializing_if` on every field keeps
/// that output to what the file actually chose to change, rather than forty nulls
/// a reader has to scan past to find the two that matter.
#[derive(Debug, Clone, Default, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RepoConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_explore: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include_globs: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exclude_globs: Option<Vec<String>>,
    /// Globs marking vendored third-party source (`thirdparty/**`, `vendor/**`, …).
    /// Diff-hygiene findings are suppressed inside them and the reviewer is told not
    /// to propose edits there — the remedy for vendored code is an upstream patch or
    /// a version bump. Setting this REPLACES the conventional defaults.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vendored: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_confidence: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_findings: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub self_critique: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agentic: Option<bool>,
    /// Toggle grouping related changed files (source + test, i18n siblings) when
    /// packing a large diff, for this repo.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_bundling: Option<bool>,
    /// Toggle fetching the head commit's CI results into the prompt for this repo.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ci_status: Option<bool>,
    /// Toggle the OSV.dev dependency vulnerability scan for this repo.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cve_scan: Option<bool>,
    /// Toggle re-anchoring a finding that drifted just off a diff line to the
    /// nearest matching diff line (else it folds to the summary), for this repo.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reanchor_findings: Option<bool>,
    /// Toggle committable suggestion blocks on findings, for this repo.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggestions: Option<bool>,
    /// Pass this repo's PR descriptions to the reviewer as a statement of intent
    /// to check the diff against. Off suppresses it for this repo only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr_body: Option<bool>,
    /// Cap on the description handed to the reviewer, for this repo.
    ///
    /// Per-repo because the right value is a property of how a team writes PRs,
    /// not of the deployment. A repo whose descriptions run long wants a higher
    /// cap than one whose PRs say "fix typo", and clipping a description makes the
    /// reviewer assert the diff exceeds its stated scope — so the repo that needs
    /// the higher cap should be able to set it without an env change, a restart,
    /// or a conversation with whoever owns the service.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr_body_max_chars: Option<usize>,
    /// Cap on the stated change intent handed to the reviewer on a local review,
    /// for this repo.
    ///
    /// Per-repo for the same reason as `pr_body_max_chars`, and reachable on this
    /// path because a local review now reads the working tree's `.prbot.toml` —
    /// the whole point of which is that a change gets reviewed under its own
    /// repository's rules before it is a PR.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub change_intent_max_chars: Option<usize>,
    /// Let the agentic reviewer's `grep` return context lines around each match,
    /// for this repo.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grep_context: Option<bool>,
    /// Extra review instructions in plain language, appended to the system prompt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    /// Instructions shaping the `/describe` output specifically — a house PR
    /// description layout, release-notes sections, a contributor table. Kept
    /// separate from `instructions` because that one governs what the reviewer
    /// looks for, and mixing "be strict about SQL" into a description prompt
    /// changes the wrong output.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub describe_instructions: Option<String>,
}

/// Parse a `.prbot.toml` file's text into a [`RepoConfig`].
///
/// # Errors
/// If the text isn't valid TOML or contains unknown keys.
///
/// # Examples
/// ```
/// # use pr_review_core::repo_config::parse;
/// let rc = parse("min_confidence = 60\ninstructions = \"Be strict about SQL.\"").unwrap();
/// assert_eq!(rc.min_confidence, Some(60));
/// ```
pub fn parse(toml_str: &str) -> anyhow::Result<RepoConfig> {
    Ok(toml::from_str(toml_str)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The knobs this incident produced are per-repo, like every other
    /// reviewer-shaping knob here.
    #[test]
    fn parses_the_pr_body_and_grep_context_knobs() {
        let rc = parse("pr_body_max_chars = 30000\npr_body = false\ngrep_context = false").unwrap();
        assert_eq!(rc.pr_body_max_chars, Some(30_000));
        assert_eq!(rc.pr_body, Some(false));
        assert_eq!(rc.grep_context, Some(false));

        // Absent stays absent, so a file that sets one does not silently reset
        // the others to a default.
        let bare = parse("min_confidence = 60").unwrap();
        assert_eq!(bare.pr_body_max_chars, None);
        assert_eq!(bare.pr_body, None);
        assert_eq!(bare.grep_context, None);
    }

    /// The local review path reads this file too now, and its cap is the one knob
    /// that only that path uses — so a repo must be able to set it without the
    /// unknown-key rejection turning a valid file into no config at all.
    #[test]
    fn parses_the_change_intent_cap() {
        let rc = parse("change_intent_max_chars = 4321").unwrap();
        assert_eq!(rc.change_intent_max_chars, Some(4321));
        // Absent stays absent, so it cannot clobber the env value.
        assert!(parse("min_confidence = 60")
            .unwrap()
            .change_intent_max_chars
            .is_none());
    }

    #[test]
    fn parses_describe_instructions() {
        let rc = parse("describe_instructions = \"Release notes format.\"").unwrap();
        assert_eq!(
            rc.describe_instructions.as_deref(),
            Some("Release notes format.")
        );
        // ...and stays absent when unset, so it can't clobber the env value.
        assert!(parse("min_confidence = 60")
            .unwrap()
            .describe_instructions
            .is_none());
    }

    #[test]
    fn parses_fields_and_instructions() {
        let rc = parse(
            r#"
            model = "anthropic/claude-opus-4"
            min_confidence = 70
            self_critique = false
            include_globs = ["src/**", "lib/**"]
            instructions = "Focus on error handling and never nit about formatting."
            "#,
        )
        .expect("valid toml should parse");

        assert_eq!(rc.model.as_deref(), Some("anthropic/claude-opus-4"));
        assert_eq!(rc.min_confidence, Some(70));
        assert_eq!(rc.self_critique, Some(false));
        assert_eq!(
            rc.include_globs,
            Some(vec!["src/**".to_string(), "lib/**".to_string()])
        );
        assert_eq!(
            rc.instructions.as_deref(),
            Some("Focus on error handling and never nit about formatting.")
        );
        // Untouched fields stay None.
        assert_eq!(rc.model_explore, None);
        assert_eq!(rc.max_findings, None);
        assert_eq!(rc.agentic, None);
    }

    #[test]
    fn empty_parses_to_all_none() {
        let rc = parse("").expect("empty toml should parse");
        assert!(rc.model.is_none());
        assert!(rc.instructions.is_none());
    }

    #[test]
    fn unknown_keys_error() {
        let err = parse("not_a_real_key = 1").unwrap_err();
        // deny_unknown_fields surfaces the offending key.
        assert!(
            err.to_string().contains("not_a_real_key"),
            "error should name the unknown key, got: {err}"
        );
    }
}
