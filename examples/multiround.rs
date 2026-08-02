//! **Phase A of the convergence experiment.** Does re-reviewing the same diff add
//! recall, or only add diff?
//!
//! This is the gate on whether vexar's convergence loop is multi-round at all. It
//! is deliberately narrow: **no fixes are applied between passes.** Each pass sees
//! the identical diff with a fresh context, so the only thing being measured is
//! whether a second and third look find planted bugs the first missed — isolated
//! from the confound of whether the fixes in between were any good.
//!
//! Usage:
//!   cargo run --example multiround -- <corpus.json> [rounds] [replicates]
//!
//! Defaults to 3 rounds × 5 replicates, per the experiment design. Replicates are
//! not optional decoration: the reviewer is non-deterministic, `bench.rs`'s own
//! docs say "a single run's numbers are noise", and the kill criterion is stated
//! *in units of that noise*. One replicate cannot evaluate it.
//!
//! ## What is measured
//!
//! - **Cumulative unique recall** — the planted bugs hit by the union of rounds
//!   1..k. Union, not per-round, because that is what a real loop accumulates.
//! - **False positives contributed per added round** — unique findings that hit no
//!   planted bug, deduped across rounds the same way, so a round that merely
//!   repeats round 1's noise is not charged for it twice.
//!
//! ## The kill criterion, fixed before the run
//!
//! 1. If union-of-3 recall exceeds round-1 recall by **less than the replicate
//!    spread**, extra rounds are indistinguishable from re-rolling the dice → CUT.
//! 2. If added rounds contribute FPs faster than they contribute caught bugs →
//!    CUT, regardless of recall.
//!
//! Either way most of Phase 5 survives; only the "convergence" story dies, and the
//! honest move is then to correct the story rather than oversell the feature.

use std::collections::{BTreeMap, BTreeSet};

use pr_review_core::config::Config;
use pr_review_core::llm::Finding;
use pr_review_core::review::{run_review, RunReviewInput};
use serde::Deserialize;

/// How many lines off a finding may be and still count as hitting an issue.
/// Same tolerance as `bench.rs`, so the numbers are comparable.
const TOLERANCE: i64 = 3;

/// How many cases review concurrently. Small on purpose — see the batch loop.
const CONCURRENT_CASES: usize = 4;

/// Corpora come in two shapes and this harness accepts both.
///
/// A **PR corpus** (`bench-corpus.json`) names live pull requests and needs a
/// provider token; a **local corpus** (`corpus-swe-*.json`) carries the diff
/// inline and needs nothing but the model. The local shape is what gives this
/// experiment its statistical power — hundreds of maintainer-validated bugs
/// rather than a dozen hand-annotated ones — and the kill criterion is stated in
/// units of replicate spread, which a handful of planted bugs cannot resolve.
#[derive(Deserialize, Clone)]
struct Case {
    #[serde(default)]
    provider: String,
    #[serde(default)]
    repo: String,
    #[serde(default)]
    pr: u64,
    #[serde(default)]
    id: String,
    /// Present on a local corpus: the diff under review.
    #[serde(default)]
    diff: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    lang: String,
    #[serde(default)]
    issues: Vec<Issue>,
}

impl Case {
    fn label(&self) -> String {
        if !self.id.is_empty() {
            return self.id.clone();
        }
        if self.diff.is_some() {
            return if self.repo.is_empty() {
                "local".to_string()
            } else {
                self.repo.clone()
            };
        }
        format!("{}#{}", self.repo, self.pr)
    }

    fn is_local(&self) -> bool {
        self.diff.is_some()
    }

    /// A synthetic `PrMeta` so the prompt has a header without any provider call.
    fn synthetic_meta(&self) -> pr_review_core::providers::PrMeta {
        pr_review_core::providers::PrMeta {
            repo: if self.repo.is_empty() {
                "local/bench".to_string()
            } else {
                self.repo.clone()
            },
            pr: 0,
            title: (!self.id.is_empty()).then(|| self.id.clone()),
            base_branch: None,
            head_sha: None,
            body: None,
            ci_status: None,
        }
    }
}

