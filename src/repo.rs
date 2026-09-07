//! Repo workspace for the agentic reviewer: a shallow clone of the PR head, plus
//! the read-only tools the model uses to investigate cross-file context
//! (`read_file`, `list_dir`, `grep`). All paths are sandboxed to the clone root.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use ignore::WalkBuilder;

/// A checked-out repository the agent can read. Backed by a temp dir that is
/// removed when this value is dropped.
pub struct Workspace {
    _tmp: Option<tempfile::TempDir>,
    root: PathBuf,
}

impl Workspace {
    /// Wrap an existing directory (used by tests). No clone, no cleanup.
    pub fn from_dir(root: impl Into<PathBuf>) -> Self {
        Self {
            _tmp: None,
            root: root.into(),
        }
    }

    /// Shallow-clone `clone_url` (which must embed any auth) and check out
    /// `head_sha`. Falls back to the default branch if the sha can't be fetched.
    pub fn clone(clone_url: &str, head_sha: Option<&str>) -> Result<Self> {
        let tmp = tempfile::tempdir().context("create temp dir for clone")?;
        let root = tmp.path().to_path_buf();

        clone_with_retry(clone_url, &root)?;

        // Best-effort: fetch + check out the exact PR head. GitHub/Bitbucket allow
        // fetching a specific SHA; if it fails we keep the default-branch checkout.
        if let Some(sha) = head_sha {
            if run_git(
                &["fetch", "--depth", "1", "--quiet", "origin", sha],
                Some(&root),
            )
            .is_ok()
            {
                let _ = run_git(&["checkout", "--quiet", sha], Some(&root));
            }
        }

        Ok(Self {
            _tmp: Some(tmp),
            root,
        })
    }

    /// Resolve a repo-relative path and ensure it stays inside the clone root.
    fn resolve(&self, rel: &str) -> Result<PathBuf> {
        let candidate = self.root.join(rel.trim_start_matches('/'));
        let canon = candidate
            .canonicalize()
            .with_context(|| format!("path not found: {rel}"))?;
        let root = self.root.canonicalize()?;
        if !canon.starts_with(&root) {
            bail!("path escapes the repository: {rel}");
        }
        Ok(canon)
    }

    /// Read a file, optionally a 1-indexed inclusive line range. The caller
    /// (`run_tool`) caps the result for the conversation budget.
    pub fn read_file(&self, rel: &str, start: Option<usize>, end: Option<usize>) -> Result<String> {
        let path = self.resolve(rel)?;
        let content = std::fs::read_to_string(&path).with_context(|| format!("read {rel}"))?;
        let lines: Vec<&str> = content.lines().collect();
        let s = start.unwrap_or(1).max(1);
        let e = end.unwrap_or(lines.len()).min(lines.len());
        if s > lines.len() {
            return Ok(String::new());
        }
        let out: String = lines[s - 1..e]
            .iter()
            .enumerate()
            .map(|(i, l)| format!("{}: {l}", s + i))
            .collect::<Vec<_>>()
            .join("\n");
        Ok(out)
    }

    /// Read a file's raw content (no line numbers), sandboxed to the clone. Used
    /// by [`crate::blast`] to tree-sit changed files; the LLM-facing reader is
    /// [`Workspace::read_file`], which numbers lines.
    pub fn read_raw(&self, rel: &str) -> Result<String> {
        let path = self.resolve(rel)?;
        std::fs::read_to_string(&path).with_context(|| format!("read {rel}"))
    }

