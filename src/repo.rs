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
        // No `create_private_dir` here: `clone_with_retry`'s first iteration
        // already finds this path present and recreates it privately, so doing
        // it twice was pure duplicate work on every successful clone. The
        // directory is empty until the clone runs, so there is no window in
        // which anything sensitive sits in it at the looser mode.

        clone_with_retry(clone_url, &root)?;

        // Best-effort: fetch + check out the exact PR head. GitHub/Bitbucket allow
        // fetching a specific SHA; if it fails we keep the default-branch checkout.
        //
        // Bounded for the same reason the clone is, and it was not: this is a
        // second round-trip to the same host, so it can draw the same
        // black-holed address and stall for the same ~134 seconds. Being
        // best-effort makes that quieter, not cheaper — the review does not
        // fail, it just takes two minutes longer for nothing, which is harder
        // to notice than an outright failure.
        //
        // No retry, deliberately: the clone already succeeded, so landing on
        // the default branch instead of the PR head is a degradation the caller
        // already tolerates.
        if let Some(sha) = head_sha {
            if run_git_bounded(
                &["fetch", "--depth", "1", "--quiet", "origin", sha],
                Some(&root),
                CLONE_ATTEMPT_TIMEOUT,
            )
            .is_ok()
            {
                let _ = run_git_bounded(
                    &["checkout", "--quiet", sha],
                    Some(&root),
                    CLONE_ATTEMPT_TIMEOUT,
                );
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
            .with_context(|| not_found_reason(rel))?;
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

// There is deliberately no unbounded `git` helper in this module any more. Every
// call to the network here goes through `run_git_bounded`, so the timeout is not
// something a future call site has to remember to opt into — the only way to run
// git from this file is with a deadline. The unbounded `run_git` that used to sit
// below became dead code the moment the fetch was bounded too, and clippy said so;
// deleting it rather than silencing that is what keeps the property true.

/// Extensions whose contents the reviewer can never read, and which are
/// therefore not worth downloading or checking out.
///
/// The agent's tools are `read_file`, `list_dir` and `grep` — all text. A `.mp4`
/// in the workspace can only ever cost bytes: the diff already reports that the
/// file changed, and that is the whole of what a reviewer could say about it.
///
/// Deliberately **not** here: `.svg` (XML, and frequently hand-edited), and no
/// data format — `.json`, `.csv`, `.yaml` and friends are all readable and are
/// often the actual subject of a review.
///
/// This is an extension list rather than a size threshold because a size
/// threshold is not obtainable. Blob sizes live in the blobs, so asking for them
/// in a blobless clone (`git ls-tree --long`) lazily fetches every object in the
/// tree — measured on a 4 GB repo, that refilled `.git` from 32 MB to 237 MB and
/// was still climbing when it was killed. Filtering by size would have to
/// download exactly what the filter exists to avoid.
const UNREADABLE_EXTENSIONS: &[&str] = &[
    // Raster images. `.svg` is absent on purpose — see above.
    "png", "jpg", "jpeg", "gif", "bmp", "ico", "webp", "tif", "tiff", "psd", "ai",
    // Video and audio. The bulk of a docs site, and unreadable to a line-based tool.
    "mp4", "mov", "avi", "mkv", "webm", "m4v", "mp3", "wav", "ogg", "flac", "m4a",
    // Archives: opaque until extracted, and nothing here extracts them.
    "zip", "tar", "gz", "tgz", "bz2", "xz", "7z", "rar",
    // Compiled and packaged artefacts.
    "pdf", "exe", "dll", "so", "dylib", "bin", "dmg", "iso", "jar", "wasm", // Fonts.
    "woff", "woff2", "ttf", "otf", "eot",
];

/// Gitignore-style patterns for `sparse-checkout --no-cone`: everything, minus
/// the extensions above.
///
/// Cone mode cannot express this — it matches directory prefixes, and the files
/// being skipped are scattered through every directory of a docs tree.
fn sparse_patterns() -> Vec<String> {
    let mut patterns = Vec::with_capacity(UNREADABLE_EXTENSIONS.len() + 1);
    patterns.push("/*".to_string());
    patterns.extend(
        UNREADABLE_EXTENSIONS
            .iter()
            .map(|ext| format!("!*.{}", any_case(ext))),
    );
    patterns
}

/// Turn an extension into a glob that matches it in any case: `png` becomes
/// `[pP][nN][gG]`.
///
/// Sparse-checkout patterns are gitignore patterns, and those are case-sensitive
/// wherever the filesystem is — which is every Linux build host. `!*.png` leaves
/// `PHOTO.PNG` and `Clip.MP4` in the checkout, and on the repos this feature
/// exists for those are exactly the files that cost the most. A character class
/// per letter is the only form that covers mixed case too; a second uppercase
/// pattern would still miss `.Png`.
///
/// Non-alphabetic characters pass through, so `7z` becomes `7[zZ]`.
fn any_case(ext: &str) -> String {
    ext.chars()
        .map(|c| {
            if c.is_ascii_alphabetic() {
                format!("[{}{}]", c.to_ascii_lowercase(), c.to_ascii_uppercase())
            } else {
                c.to_string()
            }
        })
        .collect()
}

/// Signatures of a server or a local `git` that cannot do partial clone or
/// sparse checkout. Matched lowercase against the failure's stderr.
const CAPABILITY_REFUSALS: &[&str] = &[
    "filtering not recognized",
    "filter-spec",
    "unknown option",
    "unrecognized",
    "is not a git command",
    "not supported",
    "unsupported",
];

/// Whether a failure says the *strategy* is unavailable rather than that the
/// network is having a bad minute.
///
/// This used to be `!msg.contains("timed out")` — everything that was not a
/// timeout was taken as proof the strategy was missing. That was wrong in the
/// direction that hurts: a DNS failure, a refused connection or a reset TLS
/// handshake are all non-timeouts, and any one of them on the first attempt
/// latched `lean_available` off for the rest of the clone. A repo that needs the
/// lean path to fit inside the deadline would then spend its remaining attempts
/// on the full clone that cannot fit — reproducing the exact failure this exists
/// to prevent, triggered by a network blip that the retry alone would have
/// survived.
///
/// So the test is now positive: fall back only on a signature that names a
/// missing capability. Anything unrecognised — including every transient
/// network error — retries the lean path, which is the strategy that works.
fn is_capability_failure(err: &anyhow::Error) -> bool {
    let msg = err.to_string().to_ascii_lowercase();
    // A timeout is decisive on its own: the server understood the request
    // perfectly and the packets did not arrive.
    if msg.contains("timed out") {
        return false;
    }
    CAPABILITY_REFUSALS.iter().any(|sig| msg.contains(sig))
}

/// Why a path is not in the workspace: genuinely absent, or deliberately skipped.
///
/// The clone excludes media it cannot read, so those files are missing from the
/// worktree while being perfectly present in the repository. Reporting both as
/// `path not found` tells the model something false — and the model acts on it.
/// A diff touching `assets/logo.png` could draw a confident, wrong finding that
/// the asset does not exist, which is worse than no answer because it reads like
/// a real one.
///
/// This distinguishes them by extension rather than by asking the filesystem,
/// because there is nothing to ask: the blob was never fetched.
fn not_found_reason(rel: &str) -> String {
    let excluded = std::path::Path::new(rel)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .is_some_and(|e| UNREADABLE_EXTENSIONS.contains(&e.as_str()));

    if excluded {
        format!(
            "{rel} is excluded from this workspace: the clone skips media it cannot read. \
             The file may well exist in the repository — do not conclude that it is missing."
        )
    } else {
        format!("path not found: {rel}")
    }
}

/// Empty the clone destination so `git clone` will accept it.
///
/// A previous attempt may have left a partial tree behind, and `git clone`
/// refuses a non-empty destination — so a retry into it would fail for a reason
/// that has nothing to do with the network.
///
/// Errors here are propagated rather than swallowed. A real filesystem failure —
/// permissions, ENOSPC — used to fall through to a confusing git error *and*
/// burn the remaining attempts on sleeps first, which reports a network fault
/// for a full disk.
fn reset_clone_dir(root: &Path) -> Result<()> {
    if root.exists() {
        std::fs::remove_dir_all(root).context("clear the previous clone attempt")?;
    }
    create_private_dir(root).context("recreate the clone dir")
}

/// What is left of an attempt's budget, or an error if it is spent.
///
/// The lean path runs three git commands where there used to be one. Giving each
/// its own `CLONE_ATTEMPT_TIMEOUT` would let a single attempt run for 270s, and
/// 360s once the capability fallback also ran — silently tripling the bound that
/// `clone_with_retry` exists to enforce, for precisely the slow-but-working
/// networks this feature targets. One deadline is opened per attempt and every
/// step spends from it, so "each attempt is bounded by `CLONE_ATTEMPT_TIMEOUT`"
/// stays literally true however many commands an attempt is made of.
///
/// The message says "timed out" deliberately: an exhausted budget *is* a
/// timeout, and `is_capability_failure` must not read it as a missing
/// capability and latch the lean path off.
fn remaining(deadline: Instant) -> Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|left| !left.is_zero())
        .context("git clone attempt timed out: its budget was spent by an earlier step")
}

