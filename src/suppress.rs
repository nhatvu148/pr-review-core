//! Findings a repository has already decided against: `[[suppress]]` in
//! `.prbot.toml`.
//!
//! A reviewer that is told a finding is wrong forgets it by the next PR, so a
//! team that disagrees with one rule ("`unwrap()` on a lock is fine here") sees
//! it again on every change. A suppression rule names that finding by its text,
//! optionally scopes it to paths, and says why:
//!
//! ```toml
//! [[suppress]]
//! contains = "unwrap on a poisoned mutex"
//! paths = ["src/legacy/**"]
//! reason = "a poisoned lock is a bug we want to crash on"
//! ```
//!
//! Rules are read from the **base** branch like the rest of `.prbot.toml`, so a
//! PR cannot suppress the findings about itself. Two further limits keep a stale
//! or careless rule from hiding real problems:
//!
//! - A `BLOCKING` finding is never suppressed. It is the one severity that stops
//!   a merge, and a phrase written for a nit can match one by accident.
//! - A `contains` shorter than [`MIN_CONTAINS_CHARS`] is ignored with a warning:
//!   `contains = "error"` would silence most of a review.
//!
//! Suppressed findings are counted in the summary and the run log rather than
//! vanishing, so a rule that matches far more than intended is visible.
//!
//! Known limit: only the findings are filtered. The model is not told about the
//! rules, so its own summary sentence can still mention a suppressed finding.

use serde::{Deserialize, Serialize};

use crate::llm::Finding;

/// Shortest `contains` a rule may have, in characters after whitespace is
/// collapsed. Long enough to name a specific claim, short enough for a phrase.
pub const MIN_CONTAINS_CHARS: usize = 10;

/// One `[[suppress]]` rule from `.prbot.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Suppression {
    /// Text the finding must contain, matched case-insensitively with runs of
    /// whitespace treated as one space.
    pub contains: String,
    /// Globs the finding's file must match. Empty means any file.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    /// Why the team decided against this finding. Required: a rule nobody can
    /// explain is a rule nobody can safely delete.
    pub reason: String,
}

