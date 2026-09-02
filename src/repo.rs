//! Repo workspace for the agentic reviewer: a shallow clone of the PR head, plus
//! the read-only tools the model uses to investigate cross-file context
//! (`read_file`, `list_dir`, `grep`). All paths are sandboxed to the clone root.

use std::path::{Path, PathBuf};
use std::process::Command;

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

        run_git(
            &[
                "clone",
                "--depth",
                "1",
                "--quiet",
                clone_url,
                root.to_str().unwrap(),
            ],
            None,
        )
        .context("git clone failed")?;

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
