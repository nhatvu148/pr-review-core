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
#[derive(Debug, Serialize)]
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
    /// The inline comments exactly as they were built — path, line, and the
    /// rendered body, committable suggestion block and all.
    ///
    /// Exposed because `inline_posted` is a count, and a count cannot be reviewed.
    /// Everything a reader needs to judge an inline comment *before* it reaches
    /// someone's PR — the suggestion block above all, which is applied on one
    /// click — existed only inside `finish_review` and was dropped on the way out.
    /// That left `--dry-run` unable to show what it was a dry run of, and it is
    /// why suggestions shipped with no way to preview them.
    ///
    /// Populated on every run, posted or not.
    #[serde(default)]
    pub inline_detail: Vec<InlineComment>,
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
        3 => "BLOCK",                    // a BLOCKING finding
        2 | 1 => "APPROVE WITH CHANGES", // HIGH or MEDIUM
        _ => "APPROVE",                  // LOW-only or none — don't force changes
    };
    if recommendation_rank(model_rec) >= recommendation_rank(floor) {
        model_rec.trim().to_string()
    } else {
        floor.to_string()
    }
}

/// Phrases that assert a **check outcome** on their own — nothing but a build, a
/// compile, or CI can be their subject.
const BUILD_CLAIM_MARKERS: &[&str] = &[
    "breaks the build",
    "break the build",
    "fails the build",
    "fail the build",
    "fails to compile",
    "won't compile",
    "will not compile",
    "ci would go red",
    "ci will go red",
    "fails ci",
    "fail ci",
];

/// Failure phrases that are **ambiguous alone** and only count as a check claim when
/// the body also names a check.
///
/// "will fail" is the phrase the reviewer actually used on pr-review-core#28 — and
/// also the phrase in "this will fail at runtime when the list is empty", a real
/// HIGH bug that green CI says nothing about, because CI does not run for
/// correctness. Treating it as a check claim would silently cap a genuine defect to
/// LOW and soften the recommendation floor with it.
const WEAK_FAILURE_PHRASES: &[&str] = &["will fail", "would fail", "fails when", "would break"];

/// Whole words that identify the subject as a check. Matched as **words**, not
/// substrings: "ci" appears inside "specific", "decision", "efficient".
const CHECK_CONTEXT_WORDS: &[&str] = &[
    "ci",
    "build",
    "builds",
    "compile",
    "compiles",
    "compilation",
    "clippy",
    "rustfmt",
    "fmt",
    "lint",
    "linter",
    "eslint",
    "pipeline",
    "workflow",
];

/// Multi-word check markers, matched as substrings.
const CHECK_CONTEXT_PHRASES: &[&str] = &[
    "--check",
    "cargo fmt",
    "cargo test",
    "cargo clippy",
    "test suite",
    "check job",
    "the check",
    "check will",
    "check would",
];

/// Does this finding assert an outcome a CI run settles?
///
/// Two tiers, because the obvious one-tier version is wrong. A flat list containing
/// "will fail" matches ordinary runtime findings, so a real HIGH — "this will fail
/// at runtime when the list is empty" — would be capped to LOW by a green CI that
/// never exercised that path.
fn asserts_check_outcome(body_lower: &str) -> bool {
    if BUILD_CLAIM_MARKERS.iter().any(|m| body_lower.contains(m)) {
        return true;
    }
    if !WEAK_FAILURE_PHRASES.iter().any(|m| body_lower.contains(m)) {
        return false;
    }
    if CHECK_CONTEXT_PHRASES.iter().any(|p| body_lower.contains(p)) {
        return true;
    }
    body_lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|w| CHECK_CONTEXT_WORDS.contains(&w))
}

/// Does this CI block report every reported check as passing?
///
/// Conservative in both directions: an empty block, a block with any non-passing
/// state, or one carrying the truncation notice means "not known green", so nothing
/// is demoted. Only an unambiguously all-green report licenses the demotion.
fn ci_is_all_green(ci_status: Option<&str>) -> bool {
    let Some(ci) = ci_status.map(str::trim).filter(|s| !s.is_empty()) else {
        return false;
    };
    // The Files-API-style truncation notice means the list is incomplete, so a
    // hidden failure is possible — exactly the case that must NOT be demoted.
    if ci.contains("NOT shown") {
        return false;
    }
    let states: Vec<&str> = ci
        .lines()
        .filter_map(|l| l.rsplit_once(':').map(|(_, s)| s.trim()))
        .collect();
    !states.is_empty()
        && states
            .iter()
            .all(|s| s.eq_ignore_ascii_case("success") || s.eq_ignore_ascii_case("successful"))
}

