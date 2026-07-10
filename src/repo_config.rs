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
    pub min_confidence: Option<u8>,
    pub max_findings: Option<usize>,
    pub self_critique: Option<bool>,
    pub agentic: Option<bool>,
    /// Extra review instructions in plain language, appended to the system prompt.
    pub instructions: Option<String>,
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
