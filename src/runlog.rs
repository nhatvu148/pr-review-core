//! JSONL run log: one line per review run, written to a local file or to stdout.
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
//! # Where it goes
//!
//! `PRBOT_RUN_LOG` selects the sink ([`Config::run_log`]):
//!
//! - unset or empty — **off**, the default. Nothing is written.
//! - `-` — **stdout**, one JSON line per review.
//! - anything else — that **file**, appended to, parent directories created.
//!
//! The stdout sink exists because a file needs a disk that survives the process,
//! and the platforms these bots run on increasingly have neither. Cloud Run is
//! the case that forced it: its filesystem is ephemeral *and* it runs several
//! instances at once, so every shared-file approach — a mounted volume, a GCS
//! FUSE bucket — has concurrent appenders corrupting one file. A log stream has
//! no such problem, and the platform already captures stdout and routes it
//! (Cloud Logging, then a BigQuery sink) without this crate needing credentials,
//! a client library, or a network call on the review path.
//!
//! # Privacy
//!
//! A record contains the finding text, which is review commentary on the source
//! it was written about. **Nothing in this crate ships it anywhere**: there is no
//! upload path, no client, no network call. It goes to the file you name or to
//! this process's stdout, and the default is off — so a deployed bot logs
//! nothing unless its operator sets the variable deliberately.
//!
//! Note the sinks differ in where the data comes to rest. A file stays on that
//! disk. Stdout is captured by whatever supervises the process, so on a hosted
//! platform the records land in that platform's logging system, under its
//! retention and its access control. Choose accordingly when the code under
//! review is not yours.
//!
//! [`Config::run_log`]: crate::config::Config::run_log

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::llm::{Finding, Usage};

/// Record format version. Bump when a field changes meaning or is removed, so a
/// reader over a log spanning several releases can tell the shapes apart. Adding
/// a field is not a bump — every consumer of JSONL must tolerate new keys.
pub const SCHEMA: u32 = 1;

/// Marks a line as one of ours, under the `_kind` key.
///
/// Needed because stdout is a *shared* channel: the process's own `tracing`
/// output goes to the same stream, and on a platform that parses JSON lines into
/// structured entries (Cloud Logging does) a query cannot tell a run-log record
/// from any other JSON a library decided to emit. Selecting on "is JSON" would
/// work today and silently start collecting junk the moment anything else logs
/// structured output — including this bot's own tracing, if its subscriber is
/// ever switched to `.json()`.
pub const KIND: &str = "prbot_run_log";

/// Where a run log is written.
///
/// An enum rather than an `Option<PathBuf>` with a magic `-` value: the sinks
/// have genuinely different mechanics — one creates directories and appends, the
/// other locks a shared stream — and a path-shaped type that sometimes is not a
/// path invites exactly one bug, `create_dir_all("-")`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunLogSink {
    /// Append to this file, creating it and any missing parent directories.
    File(PathBuf),
    /// Write one line to this process's stdout, for a platform that captures it.
    Stdout,
}

impl RunLogSink {
    /// Parse the `PRBOT_RUN_LOG` value. `None` for unset/empty (the default,
    /// off); `-` for stdout; anything else is a file path.
    pub fn from_env_value(v: &str) -> Option<Self> {
        match v.trim() {
            "" => None,
            "-" => Some(Self::Stdout),
            path => Some(Self::File(PathBuf::from(path))),
        }
    }
}

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
    /// Of the anchored ones, how many carried a committable suggestion block.
    ///
    /// The gap between this and `anchored` is the question the feature is judged
    /// on: a reviewer that never offers a one-click fix has not closed the gap,
    /// and one that offers it on everything is not being selective enough to be
    /// trusted with a commit button.
    #[serde(default)]
    pub suggested: usize,
}