#[derive(Deserialize, Clone)]
struct Issue {
    file: String,
    #[serde(default)]
    line: Option<u64>,
    #[serde(default)]
    #[allow(dead_code)]
    r#type: String,
    #[serde(default)]
    #[allow(dead_code)]
    note: String,
}

/// A finding hits an issue: same file, within TOLERANCE lines. Anchored and
/// file-level never cross-match — one vague summary finding must not credit every
/// line-anchored issue in the file.
fn hits(f: &Finding, i: &Issue) -> bool {
    f.file == i.file
        && match (f.line, i.line) {
            (Some(fl), Some(il)) => (fl as i64 - il as i64).abs() <= TOLERANCE,
            (None, None) => true,
            _ => false,
        }
}

/// Unique findings across rounds, deduped by the **same rule** [`hits`] uses.
///
/// This started as fixed-size bucketing (`line / (TOLERANCE + 1)`) whose comment
/// claimed "the same tolerance used for matching" — and did not deliver it.
/// Bucketing splits two findings one line apart when they straddle a boundary
/// (4 and 5) and merges two further apart inside one bucket (4 and 7). Those two
/// errors run in opposite directions and both land on the unique-FP count that
/// kill criterion 2 is computed from, so the approximation sat in exactly the
/// wrong place.
///
/// A sliding window has no closed-form key, so this keeps the seen lines per file
/// and scans. Deduping is greedy — the first finding of a cluster claims the
/// window — which is order-dependent in principle but reproduces `hits()` for the
/// question actually being asked: *is this the same defect I already counted?*
#[derive(Default)]
struct UniqueFindings {
    /// file → lines of findings already counted.
    anchored: BTreeMap<String, Vec<u64>>,
    /// Files with a summary-level (line-less) finding. Anchored and summary
    /// findings never merge, matching `hits`'s refusal to cross-match them.
    summary: BTreeSet<String>,
}

impl UniqueFindings {
    /// Record a finding. Returns `true` when it was **new**.
    fn insert(&mut self, f: &Finding) -> bool {
        match f.line {
            None => self.summary.insert(f.file.clone()),
            Some(line) => {
                let seen = self.anchored.entry(f.file.clone()).or_default();
                if seen
                    .iter()
                    .any(|&s| (s as i64 - line as i64).abs() <= TOLERANCE)
                {
                    return false;
                }
                seen.push(line);
                true
            }
        }
    }

    fn len(&self) -> usize {
        self.anchored.values().map(Vec::len).sum::<usize>() + self.summary.len()
    }
}

/// One case, one replicate: the cumulative picture after each round.
struct CaseRun {
    /// `caught[k]` = planted bugs hit by the union of rounds 0..=k.
    caught: Vec<usize>,
    /// `fps[k]` = unique non-hitting findings across the union of rounds 0..=k.
    fps: Vec<usize>,
    issues: usize,
    errored: bool,
}

/// Run one case `rounds` times and accumulate unions.
async fn run_case(cfg: &Config, client: &reqwest::Client, case: &Case, rounds: usize) -> CaseRun {
    let mut hit_issues: BTreeSet<usize> = BTreeSet::new();
    let mut seen_fps = UniqueFindings::default();
    let mut caught = Vec::with_capacity(rounds);
    let mut fps = Vec::with_capacity(rounds);
    let mut errored = false;

    for round in 1..=rounds {
        // Each pass is fully independent: a fresh call over the identical diff.
        // No state carries between rounds, which is the point — this measures
        // re-review, not memory.
        let out: anyhow::Result<Vec<Finding>> = match &case.diff {
            Some(diff) => pr_review_core::llm::review_diff(
                client,
                cfg,
                &case.synthetic_meta(),
                diff,
                None,
                None,
                &pr_review_core::prompt::review_system_prompt(cfg),
            )
            .await
            .map(|r| r.review.findings),
            None => run_review(
                cfg,
                RunReviewInput {
                    provider: case.provider.clone(),
                    repo: case.repo.clone(),
                    pr: case.pr,
                    dry_run: true,
                    placeholder: false,
                },
            )
            .await
            .map(|o| o.findings_detail),
        };

        match out {
            Ok(findings) => {
                for f in &findings {
                    let mut hit_any = false;
                    for (idx, issue) in case.issues.iter().enumerate() {
                        if hits(f, issue) {
                            hit_issues.insert(idx);
                            hit_any = true;
                        }
                    }
                    if !hit_any {
                        seen_fps.insert(f);
                    }
                }
                eprintln!(
                    "      round {round}: {} finding(s) · cumulative {}/{} bug(s), {} unique FP(s)",
                    findings.len(),
                    hit_issues.len(),
                    case.issues.len(),
                    seen_fps.len()
                );
            }
            Err(e) => {
                errored = true;
                eprintln!("      round {round}: ERROR {e}");
            }
        }
        // Record the cumulative state after every round, including a failed one,
        // so the vectors stay aligned across cases and replicates.
        caught.push(hit_issues.len());
        fps.push(seen_fps.len());
    }

    CaseRun {
        caught,
        fps,
        issues: case.issues.len(),
        errored,
    }
}

