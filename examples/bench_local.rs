//! Local benchmark harness: score the reviewer against a corpus of **raw diffs**
//! with known ground-truth issue lines — no GitHub, no live PRs. Reviews each diff
//! with [`llm::review_diff`] and reports precision / recall / F1 (overall and
//! per-language), with a replicates mode for mean ± spread.
//!
//! The point (vs `bench.rs`, which fetches live PRs): a large corpus can be built
//! automatically from public **bug-fix datasets** — a fix commit *is* a ground-truth
//! annotation. Reverse the gold fix patch to get a "bug-introducing" diff, take the
//! patched lines as the known issues, and you have hundreds of real, maintainer-
//! validated bugs with zero hand-annotation. See `scripts/swe_to_corpus.py`.
//!
//! Usage:
//!   cargo run --example bench_local -- <corpus.json> [runs]
//!
//! Requires the review env (OPENROUTER_API_KEY, model). To A/B a feature, run twice
//! with the flag toggled (e.g. `SELF_CRITIQUE=false cargo run --example bench_local -- c.json 5`).
//!
//! ## Corpus format (JSON array)
//! ```json
//! [ { "id": "astropy__astropy-12907", "lang": "python", "repo": "astropy/astropy",
//!     "diff": "<unified diff — the change under review>",
//!     "issues": [ { "file": "astropy/modeling/separable.py", "line": 245 } ] } ]
//! ```
//! A line-anchored issue matches a finding in the same file within ±`TOLERANCE`
//! lines. **Omit `line`** for a *file-level* issue — one with no single anchor line;
//! it matches a same-file summary finding. Anchored and file-level never cross-match.
//! Ground truth from fix commits is noisier than hand-annotation (a fixed line isn't
//! always the *only* place a reviewer should flag) — trust the aggregate, not one case.

use std::collections::BTreeMap;

use pr_review_core::config::Config;
use pr_review_core::llm::{review_diff, Finding};
use pr_review_core::providers::PrMeta;
use serde::Deserialize;

/// How many lines off a finding may be and still count as hitting an issue.
const TOLERANCE: i64 = 3;

#[derive(Deserialize)]
struct Case {
    #[serde(default)]
    id: String,
    /// Language label for the per-language breakdown (free-form, e.g. "rust").
    #[serde(default)]
    lang: String,
    /// `owner/repo` the diff came from — only used to label the synthetic PrMeta.
    #[serde(default)]
    repo: String,
    /// The unified diff under review.
    diff: String,
    #[serde(default)]
    issues: Vec<Issue>,
}

#[derive(Deserialize)]
struct Issue {
    file: String,
    /// Anchor line, or `null`/omitted for a **file-level** issue (a defect with no
    /// single line) — matches any finding in the same file, incl. summary findings.
    #[serde(default)]
    line: Option<u64>,
}

/// A finding hits an issue: same file, within TOLERANCE lines.
fn hits(f: &Finding, i: &Issue) -> bool {
    f.file == i.file
        && match (f.line, i.line) {
            (Some(fl), Some(il)) => (fl as i64 - il as i64).abs() <= TOLERANCE,
            // Both file-level: a summary finding ↔ a file-level issue, same file.
            (None, None) => true,
            // Never cross-match an anchored finding/issue with an unanchored one — one
            // vague finding would otherwise credit every line-anchored issue in the file.
            _ => false,
        }
}

/// Running tallies for a precision/recall computation (one bucket).
#[derive(Default, Clone)]
struct Tally {
    tp_find: usize,
    n_find: usize,
    tp_issue: usize,
    n_issue: usize,
}

impl Tally {
    fn add(&mut self, findings: &[Finding], issues: &[Issue]) {
        self.tp_find += findings
            .iter()
            .filter(|x| issues.iter().any(|i| hits(x, i)))
            .count();
        self.n_find += findings.len();
        self.tp_issue += issues
            .iter()
            .filter(|i| findings.iter().any(|x| hits(x, i)))
            .count();
        self.n_issue += issues.len();
    }
    fn precision(&self) -> f64 {
        if self.n_find == 0 {
            0.0
        } else {
            self.tp_find as f64 / self.n_find as f64
        }
    }
    fn recall(&self) -> f64 {
        if self.n_issue == 0 {
            0.0
        } else {
            self.tp_issue as f64 / self.n_issue as f64
        }
    }
}

struct RunAgg {
    overall: Tally,
    per_lang: BTreeMap<String, Tally>,
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

/// A synthetic PrMeta so the prompt has a repo/title header without any GitHub call.
fn synthetic_meta(case: &Case) -> PrMeta {
    PrMeta {
        repo: if case.repo.is_empty() {
            "local/bench".to_string()
        } else {
            case.repo.clone()
        },
        pr: 0,
        title: (!case.id.is_empty()).then(|| case.id.clone()),
        base_branch: None,
        head_sha: None,
        body: None,
        ci_status: None,
    }
}

/// One pass over the corpus: review each diff, score findings vs ground truth.
async fn run_once(cfg: &Config, client: &reqwest::Client, corpus: &[Case]) -> RunAgg {
    let mut overall = Tally::default();
    let mut per_lang: BTreeMap<String, Tally> = BTreeMap::new();
    let (mut tokens, mut errors) = (0u64, 0usize);

    for case in corpus {
        let meta = synthetic_meta(case);
        let res = review_diff(client, cfg, &meta, &case.diff, None, None).await;
        let res = match res {
            Ok(r) => r,
            Err(e) => {
                eprintln!(
                    "  ! {}: {e}",
                    if case.id.is_empty() {
                        &case.repo
                    } else {
                        &case.id
                    }
                );
                errors += 1;
                continue;
            }
        };
        let findings = &res.review.findings;
        overall.add(findings, &case.issues);
        let lang = if case.lang.is_empty() {
            "unknown"
        } else {
            &case.lang
        };
        // `BENCH_SHOW_FINDINGS=1` prints what was actually said — the same flag the
        // other two harnesses carry. A recall of 1.00 only says a finding landed on
        // the right line; whether it made the right *claim* is in the text.
        if std::env::var("BENCH_SHOW_FINDINGS").is_ok_and(|v| v == "1") {
            for x in findings {
                let anchor = x
                    .line
                    .map_or_else(|| "summary".to_string(), |l| l.to_string());
                let body: String = x.body.chars().take(300).collect();
                eprintln!("      [{}] {}:{anchor} — {body}", x.severity, x.file);
            }
        }
        per_lang
            .entry(lang.to_string())
            .or_default()
            .add(findings, &case.issues);
        tokens += res
            .usage
            .and_then(|u| u.total_tokens)
            .map(u64::from)
            .unwrap_or(0);
    }

    RunAgg {
        overall,
        per_lang,
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
            eprintln!("usage: cargo run --example bench_local -- <corpus.json> [runs]");
            std::process::exit(2);
        }
    };
    let runs: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(1).max(1);

