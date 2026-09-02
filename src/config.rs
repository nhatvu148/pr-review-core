//! Environment configuration, loaded once at process start.
//!
//! Everything has a default except secrets; values that are required only for a
//! specific path (a provider token, the OpenRouter key) are validated at the
//! point of use via [`require`].

use std::env;

/// Resolved runtime configuration.
#[derive(Clone)]
pub struct Config {
    pub port: u16,
    pub worker_token: String,
    /// Shared secret GitHub signs webhook deliveries with (X-Hub-Signature-256).
    pub github_webhook_secret: String,
    /// Shared secret Bitbucket signs webhook deliveries with (X-Hub-Signature).
    pub bitbucket_webhook_secret: String,
    /// Shared token GitLab sends webhook deliveries with (X-Gitlab-Token). Unlike
    /// GitHub/Bitbucket this is a plain token compared verbatim, not an HMAC.
    pub gitlab_webhook_secret: String,

    /// OpenRouter (or any OpenAI-compatible) API key. Resolved from
    /// `OPENROUTER_API_KEY`, falling back to `LLM_API_KEY` — the alias lets
    /// Ollama/vLLM/local OpenAI-compatible servers reuse a generic var name.
    pub openrouter_api_key: String,
    /// Synthesis model: writes the final review findings (quality matters here).
    pub openrouter_model: String,
    /// Exploration model: drives the agentic tool loop (grep/read_file/list_dir)
    /// to gather context. Cheaper — it navigates files, it doesn't judge.
    pub openrouter_model_explore: String,
    /// Base URL of the OpenAI-compatible chat-completions API. Resolved from
    /// `LLM_BASE_URL`, then `OPENROUTER_BASE_URL`, defaulting to OpenRouter — the
    /// `LLM_BASE_URL` alias lets Ollama/vLLM/local OpenAI-compatible servers work.
    pub openrouter_base_url: String,
    pub openrouter_max_tokens: u32,
    pub openrouter_temperature: f32,
    /// Whole-request timeout for a single model call. The hand-rolled loop had
    /// none, so a stalled provider hung the entire review.
    pub openrouter_timeout_secs: u64,
    /// Retries for a transient failure (429 / 5xx) on one call. The hand-rolled
    /// loop had none, so a single 429 discarded the whole review.
    pub openrouter_max_retries: u32,

    pub max_diff_chars: usize,

    /// Glob patterns of files to INCLUDE in the diff before it's sent to the LLM.
    /// Empty means include everything (subject to `exclude_globs`).
    pub include_globs: Vec<String>,
    /// Glob patterns of files to EXCLUDE from the diff (lockfiles, generated,
    /// vendored, minified) — drops noise and saves tokens before the LLM call.
    pub exclude_globs: Vec<String>,

    /// Glob patterns marking vendored third-party source. Diff-hygiene (class D)
    /// findings are suppressed inside these paths — bulk-committing upstream code is
    /// the intent of a vendoring PR, not a defect. Empty means the conventional
    /// defaults ([`crate::diff::DEFAULT_VENDORED_DIRS`]).
    pub vendored_globs: Vec<String>,

    /// When packing a large diff to the size budget, group related changed files
    /// (a source + its test, i18n siblings) into one unit so they stay adjacent
    /// and pack together instead of being scattered by priority. Fail-open.
    pub file_bundling: bool,

    /// Run a second, skeptical "self-critique" pass that removes false positives
    /// and out-of-scope nits from the findings before posting.
    pub self_critique: bool,
    /// Drop findings whose model-reported `confidence` is below this threshold.
    pub min_confidence: u8,
    /// Hard cap on the number of findings posted (after sorting by severity).
    pub max_findings: usize,

    /// Re-anchor a finding whose `line` just missed a real diff line: snap it to
    /// the nearest diff line within a small window when that line's code matches
    /// what the finding references, so a small drift posts inline instead of
    /// folding into the summary. Conservative + fail-open.
    pub reanchor_findings: bool,