/// Aggregate over one replicate: recall after each cumulative round.
struct ReplicateAgg {
    /// Recall of the union of rounds 0..=k, pooled over the whole corpus.
    recall: Vec<f64>,
    /// Unique FPs across the corpus for the union of rounds 0..=k.
    fps: Vec<usize>,
    /// Planted bugs caught across the corpus for the union of rounds 0..=k.
    caught: Vec<usize>,
    errors: usize,
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

fn range(xs: &[f64]) -> (f64, f64) {
    xs.iter()
        .fold((f64::MAX, f64::MIN), |(lo, hi), &x| (lo.min(x), hi.max(x)))
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let path = match args.next() {
        Some(p) => p,
        None => {
            eprintln!(
                "usage: cargo run --example multiround -- <corpus.json> [rounds] [replicates]"
            );
            std::process::exit(2);
        }
    };
    let rounds: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(3).max(1);
    let replicates: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(5).max(1);

    let corpus: Vec<Case> = serde_json::from_str(
        &std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}")),
    )
    .unwrap_or_else(|e| panic!("parse {path}: {e}"));

    let cfg = Config::from_env();
    let client = reqwest::Client::new();
    let local = corpus.first().is_some_and(|c| c.is_local());
    let total_issues: usize = corpus.iter().map(|c| c.issues.len()).sum();
    let total_reviews = corpus.len() * rounds * replicates;

    eprintln!(
        "multiround: {} case(s), {} planted bug(s) · {} round(s) × {} replicate(s) = {} reviews\n\
         mode={} · model={} · agentic={} · self_critique={}\n\
         No fixes are applied between rounds — this measures re-review alone.\n",
        corpus.len(),
        total_issues,
        rounds,
        replicates,
        total_reviews,
        if local {
            "local diffs (no provider)"
        } else {
            "live PRs"
        },
        cfg.openrouter_model,
        cfg.agentic,
        cfg.self_critique,
    );

    let mut per_case: BTreeMap<String, Vec<CaseRun>> = BTreeMap::new();
    let mut aggs: Vec<ReplicateAgg> = Vec::with_capacity(replicates);