/// Clone without the blobs, then check out only what is readable.
///
/// Three steps, and the order is the point:
///
/// 1. `--filter=blob:none --no-checkout` — fetch the history and trees, no file
///    contents at all.
/// 2. `sparse-checkout set` — declare which paths the worktree will contain.
/// 3. `checkout` — git now fetches, in one batch, exactly the blobs those paths
///    need. The excluded ones are never requested.
///
/// Doing (1) alone does not help: a plain `--filter=blob:none` still materialises
/// the full worktree at checkout, so every blob comes down anyway. Measured on
/// `psj/docs/website` (4.0 GB, a Docusaurus site of `.mp4`s and multi-MB `.gif`s):
/// `--depth 1` took 115s, `--depth 1 --filter=blob:none` took 87s and both
/// produced 4.0 GB. This sequence takes **13s and 400 MB**, with all 29,939
/// markdown files present. The first two both blew the 90s deadline; a review of
/// that repo was simply impossible before.
fn run_lean_clone(clone_url: &str, root_arg: &str, root: &Path, deadline: Instant) -> Result<()> {
    run_git_bounded(
        &[
            "clone",
            "--depth",
            "1",
            "--filter=blob:none",
            "--no-checkout",
            "--quiet",
            clone_url,
            root_arg,
        ],
        None,
        remaining(deadline)?,
    )?;

    let patterns = sparse_patterns();
    let mut args = vec!["sparse-checkout", "set", "--no-cone"];
    args.extend(patterns.iter().map(String::as_str));
    run_git_bounded(&args, Some(root), remaining(deadline)?)?;

    run_git_bounded(&["checkout", "--quiet"], Some(root), remaining(deadline)?)
}

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
    // `git` takes the destination as a string, so a path that is not UTF-8 cannot
    // be passed at all. Reported once here rather than panicked at on every attempt.
    let root_arg = root
        .to_str()
        .context("clone destination path is not valid UTF-8")?;
    let mut last: Option<String> = None;
    // Latched, not re-tested per attempt: a server that refuses `--filter` will
    // refuse it every time, and re-probing would spend one doomed round-trip per
    // attempt for an answer that cannot change mid-clone.
    let mut lean_available = true;

    for attempt in 1..=CLONE_ATTEMPTS {
        reset_clone_dir(root)?;

        // One budget for the whole attempt, however many git commands it takes.
        let deadline = Instant::now() + CLONE_ATTEMPT_TIMEOUT;

        let mut outcome = if lean_available {
            run_lean_clone(clone_url, root_arg, root, deadline)
        } else {
            run_git_bounded(
                &["clone", "--depth", "1", "--quiet", clone_url, root_arg],
                None,
                remaining(deadline)?,
            )
        };

        // Partial clone needs `uploadpack.allowFilter` on the server, and
        // `sparse-checkout` needs git >= 2.25 locally. Neither is universal, and
        // a self-hosted instance is exactly where they are missing. Rather than
        // making the whole clone conditional on probing for them, treat any
        // non-timeout failure of the lean path as proof it is unavailable and
        // finish this same attempt the old way — so a server without partial
        // clone loses a fast round-trip, not a review.
        let lean_lacks_support =
            lean_available && outcome.as_ref().err().is_some_and(is_capability_failure);
        if lean_lacks_support {
            lean_available = false;
            tracing::info!("partial clone unavailable — falling back to a full shallow clone");
            reset_clone_dir(root)?;
            // Spends what is left of THIS attempt's budget, not a fresh one. A
            // capability refusal is answered by the server immediately, so in
            // practice that is nearly the whole of it.
            outcome = remaining(deadline).and_then(|left| {
                run_git_bounded(
                    &["clone", "--depth", "1", "--quiet", clone_url, root_arg],
                    None,
                    left,
                )
            });
        }

        match outcome {
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

/// Create `path` as a directory only this user can enter.
///
/// The clone writes the token-bearing remote URL into `.git/config`, so the
/// directory holding it must not be readable by other local users. A review flagged
/// the retry path for recreating it with `create_dir_all`, which is subject to the
/// umask — and measuring it showed the problem is wider than that: on this platform
/// `tempfile::tempdir()` itself yields **0755**, not the 0700 its documentation
/// describes. So the exposure predates the retry, and setting the mode explicitly
/// fixes both paths rather than restoring a property that was never there.
///
/// Not `#[cfg(unix)]`-only at the call sites: on other platforms this is a plain
/// `create_dir_all`, so callers do not have to branch.
fn create_private_dir(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)?;
        // `recursive(true)` does not re-apply the mode to a directory that
        // already existed, which is exactly the tempdir case above.
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
        use std::os::unix::fs::PermissionsExt;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(path)
    }
}

