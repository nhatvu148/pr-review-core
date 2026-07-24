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
    pub fn grep(&self, pattern: &str, max_results: usize) -> Result<Vec<String>> {
        let re = regex::Regex::new(pattern).with_context(|| format!("bad regex: {pattern}"))?;
        let mut out = Vec::new();
        let root = self.root.canonicalize()?;

        for result in WalkBuilder::new(&root).hidden(false).build() {
            if out.len() >= max_results {
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
            for (i, line) in content.lines().enumerate() {
                if re.is_match(line) {
                    out.push(format!(
                        "{rel}:{}: {}",
                        i + 1,
                        crate::clip(line.trim(), 200)
                    ));
                    if out.len() >= max_results {
                        break;
                    }
                }
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

    #[test]
    fn sandbox_blocks_escape() {
        let d = fixture();
        let ws = Workspace::from_dir(d.path());
        assert!(ws.read_file("../../etc/passwd", None, None).is_err());
    }
}
