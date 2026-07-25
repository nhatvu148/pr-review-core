//! Benchmark harness: score the reviewer against a corpus of PRs with **known**
//! issues, and report precision / recall / F1 + token cost. This is the thing
//! that turns "we think blast radius / complexity help" into a number — method
//! borrowed from alibaba/open-code-review's evaluation approach.
//!
//! Usage:
//!   cargo run --example bench -- corpus.json
//!
//! Requires the same env as a real review (OPENROUTER_API_KEY, a provider token).
//! Runs each PR in **dry-run** (nothing is posted). To A/B a feature, run twice
//! with the env toggled, e.g.:
//!   BLAST_RADIUS=false cargo run --example bench -- corpus.json
//!   BLAST_RADIUS=true  cargo run --example bench -- corpus.json
//! and likewise for COMPLEXITY_METRICS / SELF_CRITIQUE / AGENTIC / the backend.
//!
//! ## Corpus format (JSON array)
//! ```json
//! [
//!   { "provider": "github", "repo": "owner/app", "pr": 42,
//!     "issues": [ { "file": "src/a.ts", "line": 88, "type": "bug", "note": "N+1" } ] }
//! ]
//! ```
//! A finding counts as matching a ground-truth issue when it's in the same file
//! within ±`TOLERANCE` lines. Scoring is proximity-based (a signal, not a proof) —
//! keep the corpus honest and the numbers are directional but useful.

use pr_review_core::config::Config;
use pr_review_core::llm::Finding;
use pr_review_core::review::{run_review, RunReviewInput};
use serde::Deserialize;

/// How many lines off a finding may be and still count as hitting an issue.
const TOLERANCE: i64 = 3;

#[derive(Deserialize)]
struct Case {
    provider: String,
    repo: String,
    pr: u64,
    #[serde(default)]
    issues: Vec<Issue>,
}

#[derive(Deserialize)]
struct Issue {
    file: String,
    line: u64,
    #[serde(default)]
    #[allow(dead_code)]
    r#type: String,
    #[serde(default)]
    #[allow(dead_code)]
    note: String,
}

/// A finding hits an issue: same file, within TOLERANCE lines.
fn hits(f: &Finding, i: &Issue) -> bool {
    f.file == i.file
        && f
            .line
            .is_some_and(|l| (l as i64 - i.line as i64).abs() <= TOLERANCE)
}

fn f1(precision: f64, recall: f64) -> f64 {
    if precision + recall == 0.0 {
        0.0
    } else {
        2.0 * precision * recall / (precision + recall)
    }
}

#[tokio::main]
async fn main() {
    let path = match std::env::args().nth(1) {
        Some(p) => p,
        None => {
            eprintln!("usage: cargo run --example bench -- <corpus.json>");
            std::process::exit(2);
        }
    };
    let corpus: Vec<Case> = serde_json::from_str(
        &std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}")),
    )
    .unwrap_or_else(|e| panic!("parse {path}: {e}"));

    let cfg = Config::from_env();
    eprintln!(
        "bench: {} case(s) · blast={} · complexity={} · self_critique={} · agentic={}\n",
        corpus.len(),
        cfg.blast_radius,
        cfg.complexity_metrics,
        cfg.self_critique,
        cfg.agentic
    );

    // Corpus-wide tallies.
    let (mut tp_find, mut n_find, mut tp_issue, mut n_issue, mut tokens) = (0usize, 0usize, 0usize, 0usize, 0u64);
    let bar = "─".repeat(78);
    println!("{bar}");
    println!(
        "{:<34}{:>7}{:>7}{:>8}{:>8}{:>8}",
        "case", "find", "issue", "prec", "recall", "tokens"
    );
    println!("{bar}");

    for case in &corpus {
        let out = run_review(
            &cfg,
            RunReviewInput {
                provider: case.provider.clone(),
                repo: case.repo.clone(),
                pr: case.pr,
                dry_run: true,
                placeholder: false,
            },
        )
        .await;
        let out = match out {
            Ok(o) => o,
            Err(e) => {
                println!("{:<34}  ERROR: {}", format!("{}#{}", case.repo, case.pr), e);
                continue;
            }
        };

        let f = &out.findings_detail;
        let matched_findings = f.iter().filter(|x| case.issues.iter().any(|i| hits(x, i))).count();
        let matched_issues = case.issues.iter().filter(|i| f.iter().any(|x| hits(x, i))).count();
        let toks = out.usage.and_then(|u| u.total_tokens).map(u64::from).unwrap_or(0);

        let prec = if f.is_empty() { f64::NAN } else { matched_findings as f64 / f.len() as f64 };
        let rec = if case.issues.is_empty() { f64::NAN } else { matched_issues as f64 / case.issues.len() as f64 };
        println!(
            "{:<34}{:>7}{:>7}{:>8}{:>8}{:>8}",
            format!("{}#{}", case.repo, case.pr),
            f.len(),
            case.issues.len(),
            fmt(prec),
            fmt(rec),
            toks
        );

        tp_find += matched_findings;
        n_find += f.len();
        tp_issue += matched_issues;
        n_issue += case.issues.len();
        tokens += toks;
    }

    println!("{bar}");
    let precision = if n_find == 0 { 0.0 } else { tp_find as f64 / n_find as f64 };
    let recall = if n_issue == 0 { 0.0 } else { tp_issue as f64 / n_issue as f64 };
    println!(
        "TOTAL  precision {:.2}  recall {:.2}  F1 {:.2}  ·  {} findings, {} known issues  ·  {} tokens",
        precision,
        recall,
        f1(precision, recall),
        n_find,
        n_issue,
        tokens
    );
    println!(
        "\n(precision = findings that hit a known issue; recall = known issues a finding hit; ±{} line tol.)",
        TOLERANCE
    );
    println!("Re-run with a flag toggled (e.g. BLAST_RADIUS=false) to A/B its effect on these numbers.");
}

fn fmt(v: f64) -> String {
    if v.is_nan() {
        "  —".to_string()
    } else {
        format!("{v:.2}")
    }
}
