//! Orchestrator: fetch the diff, run the structured AI review, anchor findings
//! to diff lines, render a summary, and (unless dry-run) post the review.

use anyhow::Result;
use serde::Serialize;

use crate::agent::agentic_review;
use crate::backend::{OpenRouterBackend, ReviewBackend, ReviewContext};
use crate::config::Config;
use crate::diff::parse_valid_lines;
use crate::llm::{Finding, Review, ReviewResult, Usage};
use crate::providers::{InlineComment, PrMeta, Provider, ReviewPost};
use crate::repo::Workspace;
use crate::repo_config;

pub struct RunReviewInput {
    pub provider: String,
    /// `owner/repo` (GitHub) or `workspace/repo` (Bitbucket).
    pub repo: String,
    pub pr: u64,
    /// When true, generate the review but do NOT post it.
    pub dry_run: bool,
    /// When true, post a "Reviewing…" placeholder comment before the LLM call so
    /// the PR shows instant feedback (used on the webhook path). Ignored on dry-run.
    pub placeholder: bool,
}

/// Result of one review run (serialized as the HTTP/CLI response).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunReviewOutput {
    pub provider: String,
    pub repo: String,
    pub pr: u64,
    pub model: String,
    pub recommendation: String,
    pub findings: usize,
    /// The post-processed findings that were posted (after self-critique,
    /// confidence floor, sort, and cap). Exposed for tooling/benchmarks that need
    /// the structured findings, not just the count. Empty on an advisory-only run.
    #[serde(default)]
    pub findings_detail: Vec<Finding>,
    pub inline_posted: usize,
    pub posted: bool,
    pub comment_url: Option<String>,
    pub summary_markdown: String,
    pub usage: Option<Usage>,
}

/// Rank a severity for sorting (higher = more urgent). Unknown severities rank 0.
pub(crate) fn severity_rank(sev: &str) -> u8 {
    match sev.to_uppercase().as_str() {
        "BLOCKING" => 3,
        "HIGH" => 2,
        "MEDIUM" => 1,
        "LOW" => 0,
        _ => 0,
    }
}

pub(crate) fn severity_emoji(sev: &str) -> &'static str {
    match sev.to_uppercase().as_str() {
        "BLOCKING" => "🚨",
        "HIGH" => "⚠️",
        "MEDIUM" => "ℹ️",
        "LOW" => "💡",
        _ => "•",
    }
}

/// Rank a recommendation so two can be compared (higher = blocks more).
fn recommendation_rank(rec: &str) -> u8 {
    let r = rec.to_uppercase();
    if r.contains("BLOCK") {
        2
    } else if r.contains("CHANGES") {
        1
    } else {
        0
    }
}

/// The recommendation actually posted: the **stronger** of the model's own verdict
/// and the floor implied by the merged findings' max severity. Deterministic hygiene
/// findings are added after the model decides, so without this a MEDIUM "swept-in
/// binary" could sit under an "APPROVE". Only ever upgrades — never softens the model.
fn effective_recommendation(model_rec: &str, findings: &[Finding]) -> String {
    let max_sev = findings
        .iter()
        .map(|f| severity_rank(&f.severity))
        .max()
        .unwrap_or(0);
    let floor = match max_sev {
        3 => "BLOCK",                     // a BLOCKING finding
        2 | 1 => "APPROVE WITH CHANGES",  // HIGH or MEDIUM
        _ => "APPROVE",                   // LOW-only or none — don't force changes
    };
    if recommendation_rank(model_rec) >= recommendation_rank(floor) {
        model_rec.trim().to_string()
    } else {
        floor.to_string()
    }
}

/// This many findings making the same claim about different files are collapsed
/// into one. Three is the point where a list stops reading as several observations
/// and starts reading as one pattern.
const BURST_THRESHOLD: usize = 3;