    let corpus: Vec<Case> = serde_json::from_str(
        &std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}")),
    )
    .unwrap_or_else(|e| panic!("parse {path}: {e}"));

    let cfg = Config::from_env();
    let client = reqwest::Client::new();
    let total_issues: usize = corpus.iter().map(|c| c.issues.len()).sum();
    let mut langs: BTreeMap<&str, usize> = BTreeMap::new();
    for c in &corpus {
        *langs
            .entry(if c.lang.is_empty() {
                "unknown"
            } else {
                &c.lang
            })
            .or_default() += 1;
    }
    let lang_summary: Vec<String> = langs.iter().map(|(l, n)| format!("{l}:{n}")).collect();
    eprintln!(
        "bench_local: {} diff(s) [{}], {} known issue(s), {} run(s) · model={} · self_critique={}\n",
        corpus.len(),
        lang_summary.join(" "),
        total_issues,
        runs,
        cfg.openrouter_model,
        cfg.self_critique,
    );

    let bar = "─".repeat(72);
    println!("{bar}");
    println!(
        "{:<10}{:>12}{:>12}{:>10}{:>12}",
        "run", "precision", "recall", "F1", "tokens"
    );
    println!("{bar}");

    let mut aggs: Vec<RunAgg> = Vec::with_capacity(runs);
    for i in 1..=runs {
        let a = run_once(&cfg, &client, &corpus).await;
        let (p, r) = (a.overall.precision(), a.overall.recall());
        println!(
            "{:<10}{:>12.2}{:>12.2}{:>10.2}{:>12}{}",
            i,
            p,
            r,
            f1(p, r),
            a.tokens,
            if a.errors > 0 {
                format!("  ({} err)", a.errors)
            } else {
                String::new()
            }
        );
        aggs.push(a);
    }
    println!("{bar}");

    let ps: Vec<f64> = aggs.iter().map(|a| a.overall.precision()).collect();
    let rs: Vec<f64> = aggs.iter().map(|a| a.overall.recall()).collect();
    let fs: Vec<f64> = ps.iter().zip(&rs).map(|(&p, &r)| f1(p, r)).collect();
    let ts: Vec<f64> = aggs.iter().map(|a| a.tokens as f64).collect();

    if runs == 1 {
        println!(
            "RESULT  precision {:.2}  recall {:.2}  F1 {:.2}  ·  {} tokens",
            ps[0], rs[0], fs[0], aggs[0].tokens
        );
    } else {
        println!(
            "mean    precision {:.2} ±{:.2}   recall {:.2} ±{:.2}   F1 {:.2} ±{:.2}   ·  {:.0} ±{:.0} tokens",
            mean(&ps), stddev(&ps), mean(&rs), stddev(&rs), mean(&fs), stddev(&fs), mean(&ts), stddev(&ts)
        );
        let (rlo, rhi) = rs
            .iter()
            .fold((f64::MAX, f64::MIN), |(lo, hi), &x| (lo.min(x), hi.max(x)));
        println!("range   recall {rlo:.2}–{rhi:.2}");
        if rhi - rlo >= 0.01 {
            println!(
                "        ⚠ recall spans {:.2} across runs — treat small mean gaps as noise.",
                rhi - rlo
            );
        }
    }

    // Per-language breakdown (averaged over runs by summing tallies).
    let mut lang_tallies: BTreeMap<String, Tally> = BTreeMap::new();
    for a in &aggs {
        for (l, t) in &a.per_lang {
            let e = lang_tallies.entry(l.clone()).or_default();
            e.tp_find += t.tp_find;
            e.n_find += t.n_find;
            e.tp_issue += t.tp_issue;
            e.n_issue += t.n_issue;
        }
    }
    if lang_tallies.len() > 1 {
        println!("\nby language (pooled over runs):");
        println!(
            "{:<12}{:>12}{:>12}{:>10}{:>10}",
            "lang", "precision", "recall", "F1", "issues"
        );
        for (l, t) in &lang_tallies {
            let (p, r) = (t.precision(), t.recall());
            println!(
                "{:<12}{:>12.2}{:>12.2}{:>10.2}{:>10}",
                l,
                p,
                r,
                f1(p, r),
                t.n_issue
            );
        }
    }

    println!(
        "\n(precision = findings that hit a known issue; recall = known issues a finding hit; ±{TOLERANCE} line tol.)"
    );
    if runs == 1 {
        println!("Pass a run count for replicates + mean ± spread, e.g. `-- corpus.json 5`.");
    }
}
