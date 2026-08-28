//! Local-only JSONL run log: one line per review run, appended to a file on the
//! machine that ran it.
//!
//! # Why
//!
//! The benchmark corpus measures the reviewer against *planted* bugs, where the
//! ground truth is known. It cannot say anything about the runs that actually
//! happen: how often the finding cap truncates, how often self-critique drops a
//! finding, how often a review is salvaged from truncated JSON, what the
//! severity mix looks like on a real repo. Those are answerable only from
//! production runs, and today they are thrown away the moment the comment posts.
//!
//! This module records them. It records **funnel counts and findings**, not a
//! verdict: nothing here knows whether a finding was correct. A run log measures
//! *behaviour*, never recall — the reviewer never learns what it missed on a real
//! PR, and no aggregate over these records can supply that.
//!
//! # Privacy
//!
//! A record contains the finding text, which is review commentary on the source
//! it was written about. **The log is therefore opt-in and never leaves the
//! machine**: it is written only when `PRBOT_RUN_LOG` names a path
//! ([`Config::run_log_path`]), there is no upload path anywhere in this crate,
//! and the default is off — so a deployed bot logs nothing unless its operator
//! sets that variable deliberately. Point it somewhere private and gitignored.
//!
//! [`Config::run_log_path`]: crate::config::Config::run_log_path

use std::io::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::llm::{Finding, Usage};

/// Record format version. Bump when a field changes meaning or is removed, so a
/// reader over a log spanning several releases can tell the shapes apart. Adding
/// a field is not a bump — every consumer of JSONL must tolerate new keys.
pub const SCHEMA: u32 = 1;

/// How many findings survived each stage between the model's answer and the
/// posted review.
///
/// The stages are recorded in pipeline order, and each is a count *after* its
/// stage runs, so the drop at any stage is the difference from the previous one.
/// `hygiene_added` is the exception: deterministic findings are merged in, so it
/// is an addition, and `after_collapse` includes them.
#[derive(Debug, Default, Clone, Serialize)]
pub struct Funnel {
    /// Findings the backend returned, before any post-processing.
    pub model_raw: usize,
    /// After the optional self-critique pass. Equal to `model_raw` when
    /// `self_critique` is off or the critique call failed.
    pub after_critique: usize,
    /// After the `min_confidence` floor.
    pub after_confidence: usize,
    /// Deterministic diff-hygiene findings merged in at this point.
    pub hygiene_added: usize,
    /// After burst collapse — includes the hygiene findings.
    pub after_collapse: usize,
    /// After the `max_findings` cap. This is what the review reports.
    pub posted_findings: usize,
    /// Of those, how many anchored to a diff line as inline comments.
    pub anchored: usize,
    /// The rest, folded into the summary because no diff line would take them.
    pub unanchored: usize,
}

/// One finding as logged: its metadata, whether it anchored, and its text.
#[derive(Debug, Clone, Serialize)]
pub struct LoggedFinding {
    pub severity: String,
    pub file: String,
    pub line: Option<u64>,
    pub confidence: Option<u8>,
    /// True when this finding was posted as an inline comment rather than folded
    /// into the summary. The join key for any later outcome pass: an inline
    /// comment on `(file, line)` is the thing a human replies to, resolves, or
    /// the reconciler later deletes.
    pub anchored: bool,
    pub body: String,
}

/// One review run.
///
/// Serialized `snake_case` — unlike [`RunReviewOutput`], which is an HTTP
/// response and stays `camelCase`. This is a local analysis artifact read by
/// `jq` and pandas, not by the bot's API consumers.
///
/// [`RunReviewOutput`]: crate::review::RunReviewOutput
#[derive(Debug, Clone, Serialize)]
pub struct RunLog {
    pub schema: u32,
    /// Seconds since the Unix epoch. Deliberately not a formatted timestamp: this
    /// crate has no date dependency, and inventing an RFC 3339 formatter to make
    /// a log line prettier is not worth the civil-calendar arithmetic. Every
    /// reader can format it.
    pub ts_unix: u64,
    /// The `pr-review-core` version that produced this record — the reason a run
    /// log is comparable across releases at all.
    pub core_version: String,
    pub provider: String,
    pub repo: String,
    pub pr: u64,
    pub head_sha: Option<String>,
    pub base_branch: Option<String>,
    /// Model as *reported by the run*, not as configured — an agent-CLI backend
    /// may not use the configured OpenRouter model at all.
    pub model: String,
    /// True when the review never posted (`--dry-run`, or a bench run).
    pub dry_run: bool,
    pub posted: bool,
    /// Whether the provider reported CI results for the reviewed commit. The text
    /// itself is not logged — it is another service's output, and only its
    /// presence changes how findings are demoted.
    pub ci_status_known: bool,
    /// Bytes of diff actually sent to the backend, after glob filtering and
    /// packing. The single best predictor of cost, and the thing a "why was this
    /// review shallow?" question starts from.
    pub diff_bytes: usize,
    /// True when whole files were packed out to fit the size budget — i.e. part
    /// of the change was never reviewed.
    pub diff_truncated: bool,
    /// Dependency advisories from the CVE scan.
    pub advisories: usize,
    /// True when the review was salvaged from a response the model cut off
    /// mid-output, so findings after the cut are missing.
    ///
    /// Detected from the marker the salvage appends to the summary. A *plain*
    /// JSON repair (malformed but complete) is not visible here: it is reported
    /// only to the tracing log, and surfacing it would change the signature of
    /// the public `parse_review_with_repair`, which downstream backends call.
    pub truncated_salvage: bool,
    pub recommendation: String,
    pub funnel: Funnel,
    pub findings: Vec<LoggedFinding>,
    pub usage: Option<Usage>,
    /// Wall-clock milliseconds for the whole run, including the provider fetches
    /// and the post.
    pub duration_ms: u64,
}