/// Cap at LOW any finding that asserts a check outcome the reviewed commit's CI has
/// already decided green, and say so in the body.
///
/// The prompt has asked for this twice and not got it. On `VinaText#10` the reviewer
/// filed two BLOCKING findings claiming a broken MFC build on a green commit; after
/// the CI block and an explicit "a passing check FALSIFIES this claim" rule shipped,
/// `pr-review-core#28` filed the same shape again — BLOCKING first, then MEDIUM once
/// the rules reached that backend, but never dropped and never restated at LOW. The
/// model downgraded its confidence without re-examining the claim.
///
/// So this is enforced in code instead. **Demote, not delete:** the *observation*
/// under such a finding is often true (a line really is 118 characters); it is the
/// inference to "the check will fail" that CI refutes. LOW keeps the observation
/// visible while removing what it costs — via the recommendation floor, a MEDIUM or
/// above turns the posted verdict into "approve with changes" or "block".
fn demote_falsified_build_claims(findings: &mut [Finding], ci_status: Option<&str>) {
    if !ci_is_all_green(ci_status) {
        return;
    }
    for f in findings.iter_mut() {
        if severity_rank(&f.severity) == 0 {
            continue; // already LOW/unknown — nothing to cap
        }
        if !asserts_check_outcome(&f.body.to_lowercase()) {
            continue;
        }
        tracing::warn!(
            "demoting a {} finding on {} to LOW: it asserts a check outcome CI reports green",
            f.severity,
            f.file
        );
        f.severity = "LOW".to_string();
        f.body = format!(
            "{}\n\n(Every CI check on the reviewed commit passed, which contradicts the \
             claim that this fails a check — capped at LOW. The underlying observation may \
             still be worth acting on; the predicted failure is not.)",
            f.body.trim_end()
        );
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
        // Severity is part of the key, so a group is uniform by construction: two
        // findings that share a phrase but not a severity are, by the model's own
        // judgement, not the same claim — and merging them would hide that.
        let key = format!("{}|{}", f.severity.to_uppercase(), burst_key(&f));
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

/// The inline comment for `f`, and whether a committable suggestion block was
/// appended to it — which happens when the model proposed one and it survives
/// validation against `current`, the exact new-side text of the anchored line.
///
/// `exact_anchor` is false for a re-anchored finding, and that alone withholds
/// the block. The replacement text was written for the line the model named; the
/// comment is going somewhere else. Prose survives being moved a few rows — it
/// describes a problem — but replacement text moved onto a different line is a
/// one-click commit that deletes code the finding never looked at.
fn inline_body_for(
    cfg: &Config,
    provider: &str,
    f: &Finding,
    current: Option<&str>,
    exact_anchor: bool,
) -> (String, bool) {
    let body = inline_body(f);
    if !cfg.suggestions || !exact_anchor || !crate::suggest::supports_suggestions(provider) {
        return (body, false);
    }
    let Some(raw) = f.suggestion.as_deref() else {
        return (body, false);
    };
    match current.and_then(|c| crate::suggest::sanitize(raw, c)) {
        Some(s) => (crate::suggest::render(provider, &body, &s), true),
        None => (body, false),
    }
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
        "const",
        "let",
        "var",
        "function",
        "return",
        "import",
        "export",
        "from",
        "class",
        "interface",
        "type",
        "public",
        "private",
        "protected",
        "static",
        "async",
        "await",
        "for",
        "while",
        "new",
        "void",
        "null",
        "true",
        "false",
        "this",
        "self",
        "def",
        "func",
        "pub",
        "use",
        "mod",
        "struct",
        "enum",
        "impl",
        "package",
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

/// How much of the change map this configuration will actually render.
///
/// Cost follows the ask, one level finer than it used to: the walkthrough reads
/// symbols and grades, the diagram additionally reads edges, and edge linking is
/// the pairwise scan. A walkthrough-only review used to pay for edges nothing
/// would draw.
fn map_detail(cfg: &Config) -> crate::changemap::MapDetail {
    use crate::changemap::MapDetail;
    match (cfg.walkthrough, cfg.diagram) {
        (_, true) => MapDetail::Full,
        (true, false) => MapDetail::Symbols,
        (false, false) => MapDetail::None,
    }
}

/// Append the change map's renderings to a summary comment.
///
/// Both are opt-in and both are *derived* — nothing here asks a model anything, so
/// a reader can check every row and every arrow against the code. Each render
/// returns an empty string when it has nothing worth showing (no files; no edges),
/// and an empty render appends nothing rather than an empty `<details>`.
fn append_change_map(
    cfg: &Config,
    provider: &str,
    map: &crate::changemap::ChangeMap,
    findings: &[Finding],
    summary: &mut String,
) {
    if cfg.walkthrough {
        let w = crate::changemap::render_walkthrough(map, findings, cfg.walkthrough_max_symbols);
        if !w.is_empty() {
            summary.push_str("\n\n");
            summary.push_str(&w);
        }
    }
    // Mermaid that a provider won't render is a wall of source in the comment —
    // strictly worse than no diagram.
    if cfg.diagram && crate::changemap::supports_mermaid(provider) {
        let d = crate::changemap::render_diagram(map);
        if !d.is_empty() {
            summary.push_str("\n\n");
            summary.push_str(&d);
        }
    }
}

/// Hidden marker on a summary comment that is **not** a finished review.
///
/// Machine-readable on purpose. A consumer's boot reconciliation asks "has this PR
/// been reviewed at its current head?", and answers it from the newest bot comment
/// — so a placeholder counted as a review makes a *died-mid-flight* review look
/// finished, which is precisely the case reconciliation exists to catch. Matching
/// on the prose instead would break the moment anyone rewords it.
pub const REVIEW_PENDING_MARKER: &str = "<!-- prbot-status:pending -->";

/// Hidden marker on a summary comment recording a review that **failed**.
pub const REVIEW_FAILED_MARKER: &str = "<!-- prbot-status:failed -->";

/// Prose from placeholders posted before the markers existed. Kept so comments
/// already sitting on open PRs are still recognised; new ones carry the marker.
const LEGACY_PENDING_PROSE: &str = "⏳ _Reviewing this PR…";

/// Whether a bot summary comment represents something **other** than a completed
/// review — a placeholder for one still running, or a notice that one failed.
///
/// Consumers deciding "does this PR still need a review?" must treat both as *no
/// review*. A placeholder means one was promised; a failure notice means one was
/// attempted. Neither is a review.
pub fn is_incomplete_review(body: &str) -> bool {
    body.contains(REVIEW_PENDING_MARKER)
        || body.contains(REVIEW_FAILED_MARKER)
        || body.contains(LEGACY_PENDING_PROSE)
}

/// Placeholder summary body shown immediately while the review runs.
fn render_pending() -> String {
    format!(
        "🤖 **Automated review**\n\n⏳ _Reviewing this PR… (this comment will update \
         shortly)_\n\n{REVIEW_PENDING_MARKER}"
    )
}

/// Summary body replacing a placeholder whose review died.
///
/// The placeholder is upserted, so this **replaces** it rather than adding a
/// comment: without it the PR keeps promising a review that will never arrive.
fn render_failed(err: &str) -> String {
    format!(
        "🤖 **Automated review**\n\n⚠️ _This review failed and produced no findings. \
         The error was:_\n\n```\n{}\n```\n\n_Re-run with `/review`._\n\n{REVIEW_FAILED_MARKER}",
        crate::clip(err, 500)
    )
}

/// Replace a "Reviewing…" placeholder with an honest failure notice.
///
/// **Only call this for a review that actually posted a placeholder.** The upsert
/// underneath *creates* the summary comment when none exists
/// (`github::upsert_summary`), so calling it on a PR the engine never commented on
/// posts a failure notice where there was silence — noise, on a PR nobody asked to
/// have reviewed. `RunReviewInput::placeholder` is the caller's record of that.
///
/// Best-effort otherwise: a failure to report a failure is logged and swallowed,
/// because the original error is the one the caller needs to surface.
pub async fn post_review_failure(
    provider_name: &str,
    cfg: &Config,
    repo: &str,
    pr: u64,
    err: &str,
) {
    let provider = match Provider::from_name(provider_name) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("could not retract placeholder for {repo}#{pr}: {e:#}");
            return;
        }
    };
    // Summary-only, so no `head_sha` is needed: that field only anchors inline
    // comments, and a failure notice has none.
    let meta = PrMeta {
        repo: repo.to_string(),
        pr,
        title: None,
        base_branch: None,
        head_sha: None,
        body: None,
        ci_status: None,
    };
    let post = ReviewPost {
        summary: render_failed(err),
        inline: Vec::new(),
    };
    let client = reqwest::Client::new();
    if let Err(e) = provider.post_review(&client, cfg, &meta, &post).await {
        tracing::warn!("could not retract placeholder for {repo}#{pr}: {e:#}");
    }
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
    pr_body_block: Option<&str>,
    repo: &str,
    system_prompt: &str,
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
        pr_body_block,
        &ws,
        system_prompt,
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
fn render_no_review_summary(
    advisories: &[crate::deps::DepAdvisory],
    hygiene: &[Finding],
) -> String {
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
        inline_detail: Vec::new(),
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

/// Everything one [`crate::runlog::RunLog`] record needs that isn't already on the
/// [`RunReviewOutput`]. Grouped into a struct because the alternative is a
/// ten-argument function whose call sites are positional booleans.
struct RunLogParts<'a> {
    out: &'a RunReviewOutput,
    meta: &'a PrMeta,
    dry_run: bool,
    diff_bytes: usize,
    diff_truncated: bool,
    advisories: usize,
    truncated_salvage: bool,
    funnel: crate::runlog::Funnel,
    /// The line each posted finding anchored to, index-aligned with
    /// `out.findings_detail`; empty means none anchored.
    anchors: &'a [Option<u64>],
    started: std::time::Instant,
}

/// Append this run to the local JSONL run log, if one is configured.
///
/// A no-op — and costs nothing to build the record — when `run_log` is unset,
/// which is the default. Never fails: see [`crate::runlog::append`].
fn log_run(cfg: &Config, p: RunLogParts<'_>) {
    let Some(sink) = cfg.run_log.as_ref() else {
        return;
    };
    let rec = crate::runlog::RunLog {
        kind: crate::runlog::KIND,
        schema: crate::runlog::SCHEMA,
        ts_unix: crate::runlog::RunLog::now_unix(),
        core_version: crate::VERSION.to_string(),
        provider: p.out.provider.clone(),
        repo: p.out.repo.clone(),
        pr: p.out.pr,
        head_sha: p.meta.head_sha.clone(),
        base_branch: p.meta.base_branch.clone(),
        model: p.out.model.clone(),
        dry_run: p.dry_run,
        posted: p.out.posted,
        ci_status_known: p.meta.ci_status.is_some(),
        diff_bytes: p.diff_bytes,
        diff_truncated: p.diff_truncated,
        advisories: p.advisories,
        truncated_salvage: p.truncated_salvage,
        recommendation: p.out.recommendation.clone(),
        funnel: p.funnel,
        findings: crate::runlog::logged_findings(&p.out.findings_detail, p.anchors),
        usage: p.out.usage.clone(),
        duration_ms: p.started.elapsed().as_millis() as u64,
    };
    crate::runlog::write(sink, &rec);
}

// ---------------------------------------------------------------------------
// Shared pipeline stages
//
// Both entry points — the PR orchestrator and the local diff-first one — run the
// SAME stages either side of the model call. They live here as functions rather
// than being copied because the two paths drifting apart is the failure this
// crate can least afford: the local path is the one the convergence loop scores
// itself on, and a filter or a demotion rule that applies to only one of them
// makes those numbers incomparable with the bot's.
// ---------------------------------------------------------------------------

/// Everything derived from the raw diff before the model sees anything.
struct PreparedDiff {
    /// Glob-filtered and size-packed — what the backend reviews. Empty when every
    /// changed file was filtered out.
    diff: String,
    /// Deterministic diff-hygiene findings (class D), computed from the RAW diff so
    /// a glob-excluded file (a vendored tree, a swept-in binary) is still seen.
    hygiene: Vec<Finding>,
    /// Note naming whole files packed out to fit the budget (NOT reviewed).
    omitted_note: Option<String>,
}

/// Glob-filter, hygiene-scan, and size-pack a raw diff.
fn prepare_diff(cfg: &Config, raw_diff: &str) -> PreparedDiff {
    // Drop noisy files (lockfiles, generated, vendored, minified) before the LLM
    // sees the diff — saves tokens and noise. Fail-open: never loses the review.
    let (diff, dropped) =
        crate::diff::filter_diff_by_globs(raw_diff, &cfg.include_globs, &cfg.exclude_globs);
    if !dropped.is_empty() {
        tracing::info!("skipped {} file(s) by glob: {:?}", dropped.len(), dropped);
    }

    // Computed from the RAW diff, and BEFORE the caller's empty-diff short-circuit,
    // so an all-excluded change still reports its swept-in binary.
    let hygiene: Vec<Finding> = crate::diff::diff_hygiene_with(raw_diff, &cfg.vendored_globs)
        .into_iter()
        .map(|h| Finding {
            severity: h.severity.to_string(),
            file: h.file,
            line: None,
            body: h.body,
            confidence: Some(100),
            suggestion: None,
        })
        .collect();

    if diff.trim().is_empty() {
        return PreparedDiff {
            diff,
            hygiene,
            omitted_note: None,
        };
    }

    // Smart size handling: keep whole files, dropping the lowest-priority ones
    // first, until the diff fits `max_diff_chars` — instead of a blunt mid-file
    // char cut. Applied ONCE here so every review path gets the same packed diff.
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

    PreparedDiff {
        diff,
        hygiene,
        omitted_note,
    }
}

/// A review after every post-processing stage, ready to post or to render.
struct FinishedReview {
    findings: Vec<Finding>,
    recommendation: String,
    summary: String,
    inline: Vec<InlineComment>,
    /// Per-stage survivor counts through the post-processing above. Collected
    /// here because this is the only place they exist: every stage consumes its
    /// predecessor's vector, so a caller holding only the final findings cannot
    /// reconstruct what the critique, the confidence floor, or the cap removed.
    funnel: crate::runlog::Funnel,
    /// The line each of `findings` was anchored to, index-aligned, or `None` for
    /// the ones that folded into the summary.
    ///
    /// Also only knowable here. The re-anchor step posts a comment on a line the
    /// finding itself never records, so `(file, line)` on a finding is NOT enough
    /// to find its comment afterwards — re-deriving it that way reports every
    /// re-anchored finding as unanchored.
    anchors: Vec<Option<u64>>,
}

/// Everything between the backend's answer and a postable review: self-critique,
/// confidence floor, hygiene merge, CI demotion, burst collapse, severity sort,
/// recommendation floor, cap, line anchoring, and the summary.
async fn finish_review(
    cfg: &Config,
    backend: &dyn ReviewBackend,
    provider: &str,
    meta: &PrMeta,
    diff: &str,
    result: &ReviewResult,
    hygiene: Vec<Finding>,
) -> FinishedReview {
    // Post-process findings before anchoring: optional self-critique pass, then a
    // confidence floor, severity sort, and a hard cap — cuts noise before posting.
    let mut funnel = crate::runlog::Funnel {
        model_raw: result.review.findings.len(),
        ..Default::default()
    };
    let mut findings = result.review.findings.clone();
    if cfg.self_critique && !findings.is_empty() {
        // Through the backend seam, so the critique runs on whatever produced the
        // review — not always OpenRouter.
        findings = match crate::llm::critique_findings(cfg, backend, meta, diff, &findings).await {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!("self-critique failed ({e:#}); keeping original findings");
                findings
            }
        };
    }
    funnel.after_critique = findings.len();
    findings.retain(|f| f.confidence.unwrap_or(100) >= cfg.min_confidence);
    funnel.after_confidence = findings.len();
    funnel.hygiene_added = hygiene.len();
    // Merge the diff-hygiene findings. Added after self-critique/confidence-floor —
    // they're facts, not guesses — but before the severity sort + cap, so they
    // compete like any other finding.
    findings.extend(hygiene);
    // A claim that CI has already falsified is capped at LOW — see the function.
    // A local review carries no CI block, so this is a no-op there.
    demote_falsified_build_claims(&mut findings, meta.ci_status.as_deref());
    // Collapse a burst of one claim repeated across many files into a single finding
    // that states the count — before the sort, so the survivor competes on merit.
    findings = collapse_bursts(findings);
    funnel.after_collapse = findings.len();
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
    funnel.posted_findings = findings.len();

    let valid = parse_valid_lines(diff);
    // Line texts confirm a re-anchor (content match) and validate a suggestion
    // against the line it would replace. Either feature alone needs them; with
    // both off the parse is skipped.
    let line_texts = if cfg.reanchor_findings || cfg.suggestions {
        crate::diff::diff_line_texts(diff)
    } else {
        std::collections::HashMap::new()
    };

    // Anchor findings whose (file, line) is actually in the diff. A finding that
    // just missed (model off-by-a-few / drift) is re-anchored to a nearby diff line
    // when its code matches; the rest fold into the summary so the provider never
    // rejects an out-of-diff anchor.
    let mut inline: Vec<InlineComment> = Vec::new();
    let mut unanchored: Vec<&Finding> = Vec::new();
    let mut anchors: Vec<Option<u64>> = Vec::with_capacity(findings.len());
    for f in &findings {
        let mut anchor = f
            .line
            .filter(|l| valid.get(&f.file).is_some_and(|s| s.contains(l)));
        // Whether the comment is going to the line the model named. A suggestion
        // is only valid against that line — see `inline_body_for`.
        let exact_anchor = anchor.is_some();
        if anchor.is_none() && cfg.reanchor_findings {
            if let (Some(l), Some(v), Some(t)) =
                (f.line, valid.get(&f.file), line_texts.get(&f.file))
            {
                anchor = reanchor(l, v, t, &f.body);
            }
        }
        anchors.push(anchor);
        match anchor {
            Some(line) => {
                let current = line_texts.get(&f.file).and_then(|t| t.get(&line));
                let (body, suggested) =
                    inline_body_for(cfg, provider, f, current.map(String::as_str), exact_anchor);
                if suggested {
                    funnel.suggested += 1;
                }
                inline.push(InlineComment {
                    path: f.file.clone(),
                    line,
                    body,
                })
            }
            None => unanchored.push(f),
        }
    }

    // `recommendation` was computed from the pre-truncation findings above.
    let summary = render_summary(&result.review, &recommendation, &unanchored, inline.len());

    funnel.anchored = inline.len();
    funnel.unanchored = unanchored.len();

    FinishedReview {
        findings,
        recommendation,
        summary,
        inline,
        funnel,
        anchors,
    }
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
    let started = std::time::Instant::now();
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

    let prepared = prepare_diff(cfg, &raw_diff);

    // If every changed file was filtered out (e.g. a lockfile-only PR) there's
    // nothing for the LLM to review — but a dependency advisory or a hygiene finding
    // on those files still deserves a comment. Post the no-review summary and return.
    if prepared.diff.trim().is_empty() {
        if !advisories.is_empty() || !prepared.hygiene.is_empty() {
            let advisory_count = advisories.len();
            let out = post_advisory_only(
                &provider,
                &client,
                cfg,
                &meta,
                &input,
                advisories,
                prepared.hygiene,
            )
            .await?;
            // No model ran on this path, so most of the funnel is vacuous: every
            // finding is deterministic hygiene, and none of them anchor.
            let n = out.findings_detail.len();
            log_run(
                cfg,
                RunLogParts {
                    out: &out,
                    meta: &meta,
                    dry_run: input.dry_run,
                    diff_bytes: 0,
                    diff_truncated: false,
                    advisories: advisory_count,
                    truncated_salvage: false,
                    funnel: crate::runlog::Funnel {
                        hygiene_added: n,
                        after_collapse: n,
                        posted_findings: n,
                        unanchored: n,
                        ..Default::default()
                    },
                    anchors: &[],
                    started,
                },
            );
            return Ok(out);
        }
        anyhow::bail!(
            "PR diff is empty (all files excluded by globs, or no changes) — nothing to review."
        );
    }
    let PreparedDiff {
        diff,
        hygiene,
        omitted_note,
    } = prepared;
    // Recorded now: `diff` is what the backend is about to see (post-filter,
    // post-pack), and an `omitted_note` means whole files were dropped to fit the
    // budget — i.e. part of this change is going unreviewed.
    let diff_bytes = diff.len();
    let diff_truncated = omitted_note.is_some();

    // Structural context: name the enclosing function/symbol of each changed line
    // so the model knows every change's scope. Tier B (tree-sitter over fetched
    // files) with a Tier A (hunk-header) fallback — fully fail-open, so a hiccup
    // just yields an empty string and the review proceeds without it.
    // Ask for exactly what will be rendered and no more: the map costs a pass the
    // prompt block doesn't need, and edge linking costs a pairwise span scan only
    // the diagram reads.
    let (structural, change_map) = if cfg.structural_context {
        crate::structure::inner(
            &provider,
            &client,
            cfg,
            &input.repo,
            &meta,
            &diff,
            map_detail(cfg),
        )
        .await
    } else {
        (String::new(), crate::changemap::ChangeMap::default())
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
    //
    // The calibration rules are composed HERE and handed over on the context —
    // not left as a const each backend is trusted to append, which is how the
    // deployed claude-code backend ran without them for months.
    let injected_rules = crate::prompt::injected_rules(cfg);
    // Composed here, once, for every backend — see `ReviewContext::pr_body`.
    let pr_body = crate::prompt::pr_body_block(cfg, &meta);
    let ctx = ReviewContext {
        client: &client,
        cfg,
        provider: Some(&provider),
        // A PR's code is only reachable by cloning; the agentic path does that
        // itself when enabled.
        local_root: None,
        repo: &input.repo,
        meta: &meta,
        diff: &diff,
        omitted_note: omitted_note.as_deref(),
        structural_context: structural_opt,
        pr_body: pr_body.as_deref(),
        injected_rules: &injected_rules,
    };
    let result = backend.review(&ctx).await?;
    let truncated = result.review.summary.contains(crate::llm::TRUNCATED_NOTE);
    let mut finished = finish_review(
        cfg,
        backend,
        &input.provider,
        &meta,
        &diff,
        &result,
        hygiene,
    )
    .await;
    if !advisories.is_empty() {
        finished.summary.push_str("\n\n");
        finished
            .summary
            .push_str(&crate::deps::render_advisories(&advisories));
    }
    // Derived renderings of the change, appended last: they describe what the PR
    // touched, which is context for the findings above rather than a finding
    // itself. Both collapse into `<details>`, so a reader who wants only the
    // verdict never has to scroll past them.
    append_change_map(
        cfg,
        provider.name(),
        &change_map,
        &finished.findings,
        &mut finished.summary,
    );

    let FinishedReview {
        findings,
        recommendation,
        summary,
        inline,
        funnel,
        anchors,
    } = finished;
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
        recommendation,
        findings: findings.len(),
        findings_detail: findings,
        inline_posted: inline_count,
        inline_detail: post.inline.clone(),
        posted: false,
        comment_url: None,
        summary_markdown: summary,
        usage: result.usage,
    };

    if !input.dry_run {
        out.comment_url = provider.post_review(&client, cfg, &meta, &post).await?;
        out.posted = true;
    }

    log_run(
        cfg,
        RunLogParts {
            out: &out,
            meta: &meta,
            dry_run: input.dry_run,
            diff_bytes,
            diff_truncated,
            advisories: advisories.len(),
            // The salvage marks a cut-off review in its own summary; that marker is
            // the only signal for it that survives to here.
            truncated_salvage: truncated,
            funnel,
            anchors: &anchors,
            started,
        },
    );

    Ok(out)
}

/// A review of a diff that is **not** a pull request: a branch, a worktree, a set
/// of staged changes. No host, no PR number, nothing to post to.
pub struct LocalReviewInput {
    /// The raw unified diff to review, as `git diff` prints it.
    pub diff: String,
    /// A checkout the new-side files can be read from, for structural context.
    /// Optional: without it the review still runs, on hunk-header context alone.
    pub repo_root: Option<std::path::PathBuf>,
    /// What to call this change in the prompt and the output — a branch name, a
    /// session id, `"staged changes"`. Purely descriptive.
    pub label: String,
}

/// Review a local diff with a caller-supplied [`ReviewBackend`].
///
/// The same pipeline as [`run_review_with`] — glob filtering, diff hygiene,
/// packing, structural context, the backend call, self-critique, the confidence
/// floor, burst collapse, the recommendation floor, the cap, and line anchoring —
/// minus the three stages that only mean something for a pull request:
///
/// - **no provider fetch**: the caller supplies the diff.
/// - **no posting**: there is nothing to post to. `posted` is always false, and the
///   rendered summary is returned for the caller to display.
/// - **no CVE scan**: [`crate::deps::scan`] reads added dependency lines out of a
///   *lockfile* diff, and a local diff carries no guarantee of lockfile semantics
///   (a worktree mid-edit, a partial `git add`). Running it on a diff those
///   assumptions don't hold for would report advisories nobody can act on.
///
/// Findings come back in `findings_detail`, anchored to diff lines exactly as they
/// would be on a PR; `inline_posted` counts the ones that anchored.
///
/// # Errors
/// If the diff is empty or entirely excluded by globs (and no hygiene finding
/// applies), or the backend fails.
pub async fn run_review_local(
    cfg: &Config,
    input: LocalReviewInput,
    backend: &dyn ReviewBackend,
) -> Result<RunReviewOutput> {
    let client = reqwest::Client::new();
    // A synthetic PrMeta so every shared stage below reads the same shape it does
    // on a PR. `pr: 0` and the absent head SHA / CI block are honest: there is no
    // PR number, no reviewed commit, and nothing has been checked.
    let meta = PrMeta {
        repo: input.label.clone(),
        pr: 0,
        title: (!input.label.trim().is_empty()).then(|| input.label.clone()),
        base_branch: None,
        head_sha: None,
        body: None,
        ci_status: None,
    };

    let prepared = prepare_diff(cfg, &input.diff);
    if prepared.diff.trim().is_empty() {
        // No LLM review is possible, but a hygiene finding on an excluded file (a
        // swept-in binary) is still a real result — return it rather than erroring.
        if !prepared.hygiene.is_empty() {
            let summary = render_no_review_summary(&[], &prepared.hygiene);
            let recommendation = effective_recommendation("APPROVE", &prepared.hygiene);
            return Ok(RunReviewOutput {
                provider: LOCAL_PROVIDER.to_string(),
                repo: input.label,
                pr: 0,
                // Empty because no model ran — these findings are deterministic.
                // The PR path's equivalent branch reports `cfg.openrouter_model`
                // here, which is the asymmetry a reviewer will notice; it is
                // deliberate, and the two are NOT going to be reconciled by copying
                // it. This path's backend may not be OpenRouter at all, so naming
                // that model would attribute a deterministic result to a model that
                // was never called and may not even be configured. (The PR path is
                // arguably wrong for the same reason, but its `model` field is in a
                // serialized response pr-review-bot consumes — a separate change.)
                model: String::new(),
                recommendation,
                findings: prepared.hygiene.len(),
                findings_detail: prepared.hygiene,
                inline_posted: 0,
                inline_detail: Vec::new(),
                posted: false,
                comment_url: None,
                summary_markdown: summary,
                usage: None,
            });
        }
        anyhow::bail!(
            "Diff is empty (all files excluded by globs, or no changes) — nothing to review."
        );
    }
    let PreparedDiff {
        diff,
        hygiene,
        omitted_note,
    } = prepared;

    // Structural context from the checkout when there is one; hunk headers (Tier A)
    // otherwise. Fail-open, like the PR path — an empty string just omits the block.
    let mut change_map = crate::changemap::ChangeMap::default();
    let structural = match (cfg.structural_context, input.repo_root.as_deref()) {
        (true, Some(root)) => {
            let (block, map) = crate::structure::local_inner(cfg, root, &diff, map_detail(cfg));
            change_map = map;
            block
        }
        (true, None) => crate::structure::hunk_context(&diff),
        (false, _) => String::new(),
    };
    if !structural.is_empty() {
        tracing::info!(
            "structural context for {}: {} line(s)",
            input.label,
            structural.lines().count()
        );
    }

    let injected_rules = crate::prompt::injected_rules(cfg);
    // The local path synthesizes `meta` with no body, so this is `None` today; it
    // is composed anyway so the two context sites cannot drift.
    let pr_body = crate::prompt::pr_body_block(cfg, &meta);
    let ctx = ReviewContext {
        client: &client,
        cfg,
        provider: None,
        local_root: input.repo_root.as_deref(),
        repo: &input.label,
        meta: &meta,
        diff: &diff,
        omitted_note: omitted_note.as_deref(),
        structural_context: (!structural.is_empty()).then_some(structural.as_str()),
        pr_body: pr_body.as_deref(),
        injected_rules: &injected_rules,
    };
    let result = backend.review(&ctx).await?;
    let mut finished =
        finish_review(cfg, backend, LOCAL_PROVIDER, &meta, &diff, &result, hygiene).await;
    // Same feature on this path: a local review's deliverable *is* its
    // `summary_markdown`, so leaving it unwired made WALKTHROUGH/DIAGRAM silently
    // a no-op for every caller of this entry point.
    append_change_map(
        cfg,
        LOCAL_PROVIDER,
        &change_map,
        &finished.findings,
        &mut finished.summary,
    );

    Ok(RunReviewOutput {
        provider: LOCAL_PROVIDER.to_string(),
        repo: input.label,
        pr: 0,
        model: result.model,
        recommendation: finished.recommendation,
        findings: finished.findings.len(),
        findings_detail: finished.findings,
        inline_posted: finished.inline.len(),
        inline_detail: finished.inline.clone(),
        posted: false,
        comment_url: None,
        summary_markdown: finished.summary,
        usage: result.usage,
    })
}

/// The `provider` a local review reports. Not a [`Provider`] — that enum is the set
/// of hosts this crate can post to, and this review has no host.
pub const LOCAL_PROVIDER: &str = "local";

#[cfg(test)]
mod local_review_tests {
    //! The diff-first entry point: no provider, no PR, no posting. These pin that
    //! it really is the same pipeline — anchoring, glob filtering, structural
    //! context — and not a second reviewer that happens to look similar.

    use std::sync::{Arc, Mutex};

    use anyhow::Result;
    use async_trait::async_trait;

    use super::{run_review_local, LocalReviewInput, LOCAL_PROVIDER};
    use crate::backend::{ReviewBackend, ReviewContext};
    use crate::config::Config;
    use crate::llm::{Finding, Review, ReviewResult};

    /// A change to `src/order.rs`: new-side line 1 is context, 2–4 are added.
    const DIFF: &str = "diff --git a/src/order.rs b/src/order.rs\n\
         --- a/src/order.rs\n\
         +++ b/src/order.rs\n\
         @@ -1,3 +1,5 @@\n\
         \x20fn total(items: &[u32]) -> u32 {\n\
         -    items.iter().sum()\n\
         +    let mut t = 0;\n\
         +    for i in items { t += i; }\n\
         +    t\n\
         \x20}\n";

    /// What a checkout of the new side looks like, for the structural-context test.
    const NEW_SIDE: &str = "fn total(items: &[u32]) -> u32 {\n    let mut t = 0;\n    for i in items { t += i; }\n    t\n}\n";

    /// What the backend was handed, for assertions.
    struct SeenCtx {
        structural: Option<String>,
        had_provider: bool,
    }

    type Seen = Arc<Mutex<Vec<SeenCtx>>>;

    /// Returns one finding anchored to a line the diff really contains, and records
    /// the context it was handed.
    struct LocalSpy {
        seen: Seen,
    }

    #[async_trait]
    impl ReviewBackend for LocalSpy {
        async fn review(&self, ctx: &ReviewContext<'_>) -> Result<ReviewResult> {
            self.seen.lock().unwrap().push(SeenCtx {
                structural: ctx.structural_context.map(str::to_string),
                had_provider: ctx.provider.is_some(),
            });
            Ok(ReviewResult {
                review: Review {
                    summary: "an accumulator replaced a fold".to_string(),
                    recommendation: "APPROVE WITH CHANGES".to_string(),
                    findings: vec![Finding {
                        severity: "MEDIUM".to_string(),
                        file: "src/order.rs".to_string(),
                        line: Some(3),
                        body: "`t` can overflow on a long list. Fix: use checked_add.".to_string(),
                        confidence: Some(90),
                        suggestion: None,
                    }],
                },
                model: "spy".to_string(),
                usage: None,
            })
        }
    }

    fn cfg() -> Config {
        let mut c = Config::from_env();
        c.self_critique = false; // exercised separately, through the seam
        c.min_confidence = 0;
        c.extra_system_prompt = String::new();
        c
    }

    fn spy() -> (LocalSpy, Seen) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        (
            LocalSpy {
                seen: Arc::clone(&seen),
            },
            seen,
        )
    }

    #[tokio::test]
    async fn a_local_diff_returns_anchored_findings() {
        let (backend, seen) = spy();
        let out = run_review_local(
            &cfg(),
            LocalReviewInput {
                diff: DIFF.to_string(),
                repo_root: None,
                label: "vexar/session-7".to_string(),
            },
            &backend,
        )
        .await
        .expect("the local review runs");

        assert_eq!(out.findings_detail.len(), 1);
        assert_eq!(
            out.inline_posted, 1,
            "the finding anchored to a real diff line"
        );
        assert_eq!(out.recommendation, "APPROVE WITH CHANGES");
        assert_eq!(out.provider, LOCAL_PROVIDER);
        assert_eq!(out.repo, "vexar/session-7");
        assert_eq!(out.pr, 0);
        assert!(!out.posted, "a local review has nothing to post to");
        assert!(out
            .summary_markdown
            .contains("an accumulator replaced a fold"));

        let seen = seen.lock().unwrap();
        assert!(
            !seen[0].had_provider,
            "no provider is invented for a local diff"
        );
    }

    /// A finding on a line the diff doesn't contain must fold into the summary
    /// rather than claiming an anchor — the same rule the PR path enforces.
    #[tokio::test]
    async fn a_finding_off_the_diff_folds_into_the_summary() {
        struct OffDiff;
        #[async_trait]
        impl ReviewBackend for OffDiff {
            async fn review(&self, _ctx: &ReviewContext<'_>) -> Result<ReviewResult> {
                Ok(ReviewResult {
                    review: Review {
                        summary: "s".to_string(),
                        recommendation: "APPROVE".to_string(),
                        findings: vec![Finding {
                            severity: "LOW".to_string(),
                            file: "src/order.rs".to_string(),
                            line: Some(900),
                            body: "something far away entirely".to_string(),
                            confidence: Some(50),
                            suggestion: None,
                        }],
                    },
                    model: "spy".to_string(),
                    usage: None,
                })
            }
        }

        let out = run_review_local(
            &cfg(),
            LocalReviewInput {
                diff: DIFF.to_string(),
                repo_root: None,
                label: "branch".to_string(),
            },
            &OffDiff,
        )
        .await
        .expect("the local review runs");

        assert_eq!(out.inline_posted, 0);
        assert!(out.summary_markdown.contains("something far away entirely"));
    }

    /// With a checkout, Tier B names the enclosing symbol from the file on disk —
    /// the local equivalent of fetching the new-side file from the provider.
    /// The feature was wired on the PR path only, so every caller of this entry
    /// point got `WALKTHROUGH=true` and no walkthrough. Reported by review on #38.
    #[tokio::test]
    async fn the_local_path_renders_the_walkthrough_too() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/order.rs"), NEW_SIDE).unwrap();

        let (backend, _seen) = spy();
        let mut c = cfg();
        c.structural_context = true;
        c.walkthrough = true;

        let out = run_review_local(
            &c,
            LocalReviewInput {
                diff: DIFF.to_string(),
                repo_root: Some(dir.path().to_path_buf()),
                label: "branch".to_string(),
            },
            &backend,
        )
        .await
        .expect("the local review runs");

        assert!(
            out.summary_markdown.contains("Walkthrough"),
            "{}",
            out.summary_markdown
        );
        assert!(
            out.summary_markdown.contains("`fn total`"),
            "{}",
            out.summary_markdown
        );
    }

    /// ...and stays off when it is off, on the same path.
    #[tokio::test]
    async fn the_local_path_appends_nothing_when_the_feature_is_off() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/order.rs"), NEW_SIDE).unwrap();

        let (backend, _seen) = spy();
        let mut c = cfg();
        c.structural_context = true;
        c.walkthrough = false;
        c.diagram = false;

        let out = run_review_local(
            &c,
            LocalReviewInput {
                diff: DIFF.to_string(),
                repo_root: Some(dir.path().to_path_buf()),
                label: "branch".to_string(),
            },
            &backend,
        )
        .await
        .expect("the local review runs");

        assert!(
            !out.summary_markdown.contains("Walkthrough"),
            "{}",
            out.summary_markdown
        );
        assert!(
            !out.summary_markdown.contains("mermaid"),
            "{}",
            out.summary_markdown
        );
    }

    #[tokio::test]
    async fn a_repo_root_supplies_tree_sitter_structural_context() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/order.rs"), NEW_SIDE).unwrap();

        let (backend, seen) = spy();
        let mut c = cfg();
        c.structural_context = true;
        run_review_local(
            &c,
            LocalReviewInput {
                diff: DIFF.to_string(),
                repo_root: Some(dir.path().to_path_buf()),
                label: "branch".to_string(),
            },
            &backend,
        )
        .await
        .expect("the local review runs");

        let seen = seen.lock().unwrap();
        let structural = seen[0].structural.as_deref().unwrap_or_default();
        assert!(
            structural.contains("Changed symbols") && structural.contains("total"),
            "Tier B did not name the enclosing function from the checkout: {structural:?}"
        );
    }

    /// Glob filtering is a shared stage, so a lockfile-only change never reaches
    /// the backend here either.
    #[tokio::test]
    async fn an_all_excluded_diff_never_reaches_the_backend() {
        let (backend, seen) = spy();
        let lockfile = "diff --git a/Cargo.lock b/Cargo.lock\n\
             --- a/Cargo.lock\n\
             +++ b/Cargo.lock\n\
             @@ -1,2 +1,3 @@\n\
             \x20[[package]]\n\
             +name = \"new-dep\"\n";

        let out = run_review_local(
            &cfg(),
            LocalReviewInput {
                diff: lockfile.to_string(),
                repo_root: None,
                label: "branch".to_string(),
            },
            &backend,
        )
        .await;

        assert!(
            seen.lock().unwrap().is_empty(),
            "the backend was not called"
        );
        // Either an honest "nothing to review" error, or a hygiene-only result —
        // never a fabricated review.
        if let Ok(o) = out {
            assert!(!o.findings_detail.is_empty());
            assert_eq!(o.inline_posted, 0);
        }
    }

    #[tokio::test]
    async fn an_empty_diff_is_an_error_not_an_empty_review() {
        let (backend, _seen) = spy();
        let err = run_review_local(
            &cfg(),
            LocalReviewInput {
                diff: String::new(),
                repo_root: None,
                label: "branch".to_string(),
            },
            &backend,
        )
        .await
        .expect_err("an empty diff has nothing to review");
        assert!(err.to_string().contains("nothing to review"));
    }
}

