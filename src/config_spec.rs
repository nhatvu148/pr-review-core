//! The engine's environment-variable surface, as data.
//!
//! Every knob `Config::from_env` reads is a string literal buried in a 130-line
//! constructor, which means the surface exists but cannot be *enumerated* — so
//! nothing can type it, document it, or check it. That is the same shape of
//! problem as a wire struct declared in four places: real, invisible, and only
//! ever noticed downstream. Twenty of these were undocumented when this table
//! was written.
//!
//! This module is the enumeration, and [`tests::spec_matches_config_rs`] is what
//! makes it true rather than aspirational: it re-reads `config.rs` and fails the
//! build if a variable is read without an entry here, or an entry names a
//! variable nothing reads. Discipline is not required, and cannot be relied on.
//!
//! Consumers use it to generate things that would otherwise be hand-maintained
//! and drift — the README table, and typed `config` options in the npm and PyPI
//! clients, which today take these as untyped strings.

/// How a variable's string value is interpreted, which is what a generated
/// client needs in order to coerce a native value back into one.
///
/// `Secret` is a `Str` that must never be echoed: it separates "render this in
/// the docs table" from "render only its name".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigKind {
    Str,
    Bool,
    Int,
    Float,
    /// Comma-separated glob list.
    Globs,
    /// A filesystem path the engine reads.
    Path,
    /// A credential. Same parsing as `Str`; never printed.
    Secret,
}

impl ConfigKind {
    /// The name a generated client should use for this kind's TypeScript type.
    pub fn ts(self) -> &'static str {
        match self {
            ConfigKind::Bool => "boolean",
            ConfigKind::Int | ConfigKind::Float => "number",
            ConfigKind::Globs => "string[]",
            _ => "string",
        }
    }

    /// The same for Python.
    pub fn py(self) -> &'static str {
        match self {
            ConfigKind::Bool => "bool",
            ConfigKind::Int => "int",
            ConfigKind::Float => "float",
            ConfigKind::Globs => "List[str]",
            _ => "str",
        }
    }
}

/// One environment variable the engine reads.
#[derive(Debug, Clone, Copy)]
pub struct ConfigVar {
    /// The canonical name — the one a generated client should emit.
    pub env: &'static str,
    /// Older or alternative spellings, in the order `from_env` tries them AFTER
    /// `env`. A client never needs these; the docs do, because they are live.
    pub aliases: &'static [&'static str],
    pub kind: ConfigKind,
    /// The engine's default, as the string a user would set. `None` means unset
    /// is meaningful in itself — empty, absent, or off.
    pub default: Option<&'static str>,
    /// One line, for a generated table or a generated doc comment.
    pub doc: &'static str,
}