/// The *shape* of a finding's claim, with the per-file parts removed: backticked
/// spans (paths, symbols) and digits (line counts, sizes) are dropped, and what
/// remains is truncated to its first dozen words.
///
/// Two findings sharing a key are making the same argument about different files.
fn burst_key(f: &Finding) -> String {
    let mut out = String::new();
    let mut in_tick = false;
    for c in f.body.chars() {
        match c {
            '`' => in_tick = !in_tick,
            _ if in_tick || c.is_ascii_digit() => {}
            _ => out.extend(c.to_lowercase()),
        }
    }
    out.split_whitespace()
        .take(12)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Collapse `BURST_THRESHOLD`+ findings that make the same claim about different
/// files into one finding that names the count and the files.
///
/// Two production failures had this signature — 111 vendored source files reported
/// as added binaries, then seven vendored files reported as oversized — and in both
/// the review read as "here are N separate problems" when the honest reading was
/// "here is one claim about N files". Collapsing is better output *and* a canary: a
/// systematic false positive is exactly a claim that repeats.
///
/// Order is preserved (each group lands where its first member was), and a group
/// keeps its highest-severity member as the representative so nothing is softened.
///
/// **A group containing a HIGH or BLOCKING finding is never collapsed.** Collapsing
/// is lossy — only the representative keeps its inline anchor and its `Fix:` text —
/// and three genuinely distinct serious bugs can share an opening phrase (the same
/// vulnerable pattern repeated across files is *also* what a real burst looks like).
/// At LOW/MEDIUM that trade is worth it; at HIGH it is not, because the cost of
/// dropping a real fix instruction exceeds the cost of a repetitive review. Both
/// recorded bursts were MEDIUM and LOW, so the canary is unaffected.
fn collapse_bursts(findings: Vec<Finding>) -> Vec<Finding> {
    let mut groups: Vec<(String, Vec<Finding>)> = Vec::new();
    for f in findings {
        let key = burst_key(&f);
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, g)) => g.push(f),
            None => groups.push((key, vec![f])),
        }
    }

    let mut out = Vec::new();
    for (key, mut group) in groups {
        if group.len() < BURST_THRESHOLD {
            out.append(&mut group);
            continue;
        }
        // Never merge serious findings — each one's anchor and fix must survive.
        let max_rank = group
            .iter()
            .map(|f| severity_rank(&f.severity))
            .max()
            .unwrap_or(0);
        if max_rank >= severity_rank("HIGH") {
            tracing::info!(
                "not collapsing {} findings at HIGH+ despite a shared claim: \"{key}\"",
                group.len()
            );
            out.append(&mut group);
            continue;
        }

        let n = group.len();
        tracing::warn!("collapsed {n} findings that make the same claim: \"{key}\"");

        // Keep the most severe member; ties keep the first (input order).
        let best = group
            .iter()
            .enumerate()
            .max_by_key(|(i, f)| (severity_rank(&f.severity), std::cmp::Reverse(*i)))
            .map_or(0, |(i, _)| i);
        let mut rep = group.remove(best);

        // Name enough files that the reader can check the pattern themselves; the
        // cap only exists to keep one comment from becoming a file listing.
        const MAX_NAMED: usize = 10;
        let mut named: Vec<String> = group
            .iter()
            .take(MAX_NAMED)
            .map(|f| format!("`{}`", f.file))
            .collect();
        if group.len() > MAX_NAMED {
            named.push(format!("and {} more", group.len() - MAX_NAMED));
        }
        rep.body = format!(
            "{} \n\nThe same applies to {} other file(s) in this change ({}) — reported once \
             rather than {n} times. If that pattern is expected here (vendored or generated \
             code, a mechanical refactor), treat this as one decision, not {n}.",
            rep.body.trim_end(),
            group.len(),
            named.join(", ")
        );
        out.push(rep);
    }
    out
}

/// Body for an inline comment: `<emoji> **SEVERITY** — <problem>. Fix: …`
fn inline_body(f: &Finding) -> String {
    format!(
        "{} **{}** — {}",
        severity_emoji(&f.severity),
        f.severity.to_uppercase(),
        f.body.trim()
    )
}

/// A finding may miss a real diff line by this many rows and still be re-anchored.
const REANCHOR_WINDOW: i64 = 3;

/// The shared symbol tying a finding to a diff line must be at least this long.
/// Short tokens (`sum`, `map`, `key`, `ctx`) collide too easily to be evidence of
/// the *same* code, so a match on them folds the finding to the summary instead of
/// risking a wrong inline anchor.
const MIN_ANCHOR_SYMBOL_LEN: usize = 4;

