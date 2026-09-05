//! The layer above one PR: what a run log says about every PR it has seen.
//!
//! A review ends when its comment posts, and until now so did the reviewer's
//! memory of it. Every record needed to answer "which of our open PRs most needs
//! a human right now?" was already being written by [`crate::runlog`] and then
//! never read. This module reads it.
//!
//! # What it is
//!
//! A fold over run-log records: newest record per PR, ranked. Nothing here calls
//! a model, a provider, or the network — it is a pure function of records you
//! already have, which is what makes a queue trustworthy enough to act on.
//!
//! # What it is not
//!
//! **It is not a list of open PRs.** A run log knows only about PRs the reviewer
//! *ran on*. A PR nobody reviewed is invisible here, and so is one whose review
//! failed before it could log. Reconciling this against the provider's list of
//! open PRs is the caller's job, because only the caller has a provider token —
//! and that reconciliation is the difference between "the queue" and "the queue
//! of what we happen to have looked at".
//!
//! **It is not a quality measure.** [`Priority`] ranks the *reviewer's own
//! verdict*, and nothing in a record knows whether that verdict was right. A P0
//! means the reviewer said BLOCK, not that the PR is broken.
//!
//! **Staleness is only partial.** A record names the commit it reviewed
//! ([`PrStatus::head_sha`]); whether that is still the PR's head needs a provider
//! call the caller must make. What this module *can* say is that two records for
//! one PR disagree on the SHA, i.e. the PR moved after a review — see
//! [`PrStatus::superseded`].

use std::collections::HashMap;

use crate::runlog::RunLog;

/// How urgently a PR wants a human, derived from the reviewer's own verdict.
///
/// Deliberately three buckets rather than a score. A score invites comparing two
/// PRs that differ by a point, which is noise: the underlying inputs are a
/// model's severity label and its self-reported confidence, neither precise
/// enough to order a list finely. Three buckets say only what the data supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Priority {
    /// The reviewer would not merge this: a BLOCK recommendation, or a finding
    /// it labelled BLOCKING.
    P0,
    /// Something serious but not disqualifying — a HIGH finding, or an
    /// "approve with changes" verdict.
    P1,
    /// Reviewed, nothing urgent.
    P2,
}

impl Priority {
    /// Short label for a table.
    pub fn label(self) -> &'static str {
        match self {
            Priority::P0 => "P0",
            Priority::P1 => "P1",
            Priority::P2 => "P2",
        }
    }
}

/// One PR as its most recent review left it.
#[derive(Debug, Clone)]
pub struct PrStatus {
    pub provider: String,
    pub repo: String,
    pub pr: u64,
    /// The commit that review actually read.
    pub head_sha: Option<String>,
    /// When that review ran (seconds since the Unix epoch).
    pub ts_unix: u64,
    pub recommendation: String,
    pub priority: Priority,
    /// Findings posted by the newest review, by severity.
    pub blocking: usize,
    pub high: usize,
    pub total_findings: usize,
    /// True when an *older* record for this PR names a different commit — the PR
    /// moved and was reviewed again. It does **not** mean the current review is
    /// stale; only the provider knows the PR's head today.
    pub superseded: bool,
}

/// Fold run-log records into one row per PR, newest review winning, ranked.
///
/// Records may arrive in any order and from any number of repos; ordering is by
/// [`RunLog::ts_unix`], so a log concatenated out of order still folds correctly.
///
/// Dry runs are **excluded**. A dry run is a rehearsal — a bench pass, a local
/// `--dry-run`, a probe — and it posts nothing, so counting it would put a PR in
/// the queue on the strength of a review no human can see.
pub fn rank(records: &[RunLog]) -> Vec<PrStatus> {
    let mut newest: HashMap<(String, String, u64), &RunLog> = HashMap::new();
    let mut seen_shas: HashMap<(String, String, u64), Vec<Option<String>>> = HashMap::new();

    for r in records.iter().filter(|r| !r.dry_run) {
        let key = (r.provider.clone(), r.repo.clone(), r.pr);
        seen_shas
            .entry(key.clone())
            .or_default()
            .push(r.head_sha.clone());
        newest
            .entry(key)
            .and_modify(|cur| {
                if r.ts_unix >= cur.ts_unix {
                    *cur = r;
                }
            })
            .or_insert(r);
    }

    let mut out: Vec<PrStatus> = newest
        .into_iter()
        .map(|(key, r)| {
            let blocking = count_severity(r, "BLOCKING");
            let high = count_severity(r, "HIGH");
            let shas = seen_shas.get(&key).map(Vec::as_slice).unwrap_or(&[]);
            PrStatus {
                provider: r.provider.clone(),
                repo: r.repo.clone(),
                pr: r.pr,
                head_sha: r.head_sha.clone(),
                ts_unix: r.ts_unix,
                recommendation: r.recommendation.clone(),
                priority: priority_of(&r.recommendation, blocking, high),
                blocking,
                high,
                total_findings: r.findings.len(),
                superseded: shas
                    .iter()
                    .filter(|s| s.is_some())
                    .any(|s| *s != r.head_sha),
            }
        })
        .collect();

    // Priority first, then the reviewer's own severity counts, then most recent.
    // Ties break on (repo, pr) so the order is stable across runs — a queue that
    // reshuffles equal rows on every refresh cannot be scanned by eye.
    out.sort_by(|a, b| {
        a.priority
            .cmp(&b.priority)
            .then(b.blocking.cmp(&a.blocking))
            .then(b.high.cmp(&a.high))
            .then(b.ts_unix.cmp(&a.ts_unix))
            .then(a.repo.cmp(&b.repo))
            .then(a.pr.cmp(&b.pr))
    });
    out
}

