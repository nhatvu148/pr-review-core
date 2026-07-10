//! Minimal unified-diff parser. Produces the set of line numbers (on the new
//! side of each file) that appear in the diff, so we only anchor inline comments
//! to lines the provider will accept — anchoring outside the diff is rejected
//! (GitHub 422 / Bitbucket 400).

use std::collections::{HashMap, HashSet};

use globset::{Glob, GlobSet, GlobSetBuilder};

/// Map each new-side file path to the set of line numbers present in the diff
/// (added or context lines). Removed lines don't advance the new-side counter.
///
/// # Examples
/// ```
/// # use pr_review_core::diff::parse_valid_lines;
/// let d = "+++ b/a.rs\n@@ -1,2 +1,3 @@\n ctx\n+added\n ctx2\n";
/// let m = parse_valid_lines(d);
/// assert!(m["a.rs"].contains(&2)); // the "+added" line is line 2 on the new side
/// ```
pub fn parse_valid_lines(diff: &str) -> HashMap<String, HashSet<u64>> {
    let mut map: HashMap<String, HashSet<u64>> = HashMap::new();
    let mut cur_path: Option<String> = None;
    let mut new_line: u64 = 0;

    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("+++ ") {
            let p = rest.trim();
            let p = p.strip_prefix("b/").unwrap_or(p);
            cur_path = if p == "/dev/null" {
                None
            } else {
                Some(p.to_string())
            };
        } else if line.starts_with("@@") {
            // @@ -old,n +new,m @@  — grab the start of the new-side range.
            if let Some(plus) = line.split('+').nth(1) {
                let num: String = plus.chars().take_while(|c| c.is_ascii_digit()).collect();
                new_line = num.parse().unwrap_or(0);
            }
        } else if let Some(path) = &cur_path {
            match line.chars().next() {
                Some('+') => {
                    map.entry(path.clone()).or_default().insert(new_line);
                    new_line += 1;
                }
                Some(' ') => {
                    map.entry(path.clone()).or_default().insert(new_line);
                    new_line += 1;
                }
                // '-' removed line: new side doesn't advance. Other markers ignored.
                _ => {}
            }
        }
    }
    map
}

/// Build a [`GlobSet`] from patterns, returning `None` if any pattern is invalid
/// (so callers can fail-open and skip filtering rather than lose the review).
fn build_globset(patterns: &[String]) -> Option<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for p in patterns {
        builder.add(Glob::new(p).ok()?);
    }
    builder.build().ok()
}

/// Extract the new-side (`b/`) path from a `diff --git a/PATH b/PATH` header line.
fn git_header_path(line: &str) -> Option<String> {
    let rest = line.strip_prefix("diff --git ")?;
    let idx = rest.find(" b/")?;
    let p = rest[idx + 3..].trim();
    (!p.is_empty()).then(|| p.to_string())
}

/// Derive a section's new-side path, preferring the `diff --git` header (when the
/// diff has git headers) and falling back to the `+++ b/PATH` line. Returns `None`
/// for `/dev/null` (deletions) or when no path can be found.
fn section_path(section: &[&str], has_git: bool) -> Option<String> {
    if has_git {
        if let Some(p) = section.first().and_then(|l| git_header_path(l)) {
            return Some(p);
        }
    }
    for l in section {
        if let Some(rest) = l.strip_prefix("+++ ") {
            let p = rest.trim();
            let p = p.strip_prefix("b/").unwrap_or(p);
            return (p != "/dev/null").then(|| p.to_string());
        }
    }
    None
}

