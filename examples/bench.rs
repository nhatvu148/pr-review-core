//! Benchmark harness: score the reviewer against a corpus of PRs with **known**
//! issues, and report precision / recall / F1 + token cost. Method borrowed from
//! alibaba/open-code-review's evaluation approach.
//!
//! Usage:
//!   cargo run --example bench -- corpus.json [runs]
//!
//! `runs` (default 1) does N replicates and reports **mean ± spread** — essential
//! because the agentic reviewer is non-deterministic, so a single run's numbers
//! are noise. Requires the same env as a real review (OPENROUTER_API_KEY, a
//! provider token). Runs each PR in **dry-run** (nothing is posted). To A/B a
//! feature, run twice with the env toggled:
//!   BLAST_RADIUS=false cargo run --example bench -- corpus.json 5
//!   BLAST_RADIUS=true  cargo run --example bench -- corpus.json 5
//!
//! ## Corpus format (JSON array)
//! ```json
//! [ { "provider": "github", "repo": "owner/app", "pr": 42,
//!     "issues": [ { "file": "src/a.ts", "line": 88, "type": "bug", "note": "N+1" } ] } ]
//! ```
//! A finding matches an issue when it's in the same file within ±`TOLERANCE` lines.
//! Proximity scoring is a signal, not a proof — keep the corpus honest.

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
    /// Anchor line, or `null`/omitted for a **file-level** issue (a defect with no
    /// single line, e.g. "this page is unreachable") — which matches any finding in
    /// the same file, including the summary findings the reviewer emits for exactly
    /// these cases.
    #[serde(default)]
    line: Option<u64>,
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
        && match (f.line, i.line) {
            // Both anchored: within tolerance.
            (Some(fl), Some(il)) => (fl as i64 - il as i64).abs() <= TOLERANCE,
            // Either side is file-level (a summary finding, or a file-level issue):
            // same file is enough. Coarser, so keep line numbers where a defect has one.
            _ => true,
        }
}

/// Aggregate scores for one full pass over the corpus.
struct RunAgg {
    precision: f64,
    recall: f64,
    f1: f64,
    tokens: u64,
    errors: usize,
}

fn f1(p: f64, r: f64) -> f64 {
    if p + r == 0.0 {
        0.0
    } else {
        2.0 * p * r / (p + r)
    }
}

fn mean(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        0.0
    } else {
        xs.iter().sum::<f64>() / xs.len() as f64
    }
}

fn stddev(xs: &[f64]) -> f64 {
    if xs.len() < 2 {
        return 0.0;
    }
    let m = mean(xs);
    (xs.iter().map(|x| (x - m).powi(2)).sum::<f64>() / (xs.len() - 1) as f64).sqrt()
}

/// One pass over the corpus: run each PR, score findings vs ground truth.
async fn run_once(cfg: &Config, corpus: &[Case]) -> RunAgg {
    let (mut tp_find, mut n_find, mut tp_issue, mut n_issue, mut tokens, mut errors) =
        (0usize, 0usize, 0usize, 0usize, 0u64, 0usize);
    for case in corpus {
        let out = run_review(
            cfg,
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
                eprintln!("  ! {}#{}: {e}", case.repo, case.pr);
                errors += 1;
                continue;
            }
        };
        let f = &out.findings_detail;
        tp_find += f.iter().filter(|x| case.issues.iter().any(|i| hits(x, i))).count();
        n_find += f.len();
        tp_issue += case.issues.iter().filter(|i| f.iter().any(|x| hits(x, i))).count();
        n_issue += case.issues.len();
        tokens += out.usage.and_then(|u| u.total_tokens).map(u64::from).unwrap_or(0);
    }
    let precision = if n_find == 0 { 0.0 } else { tp_find as f64 / n_find as f64 };
    let recall = if n_issue == 0 { 0.0 } else { tp_issue as f64 / n_issue as f64 };
    RunAgg {
        precision,
        recall,
        f1: f1(precision, recall),
        tokens,
        errors,
    }
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let path = match args.next() {
        Some(p) => p,
        None => {
            eprintln!("usage: cargo run --example bench -- <corpus.json> [runs]");
            std::process::exit(2);
        }
    };
    let runs: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(1).max(1);

    let corpus: Vec<Case> = serde_json::from_str(
        &std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}")),
    )
    .unwrap_or_else(|e| panic!("parse {path}: {e}"));

    let cfg = Config::from_env();
    let total_issues: usize = corpus.iter().map(|c| c.issues.len()).sum();
    eprintln!(
        "bench: {} case(s), {} known issue(s), {} run(s) · blast={} · complexity={} · self_critique={} · agentic={}\n",
        corpus.len(),
        total_issues,
        runs,
        cfg.blast_radius,
        cfg.complexity_metrics,
        cfg.self_critique,
        cfg.agentic
    );

    let bar = "─".repeat(72);
    println!("{bar}");
    println!("{:<10}{:>12}{:>12}{:>10}{:>12}", "run", "precision", "recall", "F1", "tokens");
    println!("{bar}");

    let mut aggs: Vec<RunAgg> = Vec::with_capacity(runs);
    for i in 1..=runs {
        let a = run_once(&cfg, &corpus).await;
        println!(
            "{:<10}{:>12.2}{:>12.2}{:>10.2}{:>12}{}",
            i,
            a.precision,
            a.recall,
            a.f1,
            a.tokens,
            if a.errors > 0 { format!("  ({} err)", a.errors) } else { String::new() }
        );
        aggs.push(a);
    }
    println!("{bar}");

    let ps: Vec<f64> = aggs.iter().map(|a| a.precision).collect();
    let rs: Vec<f64> = aggs.iter().map(|a| a.recall).collect();
    let fs: Vec<f64> = aggs.iter().map(|a| a.f1).collect();
    let ts: Vec<f64> = aggs.iter().map(|a| a.tokens as f64).collect();
    let rng = |xs: &[f64]| {
        let (lo, hi) = xs
            .iter()
            .fold((f64::MAX, f64::MIN), |(lo, hi), &x| (lo.min(x), hi.max(x)));
        (lo, hi)
    };

    if runs == 1 {
        println!(
            "RESULT  precision {:.2}  recall {:.2}  F1 {:.2}  ·  {} tokens",
            ps[0], rs[0], fs[0], aggs[0].tokens as u64
        );
    } else {
        println!(
            "mean    precision {:.2} ±{:.2}   recall {:.2} ±{:.2}   F1 {:.2} ±{:.2}   ·  {:.0} ±{:.0} tokens",
            mean(&ps), stddev(&ps), mean(&rs), stddev(&rs), mean(&fs), stddev(&fs), mean(&ts), stddev(&ts)
        );
        let (rlo, rhi) = rng(&rs);
        let (plo, phi) = rng(&ps);
        println!("range   precision {plo:.2}–{phi:.2}   recall {rlo:.2}–{rhi:.2}");
        if rhi - rlo >= 0.01 {
            println!("        ⚠ recall spans {:.2} across runs — treat small mean gaps as noise.", rhi - rlo);
        }
    }
    println!(
        "\n(precision = findings that hit a known issue; recall = known issues a finding hit; ±{} line tol.)",
        TOLERANCE
    );
    if runs > 1 {
        println!("A/B a feature by re-running with a flag toggled (e.g. BLAST_RADIUS=false).");
    } else {
        println!("Pass a run count for replicates + mean ± spread, e.g. `-- corpus.json 5`.");
    }
}