impl RunLog {
    /// Seconds since the epoch, or 0 if the clock is before it.
    pub fn now_unix() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }
}

/// Build the logged findings, marking which ones anchored.
///
/// A finding anchored iff an inline comment was posted at its `(file, line)`.
/// Matching on the pair rather than on position is what keeps this correct after
/// the re-anchor step moves a finding's line.
pub fn logged_findings(
    findings: &[Finding],
    inline: &[crate::providers::InlineComment],
) -> Vec<LoggedFinding> {
    findings
        .iter()
        .map(|f| LoggedFinding {
            severity: f.severity.clone(),
            file: f.file.clone(),
            line: f.line,
            confidence: f.confidence,
            anchored: inline
                .iter()
                .any(|c| c.path == f.file && Some(c.line) == f.line),
            body: f.body.clone(),
        })
        .collect()
}

/// Append one record to the JSONL log at `path`, creating the file and any
/// missing parent directories.
///
/// **Fail-open in every case.** A run log is an observability side effect; a full
/// disk, a bad path, or a permissions error must never cost a review that has
/// already been produced and posted. Failures are warned about and dropped.
pub fn append(path: &Path, rec: &RunLog) {
    if let Err(e) = try_append(path, rec) {
        tracing::warn!("run log: could not write {}: {e:#}", path.display());
    }
}

fn try_append(path: &Path, rec: &RunLog) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir)?;
        }
    }
    // One `write_all` of a single line ending in `\n`. Two concurrent reviews on
    // one process append to the same file, and a line assembled by several small
    // writes could interleave into an unparseable record.
    let mut line = serde_json::to_string(rec)?;
    line.push('\n');
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    f.write_all(line.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    //! The properties that matter: a record round-trips as one JSON line, append
    //! really appends, and a broken path is survivable.

    use super::*;

    fn rec() -> RunLog {
        RunLog {
            schema: SCHEMA,
            ts_unix: 1_700_000_000,
            core_version: "0.0.0-test".into(),
            provider: "github".into(),
            repo: "o/r".into(),
            pr: 7,
            head_sha: Some("abc123".into()),
            base_branch: Some("main".into()),
            model: "anthropic/claude-sonnet-4.5".into(),
            dry_run: false,
            posted: true,
            ci_status_known: true,
            diff_bytes: 4096,
            diff_truncated: false,
            advisories: 0,
            truncated_salvage: false,
            recommendation: "APPROVE WITH CHANGES".into(),
            funnel: Funnel {
                model_raw: 5,
                after_critique: 4,
                after_confidence: 3,
                hygiene_added: 1,
                after_collapse: 4,
                posted_findings: 4,
                anchored: 3,
                unanchored: 1,
            },
            findings: vec![LoggedFinding {
                severity: "HIGH".into(),
                file: "src/a.rs".into(),
                line: Some(12),
                confidence: Some(90),
                anchored: true,
                body: "unwrap on an empty vec".into(),
            }],
            usage: None,
            duration_ms: 12_345,
        }
    }

    #[test]
    fn appends_one_parseable_line_per_record() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A nested path the caller never created — parent dirs must be made.
        let path = dir.path().join("nested/runs.jsonl");

        append(&path, &rec());
        append(&path, &rec());

        let text = std::fs::read_to_string(&path).expect("log written");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "one line per record");
        for l in lines {
            let v: serde_json::Value = serde_json::from_str(l).expect("each line is JSON");
            assert_eq!(v["schema"], SCHEMA);
            assert_eq!(v["funnel"]["model_raw"], 5);
            assert_eq!(v["findings"][0]["anchored"], true);
            // snake_case, not the response type's camelCase.
            assert!(v.get("ts_unix").is_some(), "snake_case field names");
        }
    }

    /// The property the whole module rests on: logging is a side effect, and a
    /// path that cannot be written must not panic or propagate.
    #[test]
    fn an_unwritable_path_is_survivable() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A file where a directory would have to be — create_dir_all must fail.
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"x").expect("write blocker");
        append(&blocker.join("under/runs.jsonl"), &rec());
    }

    /// Anchoring is decided by `(file, line)`, not by index — a finding that
    /// folded into the summary must not be marked anchored because another one
    /// happened to post at its position.
    #[test]
    fn anchored_is_matched_on_file_and_line() {
        let findings = vec![
            Finding {
                severity: "HIGH".into(),
                file: "src/a.rs".into(),
                line: Some(12),
                body: "anchored".into(),
                confidence: None,
            },
            Finding {
                severity: "LOW".into(),
                file: "src/b.rs".into(),
                line: Some(99),
                body: "out of diff".into(),
                confidence: None,
            },
        ];
        let inline = vec![crate::providers::InlineComment {
            path: "src/a.rs".into(),
            line: 12,
            body: "anchored".into(),
        }];

        let logged = logged_findings(&findings, &inline);
        assert!(logged[0].anchored);
        assert!(!logged[1].anchored, "no inline comment at (src/b.rs, 99)");
    }
}