/// Replace the credentials in any `scheme://user:secret@host` with `***`.
///
/// The clone URL carries a token, and the error built from these arguments does
/// not stay in the process: it is logged, sent to Telegram, and posted on the
/// pull request as a failure notice. Fly scrubs its own log output, which is why
/// the token looked redacted in production — none of the other three paths do.
fn redact(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("://") {
        let (head, tail) = rest.split_at(at + 3);
        out.push_str(head);
        // Credentials end at the first `@`, and must not run past the end of the
        // authority — otherwise a later `@` in a path would swallow the host.
        match tail.find('@') {
            Some(a) if !tail[..a].contains('/') => {
                out.push_str("***");
                rest = &tail[a..];
            }
            _ => {
                rest = tail;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Kill the whole process group, not just the child.
///
/// `git` delegates the network to `git-remote-https`. Killing only the parent
/// leaves that helper alive holding the socket, so the resource this timeout
/// exists to reclaim is not reclaimed — and across three retries the leaked
/// helpers accumulate.
#[cfg(unix)]
fn kill_process_group(child: &mut std::process::Child) {
    // SAFETY: `kill` with a negated pid signals the process group of that pid.
    // The group exists because `process_group(0)` put the child in its own,
    // which also means this can never signal our own group.
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    let _ = child.kill();
}

#[cfg(not(unix))]
fn kill_process_group(child: &mut std::process::Child) {
    let _ = child.kill();
}

/// `run_git`, but killed if it outlives `timeout`.
///
/// Polls rather than blocking on `wait`: the standard library has no timed wait,
/// and a poll at this granularity costs nothing next to a network clone.
fn run_git_bounded(args: &[&str], cwd: Option<&Path>, timeout: Duration) -> Result<()> {
    let mut cmd = Command::new("git");
    cmd.args(args)
        // stdout is never read, so null it rather than pipe it. A piped stream
        // nobody drains fills its buffer and blocks the writer forever — which
        // here would stall a clone that was making progress and then have the
        // deadline kill it, reporting a network fault that never happened.
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let mut child = cmd.spawn().context("spawn git")?;

    // stderr is drained on its own thread for the same reason, and because the
    // message it carries is the only diagnosis a failure leaves behind.
    let mut pipe = child
        .stderr
        .take()
        .context("git stderr was piped above but is missing")?;
    let drain = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = std::io::Read::read_to_end(&mut pipe, &mut buf);
        buf
    });

    /// Why the loop breaks with a value instead of using `?`.
    ///
    /// `try_wait()` returning `Err` used to propagate straight out, skipping the
    /// kill, the reap and the thread join — leaking the child and its
    /// `git-remote-https` helper in exactly the way this function exists to
    /// prevent. Every exit from the loop now goes through the same cleanup.
    enum Outcome {
        Exited(std::process::ExitStatus),
        TimedOut,
        WaitFailed(std::io::Error),
    }

    let deadline = Instant::now() + timeout;
    let outcome = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Outcome::Exited(status),
            Ok(None) if Instant::now() >= deadline => {
                kill_process_group(&mut child);
                let _ = child.wait();
                break Outcome::TimedOut;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(e) => {
                kill_process_group(&mut child);
                let _ = child.wait();
                break Outcome::WaitFailed(e);
            }
        }
    };

    // Joinable now either way: the pipe closes when the process dies, so the
    // reader returns rather than hanging on a killed child.
    let stderr = drain.join().unwrap_or_default();

    match outcome {
        Outcome::WaitFailed(e) => {
            Err(e).with_context(|| format!("waiting for git {}", redact(&format!("{args:?}"))))
        }
        Outcome::TimedOut => bail!(
            "git {} timed out after {}s",
            redact(&format!("{args:?}")),
            timeout.as_secs()
        ),
        Outcome::Exited(status) if !status.success() => bail!(
            "git {} failed: {}",
            redact(&format!("{args:?}")),
            redact(&String::from_utf8_lossy(&stderr))
        ),
        Outcome::Exited(_) => Ok(()),
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

        // Deliberately NOT asserting the message says "timed out". Whether
        // `240.0.0.1` black-holes or is refused outright is a property of the
        // network the test runs on: a sandbox with a deny-all egress policy
        // sends a fast RST, and git then fails with its own error rather than
        // reaching our deadline. Both outcomes prove the thing under test — that
        // this call cannot hang — so asserting the mechanism would fail a
        // correct implementation on a restricted CI network.
        let _ = &err;

        // This is the real assertion: bounded, however it got there.
        assert!(
            elapsed < Duration::from_secs(20),
            "took {elapsed:?} — it hung. The production failures took 134s, and \
             whether this ends at our deadline or the network's refusal, it must \
             not wait that out."
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

    /// The token must never reach the error. It is logged, sent to Telegram and
    /// posted on the pull request; only the Fly log is scrubbed for us.
    #[test]
    fn the_token_never_reaches_the_error() {
        let secret = "ghs_SUPERSECRET1234567890";
        let url = format!("https://x-access-token:{secret}@github.com/o/r.git");
        let dst = std::env::temp_dir().join(format!("prc-redact-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dst);

        // A REAL host, unlike the sibling timeout test: the point here is only
        // that the message is redacted, and this fails fast on auth or DNS
        // rather than waiting out a deadline. The comment used to claim it was
        // unroutable, which was simply untrue of `github.com`.
        let err = run_git_bounded(
            &["clone", "--quiet", &url, dst.to_str().unwrap()],
            None,
            Duration::from_secs(3),
        )
        .expect_err("unroutable clone must fail");

        let msg = err.to_string();
        assert!(!msg.contains(secret), "token leaked into: {msg}");
        assert!(msg.contains("***"), "expected redaction marker in: {msg}");
        assert!(
            msg.contains("github.com/o/r.git"),
            "host was over-redacted: {msg}"
        );
        let _ = std::fs::remove_dir_all(&dst);
    }

    /// `redact` must not eat a URL that has no credentials, and must stop at the
    /// authority — a later `@` in a path is not a secret.
    #[test]
    fn redact_only_touches_credentials() {
        assert_eq!(
            redact("https://github.com/o/r.git"),
            "https://github.com/o/r.git"
        );
        assert_eq!(
            redact("https://x-access-token:abc@github.com/o/r.git"),
            "https://***@github.com/o/r.git"
        );
        assert_eq!(
            redact("https://github.com/o/r/blob/main/a@b.txt"),
            "https://github.com/o/r/blob/main/a@b.txt"
        );
        // Two of them in one string, as `{args:?}` can produce.
        assert_eq!(
            redact("[\"https://u:p@a.com/x\", \"https://u:p@b.com/y\"]"),
            "[\"https://***@a.com/x\", \"https://***@b.com/y\"]"
        );
    }

    /// The clone directory must not be readable by other local users: the clone
    /// writes the token-bearing remote URL into `.git/config`.
    ///
    /// Measured, not assumed — `tempfile::tempdir()` returned 0755 on the machine
    /// this was written on, despite documenting 0700, which is why the mode is set
    /// explicitly instead of relied upon.
    #[cfg(unix)]
    #[test]
    fn the_clone_directory_is_private() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("clone");
        create_private_dir(&root).expect("create");
        let mode = std::fs::metadata(&root).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "fresh dir is {mode:04o}, not private");

        // And again over a directory that already exists with a loose mode —
        // the retry path, and the tempdir itself.
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        create_private_dir(&root).expect("recreate over an existing dir");
        let mode = std::fs::metadata(&root).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "existing dir left at {mode:04o}");
    }

    /// Output larger than a pipe buffer must not deadlock. Before stdout was
    /// nulled and stderr drained on a thread, a chatty command would fill the
    /// buffer, block, and be killed by the deadline as if the network had failed.
    #[test]
    fn a_chatty_command_does_not_deadlock() {
        // `git help -a` prints well over a pipe buffer's worth to stdout.
        let started = Instant::now();
        let r = run_git_bounded(&["help", "-a"], None, Duration::from_secs(20));
        assert!(r.is_ok(), "chatty command failed: {r:?}");
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "took {:?} — it blocked on a full pipe",
            started.elapsed()
        );
    }
}

#[cfg(test)]
mod lean_clone_tests {
    use super::*;

    /// The pattern list must be an allow-everything followed by negations only.
    ///
    /// A missing leading `/*` inverts the whole thing — non-cone sparse-checkout
    /// with only negative patterns matches nothing, which would produce an empty
    /// worktree and a review with no context rather than a visible failure.
    #[test]
    fn sparse_patterns_include_everything_then_subtract() {
        let patterns = sparse_patterns();
        assert_eq!(patterns[0], "/*", "the first pattern must include the tree");
        assert!(
            patterns[1..].iter().all(|p| p.starts_with("!*.")),
            "every later pattern must be an extension negation: {patterns:?}"
        );
    }

    /// Formats the reviewer can actually read must never be skipped.
    ///
    /// `.svg` is the trap: it sits among image extensions in every mental list,
    /// but it is XML, it is hand-edited, and a diff that touches one is
    /// reviewable.
    #[test]
    fn readable_formats_are_not_excluded() {
        for readable in ["svg", "md", "json", "csv", "yaml", "toml", "rs", "ts"] {
            assert!(
                !UNREADABLE_EXTENSIONS.contains(&readable),
                ".{readable} is readable and must stay in the checkout"
            );
        }
    }

    /// Only a named missing capability may latch the lean path off.
    ///
    /// The first version of this asked `!msg.contains("timed out")`, so every
    /// transient network error counted as a missing capability. One DNS blip on
    /// attempt 1 then condemned attempts 2 and 3 to the full clone — which, on
    /// the repos this feature exists for, cannot fit in the deadline at all. The
    /// blip would have been survived by the retry; the latch turned it into a
    /// failed review.
    #[test]
    fn only_a_named_refusal_counts_as_a_missing_capability() {
        for transient in [
            "git [\"clone\"] timed out after 90s",
            "git [\"clone\"] failed: fatal: unable to access: Could not resolve host: example.com",
            "git [\"clone\"] failed: fatal: unable to access: Failed to connect to example.com port 443: Connection refused",
            "git [\"clone\"] failed: fatal: unable to access: OpenSSL SSL_read: Connection reset by peer",
            "git [\"clone\"] failed: error: RPC failed; curl 56 recv failure",
        ] {
            let err = anyhow::anyhow!("{transient}");
            assert!(
                !is_capability_failure(&err),
                "transient network failure must keep retrying the lean path: {transient}"
            );
        }

        for refusal in [
            "git [\"clone\"] failed: fatal: filtering not recognized by server, ignoring",
            "git [\"clone\"] failed: error: unknown option `filter=blob:none'",
            "git [\"sparse-checkout\"] failed: git: 'sparse-checkout' is not a git command",
            "git [\"clone\"] failed: fatal: invalid filter-spec 'blob:none'",
        ] {
            let err = anyhow::anyhow!("{refusal}");
            assert!(
                is_capability_failure(&err),
                "a named refusal must fall back: {refusal}"
            );
        }
    }

    /// An attempt's budget is shared by every command it runs.
    ///
    /// Both reviewers of this change caught the same thing independently: three
    /// sequential commands each holding the full `CLONE_ATTEMPT_TIMEOUT` meant
    /// one "bounded" attempt could run 270s, and 360s with the fallback.
    #[test]
    fn a_spent_budget_reads_as_a_timeout_not_a_missing_capability() {
        let already_gone = Instant::now() - Duration::from_secs(1);
        let err = remaining(already_gone).expect_err("a spent deadline must not yield time");
        assert!(
            !is_capability_failure(&err),
            "an exhausted budget is a timeout; reading it as a missing capability \
             would latch the lean path off for the remaining attempts: {err}"
        );

        let left = remaining(Instant::now() + Duration::from_secs(30))
            .expect("an open deadline must yield the time left");
        assert!(
            left <= Duration::from_secs(30) && !left.is_zero(),
            "{left:?}"
        );
    }

    /// Extensions are matched in any case.
    #[test]
    fn extension_globs_cover_every_case() {
        assert_eq!(any_case("png"), "[pP][nN][gG]");
        assert_eq!(
            any_case("7z"),
            "7[zZ]",
            "digits have no case and pass through"
        );
        assert!(
            sparse_patterns().contains(&"!*.[mM][pP]4".to_string()),
            "mp4 must be matched case-insensitively"
        );
    }

    /// End to end over `file://`, which is the only way to see the thing that
    /// matters: that the excluded blob is *never fetched*, not merely hidden.
    ///
    /// Asserting on the worktree alone would pass for a plain clone that checked
    /// the file out and deleted it, so this also asserts the object is absent
    /// from `.git` — that is the bandwidth claim, and it is the whole point.
    #[test]
    fn a_lean_clone_omits_unreadable_blobs_and_keeps_the_rest() {
        let src = tempfile::tempdir().unwrap();
        let s = src.path();
        let git = |args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .current_dir(s)
                .output()
                .expect("git runs");
            assert!(out.status.success(), "git {args:?}: {out:?}");
        };
        git(&["init", "--quiet", "--initial-branch=main"]);
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        // Partial clone is opt-in on the serving side, including for file://.
        git(&["config", "uploadpack.allowFilter", "true"]);

        std::fs::write(s.join("README.md"), "# readable\n").unwrap();
        std::fs::create_dir(s.join("assets")).unwrap();
        // Distinctive and incompressible enough that its absence is meaningful.
        let blob: Vec<u8> = (0..200_000u32)
            .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
            .collect();
        std::fs::write(s.join("assets/clip.mp4"), &blob).unwrap();
        std::fs::write(s.join("assets/diagram.svg"), "<svg></svg>\n").unwrap();
        // Mixed and upper case: gitignore patterns are case-sensitive wherever
        // the filesystem is, so these prove the character-class globs work
        // against real git rather than only against the pattern builder.
        std::fs::write(s.join("assets/PHOTO.PNG"), vec![1u8; 4096]).unwrap();
        std::fs::write(s.join("assets/Trailer.Mp4"), vec![2u8; 4096]).unwrap();
        git(&["add", "-A"]);
        git(&["commit", "--quiet", "-m", "seed"]);

        let mp4_oid = {
            let out = Command::new("git")
                .args(["rev-parse", "HEAD:assets/clip.mp4"])
                .current_dir(s)
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };

        let dst = tempfile::tempdir().unwrap();
        let root = dst.path().join("checkout");
        let url = format!("file://{}", s.display());
        clone_with_retry(&url, &root).expect("the lean clone must succeed over file://");

        assert!(
            root.join("README.md").is_file(),
            "readable text must be present"
        );
        assert!(
            root.join("assets/diagram.svg").is_file(),
            "svg is text and must survive the filter"
        );
        assert!(
            !root.join("assets/clip.mp4").exists(),
            "the excluded file must not be in the worktree"
        );
        assert!(
            !root.join("assets/PHOTO.PNG").exists(),
            "an upper-case extension must be excluded too"
        );
        assert!(
            !root.join("assets/Trailer.Mp4").exists(),
            "a mixed-case extension must be excluded too"
        );

        // The claim under test: not downloaded, not merely not checked out.
        let present = Command::new("git")
            .args(["cat-file", "-e", &mp4_oid])
            .current_dir(&root)
            .env("GIT_NO_LAZY_FETCH", "1")
            .status()
            .unwrap();
        assert!(
            !present.success(),
            "the excluded blob was fetched anyway — the filter bought nothing"
        );
    }
    /// A server without partial clone degrades, it does not fail.
    ///
    /// Written expecting the opposite, and the test was wrong: with
    /// `uploadpack.allowFilter` off — which is the **default** — the server does
    /// not reject `--filter`, it silently ignores it and sends every object. The
    /// clone therefore succeeds, and only the bandwidth saving is lost; the
    /// sparse checkout still applies, so the worktree is lean either way.
    ///
    /// That is worth pinning down, because it means the capability fallback is
    /// not the common path for old servers — it is reserved for a genuine error,
    /// such as a local git too old for `sparse-checkout`. It also means the
    /// worktree the reviewer sees is the same shape everywhere, which is the
    /// property the rest of the agent is entitled to assume.
    #[test]
    fn a_server_without_partial_clone_still_gets_a_lean_checkout() {
        let src = tempfile::tempdir().unwrap();
        let s = src.path();
        let git = |args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .current_dir(s)
                .output()
                .expect("git runs");
            assert!(out.status.success(), "git {args:?}: {out:?}");
        };
        git(&["init", "--quiet", "--initial-branch=main"]);
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        // Deliberately NOT enabling uploadpack.allowFilter: this is the default
        // server, and the lean clone must be refused by it.
        git(&["config", "uploadpack.allowFilter", "false"]);

        std::fs::write(s.join("README.md"), "# readable\n").unwrap();
        std::fs::write(s.join("clip.mp4"), vec![7u8; 1024]).unwrap();
        git(&["add", "-A"]);
        git(&["commit", "--quiet", "-m", "seed"]);

        let dst = tempfile::tempdir().unwrap();
        let root = dst.path().join("checkout");
        let url = format!("file://{}", s.display());
        clone_with_retry(&url, &root).expect("an ignored filter must not fail the clone");

        assert!(
            root.join("README.md").is_file(),
            "readable text must be checked out"
        );
        assert!(
            !root.join("clip.mp4").exists(),
            "the sparse checkout applies even when the filter was ignored, so the \
             worktree stays the same shape on every server"
        );
    }

    /// An excluded file must not be reported as a missing one.
    ///
    /// The clone skips media, so those paths are absent from the worktree while
    /// present in the repository. `path not found` is then a false statement the
    /// model acts on: a diff touching `assets/logo.png` can draw a confident,
    /// wrong finding that the asset does not exist. Caught by a reviewer on the
    /// bump PR, after the end-to-end run had missed it — that run's diff touched
    /// no media, so the agent never reached for an excluded path.
    #[test]
    fn an_excluded_path_says_so_instead_of_claiming_it_is_missing() {
        let d = tempfile::tempdir().unwrap();
        let ws = Workspace::from_dir(d.path());

        let err = ws
            .read_file("assets/logo.png", None, None)
            .expect_err("an excluded file is not in the worktree");
        let msg = err.to_string();
        assert!(
            msg.contains("excluded from this workspace"),
            "must name the real reason: {msg}"
        );
        assert!(
            !msg.contains("path not found"),
            "must not claim the file is missing from the repository: {msg}"
        );

        // Upper case reaches the same answer, since the clone excludes it too.
        let upper = ws
            .read_file("assets/LOGO.PNG", None, None)
            .expect_err("still not in the worktree");
        assert!(upper.to_string().contains("excluded from this workspace"));

        // A readable path that genuinely is not there keeps the plain message —
        // over-reporting "excluded" would hide a real typo in a path.
        let missing = ws
            .read_file("src/nope.rs", None, None)
            .expect_err("genuinely absent");
        assert!(
            missing.to_string().contains("path not found"),
            "a readable extension keeps the plain message: {missing}"
        );
    }
}