    /// Pass the PR's own description to the reviewer as a *statement of intent to
    /// verify the diff against* (coverage spec class B). The body is already
    /// fetched by every provider for `/describe`; this is what lets the review
    /// read it.
    ///
    /// It is PR-author-controlled text entering the prompt, so it is rendered
    /// inside the untrusted-content fence built by
    /// [`crate::prompt::build_user_prompt`] and governed by the
    /// `## Untrusted content` section of [`crate::prompt::REVIEW_RULES`]. Do not
    /// enable one without the other.
    pub pr_body: bool,
    /// Max characters of PR description handed to the reviewer. A description is
    /// context, not the artifact under review, so it gets a small fixed budget
    /// rather than a share of `max_diff_chars`.
    pub pr_body_max_chars: usize,

    /// Let the agentic reviewer's `grep` tool return N lines of context around
    /// each match. Off restores the previous behaviour exactly (bare matching
    /// lines), which is what makes this A/B-able.
    ///
    /// Context is what lets the reviewer judge *two sites against each other*
    /// rather than each line against itself — the recorded miss shape in
    /// `pr-review-docs/feedback/` (kuroko#1, wincrust#13, vexar#63). A bare
    /// matching line proves site B exists but not what it does.
    pub grep_context: bool,

    /// Use the agentic reviewer: clone the repo and let the model investigate
    /// cross-file context with tools, instead of a single diff-only call.
    pub agentic: bool,
    /// Max tool-loop turns for the agentic reviewer (cost guard).
    pub max_turns: usize,
    /// Char budget for accumulated tool results in the agent conversation;
    /// older results are elided once newer ones fill it (bounds per-turn tokens).
    pub max_history_chars: usize,

    /// Re-review when new commits are pushed to an open PR (GitHub `synchronize`
    /// / Bitbucket `pullrequest:updated`). Off by default so iterating on a PR
    /// doesn't trigger a fresh (expensive) review per push — `opened`/`reopened`/
    /// `ready_for_review` always review; use the manual `POST /review` endpoint
    /// to re-review on demand.
    pub review_on_update: bool,

    pub github_token: String,
    pub github_api_base: String,

    pub bitbucket_email: String,
    pub bitbucket_token: String,
    pub bitbucket_api_base: String,

    pub gitlab_token: String,
    pub gitlab_api_base: String,

    /// Signature appended to every comment the bot posts and used as the dedupe
    /// key to find/update its own comments. Injected so the library carries no
    /// hardcoded bot identity.
    pub comment_marker: String,
    /// `User-Agent` header sent on provider (GitHub) API requests.
    pub user_agent: String,
    /// `HTTP-Referer` header sent to OpenRouter (attribution).
    pub http_referer: String,
    /// `X-Title` header sent to OpenRouter (attribution).
    pub x_title: String,
    /// Extra system-prompt text appended to the built-in prompts. Lets a consumer
    /// inject a large conventions block (e.g. via a file baked into its image)
    /// without changing the library.
    pub extra_system_prompt: String,

    /// Free-form instructions shaping the `/describe` output — release-notes
    /// sections, a contributor table, a house layout. Appended last to the
    /// describe prompt and declared to outrank its built-in section list, so a
    /// repo asking for a different shape gets that shape rather than both.
    /// Overridable per-repo via `describe_instructions` in `.prbot.toml`.
    pub describe_instructions: String,

    /// Compute "structural context" (the enclosing function/symbol of each changed
    /// line) and inject it into the review prompt so the model knows each change's
    /// scope. Fully fail-open — never blocks a review.
    pub structural_context: bool,
    /// Max number of files to fetch + parse for structural context (cost guard).
    pub structural_max_files: usize,

    /// Surface deterministic complexity metrics (cyclomatic + cognitive, A–F grade)
    /// for the functions a change touches, computed with tree-sitter alongside the
    /// structural context. A cheap, LLM-free risk signal. Fully fail-open.
    pub complexity_metrics: bool,
    /// Only report a changed function when its cyclomatic complexity is at least
    /// this — keeps the block a high-signal risk flag, not noise for trivial code.
    pub complexity_min_cyclomatic: u32,

