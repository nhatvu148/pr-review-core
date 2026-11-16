//! Review rules a repository already wrote down, in the files other reviewers read.
//!
//! Teams write review guidance for whichever tool they used first: `REVIEW.md`
//! (Claude Code Review), `.cursor/BUGBOT.md` (Cursor Bugbot), a "Code Review
//! Rules" section of `AGENTS.md` (Codex). Asking them to restate it in
//! `.prbot.toml` means it is restated badly or not at all, so the engine reads
//! those files as they are, and says in the summary which ones it used.
//!
//! **Read from the base branch, never the PR head**, for the same reason as
//! `.prbot.toml`: the head is the author's, and a rule file the PR itself edits
//! could tell the reviewer to report nothing. The caller passes the trusted ref.
//!
//! **Only a section of `AGENTS.md`.** That file is mostly instructions for a
//! coding agent — build commands, layout, style — and handing all of it to a
//! reviewer spends tokens on text about a different job. Only a heading that
//! names review is taken, which is the convention Codex documents.
//!
//! **Fails open and is capped.** A missing file, a fetch error or a non-UTF-8
//! body is skipped. Each source is capped, and so is the total, so a large rule
//! file cannot crowd the diff out of the prompt.
//!
//! Path-scoped instructions (`[[path_instructions]]` in `.prbot.toml`) live here
//! too: they are the same kind of input, applied only when the PR touches a
//! matching file.

use serde::{Deserialize, Serialize};

/// Per-source cap on rule text, in characters.
pub const MAX_RULE_SOURCE_CHARS: usize = 4_000;
/// Cap on all rule text from files together, in characters.
pub const MAX_RULE_TOTAL_CHARS: usize = 8_000;

/// One rule file the engine looks for, at the repository root.
#[derive(Debug, Clone, Copy)]
pub struct RuleFile {
    /// Path from the repository root.
    pub path: &'static str,
    /// Take only the section headed by a review heading, rather than the file.
    pub review_section_only: bool,
}

/// The rule files read, in prompt order.
pub const RULE_FILES: &[RuleFile] = &[
    RuleFile {
        path: "REVIEW.md",
        review_section_only: false,
    },
    RuleFile {
        path: ".cursor/BUGBOT.md",
        review_section_only: false,
    },
    RuleFile {
        path: "AGENTS.md",
        review_section_only: true,
    },
];

/// Instructions that apply only when the PR changes a file matching `paths`.
///
/// ```toml
/// [[path_instructions]]
/// paths = ["migrations/**"]
/// instructions = "Every migration must be reversible."
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PathInstruction {
    /// Globs, matched against changed file paths.
    pub paths: Vec<String>,
    /// Plain-language instructions for the reviewer.
    pub instructions: String,
}