/// One finding as logged: its metadata, where it was posted, and its text.
#[derive(Debug, Clone, Serialize)]
pub struct LoggedFinding {
    pub severity: String,
    pub file: String,
    /// The line the *model* named. Not necessarily where the comment went — see
    /// `anchored_line`.
    pub line: Option<u64>,
    pub confidence: Option<u8>,
    /// True when this finding was posted as an inline comment rather than folded
    /// into the summary.
    pub anchored: bool,
    /// The line the inline comment was actually posted on, or `None` when the
    /// finding folded into the summary.
    ///
    /// Differs from `line` whenever the re-anchor step moved the finding onto a
    /// nearby diff line — which `REANCHOR_FINDINGS` does by default. This, not
    /// `line`, is the join key for any later outcome pass: `(file, anchored_line)`
    /// is what a human replies to and what the reconciler later deletes.
    pub anchored_line: Option<u64>,
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
    /// Always [`KIND`]. First key in the record so a reader — or a human tailing
    /// a mixed stdout stream — can identify the line without parsing it all.
    #[serde(rename = "_kind")]
    pub kind: &'static str,
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

/// Build the logged findings, given the line each one was posted on.
///
/// `anchors` is index-aligned with `findings`: `anchors[i]` is the line finding
/// `i` was anchored to, or `None` if it folded into the summary. It has to be
/// carried out of the anchoring loop rather than re-derived here, because the
/// re-anchor step posts a comment on a line the finding itself never records —
/// re-matching on the finding's own `(file, line)` would report every re-anchored
/// finding as unanchored, which is the common case with `REANCHOR_FINDINGS` on.
///
/// A shorter `anchors` reads as "not anchored" for the remaining findings, which
/// is what the advisory-only path (no model, no inline comments) wants.
pub fn logged_findings(findings: &[Finding], anchors: &[Option<u64>]) -> Vec<LoggedFinding> {
    findings
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let anchored_line = anchors.get(i).copied().flatten();
            LoggedFinding {
                severity: f.severity.clone(),
                file: f.file.clone(),
                line: f.line,
                confidence: f.confidence,
                anchored: anchored_line.is_some(),
                anchored_line,
                body: f.body.clone(),
            }
        })
        .collect()
}

/// Write one record to `sink`.
///
/// **Fail-open in every case.** A run log is an observability side effect; a full
/// disk, a bad path, a permissions error or a closed stdout must never cost a
/// review that has already been produced and posted. Failures are warned about
/// and dropped.
pub fn write(sink: &RunLogSink, rec: &RunLog) {
    let result = match sink {
        RunLogSink::File(path) => try_append(path, rec),
        RunLogSink::Stdout => try_write_stdout(rec),
    };
    if let Err(e) = result {
        let where_ = match sink {
            RunLogSink::File(p) => p.display().to_string(),
            RunLogSink::Stdout => "<stdout>".to_string(),
        };
        tracing::warn!("run log: could not write {where_}: {e:#}");
    }
}

/// Serialize one record as a single line, newline included.
///
/// One `write_all` of the whole line is what keeps concurrent reviews from
/// interleaving into an unparseable record — true of a shared file and of a
/// shared stdout alike, so both sinks go through here.
fn line_for(rec: &RunLog) -> anyhow::Result<String> {
    let mut line = serde_json::to_string(rec)?;
    line.push('\n');
    Ok(line)
}

fn try_write_stdout(rec: &RunLog) -> anyhow::Result<()> {
    // Lock once and flush: a supervisor reading the pipe (Cloud Run, a container
    // runtime) must see the record even if the process is killed moments later,
    // and Rust's stdout is line-buffered only when it is a terminal — piped, it
    // is block-buffered and a crash would eat the tail.
    let out = std::io::stdout();
    write_one(&mut out.lock(), rec)
}

/// Serialize and emit one record through `w`, as a single flushed write.
///
/// Split out so the stream path is testable: capturing the process's real stdout
/// from inside a test is not something a unit test should be doing, and the
/// property worth pinning — one flushed `write_all` of one complete line — is a
/// property of this function, not of the file descriptor.
fn write_one<W: Write>(w: &mut W, rec: &RunLog) -> anyhow::Result<()> {
    let line = line_for(rec)?;
    w.write_all(line.as_bytes())?;
    w.flush()?;
    Ok(())
}