    /// Append a **walkthrough table** to the summary comment: one row per changed
    /// file with its line counts, the definitions the change landed in, the worst
    /// complexity grade among them, and the findings filed there. Rendered from
    /// the structural parse that already ran — no model call, no extra token.
    /// Needs `structural_context`; off by default because it changes what every
    /// review comment looks like.
    pub walkthrough: bool,
    /// Max distinct symbols listed per file in the walkthrough before the cell
    /// collapses to `(+N more)` — a readability guard, not a cost one. 4 rather
    /// than 6 because a Rust name runs ~40 characters: six of them made a
    /// 174-character cell that pushed the *Findings* column off the right edge of
    /// a real PR comment.
    pub walkthrough_max_symbols: usize,
    /// Append a **mermaid change diagram** to the summary comment: changed symbols
    /// grouped by file, with an arrow wherever one names another. Derived from
    /// tree-sitter spans, never model-written. Skipped on providers that don't
    /// render mermaid (Bitbucket) and whenever there are no edges to draw.
    pub diagram: bool,
    /// Ceiling on how many changed symbols edge-linking considers, which is also
    /// the diagram's node budget. 12 is a legibility number, not a cost one: at 25
    /// the rendered graph sprawled past the width of a PR comment and mermaid's own
    /// pan controls covered a node. The pairwise
    /// span scan is cheap but quadratic, and a 200-node diagram is unreadable
    /// anyway. Past this the scan **narrows** — it ranks by complexity (test
    /// scaffolding last) and links the top slice — rather than giving up, and the
    /// diagram says it narrowed. Giving up meant no diagram on any real PR.
    pub diagram_max_nodes: usize,

    /// Compute a "blast radius" for the agentic reviewer: from the clone, find the
    /// callers and tests of each changed symbol and seed the prompt with them (also
    /// exposes a `references` tool). Agentic path only — needs the clone. Fail-open.
    pub blast_radius: bool,
    /// Max number of changed symbols to expand into a blast radius (cost guard).
    pub blast_max_symbols: usize,
    /// Max call sites listed per symbol per bucket (callers / tests) (cost guard).
    pub blast_max_refs: usize,

    /// Fetch the head commit's CI results (GitHub check runs / Bitbucket build
    /// statuses) with the PR metadata and surface them in the prompt, so the
    /// reviewer can't assert a broken build that CI already decided.
    ///
    /// One extra API call per review. Off is for tokens near their rate limit;
    /// with it off the reviewer simply sees no CI block. Fully fail-open either way.
    pub ci_status: bool,
    /// Scan changed lockfiles for known-vulnerable dependencies via OSV.dev and
    /// append advisories to the review summary. Fully fail-open — never blocks a
    /// review.
    pub cve_scan: bool,
    /// Max distinct packages queried against OSV per review (cost/fan-out guard).
    pub cve_max_packages: usize,
    /// Base URL of the OSV.dev API (override for a mirror or a test double).
    pub osv_api_base: String,

    /// Where to write the JSONL run log, one line per review — `PRBOT_RUN_LOG`.
    ///
    /// `None` (the default) disables logging entirely. `-` selects stdout; any
    /// other value is a file path. Opt-in because a record carries the finding
    /// text, which is review commentary on someone's source: a deployed bot must
    /// log nothing unless its operator asked it to. See [`crate::runlog`].
    pub run_log: Option<crate::runlog::RunLogSink>,
}