/// The section of a Markdown file headed by a review heading, without the
/// heading itself, or `None` when there is none.
///
/// A review heading is one whose text is `Review`, `Code Review`, `Review Rules`,
/// `Code Review Rules`, `Review Guidelines` or `Code Review Guidelines` (any
/// case, any level). The section runs to the next heading at the same or a
/// higher level. Lines inside a fenced code block (```` ``` ```` or `~~~`) are
/// never headings: a `# install deps` comment in a shell example must not end
/// the section.
///
/// ```
/// use pr_review_core::repo_rules::review_section;
/// let md = "# Build\nrun make\n## Code Review Rules\n- no unwrap\n### Detail\nx\n## Other\ny";
/// assert_eq!(review_section(md).as_deref(), Some("- no unwrap\n### Detail\nx"));
/// assert_eq!(review_section("# Build\nrun make"), None);
/// ```
pub fn review_section(md: &str) -> Option<String> {
    let heading = |line: &str| -> Option<(usize, String)> {
        let t = line.trim_start();
        let level = t.chars().take_while(|c| *c == '#').count();
        if level == 0 || level > 6 || !t[level..].starts_with(' ') {
            return None;
        }
        Some((
            level,
            t[level..]
                .trim()
                .trim_end_matches('#')
                .trim()
                .to_lowercase(),
        ))
    };
    let is_review = |title: &str| {
        matches!(
            title,
            "review"
                | "code review"
                | "review rules"
                | "code review rules"
                | "review guidelines"
                | "code review guidelines"
        )
    };
    // Heading level per line, `None` for a non-heading or any line in a fence.
    let mut in_fence = false;
    let lines: Vec<(&str, Option<(usize, String)>)> = md
        .lines()
        .map(|l| {
            let t = l.trim_start();
            if t.starts_with("```") || t.starts_with("~~~") {
                in_fence = !in_fence;
                return (l, None);
            }
            (l, if in_fence { None } else { heading(l) })
        })
        .collect();
    let start = lines
        .iter()
        .position(|(_, h)| h.as_ref().is_some_and(|(_, t)| is_review(t)))?;
    let level = lines[start].1.as_ref().map(|(l, _)| *l)?;
    let body: Vec<&str> = lines[start + 1..]
        .iter()
        .take_while(|(_, h)| h.as_ref().is_none_or(|(l, _)| *l > level))
        .map(|(l, _)| *l)
        .collect();
    let text = body.join("\n").trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// Clip to `max` characters on a char boundary, saying so when it cut.
fn clip_rules(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let head: String = text.chars().take(max).collect();
    format!("{head}\n[…cut at {max} characters]")
}

/// The text a [`RuleFile`] contributes, from its raw contents.
pub fn rule_text(file: &RuleFile, raw: &str) -> Option<String> {
    let text = if file.review_section_only {
        review_section(raw)?
    } else {
        raw.trim().to_string()
    };
    (!text.is_empty()).then(|| clip_rules(&text, MAX_RULE_SOURCE_CHARS))
}

/// Append found rule files to the config's injected prompt, and record each as
/// a rule source. `found` is `(label, text)`, already passed through
/// [`rule_text`].
pub fn apply_rule_files(cfg: &mut crate::config::Config, found: Vec<(String, String)>) {
    let mut budget = MAX_RULE_TOTAL_CHARS;
    let mut block = String::new();
    for (label, text) in found {
        if budget == 0 {
            tracing::info!("repo rules: {label} skipped, the rule budget is spent");
            continue;
        }
        let text = clip_rules(&text, budget);
        budget = budget.saturating_sub(text.chars().count());
        block.push_str(&format!("\n### From `{label}`\n\n{text}\n"));
        cfg.rule_sources.push(format!("`{label}`"));
    }
    if block.is_empty() {
        return;
    }
    append_prompt(
        cfg,
        &format!(
            "## Review rules from this repository\n\nThe repository's maintainers wrote \
             these for code reviewers. Apply them to the change; a violation is a finding.\n{block}"
        ),
    );
}

/// The path instructions whose globs match any of `changed`.
pub fn matching_path_instructions<'a>(
    rules: &'a [PathInstruction],
    changed: &[String],
) -> Vec<&'a PathInstruction> {
    rules
        .iter()
        .filter(|r| !r.instructions.trim().is_empty() && !r.paths.is_empty())
        .filter(|r| {
            changed
                .iter()
                .any(|p| crate::diff::path_matches_globs_strict(p, &r.paths))
        })
        .collect()
}

/// Append the path instructions that apply to `changed`, and record them.
pub fn apply_path_instructions(cfg: &mut crate::config::Config, changed: &[String]) {
    let rules = cfg.path_instructions.clone();
    let hits = matching_path_instructions(&rules, changed);
    if hits.is_empty() {
        return;
    }
    let mut block = String::from(
        "## Instructions for specific paths\n\nApply each only to changed files matching its paths.\n",
    );
    for r in hits {
        let globs = r
            .paths
            .iter()
            .map(|g| format!("`{g}`"))
            .collect::<Vec<_>>()
            .join(", ");
        block.push_str(&format!(
            "\n### Files matching {globs}\n\n{}\n",
            r.instructions.trim()
        ));
        cfg.rule_sources
            .push(format!("`.prbot.toml` path instructions for {globs}"));
    }
    append_prompt(cfg, &block);
}

fn append_prompt(cfg: &mut crate::config::Config, block: &str) {
    if cfg.extra_system_prompt.is_empty() {
        cfg.extra_system_prompt = block.to_string();
    } else {
        cfg.extra_system_prompt = format!("{}\n\n{block}", cfg.extra_system_prompt);
    }
}

/// The summary line naming the rule sources in force, or empty when none.
///
/// ```
/// let mut cfg = pr_review_core::config::Config::from_env();
/// cfg.rule_sources = vec!["`REVIEW.md`".into()];
/// assert!(pr_review_core::repo_rules::rules_note(&cfg).contains("`REVIEW.md`"));
/// cfg.rule_sources.clear();
/// assert_eq!(pr_review_core::repo_rules::rules_note(&cfg), "");
/// ```
pub fn rules_note(cfg: &crate::config::Config) -> String {
    if cfg.rule_sources.is_empty() {
        return String::new();
    }
    format!(
        "\n\n_Repository rules applied: {}._",
        cfg.rule_sources.join(", ")
    )
}