    /// List entries (dirs end with `/`) directly under a repo-relative directory.
    pub fn list_dir(&self, rel: &str) -> Result<Vec<String>> {
        let path = self.resolve(rel)?;
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&path).with_context(|| format!("list {rel}"))? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if entry.file_type()?.is_dir() {
                out.push(format!("{name}/"));
            } else {
                out.push(name);
            }
        }
        out.sort();
        Ok(out)
    }

    /// Regex-search the repo (respecting .gitignore), returning `path:line: text`
    /// matches up to `max_results`.
    ///
    /// Equivalent to [`Workspace::grep_with_context`] with no context lines.
    pub fn grep(&self, pattern: &str, max_results: usize) -> Result<Vec<String>> {
        self.grep_with_context(pattern, max_results, 0)
    }

    /// Regex-search the repo, returning each match with `context` lines either
    /// side in ripgrep's shape: `path:N: text` for a match, `path-N- text` for
    /// context, `--` between blocks.
    ///
    /// Context exists because a bare matching line proves a second site EXISTS
    /// but says nothing about what it does — and the reviewer's most common
    /// recorded miss is a defect that lives in the *interaction* of two sites that
    /// are each correct alone (`pr-review-docs/feedback/`: kuroko#1's `Clone` +
    /// `Drop`, wincrust#13's cap + count message, both vexar#63 misses). Judging
    /// that requires seeing both, and without context the reviewer must spend a
    /// second round-trip on `read_file` for every candidate.
    ///
    /// `max_results` still counts MATCHES, not output lines, so the caller's cap
    /// keeps its meaning; the caller is responsible for lowering it when asking
    /// for context, since each hit now costs `2 * context + 1` lines.
    pub fn grep_with_context(
        &self,
        pattern: &str,
        max_results: usize,
        context: usize,
    ) -> Result<Vec<String>> {
        let re = regex::Regex::new(pattern).with_context(|| format!("bad regex: {pattern}"))?;
        let mut out = Vec::new();
        let mut matches = 0usize;
        let root = self.root.canonicalize()?;

        for result in WalkBuilder::new(&root).hidden(false).build() {
            if matches >= max_results {
                break;
            }
            let entry = match result {
                Ok(e) => e,
                Err(_) => continue,
            };
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            let path = entry.path();
            // Skip large files (likely generated/binary).
            if path.metadata().map(|m| m.len() > 1_000_000).unwrap_or(true) {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(path) else {
                continue; // binary / non-utf8
            };
            let rel = path.strip_prefix(&root).unwrap_or(path).display();
            let lines: Vec<&str> = content.lines().collect();
            // Tracks the last line already emitted for this file so overlapping
            // context windows print once instead of repeating shared lines.
            let mut last_emitted: Option<usize> = None;
            for (i, line) in lines.iter().enumerate() {
                if !re.is_match(line) {
                    continue;
                }
                if context == 0 {
                    out.push(format!(
                        "{rel}:{}: {}",
                        i + 1,
                        crate::clip(line.trim(), 200)
                    ));
                } else {
                    let lo = i.saturating_sub(context);
                    let hi = (i + context).min(lines.len().saturating_sub(1));
                    // A gap since the previous block gets a `--`; an overlapping
                    // one resumes after the last line already printed, so shared
                    // context appears once.
                    let start = match last_emitted {
                        Some(prev) if lo > prev + 1 => {
                            out.push("--".to_string());
                            lo
                        }
                        Some(prev) => prev + 1,
                        None => lo,
                    };
                    for (j, ctx) in lines.iter().enumerate().take(hi + 1).skip(start) {
                        // Marked by whether the LINE matches, not by which
                        // iteration emitted it: a second match falling inside an
                        // already-printed window is still a match, and rendering
                        // it as context would hide a site from the reviewer.
                        let sep = if re.is_match(ctx) { ':' } else { '-' };
                        out.push(format!(
                            "{rel}{sep}{}{sep} {}",
                            j + 1,
                            crate::clip(ctx.trim(), 200)
                        ));
                    }
                    last_emitted = Some(hi);
                }
                matches += 1;
                if matches >= max_results {
                    break;
                }
            }
            if matches >= max_results {
                break;
            }
        }
        Ok(out)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

/// How long one clone attempt may take before it is killed, and how many
/// attempts are made.
///
/// Measured against production: successful shallow clones of these repos take
/// **2 to 4 seconds** (1–20 MB). 90s is more than an order of magnitude of
/// headroom for a slow day, and still far below the failure this exists to stop.
const CLONE_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(90);
const CLONE_ATTEMPTS: u32 = 3;

/// Shallow-clone `clone_url` into `root`, bounding each attempt and retrying.
///
/// ## Why this is not just `git clone`
///
/// A single unbounded attempt was losing whole reviews to a network blip. Four
/// production failures in one day read:
///
/// ```text
/// Failed to connect to github.com port 443 after 134192 ms
/// Failed to connect to github.com port 443 after 135126 ms
/// Failed to connect to github.com port 443 after 134852 ms
/// Failed to connect to github.com port 443 after 134760 ms
/// ```
///
/// Four failures inside a one-second band is not congestion — that is a
/// deterministic timeout being reached. It lines up with the kernel's default
/// SYN retry ladder (`tcp_syn_retries = 6`, roughly 127s of doubling backoff)
/// plus resolution overhead: the machine is SYNing an address that never
/// answers and waiting out the whole budget. `github.com` has several addresses,
/// so which one is drawn decides whether a review lives, and the very same clone
/// succeeded 4 seconds later on the next attempt.
///
/// Retrying alone would not have helped — each attempt costs those 134 seconds.
/// The timeout is the part that makes the retry affordable: a black-holed
/// address is abandoned in 90s rather than 134, and the next attempt usually
/// draws a different one.
///
/// Killed rather than waited on, because the point is to stop waiting. `git`
/// spawns `git-remote-https`, and killing the parent can leave the child holding
/// the socket, so the whole process group goes.
fn clone_with_retry(clone_url: &str, root: &Path) -> Result<()> {
    let mut last: Option<String> = None;

    for attempt in 1..=CLONE_ATTEMPTS {
        // A previous attempt may have left a partial tree behind; `git clone`
        // refuses a non-empty destination, so a retry into it would fail for a
        // reason that has nothing to do with the network.
        if root.exists() {
            let _ = std::fs::remove_dir_all(root);
        }
        let _ = std::fs::create_dir_all(root);

        match run_git_bounded(
            &[
                "clone",
                "--depth",
                "1",
                "--quiet",
                clone_url,
                root.to_str().unwrap(),
            ],
            None,
            CLONE_ATTEMPT_TIMEOUT,
        ) {
            Ok(()) => {
                if attempt > 1 {
                    tracing::info!("git clone succeeded on attempt {attempt}");
                }
                return Ok(());
            }
            Err(e) => {
                // Redacted: `clone_url` carries the token, and this string ends
                // up in a log, a Telegram message and a PR comment.
                tracing::warn!("git clone attempt {attempt}/{CLONE_ATTEMPTS} failed: {e}");
                last = Some(e.to_string());
                if attempt < CLONE_ATTEMPTS {
                    std::thread::sleep(Duration::from_secs(2 * attempt as u64));
                }
            }
        }
    }

    bail!(
        "git clone failed after {CLONE_ATTEMPTS} attempts: {}",
        last.unwrap_or_else(|| "unknown error".into())
    )
}

/// `run_git`, but killed if it outlives `timeout`.
///
/// Polls rather than blocking on `wait`: the standard library has no timed wait,
/// and a poll loop at this granularity costs nothing next to a network clone.
fn run_git_bounded(args: &[&str], cwd: Option<&Path>, timeout: Duration) -> Result<()> {
    let mut cmd = Command::new("git");
    cmd.args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let mut child = cmd.spawn().context("spawn git")?;

    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait().context("wait for git")? {
            Some(status) => {
                let out = child.wait_with_output().context("collect git output")?;
                if !status.success() {
                    bail!(
                        "git {:?} failed: {}",
                        args,
                        String::from_utf8_lossy(&out.stderr)
                    );
                }
                return Ok(());
            }
            None => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    bail!("git {:?} timed out after {}s", args, timeout.as_secs());
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

fn run_git(args: &[&str], cwd: Option<&Path>) -> Result<()> {
    let mut cmd = Command::new("git");
    cmd.args(args);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let out = cmd.output().context("spawn git")?;
    if !out.success_like() {
        bail!(
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

/// Tiny helper so the `run_git` success check reads clearly.
trait SuccessLike {
    fn success_like(&self) -> bool;
}
impl SuccessLike for std::process::Output {
    fn success_like(&self) -> bool {
        self.status.success()
    }
}

#[cfg(test)]
mod tests {
    use super::Workspace;
    use std::fs;

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/a.rs"), "fn alpha() {}\nfn beta() {}\n").unwrap();
        fs::write(dir.path().join("README.md"), "# hi\nalpha is here\n").unwrap();
        dir
    }

    #[test]
    fn read_file_range() {
        let d = fixture();
        let ws = Workspace::from_dir(d.path());
        let out = ws.read_file("src/a.rs", Some(2), Some(2)).unwrap();
        assert_eq!(out, "2: fn beta() {}");
    }

    #[test]
    fn list_dir_sorted() {
        let d = fixture();
        let ws = Workspace::from_dir(d.path());
        let entries = ws.list_dir("").unwrap();
        assert!(entries.contains(&"src/".to_string()));
        assert!(entries.contains(&"README.md".to_string()));
    }

    #[test]
    fn grep_finds_matches() {
        let d = fixture();
        let ws = Workspace::from_dir(d.path());
        let hits = ws.grep("alpha", 10).unwrap();
        assert!(hits.iter().any(|h| h.contains("src/a.rs")));
        assert!(hits.iter().any(|h| h.contains("README.md")));
    }

    /// The default path is unchanged: bare `path:line: text`, no context, no
    /// separators — so turning the feature off is a true no-op.
    #[test]
    fn zero_context_is_the_old_bare_line_format() {
        let d = fixture();
        let ws = Workspace::from_dir(d.path());
        let hits = ws.grep_with_context("alpha", 10, 0).unwrap();
        assert!(hits.iter().all(|h| !h.starts_with("--")), "{hits:?}");
        assert!(hits.iter().any(|h| h.contains(":") && h.contains("alpha")));
        assert_eq!(hits, ws.grep("alpha", 10).unwrap());
    }

    /// Context lines use `-` and the match uses `:`, so the model can tell which
    /// line actually matched — the whole point is judging the site, not finding it.
    #[test]
    fn context_lines_are_marked_differently_from_the_match() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("f.rs"),
            "one\ntwo\nthree\nNEEDLE\nfive\nsix\nseven\n",
        )
        .unwrap();
        let ws = Workspace::from_dir(d.path());
        let hits = ws.grep_with_context("NEEDLE", 10, 2).unwrap();
        let joined = hits.join("\n");
        assert!(joined.contains("f.rs:4: NEEDLE"), "{joined}");
        assert!(joined.contains("f.rs-2- two"), "{joined}");
        assert!(joined.contains("f.rs-6- six"), "{joined}");
        assert!(
            !joined.contains("one"),
            "window must not overreach: {joined}"
        );
    }

    /// Two matches whose windows overlap print the shared lines once, and a real
    /// gap gets a `--`. Without this a dense file repeats itself into the clip.
    #[test]
    fn overlapping_windows_are_merged_and_gaps_separated() {
        let d = tempfile::tempdir().unwrap();
        let mut body = String::from("HIT a\nHIT b\n");
        body.push_str(&"filler\n".repeat(20));
        body.push_str("HIT c\n");
        std::fs::write(d.path().join("g.rs"), body).unwrap();
        let ws = Workspace::from_dir(d.path());
        let hits = ws.grep_with_context("HIT", 10, 2).unwrap();
        let joined = hits.join("\n");
        // Both matches in the merged window keep the `:` marker...
        assert_eq!(joined.matches("g.rs:1: HIT a").count(), 1, "{joined}");
        assert_eq!(joined.matches("g.rs:2: HIT b").count(), 1, "{joined}");
        // ...the shared context line is printed once, not twice...
        assert_eq!(joined.matches("g.rs-3- filler").count(), 1, "{joined}");
        // ...and the far match is a separate block.
        assert_eq!(hits.iter().filter(|h| *h == "--").count(), 1, "{joined}");
        assert!(joined.contains("g.rs:23: HIT c"), "{joined}");
    }

    /// `max_results` counts MATCHES, not emitted lines — otherwise asking for
    /// context would silently shrink how many distinct sites you see.
    #[test]
    fn max_results_counts_matches_not_output_lines() {
        let d = tempfile::tempdir().unwrap();
        let mut body = String::new();
        for i in 0..10 {
            body.push_str(&format!("pad{i}\nHIT{i}\npad{i}\n"));
        }
        std::fs::write(d.path().join("h.rs"), body).unwrap();
        let ws = Workspace::from_dir(d.path());
        let hits = ws.grep_with_context("HIT", 3, 1).unwrap();
        let matched = hits.iter().filter(|h| h.contains(".rs:")).count();
        assert_eq!(matched, 3, "{hits:?}");
        assert!(
            hits.len() > matched,
            "context lines are present too: {hits:?}"
        );
    }

    #[test]
    fn sandbox_blocks_escape() {
        let d = fixture();
        let ws = Workspace::from_dir(d.path());
        assert!(ws.read_file("../../etc/passwd", None, None).is_err());
    }
}

#[cfg(test)]
mod clone_timeout_tests {
    use super::*;

    /// The timeout must actually kill the process, not merely report one.
    ///
    /// `240.0.0.1` is reserved (class E) and unroutable everywhere, so the
    /// connect hangs exactly the way a black-holed github.com address does. The
    /// production failures took **134 seconds** to give up; this asserts the
    /// bound is enforced in seconds instead.
    #[test]
    fn a_hanging_clone_is_killed_at_the_deadline() {
        let dst = std::env::temp_dir().join(format!("prc-clone-timeout-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dst);

        let started = Instant::now();
        let err = run_git_bounded(
            &[
                "clone",
                "--quiet",
                "https://240.0.0.1/x.git",
                dst.to_str().unwrap(),
            ],
            None,
            Duration::from_secs(3),
        )
        .expect_err("an unroutable clone must not succeed");
        let elapsed = started.elapsed();

        assert!(
            err.to_string().contains("timed out"),
            "expected a timeout, got: {err}"
        );
        // Generous upper bound: the point is that it is seconds, not the 134
        // the kernel's SYN ladder would otherwise cost.
        assert!(
            elapsed < Duration::from_secs(20),
            "took {elapsed:?}, so the deadline was not enforced"
        );
        let _ = std::fs::remove_dir_all(&dst);
    }

    /// A command that exits on its own must not be reported as a timeout, and
    /// its stderr must survive into the error.
    #[test]
    fn a_failing_command_reports_its_own_error() {
        // Not `rev-parse --bogus`: rev-parse echoes unknown arguments and exits
        // 0, so that premise was wrong and the test failed on the code being
        // right. An unknown SUBCOMMAND is a real non-zero exit.
        let err = run_git_bounded(&["definitely-not-a-command"], None, Duration::from_secs(30))
            .expect_err("an unknown subcommand must fail");
        let msg = err.to_string();
        assert!(
            !msg.contains("timed out"),
            "misreported as a timeout: {msg}"
        );
    }

    /// And the happy path still works, with output collected.
    #[test]
    fn a_successful_command_returns_ok() {
        run_git_bounded(&["--version"], None, Duration::from_secs(30)).expect("git --version");
    }
}