impl Config {
    /// Build the config from environment variables, applying defaults.
    pub fn from_env() -> Self {
        Self {
            // Local default avoids the common 8080 clash with Docker Desktop.
            // Production pins PORT=8080 via the Dockerfile, matching fly.toml.
            port: env_or("PORT", "8088").parse().unwrap_or(8088),
            worker_token: env::var("WORKER_TOKEN").unwrap_or_default(),
            github_webhook_secret: env::var("GITHUB_WEBHOOK_SECRET").unwrap_or_default(),
            bitbucket_webhook_secret: env::var("BITBUCKET_WEBHOOK_SECRET").unwrap_or_default(),
            gitlab_webhook_secret: env::var("GITLAB_WEBHOOK_SECRET").unwrap_or_default(),

            openrouter_api_key: env::var("OPENROUTER_API_KEY")
                .or_else(|_| env::var("LLM_API_KEY"))
                .unwrap_or_default(),
            openrouter_model: env_or("OPENROUTER_MODEL", "anthropic/claude-sonnet-4.5"),
            openrouter_model_explore: env_or("OPENROUTER_MODEL_EXPLORE", "moonshotai/kimi-k2-0905"),
            openrouter_base_url: env::var("LLM_BASE_URL")
                .or_else(|_| env::var("OPENROUTER_BASE_URL"))
                .unwrap_or_else(|_| "https://openrouter.ai/api/v1".to_string()),
            openrouter_max_tokens: env_or("OPENROUTER_MAX_TOKENS", "4000")
                .parse()
                .unwrap_or(4000),
            openrouter_temperature: env_or("OPENROUTER_TEMPERATURE", "0.2")
                .parse()
                .unwrap_or(0.2),
            openrouter_timeout_secs: env_or("OPENROUTER_TIMEOUT_SECS", "120")
                .parse()
                .unwrap_or(120),
            openrouter_max_retries: env_or("OPENROUTER_MAX_RETRIES", "3").parse().unwrap_or(3),

            max_diff_chars: env_or("MAX_DIFF_CHARS", "200000")
                .parse()
                .unwrap_or(200_000),

            include_globs: env_globs("INCLUDE_GLOBS", &[]),
            exclude_globs: env_globs(
                "EXCLUDE_GLOBS",
                &[
                    "**/*.lock",
                    "**/package-lock.json",
                    "**/pnpm-lock.yaml",
                    "**/yarn.lock",
                    "**/bun.lockb",
                    "**/Cargo.lock",
                    "**/go.sum",
                    "**/composer.lock",
                    "**/Gemfile.lock",
                    "**/poetry.lock",
                    "**/*.min.js",
                    "**/*.min.css",
                    "**/dist/**",
                    "**/build/**",
                    "**/vendor/**",
                    "**/node_modules/**",
                    "**/*.snap",
                    "**/__snapshots__/**",
                    "**/*.pb.go",
                    "**/*_generated.*",
                    "**/*.generated.*",
                ],
            ),

            // Empty = the conventional vendored directories (see `diff::is_vendored`).
            vendored_globs: env_globs("VENDORED_GLOBS", &[]),

            file_bundling: env_or("FILE_BUNDLING", "true").parse().unwrap_or(true),

            self_critique: env_or("SELF_CRITIQUE", "true").parse().unwrap_or(true),
            min_confidence: env_or("MIN_CONFIDENCE", "0").parse().unwrap_or(0),
            max_findings: env_or("MAX_FINDINGS", "20").parse().unwrap_or(20),
            reanchor_findings: env_or("REANCHOR_FINDINGS", "true").parse().unwrap_or(true),

            agentic: env_or("AGENTIC", "false").parse().unwrap_or(false),
            max_turns: env_or("MAX_TURNS", "6").parse().unwrap_or(6),
            max_history_chars: env_or("MAX_HISTORY_CHARS", "45000")
                .parse()
                .unwrap_or(45_000),

            review_on_update: env_or("REVIEW_ON_UPDATE", "false").parse().unwrap_or(false),

            github_token: env::var("GH_TOKEN").unwrap_or_default(),
            github_api_base: env_or("GH_API_BASE", "https://api.github.com"),

            bitbucket_email: env::var("BB_EMAIL").unwrap_or_default(),
            bitbucket_token: env::var("BB_API_TOKEN").unwrap_or_default(),
            bitbucket_api_base: "https://api.bitbucket.org/2.0".to_string(),

            gitlab_token: env::var("GITLAB_TOKEN").unwrap_or_default(),
            gitlab_api_base: env_or("GITLAB_API_BASE", "https://gitlab.com/api/v4"),

            comment_marker: env_or("COMMENT_MARKER", "🤖 ai-pr-review"),
            user_agent: env_or("USER_AGENT", "pr-review-core"),
            http_referer: env_or(
                "OPENROUTER_HTTP_REFERER",
                "https://github.com/nhatvu148/pr-review-core",
            ),
            x_title: env_or("OPENROUTER_X_TITLE", "pr-review"),
            extra_system_prompt: resolve_extra_system_prompt(),

            describe_instructions: env_or("DESCRIBE_INSTRUCTIONS", ""),

            pr_body: env_or("PR_BODY", "true").parse().unwrap_or(true),
            pr_body_max_chars: env_or("PR_BODY_MAX_CHARS", "4000").parse().unwrap_or(4000),
            grep_context: env_or("GREP_CONTEXT", "true").parse().unwrap_or(true),

            structural_context: env_or("STRUCTURAL_CONTEXT", "true").parse().unwrap_or(true),
            structural_max_files: env_or("STRUCTURAL_MAX_FILES", "15").parse().unwrap_or(15),

            complexity_metrics: env_or("COMPLEXITY_METRICS", "true").parse().unwrap_or(true),
            complexity_min_cyclomatic: env_or("COMPLEXITY_MIN_CYCLOMATIC", "8")
                .parse()
                .unwrap_or(8),

            walkthrough: env_or("WALKTHROUGH", "false").parse().unwrap_or(false),
            walkthrough_max_symbols: env_or("WALKTHROUGH_MAX_SYMBOLS", "4").parse().unwrap_or(4),
            diagram: env_or("DIAGRAM", "false").parse().unwrap_or(false),
            diagram_max_nodes: env_or("DIAGRAM_MAX_NODES", "12").parse().unwrap_or(12),

            blast_radius: env_or("BLAST_RADIUS", "true").parse().unwrap_or(true),
            blast_max_symbols: env_or("BLAST_MAX_SYMBOLS", "12").parse().unwrap_or(12),
            blast_max_refs: env_or("BLAST_MAX_REFS", "8").parse().unwrap_or(8),

            ci_status: env_or("CI_STATUS", "true").parse().unwrap_or(true),
            cve_scan: env_or("CVE_SCAN", "true").parse().unwrap_or(true),
            cve_max_packages: env_or("CVE_MAX_PACKAGES", "100").parse().unwrap_or(100),
            osv_api_base: env_or("OSV_API_BASE", "https://api.osv.dev"),

            // Empty or unset both mean off, so `PRBOT_RUN_LOG=` in an env file
            // disables it without deleting the line. `-` means stdout.
            run_log: env::var("PRBOT_RUN_LOG")
                .ok()
                .and_then(|v| crate::runlog::RunLogSink::from_env_value(&v)),
        }
    }