/// Count posted findings of one severity, case-insensitively.
fn count_severity(r: &RunLog, sev: &str) -> usize {
    r.findings
        .iter()
        .filter(|f| f.severity.eq_ignore_ascii_case(sev))
        .count()
}

/// Bucket a PR from the reviewer's verdict and its severity counts.
///
/// The recommendation and the findings are both consulted because they can
/// disagree: `effective_recommendation` already upgrades a verdict to match its
/// findings, but a record written before that existed, or by a backend that
/// sets the field itself, may carry a softer verdict than its findings support.
/// Taking the more urgent of the two keeps an old log readable without
/// understating it.
fn priority_of(recommendation: &str, blocking: usize, high: usize) -> Priority {
    let rec = recommendation.to_ascii_uppercase();
    if blocking > 0 || rec.contains("BLOCK") {
        Priority::P0
    } else if high > 0 || rec.contains("CHANGES") {
        Priority::P1
    } else {
        Priority::P2
    }
}

/// Parse a JSONL run log, skipping anything that is not one of our records.
///
/// The stdout sink shares a stream with tracing output, so a log can legitimately
/// contain lines this module must ignore. Filtering on the `_kind` marker is what
/// [`crate::runlog`] documents as the way to find records; "is this line JSON" is
/// not, and stops working the moment anything else emits structured output.
///
/// A record that matches `_kind` but fails to parse is skipped too — a schema
/// change across releases must not make an old log unreadable.
pub fn parse_jsonl(text: &str) -> Vec<RunLog> {
    text.lines()
        .filter(|l| l.contains(crate::runlog::KIND))
        .filter_map(|l| serde_json::from_str::<RunLog>(l).ok())
        .collect()
}

/// Render a ranked queue as a markdown table.
///
/// `now_unix` is passed in rather than read from the clock so the output is
/// reproducible in a test and in a snapshot committed to a repo.
pub fn render_queue(rows: &[PrStatus], now_unix: u64) -> String {
    if rows.is_empty() {
        return "_No reviewed PRs in this log._".to_string();
    }
    let mut s =
        String::from("| | PR | Verdict | Findings | Reviewed |\n| --- | --- | --- | --- | --- |\n");
    for r in rows {
        let sev = match (r.blocking, r.high) {
            (0, 0) => format!("{}", r.total_findings),
            (b, 0) => format!("{} ({b} blocking)", r.total_findings),
            (0, h) => format!("{} ({h} high)", r.total_findings),
            (b, h) => format!("{} ({b} blocking, {h} high)", r.total_findings),
        };
        let age = age_label(now_unix.saturating_sub(r.ts_unix));
        let moved = if r.superseded { " · re-reviewed" } else { "" };
        s.push_str(&format!(
            "| **{}** | `{}`#{} | {} | {} | {age}{moved} |\n",
            r.priority.label(),
            r.repo,
            r.pr,
            r.recommendation,
            sev,
        ));
    }
    s
}