/// Lowercase and collapse whitespace, so a rule matches however the model
/// happened to wrap or capitalise the sentence.
fn normalize(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

impl Suppression {
    /// The rule's `contains`, normalized, or `None` when it is too short to
    /// apply at all.
    fn needle(&self) -> Option<String> {
        let n = normalize(&self.contains);
        (n.chars().count() >= MIN_CONTAINS_CHARS).then_some(n)
    }

    /// Whether this rule suppresses `f`. Never true for a `BLOCKING` finding.
    #[must_use]
    pub fn matches(&self, f: &Finding) -> bool {
        self.needle()
            .is_some_and(|needle| self.matches_normalized(&needle, f, &normalize(&f.body)))
    }

    /// The one matching rule, given the rule's needle and the finding's body
    /// already normalized. [`apply`] calls this with both cached; [`matches`]
    /// computes them, so the two cannot drift apart.
    ///
    /// [`matches`]: Self::matches
    fn matches_normalized(&self, needle: &str, f: &Finding, body: &str) -> bool {
        !f.severity.eq_ignore_ascii_case("BLOCKING")
            && in_scope(self, &f.file)
            && body.contains(needle)
    }
}

/// Whether `file` is inside the rule's `paths` (any file when it has none).
fn in_scope(r: &Suppression, file: &str) -> bool {
    r.paths.is_empty() || crate::diff::path_matches_globs_strict(file, &r.paths)
}

/// Remove the findings any rule matches, returning how many were removed.
///
/// Warns for each rule too short to apply, so a misconfigured rule shows up in
/// the logs instead of silently doing nothing. Each text is normalized once here
/// rather than per comparison, since every finding is checked against every rule.
pub fn apply(rules: &[Suppression], findings: &mut Vec<Finding>) -> usize {
    let mut usable: Vec<(&Suppression, String)> = Vec::new();
    for r in rules {
        match r.needle() {
            Some(needle) => usable.push((r, needle)),
            None => tracing::warn!(
                "ignoring [[suppress]] rule {:?}: `contains` must be at least \
                 {MIN_CONTAINS_CHARS} characters so it names one finding, not most of them",
                r.contains
            ),
        }
    }
    if usable.is_empty() {
        return 0;
    }
    let before = findings.len();
    findings.retain(|f| {
        let body = normalize(&f.body);
        !usable
            .iter()
            .any(|(r, needle)| r.matches_normalized(needle, f, &body))
    });
    before - findings.len()
}

/// The summary line saying how many findings were suppressed, or `""` for none.
#[must_use]
pub fn note(suppressed: usize) -> String {
    match suppressed {
        0 => String::new(),
        1 => "\n\n_1 finding matched a `[[suppress]]` rule in `.prbot.toml` and was not posted._"
            .to_string(),
        n => format!(
            "\n\n_{n} findings matched a `[[suppress]]` rule in `.prbot.toml` and were not posted._"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finding(sev: &str, file: &str, body: &str) -> Finding {
        Finding {
            severity: sev.to_string(),
            file: file.to_string(),
            line: Some(1),
            body: body.to_string(),
            confidence: None,
            suggestion: None,
            existing_code: None,
            end_line: None,
            samples: None,
        }
    }

    fn rule(contains: &str, paths: &[&str]) -> Suppression {
        Suppression {
            contains: contains.to_string(),
            paths: paths.iter().map(|p| (*p).to_string()).collect(),
            reason: "decided".to_string(),
        }
    }

    #[test]
    fn a_rule_matches_its_text_whatever_the_case_and_wrapping() {
        let r = rule("unwrap on a poisoned mutex", &[]);
        let f = finding(
            "MEDIUM",
            "src/a.rs",
            "Calling `.lock()` then UNWRAP ON A\n  poisoned   mutex panics.",
        );
        assert!(r.matches(&f));
        assert!(!r.matches(&finding("MEDIUM", "src/a.rs", "An unrelated claim.")));
    }

    #[test]
    fn paths_scope_a_rule_to_matching_files_only() {
        let r = rule("unwrap on a poisoned mutex", &["src/legacy/**"]);
        let body = "unwrap on a poisoned mutex panics";
        assert!(r.matches(&finding("LOW", "src/legacy/old.rs", body)));
        assert!(!r.matches(&finding("LOW", "src/new.rs", body)));
    }

    #[test]
    fn a_blocking_finding_is_never_suppressed() {
        let r = rule("unwrap on a poisoned mutex", &[]);
        assert!(!r.matches(&finding(
            "BLOCKING",
            "src/a.rs",
            "unwrap on a poisoned mutex"
        )));
    }

    #[test]
    fn a_rule_too_short_to_name_one_finding_applies_to_nothing() {
        let r = rule("  error ", &[]);
        assert!(!r.matches(&finding("LOW", "a.rs", "an error here")));
    }

    #[test]
    fn apply_removes_the_matches_and_counts_them() {
        let rules = vec![rule("unwrap on a poisoned mutex", &[])];
        let mut fs = vec![
            finding("BLOCKING", "z.rs", "unwrap on a poisoned mutex"),
            finding("LOW", "a.rs", "unwrap on a poisoned mutex"),
            finding("HIGH", "b.rs", "SQL injection"),
            finding("MEDIUM", "c.rs", "an unwrap on a poisoned mutex again"),
        ];
        assert_eq!(apply(&rules, &mut fs), 2);
        let left: Vec<&str> = fs.iter().map(|f| f.file.as_str()).collect();
        assert_eq!(left, ["z.rs", "b.rs"], "BLOCKING is kept by apply too");
    }

    #[test]
    fn the_note_says_how_many_and_is_empty_for_none() {
        assert_eq!(note(0), "");
        assert!(note(1).contains("1 finding matched"));
        assert!(note(3).contains("3 findings matched"));
    }

    #[test]
    fn a_rule_without_a_reason_does_not_parse() {
        let err = toml::from_str::<Suppression>("contains = \"unwrap on a lock\"");
        assert!(err.is_err());
    }
}