#[cfg(test)]
mod orchestrator_tests {
    //! End-to-end tests of the orchestrator over a mocked provider, with a fake
    //! [`ReviewBackend`] standing in for the model.
    //!
    //! These exist because of the 2026-07-31 pr-review-core#28 incident: the
    //! calibration rules were a const each backend was trusted to append, and the
    //! deployed claude-code backend ran for months without them. Nothing in the
    //! output says "the rules are missing" — a miscalibrated review still reads
    //! like a review — so the only honest guard is a test that watches what a
    //! backend is actually handed.

    use std::sync::{Arc, Mutex};

    use anyhow::Result;
    use async_trait::async_trait;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::{run_review_with, RunReviewInput};
    use crate::backend::{ReviewBackend, ReviewContext};
    use crate::config::Config;
    use crate::llm::{Review, ReviewResult};

    const DIFF: &str = "diff --git a/src/a.rs b/src/a.rs\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1,2 +1,3 @@\n fn alpha() {}\n+fn beta() {}\n";

    /// Records the system prompt it is handed, and returns an empty review.
    ///
    /// Note what this backend does NOT do: import anything from `prompt.rs`. Its
    /// rubric is its own string; every calibration rule has to arrive on the
    /// context or it never sees one. That is the whole point of the test.
    struct SpyBackend {
        seen: Arc<Mutex<Vec<String>>>,
        /// What arrived on `ctx.pr_body`, per call. `None` is a real answer, not
        /// an absent recording.
        bodies: Arc<Mutex<Vec<Option<String>>>>,
    }

