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
    ///
    /// **Provider-dependent.** It can only ever be true where records carry a
    /// commit id, and Bitbucket's deliberately do not (`head_sha` stays `None`
    /// there because its inline comments need no commit). So on a Bitbucket log
    /// this is always false — absent, not evidence of a PR that never moved.
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
                if fold_key(r) > fold_key(cur) {
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
                // Only a record that names a commit can witness a different
                // one. Without this guard a newest record with no SHA compares
                // `Some(older) != None` and reports a PR as re-reviewed on no
                // evidence at all — which every Bitbucket log would do.
                superseded: r.head_sha.is_some()
                    && shas
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
            // The fold keys on provider, so a repo mirrored to two hosts yields
            // two rows. Without this they order by HashMap iteration — randomized
            // per process, which is the opposite of the stability claimed above.
            .then(a.provider.cmp(&b.provider))
    });
    out
}

/// The total order that decides which record for a PR wins.
///
/// Timestamps are whole seconds, so two records for one PR tie readily — a retry
/// within the same second, or the concurrent-instance interleaving the module docs
/// cite. Comparing `ts` alone (or `(ts, sha)`, which only separates records that
/// reviewed *different* commits) leaves the winner as whichever the slice happened
/// to yield first, and the fold's promise of order-independence held only almost
/// always.
///
/// Every field the rendered row is derived from is in the key, so when two records
/// tie on all of them the choice between them cannot change the output — which is
/// what makes this a total order *for this purpose* without needing to compare
/// records byte for byte.
fn fold_key(r: &RunLog) -> (u64, Option<&str>, &str, usize, usize, usize) {
    (
        r.ts_unix,
        r.head_sha.as_deref(),
        r.recommendation.as_str(),
        count_severity(r, "BLOCKING"),
        count_severity(r, "HIGH"),
        r.findings.len(),
    )
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
    // Classification comes from `review::recommendation_rank`, not a second copy
    // of the same string matching: two encodings of the same vocabulary drift the
    // moment it changes, and nothing would fail when they did.
    let rank = crate::review::recommendation_rank(recommendation);
    if blocking > 0 || rank >= 2 {
        Priority::P0
    } else if high > 0 || rank >= 1 {
        Priority::P1
    } else {
        Priority::P2
    }
}

/// What a run log yielded: the records, and how many were unreadable.
#[derive(Debug, Default)]
pub struct ParsedLog {
    pub records: Vec<RunLog>,
    /// Lines carrying the `_kind` marker that failed to deserialize.
    ///
    /// Reported rather than swallowed. Each one is a review that happened and a
    /// PR that will be missing from the queue — and a queue that quietly omits
    /// rows is worse than one that admits it cannot read them, because nothing
    /// in the output distinguishes "not in the queue" from "never reviewed".
    pub unreadable: usize,
}