/// Read the rule files from a checkout on disk (the local review path).
pub fn read_local(root: &std::path::Path) -> Vec<(String, String)> {
    RULE_FILES
        .iter()
        .filter_map(|f| {
            let raw = std::fs::read_to_string(root.join(f.path)).ok()?;
            Some((label_for(f), rule_text(f, &raw)?))
        })
        .collect()
}

/// How a rule file is named in the prompt and the summary.
pub fn label_for(f: &RuleFile) -> String {
    if f.review_section_only {
        format!("{} (review section)", f.path)
    } else {
        f.path.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn cfg() -> Config {
        let mut c = Config::from_env();
        c.extra_system_prompt.clear();
        c.rule_sources.clear();
        c.path_instructions.clear();
        c
    }

    #[test]
    fn an_agents_file_without_a_review_heading_contributes_nothing() {
        let f = RULE_FILES.iter().find(|f| f.path == "AGENTS.md").unwrap();
        assert_eq!(rule_text(f, "# Build\ncargo test\n## Style\nrustfmt"), None);
    }

    #[test]
    fn a_review_section_stops_at_the_next_heading_of_its_level() {
        let md = "## Review\n- a\n## Build\n- b";
        assert_eq!(review_section(md).as_deref(), Some("- a"));
    }

    #[test]
    fn a_hash_without_a_space_is_not_a_heading() {
        assert_eq!(review_section("#Review\n- a"), None);
    }

    /// A shell comment in an example must not end the section, and a review
    /// heading inside a fence must not start one.
    #[test]
    fn lines_inside_a_code_fence_are_never_headings() {
        let md = "## Code Review Rules\n- run the tests:\n```bash\n# install deps\nnpm ci\n```\n- no unwrap\n## Build\nmake";
        let got = review_section(md).unwrap();
        assert!(
            got.contains("# install deps") && got.contains("- no unwrap"),
            "{got}"
        );
        assert!(!got.contains("make"), "{got}");
        assert_eq!(review_section("```\n## Review\nx\n```\n"), None);
    }

    #[test]
    fn rule_files_are_labelled_and_reported_as_sources() {
        let mut c = cfg();
        apply_rule_files(
            &mut c,
            vec![
                ("REVIEW.md".into(), "Flag every unwrap.".into()),
                ("AGENTS.md (review section)".into(), "No new deps.".into()),
            ],
        );
        assert!(c.extra_system_prompt.contains("### From `REVIEW.md`"));
        assert!(c.extra_system_prompt.contains("No new deps."));
        assert_eq!(
            c.rule_sources,
            vec!["`REVIEW.md`", "`AGENTS.md (review section)`"]
        );
    }

    #[test]
    fn the_total_rule_budget_caps_what_reaches_the_prompt() {
        let mut c = cfg();
        let big = "x".repeat(MAX_RULE_TOTAL_CHARS);
        apply_rule_files(
            &mut c,
            vec![
                ("REVIEW.md".into(), big),
                ("AGENTS.md".into(), "late".into()),
            ],
        );
        assert!(!c.extra_system_prompt.contains("late"));
        assert_eq!(c.rule_sources, vec!["`REVIEW.md`"]);
    }

    #[test]
    fn path_instructions_apply_only_when_a_changed_file_matches() {
        let mut c = cfg();
        c.path_instructions = vec![
            PathInstruction {
                paths: vec!["migrations/**".into()],
                instructions: "Every migration must be reversible.".into(),
            },
            PathInstruction {
                paths: vec!["docs/**".into()],
                instructions: "Check links.".into(),
            },
        ];
        apply_path_instructions(
            &mut c,
            &["migrations/0042_add.sql".into(), "src/a.rs".into()],
        );
        assert!(c.extra_system_prompt.contains("must be reversible"));
        assert!(!c.extra_system_prompt.contains("Check links"));
        assert_eq!(
            c.rule_sources,
            vec!["`.prbot.toml` path instructions for `migrations/**`"]
        );
    }

    #[test]
    fn no_matching_path_instruction_leaves_the_prompt_alone() {
        let mut c = cfg();
        c.path_instructions = vec![PathInstruction {
            paths: vec!["migrations/**".into()],
            instructions: "x".into(),
        }];
        apply_path_instructions(&mut c, &["src/a.rs".into()]);
        assert!(c.extra_system_prompt.is_empty());
        assert!(c.rule_sources.is_empty());
    }

    #[test]
    fn local_rule_files_are_read_from_the_checkout() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("REVIEW.md"), "Flag every unwrap.").unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "# Build\nmake").unwrap();
        let found = read_local(dir.path());
        assert_eq!(
            found,
            vec![("REVIEW.md".to_string(), "Flag every unwrap.".to_string())]
        );
    }
}
