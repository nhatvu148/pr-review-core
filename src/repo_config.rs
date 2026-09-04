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
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RepoConfig {
    pub model: Option<String>,
    pub model_explore: Option<String>,
    pub include_globs: Option<Vec<String>>,
    pub exclude_globs: Option<Vec<String>>,
    /// Globs marking vendored third-party source (`thirdparty/**`, `vendor/**`, …).
    /// Diff-hygiene findings are suppressed inside them and the reviewer is told not
    /// to propose edits there — the remedy for vendored code is an upstream patch or
    /// a version bump. Setting this REPLACES the conventional defaults.
    pub vendored: Option<Vec<String>>,
    pub min_confidence: Option<u8>,
    pub max_findings: Option<usize>,
    pub self_critique: Option<bool>,
    pub agentic: Option<bool>,
    /// Toggle grouping related changed files (source + test, i18n siblings) when
    /// packing a large diff, for this repo.
    pub file_bundling: Option<bool>,
    /// Toggle fetching the head commit's CI results into the prompt for this repo.
    pub ci_status: Option<bool>,
    /// Toggle the OSV.dev dependency vulnerability scan for this repo.
    pub cve_scan: Option<bool>,
    /// Toggle re-anchoring a finding that drifted just off a diff line to the
    /// nearest matching diff line (else it folds to the summary), for this repo.
    pub reanchor_findings: Option<bool>,
    /// Toggle committable suggestion blocks on findings, for this repo.
    pub suggestions: Option<bool>,
    /// Pass this repo's PR descriptions to the reviewer as a statement of intent
    /// to check the diff against. Off suppresses it for this repo only.
    pub pr_body: Option<bool>,
    /// Cap on the description handed to the reviewer, for this repo.
    ///
    /// Per-repo because the right value is a property of how a team writes PRs,
    /// not of the deployment. A repo whose descriptions run long wants a higher
    /// cap than one whose PRs say "fix typo", and clipping a description makes the
    /// reviewer assert the diff exceeds its stated scope — so the repo that needs
    /// the higher cap should be able to set it without an env change, a restart,
    /// or a conversation with whoever owns the service.
    pub pr_body_max_chars: Option<usize>,
    /// Let the agentic reviewer's `grep` return context lines around each match,
    /// for this repo.
    pub grep_context: Option<bool>,
    /// Extra review instructions in plain language, appended to the system prompt.
    pub instructions: Option<String>,
    /// Instructions shaping the `/describe` output specifically — a house PR
    /// description layout, release-notes sections, a contributor table. Kept
    /// separate from `instructions` because that one governs what the reviewer
    /// looks for, and mixing "be strict about SQL" into a description prompt
    /// changes the wrong output.
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