/// Parse a JSONL run log, skipping anything that is not one of our records.
///
/// The stdout sink shares a stream with tracing output, so a log can legitimately
/// contain lines this module must ignore. Filtering on the `_kind` marker is what
/// [`crate::runlog`] documents as the way to find records; "is this line JSON" is
/// not, and stops working the moment anything else emits structured output.
///
/// A line that really is a record but fails to parse is counted in
/// [`ParsedLog::unreadable`] rather than dropped in silence.
///
/// "Really is a record" is decided by parsing, not by substring: a tracing line
/// that merely *mentions* the marker — this crate's own log messages do — is not
/// JSON at all, and counting it as unreadable would inflate the one number that
/// exists to be trusted, the count of PRs missing from the table.
pub fn parse_jsonl(text: &str) -> ParsedLog {
    let mut out = ParsedLog::default();
    for line in text.lines() {
        // Parse once as a generic value to ask "is this one of ours?", so the
        // question is answered by the `_kind` *field* rather than by the bytes
        // appearing anywhere in the line.
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v.get("_kind").and_then(|k| k.as_str()) != Some(crate::runlog::KIND) {
            continue;
        }
        match serde_json::from_value::<RunLog>(v) {
            Ok(r) => out.records.push(r),
            Err(_) => out.unreadable += 1,
        }
    }
    out
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
        // The provider is part of the row's identity, not decoration: the same
        // `repo#pr` on two hosts is two different PRs, and printing them
        // identically makes the table ambiguous exactly where it matters.
        s.push_str(&format!(
            "| **{}** | {}:`{}`#{} | {} | {} | {age}{moved} |\n",
            r.priority.label(),
            r.provider,
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

    /// Equal timestamps must not let arrival order decide the winner — the fold
    /// documents order-independence and whole-second stamps make ties real.
    #[test]
    fn a_timestamp_tie_folds_the_same_either_way() {
        let mut a1 = rec(1, 100, "BLOCK", &["BLOCKING"]);
        a1.head_sha = Some("aaa".into());
        let mut a2 = rec(1, 100, "APPROVE", &[]);
        a2.head_sha = Some("bbb".into());

        let fwd = rank(&[a1.clone(), a2.clone()]);
        let rev = rank(&[a2, a1]);
        assert_eq!(fwd[0].recommendation, rev[0].recommendation);
        assert_eq!(fwd[0].priority, rev[0].priority);
        assert_eq!(
            fwd[0].head_sha.as_deref(),
            Some("bbb"),
            "the stable tie-break"
        );
    }

    /// `superseded` claims the PR moved. A record with no commit id witnesses
    /// nothing — and Bitbucket records never carry one, so without this guard
    /// every Bitbucket PR would read as re-reviewed.
    #[test]
    fn a_newest_record_with_no_sha_never_claims_superseded() {
        let mut old = rec(1, 100, "BLOCK", &[]);
        old.head_sha = Some("aaa".into());
        let mut newest = rec(1, 200, "APPROVE", &[]);
        newest.head_sha = None;

        let rows = rank(&[old, newest]);
        assert!(!rows[0].superseded, "no commit id, no claim");
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

    /// A field added to a nested struct later must cost a column, never the
    /// record. Without container-level `serde(default)` on `Funnel` and
    /// `LoggedFinding`, the next field either gains makes every older record with
    /// a populated `funnel`/`findings` fail wholesale — and the PR disappears from
    /// the queue rather than losing a cell.
    #[test]
    fn a_record_whose_nested_structs_predate_a_field_still_parses() {
        let line = r#"{"_kind":"prbot_run_log","schema":1,"ts_unix":100,"provider":"github",
            "repo":"o/r","pr":5,"recommendation":"BLOCK",
            "funnel":{"model_raw":2},
            "findings":[{"severity":"HIGH","file":"a.rs"}]}"#;
        let got = parse_jsonl(&line.replace('\n', ""));
        assert_eq!(
            got.unreadable, 0,
            "a partial nested struct must not lose it"
        );
        assert_eq!(got.records.len(), 1);

        let rows = rank(&got.records);
        assert_eq!(rows[0].priority, Priority::P0, "and it still ranks");
        assert_eq!(rows[0].high, 1, "from the finding it could read");
    }

    /// Two records identical in `(ts, sha)` must still fold deterministically —
    /// `(ts, sha)` alone separates only records that reviewed different commits.
    #[test]
    fn a_tie_on_timestamp_and_sha_still_folds_deterministically() {
        let mut a = rec(1, 100, "BLOCK", &["BLOCKING"]);
        a.head_sha = Some("same".into());
        let mut b = rec(1, 100, "APPROVE", &[]);
        b.head_sha = Some("same".into());

        let fwd = rank(&[a.clone(), b.clone()]);
        let rev = rank(&[b, a]);
        assert_eq!(fwd[0].recommendation, rev[0].recommendation);
        assert_eq!(fwd[0].priority, rev[0].priority);
        assert_eq!(fwd[0].blocking, rev[0].blocking);
    }

    /// The same `repo#pr` on two hosts is two different PRs. They must order
    /// stably and must not render identically.
    #[test]
    fn two_providers_sharing_a_repo_and_number_stay_distinct() {
        let a = rec(1, 100, "APPROVE", &[]);
        let mut b = rec(1, 100, "APPROVE", &[]);
        b.provider = "gitlab".into();

        let rows = rank(&[a.clone(), b.clone()]);
        assert_eq!(rows.len(), 2, "one row per (provider, repo, pr)");
        let order: Vec<_> = rows.iter().map(|r| r.provider.as_str()).collect();
        assert_eq!(order, vec!["github", "gitlab"], "stable, not HashMap order");
        assert_eq!(
            rank(&[b, a])
                .iter()
                .map(|r| r.provider.clone())
                .collect::<Vec<_>>(),
            order
        );

        let out = render_queue(&rows, 100);
        assert!(out.contains("github:`o/r`#1"), "{out}");
        assert!(out.contains("gitlab:`o/r`#1"), "{out}");
    }

    /// A tracing line that merely mentions the marker is not a record. Counting
    /// it as unreadable would inflate the one number here that exists to be
    /// trusted — how many PRs are missing from the table.
    #[test]
    fn a_line_merely_mentioning_the_marker_is_not_a_record() {
        let text = format!(
            "INFO writing run log ({}) to stdout\nplain prose mentioning prbot_run_log again",
            crate::runlog::KIND
        );
        let got = parse_jsonl(&text);
        assert_eq!(got.records.len(), 0);
        assert_eq!(got.unreadable, 0, "not JSON, so not a record that failed");
    }

    #[test]
    fn parse_skips_foreign_and_broken_lines() {
        let good = serde_json::to_string(&rec(1, 100, "APPROVE", &[])).expect("serializes");
        let text = format!(
            "INFO some tracing line\n{{\"hello\":\"world\"}}\n{good}\n{{\"_kind\":\"prbot_run_log\"}}\n"
        );
        let got = parse_jsonl(&text);
        assert_eq!(got.records.len(), 1, "only the one real record");
        assert_eq!(got.records[0].pr, 1);
        // Valid JSON carrying our marker but missing the identity fields: a real
        // record this reader cannot use, which is what `unreadable` counts.
        assert_eq!(
            got.unreadable, 1,
            "the unusable record is counted, not lost"
        );
    }

    /// A record that fails to parse is a review that happened and a PR missing
    /// from the queue. Losing it silently is worse than any empty column.
    #[test]
    fn a_record_from_a_different_field_set_still_parses() {
        // Only the identity fields; everything else absent, as an older release
        // or a trimmed export would leave it.
        let minimal = r#"{"_kind":"prbot_run_log","schema":1,"ts_unix":100,"provider":"github","repo":"o/r","pr":5}"#;
        let got = parse_jsonl(minimal);
        assert_eq!(
            got.unreadable, 0,
            "a missing column must not lose the record"
        );
        assert_eq!(got.records.len(), 1);
        assert_eq!(got.records[0].pr, 5);
        assert!(rank(&got.records).len() == 1, "and it reaches the queue");
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
        assert!(out.contains("github:`o/r`#1"), "{out}");
        assert!(out.contains("2 (1 blocking, 1 high)"), "{out}");
        assert!(out.contains("2h"), "{out}");
    }

    #[test]
    fn an_empty_log_says_so_rather_than_rendering_an_empty_table() {
        assert!(render_queue(&[], 0).contains("No reviewed PRs"));
    }
}