/// Every variable the engine reads, canonical name first.
pub const SPEC: &[ConfigVar] = &[
    ConfigVar {
        env: "AGENTIC",
        aliases: &[],
        kind: ConfigKind::Bool,
        default: Some("false"),
        doc: "Clone the repo and let the model investigate cross-file context (grep / read_file / list_dir) before writing findings.",
    },
    ConfigVar {
        env: "BB_API_TOKEN",
        aliases: &[],
        kind: ConfigKind::Secret,
        default: None,
        doc: "Atlassian API token for Bitbucket.",
    },
    ConfigVar {
        env: "BB_EMAIL",
        aliases: &[],
        kind: ConfigKind::Str,
        default: None,
        doc: "Atlassian account email, paired with BB_API_TOKEN for Bitbucket basic auth.",
    },
    ConfigVar {
        env: "BITBUCKET_WEBHOOK_SECRET",
        aliases: &[],
        kind: ConfigKind::Secret,
        default: None,
        doc: "HMAC secret for Bitbucket webhook deliveries.",
    },
    ConfigVar {
        env: "BLAST_MAX_REFS",
        aliases: &[],
        kind: ConfigKind::Int,
        default: Some("8"),
        doc: "References reported per symbol by the blast-radius scan.",
    },
    ConfigVar {
        env: "BLAST_MAX_SYMBOLS",
        aliases: &[],
        kind: ConfigKind::Int,
        default: Some("12"),
        doc: "Changed symbols the blast-radius scan will follow.",
    },
    ConfigVar {
        env: "BLAST_RADIUS",
        aliases: &[],
        kind: ConfigKind::Bool,
        default: Some("true"),
        doc: "Precompute callers, tests and type uses of changed symbols and seed the agentic reviewer with them. Measured no recall gain on well-named repos; may help on large monorepos.",
    },
    ConfigVar {
        env: "CI_STATUS",
        aliases: &[],
        kind: ConfigKind::Bool,
        default: Some("true"),
        doc: "Fetch the head commit's CI results so the reviewer cannot assert a broken build CI already decided. One extra API call per review.",
    },
    ConfigVar {
        env: "COMMENT_MARKER",
        aliases: &[],
        kind: ConfigKind::Str,
        default: Some("🤖 ai-pr-review"),
        doc: "Signature appended to every comment, and the dedupe key for finding the bot's own comments on re-review.",
    },
    ConfigVar {
        env: "COMPLEXITY_METRICS",
        aliases: &[],
        kind: ConfigKind::Bool,
        default: Some("true"),
        doc: "Report cyclomatic and cognitive complexity (A-F) for touched functions. Deterministic; no model call.",
    },
    ConfigVar {
        env: "COMPLEXITY_MIN_CYCLOMATIC",
        aliases: &[],
        kind: ConfigKind::Int,
        default: Some("8"),
        doc: "Only surface functions at or above this cyclomatic complexity.",
    },
    ConfigVar {
        env: "CVE_MAX_PACKAGES",
        aliases: &[],
        kind: ConfigKind::Int,
        default: Some("100"),
        doc: "Distinct packages queried against OSV per review.",
    },
    ConfigVar {
        env: "CVE_SCAN",
        aliases: &[],
        kind: ConfigKind::Bool,
        default: Some("true"),
        doc: "Check added lockfile entries against OSV.dev for known vulnerabilities.",
    },
    ConfigVar {
        env: "DESCRIBE_INSTRUCTIONS",
        aliases: &[],
        kind: ConfigKind::Str,
        default: None,
        doc: "Free-form instructions shaping /describe output. Outranks the built-in layout.",
    },
    ConfigVar {
        env: "DIAGRAM",
        aliases: &[],
        kind: ConfigKind::Bool,
        default: Some("false"),
        doc: "Append the mermaid change diagram. Skipped on Bitbucket, and whenever there are no edges to draw.",
    },
    ConfigVar {
        env: "DIAGRAM_MAX_NODES",
        aliases: &[],
        kind: ConfigKind::Int,
        default: Some("12"),
        doc: "Symbols considered for edge linking, and so the diagram's node budget.",
    },
    ConfigVar {
        env: "EXCLUDE_GLOBS",
        aliases: &[],
        kind: ConfigKind::Globs,
        default: None,
        doc: "Globs skipped before the model call. Setting this REPLACES the lockfile/generated/vendored/minified defaults.",
    },
    ConfigVar {
        env: "EXTRA_SYSTEM_PROMPT",
        aliases: &[],
        kind: ConfigKind::Str,
        default: None,
        doc: "Appended to the built-in system prompts. Your conventions, in plain language. Set but empty is the same as unset: EXTRA_SYSTEM_PROMPT_FILE is consulted either way.",
    },
    ConfigVar {
        env: "EXTRA_SYSTEM_PROMPT_FILE",
        aliases: &[],
        kind: ConfigKind::Path,
        default: None,
        doc: "Path whose contents are used when EXTRA_SYSTEM_PROMPT is unset OR empty. For baking a large conventions block into an image.",
    },
    ConfigVar {
        env: "FILE_BUNDLING",
        aliases: &[],
        kind: ConfigKind::Bool,
        default: Some("true"),
        doc: "Keep related files (a source and its test, i18n siblings) adjacent when packing, so the model reviews them together.",
    },
    ConfigVar {
        env: "GH_API_BASE",
        aliases: &[],
        kind: ConfigKind::Str,
        default: Some("https://api.github.com"),
        doc: "GitHub API base. Point at a GitHub Enterprise host.",
    },
    ConfigVar {
        env: "GH_TOKEN",
        aliases: &[],
        kind: ConfigKind::Secret,
        default: None,
        doc: "GitHub token used to read the PR and post comments.",
    },
    ConfigVar {
        env: "GITHUB_WEBHOOK_SECRET",
        aliases: &[],
        kind: ConfigKind::Secret,
        default: None,
        doc: "HMAC secret GitHub signs webhook deliveries with.",
    },
    ConfigVar {
        env: "GITLAB_API_BASE",
        aliases: &[],
        kind: ConfigKind::Str,
        default: Some("https://gitlab.com/api/v4"),
        doc: "GitLab API base. Point at a self-hosted instance.",
    },
    ConfigVar {
        env: "GITLAB_TOKEN",
        aliases: &[],
        kind: ConfigKind::Secret,
        default: None,
        doc: "GitLab token used to read the MR and post notes.",
    },
    ConfigVar {
        env: "GITLAB_WEBHOOK_SECRET",
        aliases: &[],
        kind: ConfigKind::Secret,
        default: None,
        doc: "Token GitLab sends as X-Gitlab-Token on webhook deliveries.",
    },
    ConfigVar {
        env: "GREP_CONTEXT",
        aliases: &[],
        kind: ConfigKind::Bool,
        default: Some("true"),
        doc: "Let the agentic reviewer's grep request 1-8 lines of context per match, so it can judge a second site without a read_file round trip.",
    },
    ConfigVar {
        env: "INCLUDE_GLOBS",
        aliases: &[],
        kind: ConfigKind::Globs,
        default: None,
        doc: "If set, ONLY files matching these globs are reviewed.",
    },
    ConfigVar {
        env: "LLM_BASE_URL",
        aliases: &["OPENROUTER_BASE_URL"],
        kind: ConfigKind::Str,
        default: Some("https://openrouter.ai/api/v1"),
        doc: "OpenAI-compatible endpoint, e.g. http://localhost:11434/v1 for Ollama.",
    },
    ConfigVar {
        env: "MAX_DIFF_CHARS",
        aliases: &[],
        kind: ConfigKind::Int,
        default: Some("200000"),
        doc: "Size budget for the packed diff. Beyond it, whole files are ranked and dropped rather than truncated mid-hunk.",
    },
    ConfigVar {
        env: "MAX_FINDINGS",
        aliases: &[],
        kind: ConfigKind::Int,
        default: Some("20"),
        doc: "Cap findings per PR, ranked by severity then confidence.",
    },
    ConfigVar {
        env: "MAX_HISTORY_CHARS",
        aliases: &[],
        kind: ConfigKind::Int,
        default: Some("45000"),
        doc: "Cap on the agentic conversation carried between turns.",
    },
    ConfigVar {
        env: "MAX_TURNS",
        aliases: &[],
        kind: ConfigKind::Int,
        default: Some("6"),
        doc: "Tool-call turns the agentic reviewer may take before it must conclude.",
    },
    ConfigVar {
        env: "MIN_CONFIDENCE",
        aliases: &[],
        kind: ConfigKind::Int,
        default: Some("0"),
        doc: "Drop findings below this confidence (0-100).",
    },
    ConfigVar {
        env: "OPENROUTER_API_KEY",
        aliases: &["LLM_API_KEY"],
        kind: ConfigKind::Secret,
        default: None,
        doc: "API key for the OpenAI-compatible endpoint. Required for every review.",
    },
    ConfigVar {
        env: "OPENROUTER_HTTP_REFERER",
        aliases: &[],
        kind: ConfigKind::Str,
        default: Some("https://github.com/nhatvu148/pr-review-core"),
        doc: "HTTP-Referer sent to OpenRouter, for its dashboard attribution.",
    },
    ConfigVar {
        env: "OPENROUTER_MAX_RETRIES",
        aliases: &[],
        kind: ConfigKind::Int,
        default: Some("3"),
        doc: "Retries on a failed or rate-limited model call.",
    },
    ConfigVar {
        env: "OPENROUTER_MAX_TOKENS",
        aliases: &[],
        kind: ConfigKind::Int,
        default: Some("4000"),
        doc: "Cap on completion tokens per model call.",
    },
    ConfigVar {
        env: "OPENROUTER_MODEL",
        aliases: &[],
        kind: ConfigKind::Str,
        default: Some("anthropic/claude-sonnet-4.5"),
        doc: "Model that writes the review, and the synthesis half of the agentic split.",
    },
    ConfigVar {
        env: "OPENROUTER_MODEL_EXPLORE",
        aliases: &[],
        kind: ConfigKind::Str,
        default: Some("moonshotai/kimi-k2-0905"),
        doc: "Cheaper model for the agentic explore turns, before synthesis.",
    },
    ConfigVar {
        env: "OPENROUTER_TEMPERATURE",
        aliases: &[],
        kind: ConfigKind::Float,
        default: Some("0.2"),
        doc: "Sampling temperature. Low on purpose: a review should be reproducible.",
    },
    ConfigVar {
        env: "OPENROUTER_TIMEOUT_SECS",
        aliases: &[],
        kind: ConfigKind::Int,
        default: Some("120"),
        doc: "Per-request timeout for a model call.",
    },
    ConfigVar {
        env: "OPENROUTER_X_TITLE",
        aliases: &[],
        kind: ConfigKind::Str,
        default: Some("pr-review"),
        doc: "X-Title sent to OpenRouter, for its dashboard attribution.",
    },
    ConfigVar {
        env: "OSV_API_BASE",
        aliases: &[],
        kind: ConfigKind::Str,
        default: Some("https://api.osv.dev"),
        doc: "OSV API base. Override for a mirror or a test double.",
    },
    ConfigVar {
        env: "PORT",
        aliases: &[],
        kind: ConfigKind::Int,
        default: Some("8088"),
        doc: "HTTP port for a bot serving webhooks. 8088 locally to dodge the usual Docker Desktop clash on 8080.",
    },
    ConfigVar {
        env: "PRBOT_RUN_LOG",
        aliases: &[],
        kind: ConfigKind::Path,
        default: None,
        doc: "Path to append one JSON record per review to. `-` means stdout; empty means off, so a line in an env file can disable it without being deleted.",
    },
    ConfigVar {
        env: "PR_BODY",
        aliases: &[],
        kind: ConfigKind::Bool,
        default: Some("true"),
        doc: "Give the reviewer the PR's own description as a statement of intent to check the diff against. Rendered inside an untrusted fence, so it can never direct the review.",
    },
    ConfigVar {
        env: "PR_BODY_MAX_CHARS",
        aliases: &[],
        kind: ConfigKind::Int,
        default: Some("12000"),
        doc: "Cap on the description handed to the reviewer. A clipped one is marked truncated, so absence is not read as out-of-scope.",
    },
    ConfigVar {
        env: "REANCHOR_FINDINGS",
        aliases: &[],
        kind: ConfigKind::Bool,
        default: Some("true"),
        doc: "Snap a finding that drifted just off a diff line onto the nearest diff line sharing its code symbol, instead of folding it into the summary.",
    },
    ConfigVar {
        env: "REVIEW_ON_UPDATE",
        aliases: &[],
        kind: ConfigKind::Bool,
        default: Some("false"),
        doc: "Re-review automatically when a PR gets new commits. Off by default: pushing is the inner loop, and every round costs a full review.",
    },
    ConfigVar {
        env: "SELF_CRITIQUE",
        aliases: &[],
        kind: ConfigKind::Bool,
        default: Some("true"),
        doc: "Second skeptical pass that removes false positives and low-value nits.",
    },
    ConfigVar {
        env: "STRUCTURAL_CONTEXT",
        aliases: &[],
        kind: ConfigKind::Bool,
        default: Some("true"),
        doc: "Name the enclosing function or symbol of each changed line, via tree-sitter, with no clone.",
    },
    ConfigVar {
        env: "STRUCTURAL_MAX_FILES",
        aliases: &[],
        kind: ConfigKind::Int,
        default: Some("15"),
        doc: "Files fetched for structural context before it stops.",
    },
    ConfigVar {
        env: "SUGGESTIONS",
        aliases: &[],
        kind: ConfigKind::Bool,
        default: Some("true"),
        doc: "Attach a committable suggestion block when the model proposed replacement text that validates against the anchored line. Never on a re-anchored finding.",
    },
    ConfigVar {
        env: "USER_AGENT",
        aliases: &[],
        kind: ConfigKind::Str,
        default: Some("pr-review-core"),
        doc: "User-Agent sent to provider APIs.",
    },
    ConfigVar {
        env: "VENDORED_GLOBS",
        aliases: &[],
        kind: ConfigKind::Globs,
        default: None,
        doc: "Globs marking third-party source: hygiene findings are suppressed inside them and the reviewer is told not to edit there. Setting this REPLACES the defaults.",
    },
    ConfigVar {
        env: "WALKTHROUGH",
        aliases: &[],
        kind: ConfigKind::Bool,
        default: Some("false"),
        doc: "Append the per-file walkthrough table to the summary comment.",
    },
    ConfigVar {
        env: "WALKTHROUGH_MAX_SYMBOLS",
        aliases: &[],
        kind: ConfigKind::Int,
        default: Some("4"),
        doc: "Symbols listed per file before the cell collapses to (+N more).",
    },
    ConfigVar {
        env: "WORKER_TOKEN",
        aliases: &[],
        kind: ConfigKind::Secret,
        default: None,
        doc: "Shared secret authenticating a bot's own async worker callback.",
    },
];