fn try_append(path: &Path, rec: &RunLog) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir)?;
        }
    }
    let line = line_for(rec)?;
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
            kind: KIND,
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
                suggested: 0,
            },
            findings: vec![LoggedFinding {
                severity: "HIGH".into(),
                file: "src/a.rs".into(),
                line: Some(12),
                confidence: Some(90),
                anchored: true,
                anchored_line: Some(12),
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

        write(&RunLogSink::File(path.clone()), &rec());
        write(&RunLogSink::File(path.clone()), &rec());

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
        write(&RunLogSink::File(blocker.join("under/runs.jsonl")), &rec());
    }

    fn finding(file: &str, line: Option<u64>) -> Finding {
        Finding {
            severity: "HIGH".into(),
            file: file.into(),
            line,
            body: "a body".into(),
            confidence: None,
            suggestion: None,
        }
    }

    /// The posted line comes from `anchors`, and is never re-derived from the
    /// finding's own line: a re-anchored finding keeps the line the model named
    /// while its comment goes somewhere else, and both belong in the record.
    #[test]
    fn the_posted_line_comes_from_the_anchors_not_the_finding() {
        let findings = vec![
            finding("src/a.rs", Some(12)), // anchored where the model said
            finding("src/a.rs", Some(20)), // re-anchored two lines down
            finding("src/b.rs", Some(99)), // folded into the summary
        ];
        let anchors = vec![Some(12), Some(22), None];

        let logged = logged_findings(&findings, &anchors);

        assert!(logged[0].anchored);
        assert_eq!(logged[0].anchored_line, Some(12));

        assert!(logged[1].anchored, "a re-anchored finding IS anchored");
        assert_eq!(logged[1].line, Some(20), "the model's line is preserved");
        assert_eq!(logged[1].anchored_line, Some(22), "the posted line differs");

        assert!(!logged[2].anchored);
        assert_eq!(logged[2].anchored_line, None);
    }

    /// `PRBOT_RUN_LOG` is the whole configuration surface, so its parsing is the
    /// whole way this feature gets turned on, off, or pointed somewhere.
    #[test]
    fn the_env_value_selects_the_sink() {
        use RunLogSink::*;
        assert_eq!(RunLogSink::from_env_value(""), None, "unset/empty is OFF");
        assert_eq!(
            RunLogSink::from_env_value("   "),
            None,
            "whitespace is still off — `PRBOT_RUN_LOG= ` in an env file"
        );
        assert_eq!(RunLogSink::from_env_value("-"), Some(Stdout));
        assert_eq!(
            RunLogSink::from_env_value("  -  "),
            Some(Stdout),
            "trimmed, so a stray space in a YAML env block still means stdout"
        );
        assert_eq!(
            RunLogSink::from_env_value("/data/runs.jsonl"),
            Some(File(PathBuf::from("/data/runs.jsonl")))
        );
        // The one that must never be a path: `-` is claimed, but `./-` is a file.
        assert_eq!(
            RunLogSink::from_env_value("./-"),
            Some(File(PathBuf::from("./-")))
        );
    }

    /// The stream sink emits exactly one complete line, and it is tagged.
    ///
    /// `_kind` is what makes the record findable on stdout, which is a channel
    /// shared with this process's own tracing output. Without it a Cloud Logging
    /// query has to select on "is this JSON", which silently starts collecting
    /// anything else that ever logs structured output.
    #[test]
    fn the_stream_sink_writes_one_tagged_line() {
        let mut buf: Vec<u8> = Vec::new();
        write_one(&mut buf, &rec()).expect("written");
        write_one(&mut buf, &rec()).expect("written");

        let text = String::from_utf8(buf).expect("utf-8");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "one line per record, newline-terminated");
        assert!(
            text.ends_with('\n'),
            "a reader splitting on newline sees both"
        );

        let v: serde_json::Value = serde_json::from_str(lines[0]).expect("JSON");
        assert_eq!(v["_kind"], KIND);
        assert_eq!(v["funnel"]["model_raw"], 5);
    }

    /// Both sinks serialize identically — the sink chooses the destination, never
    /// the content, so a file log and a stdout log are the same dataset.
    #[test]
    fn both_sinks_produce_the_same_record() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("runs.jsonl");
        write(&RunLogSink::File(path.clone()), &rec());

        let mut buf: Vec<u8> = Vec::new();
        write_one(&mut buf, &rec()).expect("written");

        assert_eq!(
            std::fs::read_to_string(&path).expect("file"),
            String::from_utf8(buf).expect("utf-8")
        );
    }

    /// The advisory-only path has findings and no anchors at all.
    #[test]
    fn a_short_anchors_slice_reads_as_unanchored() {
        let findings = vec![finding("src/a.rs", Some(1)), finding("src/b.rs", None)];
        let logged = logged_findings(&findings, &[]);
        assert!(logged
            .iter()
            .all(|f| !f.anchored && f.anchored_line.is_none()));
    }
}