/// Drop per-file sections of a unified diff that don't pass the include/exclude
/// glob filters — removing lockfiles, generated, vendored, and minified files
/// before the diff is sent to the LLM (saves tokens and noise).
///
/// A file section is KEPT iff `(include is empty OR include matches path) AND NOT
/// exclude matches path`. Sections whose path can't be derived are kept
/// (fail-open). If the diff can't be parsed into sections at all, the original
/// diff is returned unchanged with an empty dropped list — the review is never
/// lost to a parse failure.
///
/// Returns `(kept_diff, dropped_paths)`.
///
/// # Examples
/// ```
/// # use pr_review_core::diff::filter_diff_by_globs;
/// let d = "diff --git a/Cargo.lock b/Cargo.lock\n+++ b/Cargo.lock\n@@ -1 +1 @@\n+x\n\
///          diff --git a/src/a.rs b/src/a.rs\n+++ b/src/a.rs\n@@ -1 +1 @@\n+y\n";
/// let (kept, dropped) = filter_diff_by_globs(d, &[], &["**/Cargo.lock".to_string()]);
/// assert_eq!(dropped, vec!["Cargo.lock".to_string()]);
/// assert!(kept.contains("src/a.rs") && !kept.contains("Cargo.lock"));
/// ```
pub fn filter_diff_by_globs(
    diff: &str,
    include: &[String],
    exclude: &[String],
) -> (String, Vec<String>) {
    // Bad glob in either set -> fail-open (skip filtering, keep the whole diff).
    let (include_set, exclude_set) = match (build_globset(include), build_globset(exclude)) {
        (Some(i), Some(e)) => (i, e),
        _ => return (diff.to_string(), Vec::new()),
    };

    let lines: Vec<&str> = diff.lines().collect();
    let has_git = lines.iter().any(|l| l.starts_with("diff --git "));

    // Section start indices: `diff --git` lines, or (fallback) the `--- ` line
    // preceding each `+++ ` marker — or the `+++ ` line itself if none precedes.
    let starts: Vec<usize> = if has_git {
        lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.starts_with("diff --git "))
            .map(|(i, _)| i)
            .collect()
    } else {
        let mut s = Vec::new();
        for (i, l) in lines.iter().enumerate() {
            if l.starts_with("+++ ") {
                if i > 0 && lines[i - 1].starts_with("--- ") {
                    s.push(i - 1);
                } else {
                    s.push(i);
                }
            }
        }
        s
    };

    // Nothing parseable -> fail-open.
    if starts.is_empty() {
        return (diff.to_string(), Vec::new());
    }

    let mut kept: Vec<&str> = Vec::new();
    let mut dropped: Vec<String> = Vec::new();

    // Any preamble before the first section is always kept.
    kept.extend_from_slice(&lines[..starts[0]]);

    for (idx, &start) in starts.iter().enumerate() {
        let end = starts.get(idx + 1).copied().unwrap_or(lines.len());
        let section = &lines[start..end];
        match section_path(section, has_git) {
            Some(path) => {
                let keep = (include.is_empty() || include_set.is_match(&path))
                    && !exclude_set.is_match(&path);
                if keep {
                    kept.extend_from_slice(section);
                } else {
                    dropped.push(path);
                }
            }
            // Unknown path -> fail-open, keep the section.
            None => kept.extend_from_slice(section),
        }
    }

    // Nothing dropped -> return the original untouched (preserve exact bytes).
    if dropped.is_empty() {
        return (diff.to_string(), Vec::new());
    }

    let mut out = kept.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    (out, dropped)
}

#[cfg(test)]
mod tests {
    use super::{filter_diff_by_globs, parse_valid_lines};

    #[test]
    fn anchors_added_and_context_lines() {
        let d = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,2 +1,3 @@\n ctx1\n+added\n ctx2\n";
        let m = parse_valid_lines(d);
        let s = &m["a.rs"];
        assert!(s.contains(&1)); // ctx1  -> new line 1
        assert!(s.contains(&2)); // +added -> new line 2
        assert!(s.contains(&3)); // ctx2  -> new line 3
    }

    #[test]
    fn removed_lines_do_not_advance_new_side() {
        let d = "+++ b/x.rs\n@@ -1,3 +1,2 @@\n keep1\n-removed\n keep2\n";
        let m = parse_valid_lines(d);
        let s = &m["x.rs"];
        assert!(s.contains(&1)); // keep1 -> 1
        assert!(s.contains(&2)); // keep2 -> 2 (the removed line didn't advance)
        assert!(!s.contains(&3));
    }

    #[test]
    fn handles_multiple_files() {
        let d = "+++ b/a.rs\n@@ -0,0 +1 @@\n+one\n+++ b/b.rs\n@@ -0,0 +1 @@\n+two\n";
        let m = parse_valid_lines(d);
        assert!(m["a.rs"].contains(&1));
        assert!(m["b.rs"].contains(&1));
    }

    #[test]
    fn skips_dev_null_deletions() {
        let d = "+++ /dev/null\n@@ -1,1 +0,0 @@\n-gone\n";
        let m = parse_valid_lines(d);
        assert!(m.is_empty());
    }

    #[test]
    fn drops_lockfile_section_keeps_source_section() {
        let d = "diff --git a/package-lock.json b/package-lock.json\n\
                 index abc..def 100644\n\
                 --- a/package-lock.json\n\
                 +++ b/package-lock.json\n\
                 @@ -1 +1 @@\n\
                 -old\n\
                 +new\n\
                 diff --git a/src/x.ts b/src/x.ts\n\
                 index 111..222 100644\n\
                 --- a/src/x.ts\n\
                 +++ b/src/x.ts\n\
                 @@ -1 +1 @@\n\
                 -a\n\
                 +b\n";
        let exclude = vec!["**/package-lock.json".to_string()];
        let (kept, dropped) = filter_diff_by_globs(d, &[], &exclude);
        assert_eq!(dropped, vec!["package-lock.json".to_string()]);
        assert!(kept.contains("src/x.ts"));
        assert!(!kept.contains("package-lock.json"));
    }

    #[test]
    fn fallback_splits_on_plusplusplus_when_no_git_headers() {
        let d = "--- a/foo.js\n\
                 +++ b/foo.js\n\
                 @@ -1 +1 @@\n\
                 -x\n\
                 +y\n\
                 --- a/bar.min.js\n\
                 +++ b/bar.min.js\n\
                 @@ -1 +1 @@\n\
                 -a\n\
                 +b\n";
        let exclude = vec!["**/*.min.js".to_string()];
        let (kept, dropped) = filter_diff_by_globs(d, &[], &exclude);
        assert_eq!(dropped, vec!["bar.min.js".to_string()]);
        assert!(kept.contains("foo.js"));
        assert!(!kept.contains("bar.min.js"));
    }
}