    for rep in 1..=replicates {
        eprintln!("── replicate {rep}/{replicates} ──");
        let mut caught_total = vec![0usize; rounds];
        let mut fps_total = vec![0usize; rounds];
        let mut issues_total = 0usize;
        let mut errors = 0usize;

        // Cases are independent HTTP calls, so they run in batches rather than
        // one at a time — 600 sequential reviews is ~5 hours of wall clock for an
        // experiment that is entirely I/O-bound. The batch is deliberately small:
        // the point is to stop waiting on latency, not to see how hard the
        // provider can be pushed, and a 429 storm would cost more rows than the
        // concurrency saves.
        let mut ordered: Vec<(String, CaseRun)> = Vec::with_capacity(corpus.len());
        for chunk in corpus.chunks(CONCURRENT_CASES) {
            let mut set = tokio::task::JoinSet::new();
            for case in chunk {
                let (cfg, client, case) = (cfg.clone(), client.clone(), case.clone());
                set.spawn(async move {
                    let run = run_case(&cfg, &client, &case, rounds).await;
                    (case.label(), run)
                });
            }
            while let Some(joined) = set.join_next().await {
                match joined {
                    Ok(pair) => ordered.push(pair),
                    // A panicked task is a lost case-replicate exactly like an
                    // errored round, and has to be counted as one. Dropping it
                    // silently would exempt panics from the 10% exclusion ceiling
                    // and from the loss report this file promises — the same
                    // shape of hole the other three guards were added to close.
                    Err(e) => {
                        eprintln!("      task panicked, excluded: {e}");
                        errors += 1;
                    }
                }
            }
        }
        // Sort so the report order is stable regardless of completion order.
        ordered.sort_by(|a, b| a.0.cmp(&b.0));

        for (label, run) in ordered {
            let _ = &label;
            // A case-replicate with a failed round is EXCLUDED from the aggregate,
            // not counted as zero.
            //
            // Across 600 reviews a few transient failures are certain — a model
            // emitting an invalid JSON escape, a rate limit. Counting those rounds
            // as "found nothing" would understate the cumulative union and bias the
            // result toward CUT, which is the direction that kills the feature. And
            // a union-of-3 missing one of its three rounds is not a union-of-3.
            // Dropping the whole case-replicate keeps every surviving row a
            // like-for-like comparison; the number dropped is reported so the loss
            // stays visible.
            if run.errored {
                errors += 1;
                eprintln!("      \u{21b3} excluded (a round failed)");
                // Excluded from the per-case table as well as the aggregate.
                // Keeping it here would average a truncated run in beside complete
                // ones, and the per-case column is exactly what a reader uses to
                // judge WHERE the gains are — the density observation is read off
                // it. Same exclusion, same reason, both places.
                continue;
            }
            issues_total += run.issues;
            for k in 0..rounds {
                caught_total[k] += run.caught[k];
                fps_total[k] += run.fps[k];
            }
            per_case.entry(label).or_default().push(run);
        }

        let recall: Vec<f64> = caught_total
            .iter()
            .map(|c| {
                if issues_total == 0 {
                    0.0
                } else {
                    *c as f64 / issues_total as f64
                }
            })
            .collect();
        eprintln!(
            "  → recall by cumulative round: {}\n",
            recall
                .iter()
                .enumerate()
                .map(|(i, r)| format!("r1..{}={r:.2}", i + 1))
                .collect::<Vec<_>>()
                .join("  ")
        );
        aggs.push(ReplicateAgg {
            recall,
            fps: fps_total,
            caught: caught_total,
            errors,
        });
    }

    // ---- report ----------------------------------------------------------
    let bar = "─".repeat(76);
    println!("{bar}");
    println!(
        "{:<16}{:>12}{:>10}{:>14}{:>12}",
        "cumulative", "recall mean", "±sd", "range", "unique FPs"
    );
    println!("{bar}");

    let mut recall_by_round: Vec<Vec<f64>> = vec![Vec::new(); rounds];
    for a in &aggs {
        for (k, r) in a.recall.iter().enumerate() {
            recall_by_round[k].push(*r);
        }
    }
    let fp_mean: Vec<f64> = (0..rounds)
        .map(|k| mean(&aggs.iter().map(|a| a.fps[k] as f64).collect::<Vec<_>>()))
        .collect();
    let caught_mean: Vec<f64> = (0..rounds)
        .map(|k| mean(&aggs.iter().map(|a| a.caught[k] as f64).collect::<Vec<_>>()))
        .collect();

    for k in 0..rounds {
        let (lo, hi) = range(&recall_by_round[k]);
        println!(
            "{:<16}{:>12.3}{:>10.3}{:>14}{:>12.1}",
            format!("rounds 1..{}", k + 1),
            mean(&recall_by_round[k]),
            stddev(&recall_by_round[k]),
            format!("{lo:.3}–{hi:.3}"),
            fp_mean[k],
        );
    }
    println!("{bar}");