    /// Return a clone of this config with any fields set in `rc` overridden.
    ///
    /// Only `Some(..)` fields of the per-repo [`RepoConfig`] take effect; the rest
    /// keep the env-derived value. `rc.instructions` is *appended* to
    /// [`Config::extra_system_prompt`] (newline-separated) rather than replacing it,
    /// so a consumer's baked-in conventions block and the repo's own instructions
    /// both reach the model.
    ///
    /// # Examples
    /// ```
    /// # use pr_review_core::config::Config;
    /// # use pr_review_core::repo_config::RepoConfig;
    /// let base = Config::from_env();
    /// let rc = RepoConfig { min_confidence: Some(80), ..Default::default() };
    /// let effective = base.with_repo_overrides(&rc);
    /// assert_eq!(effective.min_confidence, 80);
    /// ```
    pub fn with_repo_overrides(&self, rc: &crate::repo_config::RepoConfig) -> Config {
        let mut cfg = self.clone();
        if let Some(v) = &rc.model {
            cfg.openrouter_model = v.clone();
        }
        if let Some(v) = &rc.model_explore {
            cfg.openrouter_model_explore = v.clone();
        }
        if let Some(v) = &rc.include_globs {
            cfg.include_globs = v.clone();
        }
        if let Some(v) = &rc.exclude_globs {
            cfg.exclude_globs = v.clone();
        }
        if let Some(v) = &rc.vendored {
            cfg.vendored_globs = v.clone();
        }
        if let Some(v) = rc.ci_status {
            cfg.ci_status = v;
        }
        if let Some(v) = rc.min_confidence {
            cfg.min_confidence = v;
        }
        if let Some(v) = rc.max_findings {
            cfg.max_findings = v;
        }
        if let Some(v) = rc.self_critique {
            cfg.self_critique = v;
        }
        if let Some(v) = rc.agentic {
            cfg.agentic = v;
        }
        if let Some(v) = rc.file_bundling {
            cfg.file_bundling = v;
        }
        if let Some(v) = rc.reanchor_findings {
            cfg.reanchor_findings = v;
        }
        if let Some(v) = &rc.instructions {
            let extra = v.trim();
            if !extra.is_empty() {
                if cfg.extra_system_prompt.is_empty() {
                    cfg.extra_system_prompt = extra.to_string();
                } else {
                    cfg.extra_system_prompt = format!("{}\n{extra}", cfg.extra_system_prompt);
                }
            }
        }
        // REPLACES rather than appends, unlike `instructions` above. A layout is
        // not additive: a repo that asks for release-notes sections wants those
        // sections, not those plus whatever the deployment's default layout said.
        if let Some(v) = &rc.describe_instructions {
            let extra = v.trim();
            if !extra.is_empty() {
                cfg.describe_instructions = extra.to_string();
            }
        }
        cfg
    }
}