/// The option name a TypeScript client should expose for this variable.
///
/// Derived, not chosen: `MIN_CONFIDENCE` becomes `minConfidence`. Doing this in
/// Rust rather than in the generator means both language clients get their names
/// from one definition and cannot drift into calling the same knob two things.
pub fn camel_case(env: &str) -> String {
    let mut out = String::with_capacity(env.len());
    let mut upper = false;
    for (i, c) in env.chars().enumerate() {
        if c == '_' {
            upper = true;
        } else if i == 0 {
            out.push(c.to_ascii_lowercase());
        } else if upper {
            out.push(c.to_ascii_uppercase());
            upper = false;
        } else {
            out.push(c.to_ascii_lowercase());
        }
    }
    out
}

/// The same for Python: `MIN_CONFIDENCE` becomes `min_confidence`.
pub fn snake_case(env: &str) -> String {
    env.to_ascii_lowercase()
}

/// The whole surface as JSON, for the client generators.
///
/// Carries the derived key names alongside the env name and the language types,
/// so a generator transcribes rather than decides. Everything a typed `config`
/// option needs is here; nothing about it is inferred downstream.
pub fn as_json() -> String {
    let vars: Vec<serde_json::Value> = SPEC
        .iter()
        .map(|v| {
            serde_json::json!({
                "env": v.env,
                "aliases": v.aliases,
                "camel": camel_case(v.env),
                "snake": snake_case(v.env),
                "kind": format!("{:?}", v.kind),
                "ts": v.kind.ts(),
                "py": v.kind.py(),
                "default": v.default,
                "secret": v.kind == ConfigKind::Secret,
                "doc": v.doc,
            })
        })
        .collect();
    serde_json::to_string_pretty(&serde_json::json!({ "vars": vars }))
        .expect("SPEC is plain data and always serializes")
}