    // ---- the kill criterion ----------------------------------------------
    let r1 = mean(&recall_by_round[0]);
    let rn = mean(&recall_by_round[rounds - 1]);
    let gain = rn - r1;
    let (lo1, hi1) = range(&recall_by_round[0]);
    let spread = hi1 - lo1;

    let errors: usize = aggs.iter().map(|a| a.errors).sum();

    // A verdict requires data. Without this guard an all-zero run prints
    // "SURVIVES", because a gain of 0.000 is not less than a spread of 0.000 —
    // a harness confidently answering a question it never got to ask.
    //
    // But "any error at all" is the wrong bar for a 600-review run, where a few
    // transient failures are certain. Errored case-replicates are already excluded
    // from the aggregate above; what matters here is whether ENOUGH survived. The
    // threshold is deliberately explicit rather than a silent best-effort: a run
    // that lost a fifth of its rows is not the experiment that was designed.
    let attempted = corpus.len() * replicates;
    let excluded_frac = errors as f64 / attempted.max(1) as f64;
    const MAX_EXCLUDED: f64 = 0.10;

    let no_data = total_issues == 0 || caught_mean[rounds - 1] == 0.0;
    let too_lossy = excluded_frac > MAX_EXCLUDED;
    if no_data || too_lossy {
        println!("\n{bar}");
        println!("NO VERDICT — the run did not produce evaluable data.");
        if too_lossy {
            println!(
                "  · {errors}/{attempted} case-replicate(s) ({:.0}%) were excluded after a",
                excluded_frac * 100.0
            );
            println!(
                "    failed round — above the {:.0}% ceiling for a readable result.",
                MAX_EXCLUDED * 100.0
            );
        }
        if total_issues == 0 {
            println!("  · no planted bugs survived exclusion, so recall is undefined.");
        } else if caught_mean[rounds - 1] == 0.0 {
            println!("  · no planted bug was caught in any round, by any replicate.");
            println!("    That is a finding about the corpus or the config, not about rounds.");
        }
        println!("{bar}");
        println!("\nFix the above and re-run. Do not read the table as a result.");
        std::process::exit(1);
    }
    if errors > 0 {
        println!(
            "\nNote: {errors}/{attempted} case-replicate(s) ({:.1}%) were excluded after a failed\n\
             round (transient model/API errors). They are dropped whole rather than counted\n\
             as zero, which would have biased the result toward CUT.",
            excluded_frac * 100.0
        );
    }

    println!("\nKILL CRITERION (fixed before the run)\n");
    println!(
        "  1. recall gain from extra rounds : {gain:+.3}  (round 1 = {r1:.3} → union of {rounds} = {rn:.3})"
    );
    println!("     replicate spread of round 1  : {spread:.3}  (range {lo1:.3}–{hi1:.3})");
    // `<=`, not `<`, and a separate zero check. With a saturated corpus both gain
    // and spread are 0.000, and `0 < 0` is false — which printed SURVIVES for a
    // run where extra rounds demonstrably added nothing. A gain that does not
    // EXCEED the noise is not evidence, and a gain of zero is not evidence of
    // anything at all.
    let recall_verdict = if gain <= spread || gain <= 0.0 {
        "CUT — the gain is inside the noise: extra rounds are indistinguishable from re-rolling."
    } else {
        "SURVIVES — the gain exceeds replicate spread."
    };
    println!("     → {recall_verdict}\n");

    let added_fps = fp_mean[rounds - 1] - fp_mean[0];
    let added_catches = caught_mean[rounds - 1] - caught_mean[0];
    println!("  2. false positives added by rounds 2..{rounds} : {added_fps:+.1}");
    println!("     planted bugs  added by rounds 2..{rounds} : {added_catches:+.1}");
    let fp_verdict = if added_fps > added_catches {
        "CUT — added rounds contribute noise faster than they contribute bugs."
    } else {
        "SURVIVES — added rounds contribute bugs at least as fast as noise."
    };
    println!("     → {fp_verdict}\n");