/// Coarse age: a queue is scanned, not audited, and "3d" reads faster than a
/// timestamp.
fn age_label(secs: u64) -> String {
    match secs {
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runlog::{Funnel, LoggedFinding, RunLog, KIND, SCHEMA};

    fn finding(sev: &str) -> LoggedFinding {
        LoggedFinding {
            severity: sev.to_string(),
            file: "a.rs".into(),
            line: Some(1),
            confidence: Some(90),
            anchored: true,
            anchored_line: Some(1),
            body: "x".into(),
        }
    }

    fn rec(pr: u64, ts: u64, recommendation: &str, sevs: &[&str]) -> RunLog {
        RunLog {
            kind: KIND,
            schema: SCHEMA,
            ts_unix: ts,
            core_version: "test".into(),
            provider: "github".into(),
            repo: "o/r".into(),
            pr,
            head_sha: Some(format!("sha{ts}")),
            base_branch: None,
            model: "m".into(),
            dry_run: false,
            posted: true,
            ci_status_known: false,
            diff_bytes: 10,
            diff_truncated: false,
            advisories: 0,
            truncated_salvage: false,
            recommendation: recommendation.into(),
            funnel: Funnel::default(),
            findings: sevs.iter().map(|s| finding(s)).collect(),
            usage: None,
            duration_ms: 1,
        }
    }

    #[test]
    fn blocking_outranks_high_outranks_clean() {
        let rows = rank(&[
            rec(1, 100, "APPROVE", &[]),
            rec(2, 100, "APPROVE WITH CHANGES", &["HIGH"]),
            rec(3, 100, "BLOCK", &["BLOCKING"]),
        ]);
        let got: Vec<_> = rows.iter().map(|r| (r.pr, r.priority)).collect();
        assert_eq!(
            got,
            vec![(3, Priority::P0), (2, Priority::P1), (1, Priority::P2)]
        );
    }

    /// A verdict softer than its findings must not soften the bucket. Old records
    /// predate the recommendation floor, and a backend may set the field itself.
    #[test]
    fn findings_can_upgrade_a_soft_verdict() {
        let rows = rank(&[rec(1, 100, "APPROVE", &["BLOCKING"])]);
        assert_eq!(rows[0].priority, Priority::P0);
    }

    /// A BLOCK with no findings still blocks — the verdict is the reviewer's, and
    /// the cap or the confidence floor can empty the list beneath it.
    #[test]
    fn a_verdict_alone_can_set_the_bucket() {
        let rows = rank(&[rec(1, 100, "BLOCK", &[])]);
        assert_eq!(rows[0].priority, Priority::P0);
    }

    #[test]
    fn the_newest_review_of_a_pr_wins() {
        let rows = rank(&[
            rec(1, 100, "BLOCK", &["BLOCKING"]),
            rec(1, 200, "APPROVE", &[]),
        ]);
        assert_eq!(rows.len(), 1, "one row per PR");
        assert_eq!(rows[0].priority, Priority::P2, "the later review governs");
        assert_eq!(rows[0].ts_unix, 200);
    }

    /// Out-of-order records must fold the same way — a log can be concatenated
    /// from several files, or interleaved by concurrent instances on stdout.
    #[test]
    fn record_order_does_not_matter() {
        let a = rank(&[rec(1, 100, "BLOCK", &[]), rec(1, 200, "APPROVE", &[])]);
        let b = rank(&[rec(1, 200, "APPROVE", &[]), rec(1, 100, "BLOCK", &[])]);
        assert_eq!(a[0].ts_unix, b[0].ts_unix);
        assert_eq!(a[0].priority, b[0].priority);
    }

    #[test]
    fn a_pr_reviewed_at_two_shas_is_marked_superseded() {
        let rows = rank(&[rec(1, 100, "BLOCK", &[]), rec(1, 200, "APPROVE", &[])]);
        assert!(rows[0].superseded, "sha100 then sha200");

        let same = rank(&[rec(2, 100, "BLOCK", &[])]);
        assert!(!same[0].superseded, "one review, one sha");
    }

    /// A dry run posts nothing, so no human can act on it — it must not put a PR
    /// in a queue of things wanting attention.
    #[test]
    fn dry_runs_are_excluded() {
        let mut d = rec(1, 100, "BLOCK", &["BLOCKING"]);
        d.dry_run = true;
        assert!(rank(&[d]).is_empty());
    }

    #[test]
    fn ties_are_stable_across_runs() {
        let recs = vec![rec(9, 100, "APPROVE", &[]), rec(4, 100, "APPROVE", &[])];
        let a: Vec<_> = rank(&recs).iter().map(|r| r.pr).collect();
        let b: Vec<_> = rank(&recs).iter().map(|r| r.pr).collect();
        assert_eq!(a, b);
        assert_eq!(a, vec![4, 9], "lower PR number first");
    }

    #[test]
    fn parse_skips_foreign_and_broken_lines() {
        let good = serde_json::to_string(&rec(1, 100, "APPROVE", &[])).expect("serializes");
        let text = format!(
            "INFO some tracing line\n{{\"hello\":\"world\"}}\n{good}\n{{\"_kind\":\"prbot_run_log\",\"broken\":\n"
        );
        let got = parse_jsonl(&text);
        assert_eq!(got.len(), 1, "only the one real record");
        assert_eq!(got[0].pr, 1);
    }

    #[test]
    fn a_record_round_trips() {
        let r = rec(7, 100, "BLOCK", &["BLOCKING", "HIGH"]);
        let back: RunLog =
            serde_json::from_str(&serde_json::to_string(&r).expect("ser")).expect("de");
        assert_eq!(back.pr, 7);
        assert_eq!(back.kind, KIND, "the marker is restored from the constant");
        assert_eq!(back.findings.len(), 2);
    }

    #[test]
    fn render_names_the_severities_that_drove_the_rank() {
        let rows = rank(&[rec(1, 100, "BLOCK", &["BLOCKING", "HIGH"])]);
        let out = render_queue(&rows, 100 + 7200);
        assert!(out.contains("**P0**"), "{out}");
        assert!(out.contains("`o/r`#1"), "{out}");
        assert!(out.contains("2 (1 blocking, 1 high)"), "{out}");
        assert!(out.contains("2h"), "{out}");
    }

    #[test]
    fn an_empty_log_says_so_rather_than_rendering_an_empty_table() {
        assert!(render_queue(&[], 0).contains("No reviewed PRs"));
    }
}