    #[async_trait]
    impl ReviewBackend for SpyBackend {
        async fn review(&self, ctx: &ReviewContext<'_>) -> Result<ReviewResult> {
            self.bodies
                .lock()
                .unwrap()
                .push(ctx.pr_body.map(str::to_string));
            self.seen
                .lock()
                .unwrap()
                .push(ctx.system_prompt("MY OWN RUBRIC."));
            Ok(ReviewResult {
                review: Review {
                    summary: "nothing to report".to_string(),
                    recommendation: "APPROVE".to_string(),
                    findings: Vec::new(),
                },
                model: "spy".to_string(),
                usage: None,
            })
        }
    }

    /// A GitHub stub serving one PR's metadata and diff. Anything else — notably
    /// `.prbot.toml` — 404s, which the provider reads as "absent".
    async fn github_stub() -> MockServer {
        github_stub_with(DIFF).await
    }

    /// [`github_stub`] over a caller-chosen diff, for tests that need particular
    /// line numbers on the new side.
    async fn github_stub_with(diff: &str) -> MockServer {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/pulls/1"))
            .and(header("accept", "application/vnd.github.diff"))
            .respond_with(ResponseTemplate::new(200).set_body_string(diff.to_string()))
            .mount(&s)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/pulls/1"))
            .and(header("accept", "application/vnd.github+json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "title": "a change",
                "body": null,
                "base": { "ref": "main" },
                "head": { "sha": "deadbeef" },
            })))
            .mount(&s)
            .await;
        s
    }

    /// [`github_stub`] whose PR carries a description — the class B input.
    async fn github_stub_with_body(body: &str) -> MockServer {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/pulls/1"))
            .and(header("accept", "application/vnd.github.diff"))
            .respond_with(ResponseTemplate::new(200).set_body_string(DIFF.to_string()))
            .mount(&s)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/pulls/1"))
            .and(header("accept", "application/vnd.github+json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "title": "a change",
                "body": body,
                "base": { "ref": "main" },
                "head": { "sha": "deadbeef" },
            })))
            .mount(&s)
            .await;
        s
    }

    fn cfg_for(base: &str) -> Config {
        let mut c = Config::from_env();
        c.github_api_base = base.to_string();
        c.github_token = "test-token".to_string();
        c.extra_system_prompt = String::new(); // don't inherit a real one from env
                                               // Everything that would otherwise reach the network past the two stubs.
        c.ci_status = false;
        c.cve_scan = false;
        c.structural_context = false;
        c.self_critique = false;
        c.agentic = false;
        c
    }

    fn input() -> RunReviewInput {
        RunReviewInput {
            provider: "github".to_string(),
            repo: "o/r".to_string(),
            pr: 1,
            dry_run: true,
            placeholder: false,
        }
    }

    /// The orchestrator composes the fenced PR description ONCE and hands it to
    /// whatever backend is in play.
    ///
    /// This is the `injected_rules` lesson repeating. `PR_BODY` first shipped
    /// derived inside the in-crate prompt builders, so it reached the diff-only
    /// path, then the agentic one after review — and no agent-CLI backend at all,
    /// because each of those builds its own prompt and never reads `meta.body`.
    /// A deployment on `backend=claude-code` had the feature silently absent, the
    /// same shape as the #28 incident this module exists for.
    #[tokio::test]
    async fn the_orchestrator_hands_the_fenced_pr_description_to_every_backend() {
        let srv = github_stub_with_body("Adds exponential backoff on 5xx.").await;
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let backend = SpyBackend {
            seen: Arc::new(Mutex::new(Vec::new())),
            bodies: Arc::clone(&bodies),
        };
        let mut cfg = cfg_for(&srv.uri());
        cfg.pr_body = true;

        run_review_with(&cfg, input(), &backend)
            .await
            .expect("the review runs");

        let bodies = bodies.lock().unwrap();
        assert_eq!(bodies.len(), 1, "the backend was called once");
        let block = bodies[0]
            .as_deref()
            .expect("the description reached the backend");
        assert!(
            block.contains("Adds exponential backoff on 5xx."),
            "{block}"
        );
        // Handed over ALREADY FENCED, so a backend that appends it verbatim — the
        // obvious thing to do — cannot lose the fence.
        assert!(
            block.contains("## PR description — written by the PR author"),
            "{block}"
        );
        assert!(
            block.contains(crate::prompt::UNTRUSTED_STEM_FOR_TESTS),
            "{block}"
        );
    }

    /// Off means no backend sees it, so the A/B has a real control arm.
    #[tokio::test]
    async fn no_backend_sees_the_description_when_the_flag_is_off() {
        let srv = github_stub_with_body("Adds exponential backoff on 5xx.").await;
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let backend = SpyBackend {
            seen: Arc::new(Mutex::new(Vec::new())),
            bodies: Arc::clone(&bodies),
        };
        let mut cfg = cfg_for(&srv.uri());
        cfg.pr_body = false;

        run_review_with(&cfg, input(), &backend)
            .await
            .expect("the review runs");

        assert_eq!(bodies.lock().unwrap()[0], None);
    }

    #[tokio::test]
    async fn the_orchestrator_hands_the_review_rules_to_every_backend() {
        let srv = github_stub().await;
        let seen = Arc::new(Mutex::new(Vec::new()));
        let backend = SpyBackend {
            seen: Arc::clone(&seen),
            bodies: Arc::new(Mutex::new(Vec::new())),
        };

        run_review_with(&cfg_for(&srv.uri()), input(), &backend)
            .await
            .expect("the review runs");

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "the backend was called once");
        let prompt = &seen[0];
        assert!(
            prompt.starts_with("MY OWN RUBRIC."),
            "the backend's own rubric leads; the rules follow it"
        );
        // One assertion per recorded production failure the rules encode.
        for rule in [
            "THROWS is never LOW",
            "config that ENFORCES it",
            "breaks the build",
            "vendored",
            "raise it ONCE",
            "middleware, guard, decorator",
        ] {
            assert!(
                prompt.contains(rule),
                "the backend never received the rule containing {rule:?}"
            );
        }
    }

    /// Returns a fixed set of findings so the run log's funnel has something to
    /// count at every stage.
    struct FunnelBackend;

    #[async_trait]
    impl ReviewBackend for FunnelBackend {
        async fn review(&self, _ctx: &ReviewContext<'_>) -> Result<ReviewResult> {
            let f = |sev: &str, file: &str, line: Option<u64>, conf: u8, body: &str| {
                crate::llm::Finding {
                    severity: sev.to_string(),
                    file: file.to_string(),
                    line,
                    body: body.to_string(),
                    confidence: Some(conf),
                    suggestion: None,
                }
            };
            Ok(ReviewResult {
                review: Review {
                    summary: "several things".to_string(),
                    recommendation: "APPROVE WITH CHANGES".to_string(),
                    // New-side line 2 is the added line in DIFF, so it anchors;
                    // src/zzz.rs is not in the diff at all, so it cannot.
                    findings: vec![
                        f("HIGH", "src/a.rs", Some(2), 90, "a null deref"),
                        f("HIGH", "src/a.rs", Some(2), 80, "an unchecked index"),
                        f("MEDIUM", "src/zzz.rs", Some(99), 70, "not in this diff"),
                        f("LOW", "src/a.rs", Some(2), 60, "capped out"),
                        f("LOW", "src/a.rs", Some(2), 10, "below the floor"),
                    ],
                },
                model: "spy".to_string(),
                usage: None,
            })
        }
    }

    /// The run log's reason to exist: the per-stage counts are the only record of
    /// what the pipeline removed. A caller holding the posted findings can see
    /// three of them and cannot tell that the model proposed five, that the
    /// confidence floor took one, and that the cap took another.
    #[tokio::test]
    async fn the_run_log_records_the_full_finding_funnel() {
        let srv = github_stub().await;
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("runs.jsonl");

        let mut cfg = cfg_for(&srv.uri());
        cfg.min_confidence = 50; // drops the conf-10 finding
        cfg.max_findings = 3; // drops the lowest-ranked survivor
        cfg.reanchor_findings = false; // keep anchoring exact for the assertions
        cfg.run_log = Some(crate::runlog::RunLogSink::File(log.clone()));

        run_review_with(&cfg, input(), &FunnelBackend)
            .await
            .expect("the review runs");

        let text = std::fs::read_to_string(&log).expect("a record was written");
        let v: serde_json::Value =
            serde_json::from_str(text.trim()).expect("one parseable JSON line");

        assert_eq!(v["repo"], "o/r");
        assert_eq!(v["pr"], 1);
        assert_eq!(v["model"], "spy");
        assert_eq!(v["head_sha"], "deadbeef");
        assert_eq!(v["dry_run"], true);
        assert_eq!(v["posted"], false, "a dry run posts nothing");
        assert_eq!(v["recommendation"], "APPROVE WITH CHANGES");

        let f = &v["funnel"];
        assert_eq!(f["model_raw"], 5, "what the backend proposed");
        assert_eq!(
            f["after_critique"], 5,
            "self-critique is off in this config"
        );
        assert_eq!(
            f["after_confidence"], 4,
            "the conf-10 finding fell to the floor"
        );
        assert_eq!(f["hygiene_added"], 0);
        assert_eq!(f["after_collapse"], 4);
        assert_eq!(
            f["posted_findings"], 3,
            "the cap took the lowest-ranked one"
        );
        assert_eq!(f["anchored"], 2, "both src/a.rs:2 findings hit a diff line");
        assert_eq!(f["unanchored"], 1, "src/zzz.rs:99 is not in the diff");

        let findings = v["findings"].as_array().expect("findings array");
        assert_eq!(findings.len(), 3, "the posted findings, with their text");
        assert_eq!(findings[0]["severity"], "HIGH");
        assert_eq!(findings[0]["confidence"], 90);
        assert_eq!(findings[0]["anchored"], true);
        assert_eq!(findings[0]["anchored_line"], 2);
        assert_eq!(findings[2]["file"], "src/zzz.rs");
        assert_eq!(findings[2]["anchored"], false);
        assert!(findings[2]["anchored_line"].is_null());
    }

    /// New-side lines: 1 is context, 2 and 3 are added. A finding that names
    /// `calcTotal` but claims line 5 re-anchors onto line 3.
    const DRIFT_DIFF: &str = "diff --git a/src/order.ts b/src/order.ts\n--- a/src/order.ts\n+++ b/src/order.ts\n@@ -1,1 +1,3 @@\n const items = [];\n+const subtotal = sum(items);\n+return calcTotal(order, tax);\n";

    /// Proposes one finding whose line is off the diff but whose body names a
    /// symbol on a nearby added line — the re-anchor path.
    struct DriftBackend;

    #[async_trait]
    impl ReviewBackend for DriftBackend {
        async fn review(&self, _ctx: &ReviewContext<'_>) -> Result<ReviewResult> {
            Ok(ReviewResult {
                review: Review {
                    summary: "one drifted finding".to_string(),
                    recommendation: "APPROVE WITH CHANGES".to_string(),
                    findings: vec![crate::llm::Finding {
                        severity: "HIGH".to_string(),
                        file: "src/order.ts".to_string(),
                        line: Some(5),
                        body: "`calcTotal` now needs a tax arg.".to_string(),
                        confidence: Some(90),
                        suggestion: None,
                    }],
                },
                model: "spy".to_string(),
                usage: None,
            })
        }
    }

    /// A re-anchored finding must be logged as anchored, at the line the comment
    /// actually went to.
    ///
    /// The first version of this module decided `anchored` by re-matching the
    /// finding's own `(file, line)` against the posted comments. `reanchor` only
    /// ever returns a line *different* from the finding's, and the finding is
    /// never updated with it — so every re-anchored finding logged as
    /// `anchored: false` despite having a comment on the PR. With
    /// `REANCHOR_FINDINGS` defaulting to on, that is the common case, and the
    /// per-finding flags would have disagreed with `funnel.anchored` in most
    /// production records.
    #[tokio::test]
    async fn a_reanchored_finding_logs_the_line_its_comment_went_to() {
        let srv = github_stub_with(DRIFT_DIFF).await;
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("runs.jsonl");

        let mut cfg = cfg_for(&srv.uri());
        cfg.reanchor_findings = true; // the default, and the bug's precondition
        cfg.run_log = Some(crate::runlog::RunLogSink::File(log.clone()));

        run_review_with(&cfg, input(), &DriftBackend)
            .await
            .expect("the review runs");

        let text = std::fs::read_to_string(&log).expect("a record was written");
        let v: serde_json::Value =
            serde_json::from_str(text.trim()).expect("one parseable JSON line");

        assert_eq!(v["funnel"]["anchored"], 1, "the comment was posted inline");
        assert_eq!(v["funnel"]["unanchored"], 0);

        let f = &v["findings"][0];
        assert_eq!(f["line"], 5, "the line the model named is preserved");
        assert_eq!(f["anchored"], true, "it did anchor — this was the bug");
        assert_eq!(f["anchored_line"], 3, "onto the line naming calcTotal");
    }

    /// Proposes one finding carrying a suggestion, at a caller-chosen line.
    struct SuggestBackend {
        line: u64,
    }

    #[async_trait]
    impl ReviewBackend for SuggestBackend {
        async fn review(&self, _ctx: &ReviewContext<'_>) -> Result<ReviewResult> {
            Ok(ReviewResult {
                review: Review {
                    summary: "one finding with a fix".to_string(),
                    recommendation: "APPROVE WITH CHANGES".to_string(),
                    findings: vec![crate::llm::Finding {
                        severity: "HIGH".to_string(),
                        file: "src/order.ts".to_string(),
                        line: Some(self.line),
                        body: "`calcTotal` now needs a region arg.".to_string(),
                        confidence: Some(90),
                        suggestion: Some("return calcTotal(order, tax, region);".to_string()),
                    }],
                },
                model: "spy".to_string(),
                usage: None,
            })
        }
    }

    /// Runs a review over [`DRIFT_DIFF`] with `cfg` and returns its run-log record.
    async fn record_for(cfg: &mut Config, backend: &dyn ReviewBackend) -> serde_json::Value {
        let srv = github_stub_with(DRIFT_DIFF).await;
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("runs.jsonl");
        cfg.github_api_base = srv.uri();
        cfg.run_log = Some(crate::runlog::RunLogSink::File(log.clone()));

        run_review_with(cfg, input(), backend)
            .await
            .expect("the review runs");

        let text = std::fs::read_to_string(&log).expect("a record was written");
        serde_json::from_str(text.trim()).expect("one parseable JSON line")
    }

    /// A finding anchored to the line the model named gets its suggestion.
    #[tokio::test]
    async fn an_exactly_anchored_finding_carries_its_suggestion() {
        let mut cfg = cfg_for("http://replaced");
        cfg.suggestions = true;

        // Line 3 of DRIFT_DIFF is `return calcTotal(order, tax);` — a real diff
        // line, so no re-anchoring is involved.
        let v = record_for(&mut cfg, &SuggestBackend { line: 3 }).await;

        assert_eq!(v["funnel"]["anchored"], 1);
        assert_eq!(v["funnel"]["suggested"], 1, "the block was attached");
    }

    /// A **re-anchored** finding must never carry one.
    ///
    /// The replacement text was written for the line the model named; the comment
    /// is going to a different line. Prose survives that move — it describes a
    /// problem — but replacement text does not: applied, it overwrites a line the
    /// finding never looked at, and it does so on one click. This is the single
    /// rule that makes suggestions safe to default on alongside
    /// `REANCHOR_FINDINGS`, which is itself on by default.
    #[tokio::test]
    async fn a_reanchored_finding_never_carries_a_suggestion() {
        let mut cfg = cfg_for("http://replaced");
        cfg.suggestions = true;
        cfg.reanchor_findings = true;

        // Line 5 is off the diff; the body names `calcTotal`, so it re-anchors
        // onto line 3 — the same path the test above exercises for logging.
        let v = record_for(&mut cfg, &SuggestBackend { line: 5 }).await;

        assert_eq!(v["funnel"]["anchored"], 1, "it still posts inline");
        assert_eq!(
            v["funnel"]["suggested"], 0,
            "but with no button, because the comment moved"
        );
    }

    /// The rendered block reaches the caller, so a dry run can show what would
    /// post.
    ///
    /// `inline_posted` is a count and a count cannot be reviewed: the first cut of
    /// suggestions shipped with the rendered bodies discarded inside
    /// `finish_review`, which left no way to read a one-click commit before it
    /// reached someone's PR.
    #[tokio::test]
    async fn the_caller_can_read_the_comment_that_would_post() {
        let srv = github_stub_with(DRIFT_DIFF).await;
        let mut cfg = cfg_for(&srv.uri());
        cfg.suggestions = true;

        let out = run_review_with(&cfg, input(), &SuggestBackend { line: 3 })
            .await
            .expect("the review runs");

        assert_eq!(out.inline_detail.len(), 1, "the comment is exposed");
        let c = &out.inline_detail[0];
        assert_eq!((c.path.as_str(), c.line), ("src/order.ts", 3));
        assert!(c.body.contains("region arg"), "the prose is there");
        assert!(
            c.body.contains(
                "```suggestion
return calcTotal(order, tax, region);
```"
            ),
            "and so is the block, rendered exactly as it posts: {}",
            c.body
        );
    }

    /// Off by config means off, even on a finding that would have qualified.
    #[tokio::test]
    async fn suggestions_off_withholds_the_block() {
        let mut cfg = cfg_for("http://replaced");
        cfg.suggestions = false;

        let v = record_for(&mut cfg, &SuggestBackend { line: 3 }).await;

        assert_eq!(v["funnel"]["anchored"], 1);
        assert_eq!(v["funnel"]["suggested"], 0);
    }

    /// The privacy default. A record carries review commentary on someone's
    /// source, so an operator who never asked for a log must not get one — and
    /// the check is a config field being unset, not a path that happens to fail.
    #[tokio::test]
    async fn no_run_log_is_written_unless_one_is_configured() {
        let srv = github_stub().await;
        let dir = tempfile::tempdir().expect("tempdir");

        let mut cfg = cfg_for(&srv.uri());
        cfg.run_log = None;

        run_review_with(&cfg, input(), &FunnelBackend)
            .await
            .expect("the review runs");

        let written: Vec<_> = std::fs::read_dir(dir.path())
            .expect("readable tempdir")
            .collect();
        assert!(written.is_empty(), "nothing was logged anywhere");
    }

    /// Proposes two findings, and answers `complete()` with whatever the test
    /// configured — the critique reply, or an error. Records what it was asked.
    struct CritiqueSpy {
        asked: Arc<Mutex<Vec<(String, String)>>>,
        reply: Option<&'static str>,
    }

    fn proposed(file: &str, body: &str) -> crate::llm::Finding {
        crate::llm::Finding {
            severity: "MEDIUM".to_string(),
            file: file.to_string(),
            line: None,
            body: body.to_string(),
            confidence: Some(80),
            suggestion: None,
        }
    }

    #[async_trait]
    impl ReviewBackend for CritiqueSpy {
        async fn review(&self, _ctx: &ReviewContext<'_>) -> Result<ReviewResult> {
            Ok(ReviewResult {
                review: Review {
                    summary: "two things".to_string(),
                    recommendation: "APPROVE WITH CHANGES".to_string(),
                    findings: vec![
                        proposed("src/a.rs", "a real problem"),
                        proposed("src/a.rs", "a noisy nit"),
                    ],
                },
                model: "spy".to_string(),
                usage: None,
            })
        }

        async fn complete(&self, _cfg: &Config, system: &str, user: &str) -> Result<String> {
            self.asked
                .lock()
                .unwrap()
                .push((system.to_string(), user.to_string()));
            match self.reply {
                Some(r) => Ok(r.to_string()),
                None => anyhow::bail!("backend is down"),
            }
        }
    }

    fn critique_cfg(base: &str) -> Config {
        let mut c = cfg_for(base);
        c.self_critique = true;
        c.min_confidence = 0; // isolate the critique from the confidence floor
        c
    }

    /// The critique used to post to OpenRouter directly, so a consumer running its
    /// own backend silently lost the noise filter. It must now travel the seam.
    #[tokio::test]
    async fn the_critique_runs_on_the_backend_that_produced_the_review() {
        let srv = github_stub().await;
        let asked = Arc::new(Mutex::new(Vec::new()));
        let backend = CritiqueSpy {
            asked: Arc::clone(&asked),
            reply: Some(
                r#"[{"severity":"MEDIUM","file":"src/a.rs","body":"a real problem","confidence":90}]"#,
            ),
        };

        let out = run_review_with(&critique_cfg(&srv.uri()), input(), &backend)
            .await
            .expect("the review runs");

        let asked = asked.lock().unwrap();
        assert_eq!(asked.len(), 1, "complete() carried the critique");
        let (system, user) = &asked[0];
        assert!(
            system.contains("skeptical senior reviewer"),
            "the critique system prompt reached the backend: {system}"
        );
        assert!(
            user.contains("PROPOSED FINDINGS") && user.contains("a noisy nit"),
            "the proposed findings reached the backend"
        );
        assert_eq!(
            out.findings_detail.len(),
            1,
            "the critique's pruning is honored"
        );
        assert!(out.findings_detail[0].body.contains("a real problem"));
    }

    /// Fail-open: a critique that errors must never cost the review its findings.
    #[tokio::test]
    async fn a_failed_critique_keeps_the_original_findings() {
        let srv = github_stub().await;
        let backend = CritiqueSpy {
            asked: Arc::new(Mutex::new(Vec::new())),
            reply: None, // complete() errors
        };

        let out = run_review_with(&critique_cfg(&srv.uri()), input(), &backend)
            .await
            .expect("the review still succeeds");

        assert_eq!(out.findings_detail.len(), 2, "both proposals survive");
    }

    /// The shape guard: elements came back but none parsed, so the critique is
    /// broken rather than decisive — error, and the caller keeps everything.
    /// Returning the empty list would delete every finding in the review.
    #[tokio::test]
    async fn an_unparseable_critique_keeps_the_original_findings() {
        let srv = github_stub().await;
        let backend = CritiqueSpy {
            asked: Arc::new(Mutex::new(Vec::new())),
            reply: Some(r#"[{"nonsense":true},{"also":"nonsense"}]"#),
        };

        let out = run_review_with(&critique_cfg(&srv.uri()), input(), &backend)
            .await
            .expect("the review still succeeds");

        assert_eq!(out.findings_detail.len(), 2, "both proposals survive");
    }

    /// A critique that legitimately drops everything is still obeyed — the guard
    /// above keys on *unparseable* elements, not on an honest empty verdict.
    #[tokio::test]
    async fn an_empty_critique_verdict_is_obeyed() {
        let srv = github_stub().await;
        let backend = CritiqueSpy {
            asked: Arc::new(Mutex::new(Vec::new())),
            reply: Some("[]"),
        };

        let out = run_review_with(&critique_cfg(&srv.uri()), input(), &backend)
            .await
            .expect("the review still succeeds");

        assert_eq!(out.findings_detail.len(), 0, "all findings were noise");
    }

    #[tokio::test]
    async fn the_consumers_extra_prompt_rides_along_with_the_rules() {
        let srv = github_stub().await;
        let mut cfg = cfg_for(&srv.uri());
        cfg.extra_system_prompt = "House rule: never widen a public API.".to_string();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let backend = SpyBackend {
            seen: Arc::clone(&seen),
            bodies: Arc::new(Mutex::new(Vec::new())),
        };

        run_review_with(&cfg, input(), &backend)
            .await
            .expect("the review runs");

        let seen = seen.lock().unwrap();
        let prompt = &seen[0];
        assert!(
            prompt.contains("THROWS is never LOW"),
            "rules still present"
        );
        assert!(
            prompt.contains("House rule: never widen a public API."),
            "a consumer's extra prompt reaches the backend too"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{
        burst_key, collapse_bursts, demote_falsified_build_claims, effective_recommendation,
        idents, line_symbols, reanchor, render_no_review_summary,
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
            suggestion: None,
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

    /// Collapsing must never soften the verdict: a group is uniform in severity, so
    /// whatever `effective_recommendation` would have seen, it still sees.
    #[test]
    fn a_uniform_group_collapses_at_its_own_severity() {
        let findings = vec![
            f("MEDIUM", "a.zip", "A binary file `a.zip` was added."),
            f("MEDIUM", "b.zip", "A binary file `b.zip` was added."),
            f("MEDIUM", "c.zip", "A binary file `c.zip` was added."),
        ];
        let out = collapse_bursts(findings);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].severity, "MEDIUM");
        assert_eq!(out[0].file, "a.zip");
    }

    /// Severity is part of the group key, so a shared phrase at two severities is
    /// two claims, not one. Splitting means neither collapses (each is below the
    /// threshold) — the safe direction: nothing is merged that shouldn't be.
    #[test]
    fn a_mixed_severity_group_is_not_collapsed() {
        let findings = vec![
            f("LOW", "a.zip", "A binary file `a.zip` was added."),
            f("MEDIUM", "b.zip", "A binary file `b.zip` was added."),
            f("LOW", "c.zip", "A binary file `c.zip` was added."),
        ];
        let out = collapse_bursts(findings);
        assert_eq!(out.len(), 3, "2 LOW + 1 MEDIUM: neither reaches 3");
        assert!(out.iter().all(|f| !f.body.contains("other file(s)")));
    }

    /// pr-review-core#28, twice: a BLOCKING then MEDIUM finding claiming
    /// `cargo fmt --check` would fail, on a commit whose three checks were green.
    /// The prompt rule did not stop it, so this is enforced here.
    #[test]
    fn a_build_claim_is_capped_at_low_when_ci_is_green() {
        let green = "- fmt + clippy + test: success\n- cargo package: success";
        let mut findings = vec![f(
            "BLOCKING",
            "src/blast.rs",
            "This line is 118 chars, so `cargo fmt --check` will fail on this commit.",
        )];
        demote_falsified_build_claims(&mut findings, Some(green));
        assert_eq!(findings[0].severity, "LOW");
        assert!(findings[0]
            .body
            .contains("Every CI check on the reviewed commit passed"));
        // Demoted, never deleted — the observation survives.
        assert!(findings[0].body.contains("118 chars"));
    }

    #[test]
    fn a_build_claim_survives_when_ci_is_not_green() {
        let body = "`cargo fmt --check` will fail on this commit.";
        for ci in [
            None,                                    // no CI reported
            Some("- fmt: failure\n- test: success"), // a real failure
            Some("- fmt: in_progress"),              // not finished
            Some(""),                                // empty block
            // Truncated list: a hidden failure is possible, so this must not demote.
            Some("- a: success\n- [12 further check(s) NOT shown — this commit has 312.]"),
        ] {
            let mut findings = vec![f("BLOCKING", "src/x.rs", body)];
            demote_falsified_build_claims(&mut findings, ci);
            assert_eq!(
                findings[0].severity, "BLOCKING",
                "wrongly demoted with ci={ci:?}"
            );
        }
    }

    /// Raised on PR #30, and correct: a flat marker list containing "will fail"
    /// matches ordinary runtime findings. Green CI would then cap a real HIGH to LOW
    /// and soften the recommendation floor with it — a silent downgrade of a genuine
    /// bug, from a mechanism added to *reduce* wrong verdicts. The original test
    /// missed it by using bodies with no failure phrase at all.
    #[test]
    fn a_runtime_failure_claim_is_not_a_check_claim() {
        let green = "- fmt + clippy + test: success";
        for body in [
            "This will fail at runtime when the list is empty.",
            "The request would fail if the token has expired.",
            "`parse()` fails when the header is absent, and the error is swallowed.",
            "This would break existing callers that pass a null id.",
        ] {
            let mut findings = vec![f("HIGH", "src/x.rs", body)];
            demote_falsified_build_claims(&mut findings, Some(green));
            assert_eq!(
                findings[0].severity, "HIGH",
                "green CI wrongly capped a runtime finding: {body:?}"
            );
        }
    }

    /// The other half: the same weak phrase IS a check claim once the body names a
    /// check, which is the shape pr-review-core#28 actually produced.
    #[test]
    fn a_weak_phrase_counts_once_a_check_is_named() {
        let green = "- fmt + clippy + test: success";
        for body in [
            "This line is 118 chars, so `cargo fmt --check` will fail on this commit.",
            "The clippy job will fail on this unwrap.",
            "CI will fail because the lockfile is stale.",
            "This breaks the build on MSVC.",
        ] {
            let mut findings = vec![f("BLOCKING", "src/x.rs", body)];
            demote_falsified_build_claims(&mut findings, Some(green));
            assert_eq!(findings[0].severity, "LOW", "not capped: {body:?}");
        }
    }

    /// "ci" is a substring of ordinary English — match it as a word, not a substring.
    #[test]
    fn check_context_words_match_whole_words_only() {
        let green = "- test: success";
        let mut findings = vec![f(
            "HIGH",
            "src/x.rs",
            "The specific decision here will fail for efficient callers.",
        )];
        demote_falsified_build_claims(&mut findings, Some(green));
        assert_eq!(
            findings[0].severity, "HIGH",
            "'ci' inside specific/decision/efficient must not count as a check"
        );
    }

    /// Only claims about a *check outcome* are capped. A real bug found on a commit
    /// with green CI is not evidence of anything — CI does not run for correctness.
    #[test]
    fn an_ordinary_finding_is_untouched_by_green_ci() {
        let green = "- test: success";
        let mut findings = vec![
            f("HIGH", "a.rs", "Unvalidated input reaches the SQL query."),
            f(
                "MEDIUM",
                "b.rs",
                "This handler can panic on an empty slice.",
            ),
        ];
        demote_falsified_build_claims(&mut findings, Some(green));
        assert_eq!(findings[0].severity, "HIGH");
        assert_eq!(findings[1].severity, "MEDIUM");
        assert!(!findings[0].body.contains("CI check"));
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
            suggestion: None,
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
        assert_eq!(
            effective_recommendation("APPROVE", &[finding("BLOCKING")]),
            "BLOCK"
        );
        // A LOW-only finding does NOT force changes.
        assert_eq!(
            effective_recommendation("APPROVE", &[finding("LOW")]),
            "APPROVE"
        );
        // The model's stronger verdict is never softened by weaker findings.
        assert_eq!(
            effective_recommendation("BLOCK", &[finding("LOW")]),
            "BLOCK"
        );
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
        assert!(!s
            .iter()
            .any(|w| w == "export" || w == "function" || w == "o"));
    }

    #[test]
    fn reanchor_snaps_to_the_matching_diff_line() {
        let valid: HashSet<u64> = [8, 10, 12].into_iter().collect();
        let mut texts = HashMap::new();
        texts.insert(8, "  const subtotal = sum(items);".to_string());
        texts.insert(10, "  return calcTotal(order, tax);".to_string());
        texts.insert(12, "}".to_string());
        // Finding drifted to line 9; its body names calcTotal, which is on line 10.
        let got = reanchor(
            9,
            &valid,
            &texts,
            "`calcTotal` now needs a tax arg. Fix: pass it.",
        );
        assert_eq!(got, Some(10));
    }

    #[test]
    fn reanchor_declines_without_a_content_match() {
        let valid: HashSet<u64> = [8, 10].into_iter().collect();
        let mut texts = HashMap::new();
        texts.insert(8, "  const x = 1;".to_string());
        texts.insert(10, "  const y = 2;".to_string());
        assert_eq!(
            reanchor(9, &valid, &texts, "Missing null check on user.roles"),
            None
        );
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
        assert_eq!(
            reanchor(9, &valid, &texts, "calcTotal needs a tax arg"),
            Some(8)
        );
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

#[cfg(test)]
mod change_map_tests {
    use super::*;
    use crate::changemap::{ChangeMap, Edge, EdgeKind, FileEntry, SymbolNode};

    fn map() -> ChangeMap {
        ChangeMap {
            symbols: vec![
                SymbolNode {
                    label: "fn",
                    name: "caller".into(),
                    file: "a.rs".into(),
                    start: 1,
                    end: 3,
                    cyclomatic: Some(12),
                    cognitive: Some(9),
                },
                SymbolNode {
                    label: "fn",
                    name: "callee".into(),
                    file: "b.rs".into(),
                    start: 1,
                    end: 2,
                    cyclomatic: None,
                    cognitive: None,
                },
            ],
            files: vec![
                FileEntry {
                    path: "a.rs".into(),
                    added: 3,
                    removed: 1,
                    symbols: vec![0],
                    worst: None,
                },
                FileEntry {
                    path: "b.rs".into(),
                    added: 2,
                    removed: 0,
                    symbols: vec![1],
                    worst: None,
                },
            ],
            edges: vec![Edge {
                from: 0,
                to: 1,
                kind: EdgeKind::Call,
            }],
            edges_truncated: false,
        }
    }

    fn conf(walkthrough: bool, diagram: bool) -> Config {
        let mut c = Config::from_env();
        c.walkthrough = walkthrough;
        c.diagram = diagram;
        c
    }

    #[test]
    fn both_blocks_land_on_the_summary_when_enabled() {
        let mut s = "🤖 **Automated review**".to_string();
        append_change_map(&conf(true, true), "github", &map(), &[], &mut s);
        assert!(s.contains("🗺️ <b>Walkthrough</b>"), "{s}");
        assert!(s.contains("```mermaid"), "{s}");
    }

    /// Both are off by default: they change what every review comment looks like,
    /// and that is the operator's call, not this crate's.
    #[test]
    fn nothing_is_appended_when_both_are_off() {
        let before = "🤖 **Automated review**".to_string();
        let mut s = before.clone();
        append_change_map(&conf(false, false), "github", &map(), &[], &mut s);
        assert_eq!(s, before);
    }

    /// Bitbucket renders no mermaid, so the block would post as a wall of source.
    /// The walkthrough is plain markdown and still goes out.
    #[test]
    fn bitbucket_gets_the_table_but_not_the_diagram() {
        let mut s = String::new();
        append_change_map(&conf(true, true), "bitbucket", &map(), &[], &mut s);
        assert!(s.contains("Walkthrough"), "{s}");
        assert!(!s.contains("mermaid"), "{s}");
    }

    /// A review with no structural context (parse failed, feature off, no head
    /// SHA) must append nothing at all rather than an empty `<details>` block.
    #[test]
    fn an_empty_map_appends_nothing() {
        let mut s = String::new();
        append_change_map(
            &conf(true, true),
            "github",
            &ChangeMap::default(),
            &[],
            &mut s,
        );
        assert!(s.is_empty(), "{s}");
    }

    /// The findings column is the join that makes the table worth reading: it
    /// says *where* the review landed, not just what changed.
    #[test]
    fn the_table_attributes_each_finding_to_its_file() {
        let findings = vec![Finding {
            severity: "BLOCKING".into(),
            file: "b.rs".into(),
            line: Some(1),
            body: "x".into(),
            confidence: None,
            suggestion: None,
        }];
        let mut s = String::new();
        append_change_map(&conf(true, false), "github", &map(), &findings, &mut s);
        let b_row = s.lines().find(|l| l.contains("`b.rs`")).expect("b.rs row");
        assert!(b_row.contains("🚨"), "{b_row}");
        let a_row = s.lines().find(|l| l.contains("`a.rs`")).expect("a.rs row");
        assert!(a_row.contains("| — |"), "{a_row}");
    }
}

#[cfg(test)]
mod placeholder_tests {
    use super::*;

    /// The whole point: a placeholder must not read as a finished review, or a
    /// consumer's boot sweep counts a died-mid-flight review as covered — which is
    /// exactly the case it exists to catch.
    #[test]
    fn a_placeholder_is_not_a_completed_review() {
        assert!(is_incomplete_review(&render_pending()));
        assert!(is_incomplete_review(&render_failed("boom")));
    }

    /// The other direction. A real review misread as incomplete would be reviewed
    /// again on every boot — duplicate comments, duplicate spend, forever.
    #[test]
    fn a_real_review_is_not_flagged_incomplete() {
        let real = render_summary(
            &Review {
                summary: "Looks fine.".into(),
                recommendation: "APPROVE".into(),
                findings: vec![],
            },
            "APPROVE",
            &[],
            0,
        );
        assert!(
            !is_incomplete_review(&real),
            "a finished review must never look incomplete: {real}"
        );
        // And nothing incidental trips it.
        assert!(!is_incomplete_review(
            "🤖 **Automated review**\n\nAll good."
        ));
    }

    /// Placeholders posted before the markers existed are still on open PRs right
    /// now. If the predicate only knew about the marker, those would stay invisible
    /// to reconciliation permanently — the exact bug, just grandfathered.
    #[test]
    fn a_legacy_placeholder_without_the_marker_is_still_recognised() {
        let legacy = "🤖 **Automated review**\n\n⏳ _Reviewing this PR… (this comment \
                      will update shortly)_\n\n_🤖 pr-review-bot_";
        assert!(
            is_incomplete_review(legacy),
            "a placeholder already sitting on a PR must be recognised too"
        );
    }

    /// The failure notice has to carry the error, or it is just a different lie.
    #[test]
    fn the_failure_notice_names_the_error_and_how_to_retry() {
        let body = render_failed("subtype=error_max_turns, after 31/30 turn(s)");
        assert!(body.contains("error_max_turns"), "{body}");
        assert!(body.contains("/review"), "says how to retry: {body}");
        assert!(
            !body.contains("update shortly"),
            "no longer promises an update"
        );
    }
}