/// Extract identifier tokens (`[A-Za-z_][A-Za-z0-9_]*`) from `s`.
fn idents(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for ch in s.chars() {
        if ch.is_alphanumeric() || ch == '_' {
            cur.push(ch);
        } else if !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Significant symbols on a code line: identifiers ≥3 chars minus common keywords
/// — the tokens whose presence in a finding body ties the finding to this line.
fn line_symbols(text: &str) -> Vec<String> {
    const KW: &[&str] = &[
        "const", "let", "var", "function", "return", "import", "export", "from", "class",
        "interface", "type", "public", "private", "protected", "static", "async", "await", "for",
        "while", "new", "void", "null", "true", "false", "this", "self", "def", "func", "pub",
        "use", "mod", "struct", "enum", "impl", "package",
    ];
    idents(text)
        .into_iter()
        .filter(|w| w.len() >= 3 && !KW.contains(&w.as_str()))
        .collect()
}

/// Re-anchor a finding at `line` (which isn't itself a diff line) to the nearest
/// diff line within [`REANCHOR_WINDOW`] whose code shares a *significant* symbol
/// with the finding body. Conservative — no shared significant symbol means `None`
/// (the finding folds to the summary). Both sides are filtered through
/// [`line_symbols`] (identifiers ≥3 chars, common keywords dropped) and the shared
/// symbol must be ≥[`MIN_ANCHOR_SYMBOL_LEN`], so a collision on a short/incidental
/// token can't snap a finding to an unrelated line. Matching *code-line* symbols
/// (not raw prose) sidesteps prose/code ambiguity. Candidates are ordered by
/// (distance, line number) so ties resolve deterministically.
fn reanchor(
    line: u64,
    valid: &std::collections::HashSet<u64>,
    texts: &std::collections::HashMap<u64, String>,
    body: &str,
) -> Option<u64> {
    // Filter the body the same way as the code line, so only significant shared
    // symbols count — asymmetric filtering would let generic words match.
    let body_words: std::collections::HashSet<String> = line_symbols(body).into_iter().collect();
    if body_words.is_empty() {
        return None;
    }
    let mut cands: Vec<u64> = valid
        .iter()
        .copied()
        .filter(|&c| c != line && (c as i64 - line as i64).abs() <= REANCHOR_WINDOW)
        .collect();
    // (distance, line) — deterministic tie-break: equidistant candidates prefer the
    // lower line number rather than HashSet iteration order.
    cands.sort_by_key(|&c| ((c as i64 - line as i64).abs(), c));
    for c in cands {
        if let Some(text) = texts.get(&c) {
            if line_symbols(text)
                .iter()
                .any(|s| s.len() >= MIN_ANCHOR_SYMBOL_LEN && body_words.contains(s))
            {
                return Some(c);
            }
        }
    }
    None
}

/// The summary comment: overall + recommendation + any findings that couldn't be
/// anchored to a diff line (line-anchored ones go inline).
fn render_summary(
    review: &Review,
    recommendation: &str,
    unanchored: &[&Finding],
    inline_count: usize,
) -> String {
    let mut s = format!(
        "🤖 **Automated review**\n\n{}\n\n**Recommendation:** {}",
        review.summary.trim(),
        recommendation.trim()
    );
    if inline_count > 0 {
        s.push_str(&format!("\n\n_{inline_count} inline comment(s) below._"));
    }
    if unanchored.is_empty() {
        if inline_count == 0 {
            s.push_str("\n\nNo blocking issues found.");
        }
    } else {
        s.push_str("\n\n## Findings");
        for f in unanchored {
            let loc = match f.line {
                Some(l) => format!("`{}` (~{l})", f.file),
                None => format!("`{}`", f.file),
            };
            s.push_str(&format!(
                "\n- {} **{}** — {loc} — {}",
                severity_emoji(&f.severity),
                f.severity.to_uppercase(),
                f.body.trim()
            ));
        }
    }
    s.push_str("\n\n_Automated advisory review — a human still owns the merge decision._");
    s
}

/// Placeholder summary body shown immediately while the review runs.
fn render_pending() -> String {
    "🤖 **Automated review**\n\n⏳ _Reviewing this PR… (this comment will update shortly)_"
        .to_string()
}

/// Clone the repo (off the async runtime) and run the agentic reviewer.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_agentic(
    provider: &Provider,
    client: &reqwest::Client,
    cfg: &Config,
    meta: &PrMeta,
    diff: &str,
    omitted_note: Option<&str>,
    structural_context: Option<&str>,
    repo: &str,
) -> Result<ReviewResult> {
    let url = provider.clone_url(cfg, repo)?;
    let sha = meta.head_sha.clone();
    // git clone is blocking — keep it off the async worker threads.
    let ws = tokio::task::spawn_blocking(move || Workspace::clone(&url, sha.as_deref())).await??;
    agentic_review(
        client,
        cfg,
        meta,
        diff,
        omitted_note,
        structural_context,
        &ws,
    )
    .await
}

/// Load an optional per-repo `.prbot.toml` and merge it over `base`, returning the
/// effective config for this one review.
///
/// Fully fail-open: a missing file, a fetch error, or a parse error all log and
/// return `base.clone()` so a repo config problem can never break the review.
pub(crate) async fn load_repo_config(
    provider: &Provider,
    client: &reqwest::Client,
    base: &Config,
    repo: &str,
    meta: &PrMeta,
) -> Config {
    // Prefer the exact head commit; fall back to the base branch when the provider
    // didn't give us a head SHA (e.g. Bitbucket meta). If neither is available,
    // there's nothing to fetch against — use the base config as-is.
    let git_ref = match (meta.head_sha.as_deref(), meta.base_branch.as_deref()) {
        (Some(sha), _) if !sha.is_empty() => sha,
        (_, Some(branch)) if !branch.is_empty() => branch,
        _ => return base.clone(),
    };

    match provider
        .get_file_contents(client, base, repo, git_ref, ".prbot.toml")
        .await
    {
        Ok(Some(text)) => match repo_config::parse(&text) {
            Ok(rc) => {
                tracing::info!("applied .prbot.toml overrides for {repo}");
                base.with_repo_overrides(&rc)
            }
            Err(e) => {
                tracing::warn!("ignoring invalid .prbot.toml for {repo}: {e:#}");
                base.clone()
            }
        },
        // No file, or any fetch error — proceed with the base config (fail-open).
        Ok(None) => base.clone(),
        Err(e) => {
            tracing::warn!("could not fetch .prbot.toml for {repo}: {e:#}");
            base.clone()
        }
    }
}

/// Build the summary comment for a PR with nothing for the LLM to review, from any
/// dependency advisories and/or deterministic hygiene findings. Pure — no I/O — so
/// the "a swept-in binary must still produce a comment" path is directly testable.
fn render_no_review_summary(advisories: &[crate::deps::DepAdvisory], hygiene: &[Finding]) -> String {
    let mut s = String::from(
        "🤖 **Automated review**\n\nNo reviewable source changes (all files excluded by filters).",
    );
    if !advisories.is_empty() {
        s.push_str("\n\n");
        s.push_str(&crate::deps::render_advisories(advisories));
    }
    if !hygiene.is_empty() {
        s.push_str("\n\n## Findings");
        for f in hygiene {
            s.push_str(&format!(
                "\n- {} **{}** — `{}` — {}",
                severity_emoji(&f.severity),
                f.severity.to_uppercase(),
                f.file,
                f.body.trim()
            ));
        }
    }
    s.push_str("\n\n_Automated advisory review — a human still owns the merge decision._");
    s
}

/// Post a summary when there's nothing for the LLM to review (every file was
/// filtered out) but a dependency advisory and/or a deterministic diff-hygiene
/// finding still deserves a comment. Skipped on dry-run.
async fn post_advisory_only(
    provider: &Provider,
    client: &reqwest::Client,
    cfg: &Config,
    meta: &PrMeta,
    input: &RunReviewInput,
    advisories: Vec<crate::deps::DepAdvisory>,
    hygiene: Vec<Finding>,
) -> Result<RunReviewOutput> {
    let summary = render_no_review_summary(&advisories, &hygiene);
    // A CVE advisory always warrants changes; hygiene findings upgrade from there.
    let baseline = if advisories.is_empty() {
        "APPROVE"
    } else {
        "APPROVE WITH CHANGES"
    };
    let recommendation = effective_recommendation(baseline, &hygiene);

    let post = ReviewPost {
        summary: summary.clone(),
        inline: Vec::new(),
    };
    let mut out = RunReviewOutput {
        provider: provider.name().to_string(),
        repo: input.repo.clone(),
        pr: input.pr,
        model: cfg.openrouter_model.clone(),
        recommendation,
        findings: hygiene.len(),
        findings_detail: hygiene,
        inline_posted: 0,
        posted: false,
        comment_url: None,
        summary_markdown: summary,
        usage: None,
    };
    if !input.dry_run {
        out.comment_url = provider.post_review(client, cfg, meta, &post).await?;
        out.posted = true;
    }
    Ok(out)
}

/// Review one pull request end-to-end, using the default [`OpenRouterBackend`]
/// (a Claude model via OpenRouter).
///
/// # Errors
/// On unknown provider, empty diff, or any provider/LLM API failure.
pub async fn run_review(cfg: &Config, input: RunReviewInput) -> Result<RunReviewOutput> {
    run_review_with(cfg, input, &OpenRouterBackend).await
}

/// Review one pull request end-to-end with a caller-supplied [`ReviewBackend`].
///
/// Identical to [`run_review`] except the model step is delegated to `backend`,
/// letting a consumer plug in a different reviewer (e.g. an AI agent CLI) while
/// reusing all of the diff preparation, finding post-processing, anchoring, and
/// posting logic. [`run_review`] is just this with [`OpenRouterBackend`].
///
/// # Errors
/// On unknown provider, empty diff, or any provider/backend failure.
pub async fn run_review_with(
    cfg: &Config,
    input: RunReviewInput,
    backend: &dyn ReviewBackend,
) -> Result<RunReviewOutput> {
    let provider = Provider::from_name(&input.provider)?;
    let client = reqwest::Client::new();

    let meta = provider
        .get_meta(&client, cfg, &input.repo, input.pr)
        .await?;

    // Merge an optional per-repo `.prbot.toml` (fetched from the PR head) over the
    // env config; shadow `cfg` so every step below — glob filter, model choice,
    // agentic decision, self-critique, caps, and prompt — honors the overrides.
    let effective = load_repo_config(&provider, &client, cfg, &input.repo, &meta).await;
    let cfg = &effective;

    // Instant feedback: drop a "Reviewing…" summary comment before the slow LLM
    // call. It's upserted, so the real review updates this same comment.
    if input.placeholder && !input.dry_run {
        let pending = ReviewPost {
            summary: render_pending(),
            inline: Vec::new(),
        };
        if let Err(e) = provider.post_review(&client, cfg, &meta, &pending).await {
            tracing::warn!(
                "placeholder comment failed for {}#{}: {e:#}",
                input.repo,
                input.pr
            );
        }
    }

    let raw_diff = provider
        .get_diff(&client, cfg, &input.repo, input.pr)
        .await?;

    // Dependency vulnerability scan runs on the RAW diff: lockfiles are dropped
    // by the glob filter below (and never reach the LLM), so we must read added
    // dependency lines before that. Fully fail-open — returns [] on any error.
    let advisories = crate::deps::scan(&client, cfg, &raw_diff).await;
    if !advisories.is_empty() {
        tracing::info!(
            "OSV: {} dependency advisor(y/ies) for {}#{}",
            advisories.len(),
            input.repo,
            input.pr
        );
    }

    // Drop noisy files (lockfiles, generated, vendored, minified) before the LLM
    // sees the diff — saves tokens and noise. Fail-open: never loses the review.
    let (diff, dropped) =
        crate::diff::filter_diff_by_globs(&raw_diff, &cfg.include_globs, &cfg.exclude_globs);
    if !dropped.is_empty() {
        tracing::info!("skipped {} file(s) by glob: {:?}", dropped.len(), dropped);
    }

    // Deterministic diff-hygiene findings (class D). Computed from the RAW diff so a
    // glob-excluded file (a vendored tree, a swept-in binary) is still seen — which
    // is the whole point, so it must run *before* the empty-diff short-circuit below.
    let hygiene: Vec<Finding> = crate::diff::diff_hygiene_with(&raw_diff, &cfg.vendored_globs)
        .into_iter()
        .map(|h| Finding {
            severity: h.severity.to_string(),
            file: h.file,
            line: None,
            body: h.body,
            confidence: Some(100),
        })
        .collect();

    // If every changed file was filtered out (e.g. a lockfile-only PR) there's
    // nothing for the LLM to review — but a dependency advisory or a hygiene finding
    // on those files still deserves a comment. Post the no-review summary and return.
    if diff.trim().is_empty() {
        if !advisories.is_empty() || !hygiene.is_empty() {
            return post_advisory_only(&provider, &client, cfg, &meta, &input, advisories, hygiene)
                .await;
        }
        anyhow::bail!(
            "PR diff is empty (all files excluded by globs, or no changes) — nothing to review."
        );
    }

    // Smart size handling: keep whole files, dropping the lowest-priority ones
    // first, until the diff fits `max_diff_chars` — instead of a blunt mid-file
    // char cut. Applied ONCE here so both review paths get the same packed diff.
    let (diff, packed_dropped) = if cfg.file_bundling {
        crate::diff::pack_diff_bundled(&diff, cfg.max_diff_chars)
    } else {
        crate::diff::pack_diff(&diff, cfg.max_diff_chars)
    };
    if !packed_dropped.is_empty() {
        tracing::info!(
            "packed diff: omitted {} lower-priority file(s) to fit budget: {:?}",
            packed_dropped.len(),
            packed_dropped
        );
    }
    // Surfaced to the model so it knows these files were NOT reviewed.
    let omitted_note = (!packed_dropped.is_empty()).then(|| {
        format!(
            "{} file(s) were omitted to fit the size limit and were NOT reviewed: {}",
            packed_dropped.len(),
            packed_dropped.join(", ")
        )
    });

    // Structural context: name the enclosing function/symbol of each changed line
    // so the model knows every change's scope. Tier B (tree-sitter over fetched
    // files) with a Tier A (hunk-header) fallback — fully fail-open, so a hiccup
    // just yields an empty string and the review proceeds without it.
    let structural = if cfg.structural_context {
        crate::structure::structural_context(&provider, &client, cfg, &input.repo, &meta, &diff)
            .await
    } else {
        String::new()
    };
    if !structural.is_empty() {
        tracing::info!(
            "structural context for {}#{}: {} line(s)",
            input.repo,
            input.pr,
            structural.lines().count()
        );
    }
    let structural_opt = (!structural.is_empty()).then_some(structural.as_str());

    // Delegate the model step to the backend. The default OpenRouterBackend runs
    // the agentic path (clone + tools) when enabled and falls back to diff-only
    // on failure; a custom backend (e.g. an agent CLI) decides its own strategy.
    let ctx = ReviewContext {
        client: &client,
        cfg,
        provider: &provider,
        repo: &input.repo,
        meta: &meta,
        diff: &diff,
        omitted_note: omitted_note.as_deref(),
        structural_context: structural_opt,
    };
    let result = backend.review(&ctx).await?;
    // Post-process findings before anchoring: optional self-critique pass, then a
    // confidence floor, severity sort, and a hard cap — cuts noise before posting.
    let mut findings = result.review.findings.clone();
    if cfg.self_critique && !findings.is_empty() {
        findings = match crate::llm::critique_findings(&client, cfg, &meta, &diff, &findings).await
        {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!("self-critique failed ({e:#}); keeping original findings");
                findings
            }
        };
    }
    findings.retain(|f| f.confidence.unwrap_or(100) >= cfg.min_confidence);
    // Merge the diff-hygiene findings (computed earlier, before the empty-diff
    // short-circuit). Added after self-critique/confidence-floor — they're facts,
    // not guesses — but before the severity sort + cap, so they compete like any.
    findings.extend(hygiene);
    // Collapse a burst of one claim repeated across many files into a single finding
    // that states the count — before the sort, so the survivor competes on merit.
    findings = collapse_bursts(findings);
    findings.sort_by(|a, b| {
        severity_rank(&b.severity)
            .cmp(&severity_rank(&a.severity))
            .then(b.confidence.unwrap_or(0).cmp(&a.confidence.unwrap_or(0)))
    });
    // Recommendation reflects the *merged* findings (incl. deterministic hygiene the
    // model never saw), upgraded from — never softening — the model's own verdict.
    // Computed BEFORE the cap so a hygiene finding truncated out of the posted list
    // still can't leave the recommendation understating a real problem.
    let recommendation = effective_recommendation(&result.review.recommendation, &findings);
    findings.truncate(cfg.max_findings);

    let valid = parse_valid_lines(&diff);
    // Line texts are only needed to confirm a re-anchor (content match).
    let line_texts = if cfg.reanchor_findings {
        crate::diff::diff_line_texts(&diff)
    } else {
        std::collections::HashMap::new()
    };

    // Anchor findings whose (file, line) is actually in the diff. A finding that
    // just missed (model off-by-a-few / drift) is re-anchored to a nearby diff line
    // when its code matches; the rest fold into the summary so the provider never
    // rejects an out-of-diff anchor.
    let mut inline: Vec<InlineComment> = Vec::new();
    let mut unanchored: Vec<&Finding> = Vec::new();
    for f in &findings {
        let mut anchor = f
            .line
            .filter(|l| valid.get(&f.file).is_some_and(|s| s.contains(l)));
        if anchor.is_none() && cfg.reanchor_findings {
            if let (Some(l), Some(v), Some(t)) =
                (f.line, valid.get(&f.file), line_texts.get(&f.file))
            {
                anchor = reanchor(l, v, t, &f.body);
            }
        }
        match anchor {
            Some(line) => inline.push(InlineComment {
                path: f.file.clone(),
                line,
                body: inline_body(f),
            }),
            None => unanchored.push(f),
        }
    }

    // `recommendation` was computed from the pre-truncation findings above.
    let mut summary = render_summary(&result.review, &recommendation, &unanchored, inline.len());
    if !advisories.is_empty() {
        summary.push_str("\n\n");
        summary.push_str(&crate::deps::render_advisories(&advisories));
    }
    let inline_count = inline.len();
    let post = ReviewPost {
        summary: summary.clone(),
        inline,
    };

    let mut out = RunReviewOutput {
        provider: provider.name().to_string(),
        repo: input.repo.clone(),
        pr: input.pr,
        model: result.model,
        recommendation: recommendation.clone(),
        findings: findings.len(),
        findings_detail: findings.clone(),
        inline_posted: inline_count,
        posted: false,
        comment_url: None,
        summary_markdown: summary,
        usage: result.usage,
    };

    if !input.dry_run {
        out.comment_url = provider.post_review(&client, cfg, &meta, &post).await?;
        out.posted = true;
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::{
        burst_key, collapse_bursts, effective_recommendation, idents, line_symbols, reanchor,
        render_no_review_summary,
    };
    use crate::llm::Finding;
    use std::collections::{HashMap, HashSet};

    fn f(severity: &str, file: &str, body: &str) -> Finding {
        Finding {
            severity: severity.to_string(),
            file: file.to_string(),
            line: None,
            body: body.to_string(),
            confidence: Some(100),
        }
    }

    /// The VinaText#20 round-2 shape: one claim, seven files.
    #[test]
    fn a_repeated_claim_collapses_into_one_finding() {
        let findings = vec![
            f("LOW", "a.cxx", "`a.cxx` adds 2192 lines in one new file."),
            f("LOW", "b.cxx", "`b.cxx` adds 1868 lines in one new file."),
            f("LOW", "c.cxx", "`c.cxx` adds 1268 lines in one new file."),
            f("HIGH", "d.rs", "Unvalidated input reaches the query."),
        ];
        let out = collapse_bursts(findings);
        assert_eq!(out.len(), 2, "three same-shape findings become one");
        assert!(out[0].body.contains("2 other file(s)"));
        assert!(out[0].body.contains("`b.cxx`"));
        // The unrelated finding is untouched.
        assert_eq!(out[1].file, "d.rs");
        assert!(!out[1].body.contains("other file(s)"));
    }

    #[test]
    fn two_of_a_kind_are_left_alone() {
        let findings = vec![
            f("LOW", "a.cxx", "`a.cxx` adds 2192 lines in one new file."),
            f("LOW", "b.cxx", "`b.cxx` adds 1868 lines in one new file."),
        ];
        assert_eq!(collapse_bursts(findings).len(), 2);
    }

    /// Raised on PR #27: collapsing is lossy — only the representative keeps its
    /// inline anchor and its `Fix:` text — so three distinct serious bugs that share
    /// an opening phrase must NOT be merged. Both recorded bursts were MEDIUM/LOW.
    #[test]
    fn serious_findings_are_never_collapsed() {
        let same_claim = "SQL built by string concatenation reaches the driver.";
        let findings = vec![
            f("HIGH", "a.rs", same_claim),
            f("HIGH", "b.rs", same_claim),
            f("HIGH", "c.rs", same_claim),
        ];
        let out = collapse_bursts(findings);
        assert_eq!(out.len(), 3, "each HIGH keeps its own anchor and fix");
        assert!(out.iter().all(|f| !f.body.contains("other file(s)")));

        // One HIGH in the group protects the whole group.
        let mixed = vec![
            f("LOW", "a.rs", same_claim),
            f("LOW", "b.rs", same_claim),
            f("BLOCKING", "c.rs", same_claim),
        ];
        assert_eq!(collapse_bursts(mixed).len(), 3);
    }

    /// Collapsing must never soften the verdict: the survivor carries the group's
    /// highest severity, so `effective_recommendation` still sees it.
    #[test]
    fn the_collapsed_finding_keeps_the_highest_severity() {
        let findings = vec![
            f("LOW", "a.zip", "A binary file `a.zip` was added."),
            f("MEDIUM", "b.zip", "A binary file `b.zip` was added."),
            f("LOW", "c.zip", "A binary file `c.zip` was added."),
        ];
        let out = collapse_bursts(findings);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].severity, "MEDIUM");
        assert_eq!(out[0].file, "b.zip");
    }

    #[test]
    fn burst_key_ignores_paths_and_numbers_but_not_the_claim() {
        let a = f("LOW", "a.rs", "`a.rs` adds 2192 lines in one new file.");
        let b = f("LOW", "b.rs", "`b.rs` adds 17 lines in one new file.");
        let c = f("LOW", "c.rs", "`c.rs` leaks a handle on the error path.");
        assert_eq!(burst_key(&a), burst_key(&b));
        assert_ne!(burst_key(&a), burst_key(&c));
    }

    fn finding(severity: &str) -> Finding {
        Finding {
            severity: severity.to_string(),
            file: "assets/ime.zip".to_string(),
            line: None,
            body: "A binary file `assets/ime.zip` was added. Fix: drop it.".to_string(),
            confidence: Some(100),
        }
    }

    #[test]
    fn recommendation_upgrades_but_never_downgrades() {
        // A MEDIUM hygiene finding lifts an APPROVE to "approve with changes".
        assert_eq!(
            effective_recommendation("APPROVE", &[finding("MEDIUM")]),
            "APPROVE WITH CHANGES"
        );
        // A BLOCKING finding forces a block.
        assert_eq!(effective_recommendation("APPROVE", &[finding("BLOCKING")]), "BLOCK");
        // A LOW-only finding does NOT force changes.
        assert_eq!(effective_recommendation("APPROVE", &[finding("LOW")]), "APPROVE");
        // The model's stronger verdict is never softened by weaker findings.
        assert_eq!(effective_recommendation("BLOCK", &[finding("LOW")]), "BLOCK");
        // No findings → the model's verdict is kept verbatim.
        assert_eq!(effective_recommendation("APPROVE", &[]), "APPROVE");
    }

    #[test]
    fn no_review_summary_still_names_a_swept_in_binary() {
        // The regression the wiring guards against: an all-excluded PR whose only
        // change is a binary must still produce a comment that names the file.
        let s = render_no_review_summary(&[], &[finding("MEDIUM")]);
        assert!(s.contains("assets/ime.zip"));
        assert!(s.contains("MEDIUM"));
        assert!(s.contains("## Findings"));
    }

    #[test]
    fn idents_extracts_tokens() {
        assert_eq!(idents("foo.bar(baz_qux)"), vec!["foo", "bar", "baz_qux"]);
    }

    #[test]
    fn line_symbols_drops_keywords_and_short_tokens() {
        let s = line_symbols("export function calcTotal(o) {");
        assert!(s.contains(&"calcTotal".to_string()));
        assert!(!s.iter().any(|w| w == "export" || w == "function" || w == "o"));
    }

    #[test]
    fn reanchor_snaps_to_the_matching_diff_line() {
        let valid: HashSet<u64> = [8, 10, 12].into_iter().collect();
        let mut texts = HashMap::new();
        texts.insert(8, "  const subtotal = sum(items);".to_string());
        texts.insert(10, "  return calcTotal(order, tax);".to_string());
        texts.insert(12, "}".to_string());
        // Finding drifted to line 9; its body names calcTotal, which is on line 10.
        let got = reanchor(9, &valid, &texts, "`calcTotal` now needs a tax arg. Fix: pass it.");
        assert_eq!(got, Some(10));
    }

    #[test]
    fn reanchor_declines_without_a_content_match() {
        let valid: HashSet<u64> = [8, 10].into_iter().collect();
        let mut texts = HashMap::new();
        texts.insert(8, "  const x = 1;".to_string());
        texts.insert(10, "  const y = 2;".to_string());
        assert_eq!(reanchor(9, &valid, &texts, "Missing null check on user.roles"), None);
    }

    #[test]
    fn reanchor_declines_on_a_short_shared_token() {
        // The only token shared with a nearby line is 3 chars ("sum") — below the
        // distinctiveness floor, so no snap (avoids anchoring on generic collisions).
        let valid: HashSet<u64> = [10].into_iter().collect();
        let mut texts = HashMap::new();
        texts.insert(10, "  const total = sum(items);".to_string());
        assert_eq!(reanchor(9, &valid, &texts, "sum is off by one"), None);
    }

    #[test]
    fn reanchor_ties_break_on_lower_line_number() {
        // Lines 8 and 10 are both distance 2 from 9 and both content-match. The pick
        // must be deterministic (lower line), independent of HashSet order.
        let valid: HashSet<u64> = [8, 10].into_iter().collect();
        let mut texts = HashMap::new();
        texts.insert(8, "  calcTotal(order);".to_string());
        texts.insert(10, "  calcTotal(basket);".to_string());
        assert_eq!(reanchor(9, &valid, &texts, "calcTotal needs a tax arg"), Some(8));
    }

    #[test]
    fn reanchor_ignores_lines_outside_the_window() {
        let valid: HashSet<u64> = [20].into_iter().collect();
        let mut texts = HashMap::new();
        texts.insert(20, "  calcTotal();".to_string());
        // 20 is 11 rows from 9 — outside REANCHOR_WINDOW, so no snap even though the
        // symbol matches.
        assert_eq!(reanchor(9, &valid, &texts, "calcTotal issue"), None);
    }
}