    let cut = gain <= spread || gain <= 0.0 || added_fps > added_catches;
    println!("{bar}");
    println!(
        "VERDICT: {}",
        if cut {
            "CUT multi-round convergence. Stage 5 ships max_rounds default = 1."
        } else {
            "Multi-round convergence SURVIVES. Stage 5 ships max_rounds default = 3."
        }
    );
    println!("{bar}");

    // ---- per-case, so one dominant case is visible ------------------------
    println!("\nper case (mean cumulative bugs caught, pooled over replicates):");
    println!("{:<40}{:>10}{:>26}", "case", "planted", "caught by round");
    for (label, runs) in &per_case {
        let issues = runs.first().map(|r| r.issues).unwrap_or(0);
        let by_round: Vec<String> = (0..rounds)
            .map(|k| {
                let m = mean(&runs.iter().map(|r| r.caught[k] as f64).collect::<Vec<_>>());
                format!("{m:.1}")
            })
            .collect();
        println!("{:<40}{:>10}{:>26}", label, issues, by_round.join(" → "));
    }
    println!(
        "\n(recall = planted bugs a finding hit, ±{TOLERANCE} line tolerance; \
         FPs deduped by file+line bucket across rounds)"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finding(file: &str, line: Option<u64>) -> Finding {
        Finding {
            severity: "LOW".to_string(),
            file: file.to_string(),
            line,
            body: "x".to_string(),
            confidence: Some(50),
        }
    }

    /// The old bucketing's failure mode, and the reason it mattered: two findings
    /// ONE line apart are the same defect, but `3/4 = 0` and `4/4 = 1` put them in
    /// different buckets and counted two. Every such split inflated the unique-FP
    /// count that kill criterion 2 is computed from.
    #[test]
    fn findings_within_tolerance_are_one_finding() {
        let mut u = UniqueFindings::default();
        assert!(u.insert(&finding("a.rs", Some(3))));
        assert!(
            !u.insert(&finding("a.rs", Some(4))),
            "1 line apart is one defect"
        );
        assert!(
            !u.insert(&finding("a.rs", Some(6))),
            "3 apart is still within tolerance"
        );
        assert_eq!(u.len(), 1);
    }

    #[test]
    fn findings_beyond_tolerance_are_distinct() {
        let mut u = UniqueFindings::default();
        assert!(u.insert(&finding("a.rs", Some(10))));
        assert!(
            u.insert(&finding("a.rs", Some(14))),
            "4 apart exceeds tolerance"
        );
        assert_eq!(u.len(), 2);
    }

    #[test]
    fn the_same_line_in_two_files_is_two_findings() {
        let mut u = UniqueFindings::default();
        assert!(u.insert(&finding("a.rs", Some(10))));
        assert!(u.insert(&finding("b.rs", Some(10))));
        assert_eq!(u.len(), 2);
    }

    /// Anchored and summary findings never merge — matching `hits`, which refuses
    /// to cross-match them so one vague finding cannot absorb a located one.
    #[test]
    fn summary_and_anchored_findings_do_not_merge() {
        let mut u = UniqueFindings::default();
        assert!(u.insert(&finding("a.rs", None)));
        assert!(u.insert(&finding("a.rs", Some(10))));
        assert!(!u.insert(&finding("a.rs", None)), "one summary per file");
        assert_eq!(u.len(), 2);
    }

    /// The direction of the old error, which is what makes it worth reporting:
    /// with TOLERANCE=3 the bucket width was 4, so two lines in one bucket were
    /// always within tolerance — bucketing could therefore only ever SPLIT a
    /// cluster, never merge one. Old unique-FP counts were an upper bound, and
    /// criterion 2 fired on `added_fps > added_catches`.
    #[test]
    fn the_old_bucketing_could_only_overcount() {
        let bucket = |l: i64| l / (TOLERANCE + 1);
        for a in 0..200i64 {
            for b in a..(a + 10).min(200) {
                let same_bucket = bucket(a) == bucket(b);
                let within_tolerance = (b - a) <= TOLERANCE;
                if same_bucket {
                    assert!(
                        within_tolerance,
                        "lines {a},{b} shared a bucket but exceed tolerance — \
                         bucketing would have merged two distinct findings"
                    );
                }
            }
        }
    }
}