/// The whole surface as a markdown table, for the README.
///
/// Generated rather than hand-written because the hand-written one drifted: it
/// covered 41 of the 61 names in use when this was added, and nothing could have
/// told you which 20 were missing. A generated table is wrong only if [`SPEC`]
/// is wrong, and `spec_matches_config_rs` is what stops that.
///
/// A `Secret`'s default is never rendered — they are all empty today, but a
/// table that prints one the day that changes is a table that leaks a key into a
/// README.
pub fn markdown_table() -> String {
    let mut out = String::from("| Env var | Default | Meaning |\n| --- | --- | --- |\n");
    for v in SPEC {
        let names = if v.aliases.is_empty() {
            format!("`{}`", v.env)
        } else {
            // Aliases are live, so a reader searching for the old name must find
            // this row rather than conclude the knob was removed.
            format!(
                "`{}`{}",
                v.env,
                v.aliases
                    .iter()
                    .map(|a| format!(" / `{a}`"))
                    .collect::<String>()
            )
        };
        let default = match (v.kind, v.default) {
            (ConfigKind::Secret, _) => "*(unset)*".to_string(),
            (_, Some(d)) => format!("`{d}`"),
            (_, None) => "*(unset)*".to_string(),
        };
        out.push_str(&format!("| {names} | {default} | {} |\n", v.doc));
    }
    out
}