/// Resolve the extra system prompt: prefer the inline `EXTRA_SYSTEM_PROMPT` env
/// var, fall back to the contents of the file named by `EXTRA_SYSTEM_PROMPT_FILE`,
/// else empty.
fn resolve_extra_system_prompt() -> String {
    match env::var("EXTRA_SYSTEM_PROMPT") {
        Ok(v) if !v.is_empty() => v,
        _ => match env::var("EXTRA_SYSTEM_PROMPT_FILE") {
            Ok(path) if !path.is_empty() => std::fs::read_to_string(path).unwrap_or_default(),
            _ => String::new(),
        },
    }
}

fn env_or(name: &str, default: &str) -> String {
    env::var(name).unwrap_or_else(|_| default.to_string())
}

/// Parse a comma-separated list of glob patterns from `name`, trimming each entry
/// and dropping empties. Falls back to `default` when the var is unset or empty.
fn env_globs(name: &str, default: &[&str]) -> Vec<String> {
    match env::var(name) {
        Ok(v) if !v.trim().is_empty() => v
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
        _ => default.iter().map(|s| s.to_string()).collect(),
    }
}

/// Ensure a required value is present, with a clear error naming the env var.
///
/// # Examples
/// ```
/// # use pr_review_core::config::require;
/// assert!(require("", "OPENROUTER_API_KEY").is_err());
/// assert!(require("sk-or-x", "OPENROUTER_API_KEY").is_ok());
/// ```
pub fn require(value: &str, name: &str) -> anyhow::Result<()> {
    if value.is_empty() {
        anyhow::bail!("Missing required env var: {name}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo_config::RepoConfig;

    #[test]
    fn overrides_only_set_fields_and_appends_instructions() {
        let mut base = Config::from_env();
        base.openrouter_model = "base/model".to_string();
        base.min_confidence = 10;
        base.max_findings = 5;
        base.self_critique = true;
        base.agentic = false;
        base.file_bundling = true;
        base.reanchor_findings = true;
        base.extra_system_prompt = "BASE CONVENTIONS".to_string();

        let rc = RepoConfig {
            model: Some("repo/model".to_string()),
            min_confidence: Some(75),
            self_critique: Some(false),
            agentic: Some(true),
            file_bundling: Some(false),
            reanchor_findings: Some(false),
            include_globs: Some(vec!["src/**".to_string()]),
            instructions: Some("Never nit about formatting.".to_string()),
            ..Default::default()
        };

        let eff = base.with_repo_overrides(&rc);

        // Overridden fields take the repo value.
        assert_eq!(eff.openrouter_model, "repo/model");
        assert_eq!(eff.min_confidence, 75);
        assert!(!eff.self_critique);
        assert!(eff.agentic);
        assert!(!eff.file_bundling);
        assert!(!eff.reanchor_findings);
        assert_eq!(eff.include_globs, vec!["src/**".to_string()]);
        // Untouched field keeps the base value.
        assert_eq!(eff.max_findings, 5);
        // Instructions are appended, not replaced.
        assert_eq!(
            eff.extra_system_prompt,
            "BASE CONVENTIONS\nNever nit about formatting."
        );
    }

    #[test]
    fn empty_repo_config_is_a_noop() {
        let mut base = Config::from_env();
        base.openrouter_model = "keep/me".to_string();
        base.extra_system_prompt = "keep".to_string();

        let eff = base.with_repo_overrides(&RepoConfig::default());

        assert_eq!(eff.openrouter_model, "keep/me");
        assert_eq!(eff.extra_system_prompt, "keep");
    }

    #[test]
    fn instructions_set_when_base_prompt_empty() {
        let mut base = Config::from_env();
        base.extra_system_prompt = String::new();
        let rc = RepoConfig {
            instructions: Some("Focus on security.".to_string()),
            ..Default::default()
        };
        let eff = base.with_repo_overrides(&rc);
        assert_eq!(eff.extra_system_prompt, "Focus on security.");
    }
}