/// Look up a variable by its canonical name or any alias.
pub fn find(name: &str) -> Option<&'static ConfigVar> {
    SPEC.iter()
        .find(|v| v.env == name || v.aliases.contains(&name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// Every name in the spec, canonical and aliased.
    fn spec_names() -> BTreeSet<&'static str> {
        SPEC.iter()
            .flat_map(|v| std::iter::once(v.env).chain(v.aliases.iter().copied()))
            .collect()
    }

    /// Every name `config.rs` actually reads from the environment.
    ///
    /// Whitespace-tolerant on purpose. A single-line pattern reads correctly and
    /// is wrong: two of these calls wrap their argument onto the next line, and a
    /// naive regex silently found 59 of 61 — an under-count is the one failure
    /// this test must not have, because it makes a missing entry look fine.
    fn names_read_by_config_rs() -> BTreeSet<String> {
        let src = include_str!("config.rs");
        let re = regex::Regex::new(r#"(?:env::var|env_or|env_globs)\s*\(\s*"([A-Z][A-Z0-9_]*)""#)
            .expect("valid regex");
        re.captures_iter(src).map(|c| c[1].to_string()).collect()
    }

    /// The spec and the code must name exactly the same set.
    ///
    /// Both directions matter. A variable read without an entry is undocumented
    /// and untypeable — the state this module was written to end. An entry for a
    /// variable nothing reads is worse: it documents a knob that does nothing,
    /// and a user who sets it gets silence.
    #[test]
    fn spec_matches_config_rs() {
        let read = names_read_by_config_rs();
        let spec = spec_names();
        let missing: Vec<_> = read.iter().filter(|n| !spec.contains(n.as_str())).collect();
        let stale: Vec<_> = spec.iter().filter(|n| !read.contains(**n)).collect();
        assert!(
            missing.is_empty() && stale.is_empty(),
            "config spec drift.\n  read by config.rs but not in SPEC: {missing:?}\n  \
             in SPEC but not read by config.rs: {stale:?}\n  \
             Fix src/config_spec.rs so the two agree."
        );
    }

    /// A doc line is what a generated table and a generated type comment carry,
    /// so an empty one ships an undocumented knob under the appearance of a
    /// documented surface.
    #[test]
    fn every_var_is_documented() {
        for v in SPEC {
            assert!(!v.doc.trim().is_empty(), "{} has no doc", v.env);
            assert!(v.doc.len() > 20, "{} has a stub doc: {:?}", v.env, v.doc);
        }
    }

    /// Canonical names and aliases must not collide, or `find` would depend on
    /// iteration order and a client could emit a name the engine reads second.
    #[test]
    fn names_are_unique() {
        let mut seen = BTreeSet::new();
        for v in SPEC {
            for name in std::iter::once(v.env).chain(v.aliases.iter().copied()) {
                assert!(seen.insert(name), "{name} appears twice in SPEC");
            }
        }
    }

    /// A secret's default must never reach the table. They are all empty today,
    /// which is exactly why this needs a test: nothing would notice the day one
    /// stopped being.
    #[test]
    fn the_table_never_prints_a_secret_default() {
        let table = markdown_table();
        assert!(table.contains("`OPENROUTER_API_KEY` / `LLM_API_KEY`"));
        for v in SPEC.iter().filter(|v| v.kind == ConfigKind::Secret) {
            assert!(table.contains(&format!("`{}`", v.env)), "{} missing", v.env);
        }
        // One row per variable, plus a header and a separator.
        assert_eq!(table.lines().count(), SPEC.len() + 2);
    }

    /// Key names are derived, so the derivation is the thing to test.
    #[test]
    fn keys_are_derived_predictably() {
        assert_eq!(camel_case("MIN_CONFIDENCE"), "minConfidence");
        assert_eq!(
            camel_case("OPENROUTER_MODEL_EXPLORE"),
            "openrouterModelExplore"
        );
        assert_eq!(camel_case("AGENTIC"), "agentic");
        assert_eq!(snake_case("MIN_CONFIDENCE"), "min_confidence");
        assert_eq!(snake_case("PR_BODY_MAX_CHARS"), "pr_body_max_chars");
    }

    /// Two variables must never collapse onto one option name, in either
    /// language — a collision would silently drop one knob from the client.
    #[test]
    fn derived_keys_are_unique() {
        for (label, f) in [
            ("camel", camel_case as fn(&str) -> String),
            ("snake", snake_case as fn(&str) -> String),
        ] {
            let mut seen = BTreeSet::new();
            for v in SPEC {
                let k = f(v.env);
                assert!(seen.insert(k.clone()), "{label} key {k:?} is used twice");
            }
        }
    }

    /// The JSON is what the generators read, so it must carry every variable
    /// with every field they need.
    #[test]
    fn the_json_carries_the_whole_spec() {
        let v: serde_json::Value = serde_json::from_str(&as_json()).expect("valid json");
        let vars = v["vars"].as_array().expect("vars array");
        assert_eq!(vars.len(), SPEC.len());
        for var in vars {
            for field in ["env", "camel", "snake", "kind", "ts", "py", "doc"] {
                assert!(!var[field].is_null(), "{field} missing from {var:?}");
            }
        }
    }

    #[test]
    fn find_resolves_aliases_to_their_canonical_entry() {
        assert_eq!(find("LLM_API_KEY").unwrap().env, "OPENROUTER_API_KEY");
        assert_eq!(find("OPENROUTER_BASE_URL").unwrap().env, "LLM_BASE_URL");
        assert!(find("NOT_A_REAL_VAR").is_none());
    }
}
