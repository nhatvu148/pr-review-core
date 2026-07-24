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
                // Added ('+') and context (' ') lines both exist on the new side,
                // so both are valid anchors and both advance the new-side counter.
                Some('+') | Some(' ') => {
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

/// Split a unified diff into per-file sections, returning `(path, section_text)`
/// for each. Sections start at `diff --git ` lines (primary) or, when the diff
/// carries no git headers, at the `--- ` line preceding each `+++ ` marker (or
/// the `+++ ` line itself). This is the same splitting the glob filter and the
/// size packer both build on.
///
/// The derived `path` is the new-side (`b/`) path; it is the empty string for a
/// section whose path can't be determined (a `/dev/null` deletion or a leading
/// preamble). `section_text` reproduces the section's lines with a trailing
/// newline, so concatenating every section reconstructs the diff.
///
/// If the diff can't be parsed into any section, a single entry
/// `("".to_string(), diff.to_string())` is returned so callers degrade
/// gracefully rather than losing the diff.
///
/// # Examples
/// ```
/// # use pr_review_core::diff::split_diff_sections;
/// let d = "diff --git a/src/a.rs b/src/a.rs\n+++ b/src/a.rs\n@@ -1 +1 @@\n+y\n";
/// let secs = split_diff_sections(d);
/// assert_eq!(secs.len(), 1);
/// assert_eq!(secs[0].0, "src/a.rs");
/// ```
pub fn split_diff_sections(diff: &str) -> Vec<(String, String)> {
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

    // Nothing parseable -> single fallback entry so callers never lose the diff.
    if starts.is_empty() {
        return vec![(String::new(), diff.to_string())];
    }

    // Reproduce a slice of lines as text with a trailing newline; concatenating
    // every section's text reconstructs the original diff.
    let emit = |slice: &[&str]| -> String {
        let mut t = slice.join("\n");
        if !t.is_empty() {
            t.push('\n');
        }
        t
    };

    let mut sections: Vec<(String, String)> = Vec::new();

    // Any preamble before the first section is carried as an empty-path section.
    if starts[0] > 0 {
        sections.push((String::new(), emit(&lines[..starts[0]])));
    }

    for (idx, &start) in starts.iter().enumerate() {
        let end = starts.get(idx + 1).copied().unwrap_or(lines.len());
        let section = &lines[start..end];
        let path = section_path(section, has_git).unwrap_or_default();
        sections.push((path, emit(section)));
    }

    sections
}

/// Priority score for a file path, used to decide which whole files to keep when
/// a diff is too large: source code = 2, tests/specs/fixtures/snapshots = 1,
/// docs/config/other = 0. An empty (preamble/unknown) path scores 2 so it's
/// preserved. Deliberately a simple, documented heuristic.
fn file_priority(path: &str) -> u8 {
    let p = path.to_ascii_lowercase();
    // Tests, specs, fixtures, snapshots — useful context but lower value.
    if p.contains("test")
        || p.contains("spec")
        || p.contains("__tests__")
        || p.contains(".snap")
        || p.contains("fixtures")
    {
        return 1;
    }
    // Docs, config, and other low-signal files.
    if p.ends_with(".md")
        || p.ends_with(".txt")
        || p.starts_with("docs/")
        || p.contains("/docs/")
        || p.ends_with(".lock")
        || (p.ends_with(".json") && !p.contains('/'))
    {
        return 0;
    }
    // Everything else (including source and unknown/preamble) is highest value.
    2
}

/// Fit `diff` within `max_chars` by keeping whole file sections, dropping the
/// lowest-priority files first. Returns `(packed_diff, dropped_paths)`. If the
/// diff already fits, returns it unchanged with an empty dropped list.
///
/// Sections are ranked by [`file_priority`] (DESC) then by char-length (ASC, so
/// more small high-value files fit), then greedily accumulated while the running
/// total stays within budget. Kept sections are re-emitted in their ORIGINAL
/// diff order so the packed diff still reads top-to-bottom. If not even the
/// smallest single section fits, the best one is kept anyway (never returns
/// empty for a non-empty diff — a downstream safety clamp trims the hard cap).
///
/// # Examples
/// ```
/// # use pr_review_core::diff::pack_diff;
/// let d = "diff --git a/src/a.rs b/src/a.rs\n+++ b/src/a.rs\n@@ -1 +1 @@\n+y\n";
/// let (packed, dropped) = pack_diff(d, 10_000);
/// assert_eq!(packed, d);
/// assert!(dropped.is_empty());
/// ```
pub fn pack_diff(diff: &str, max_chars: usize) -> (String, Vec<String>) {
    if diff.chars().count() <= max_chars {
        return (diff.to_string(), Vec::new());
    }

    let sections = split_diff_sections(diff);

    // Rank order: priority DESC, then section length ASC (prefer more, smaller
    // high-value files). Original indices are preserved for output ordering.
    let mut order: Vec<usize> = (0..sections.len()).collect();
    order.sort_by(|&a, &b| {
        let (pa, pb) = (file_priority(&sections[a].0), file_priority(&sections[b].0));
        pb.cmp(&pa).then_with(|| {
            sections[a]
                .1
                .chars()
                .count()
                .cmp(&sections[b].1.chars().count())
        })
    });

    // Greedily keep sections whose cumulative length stays within budget; skip
    // (drop) any that would overflow, still trying smaller later ones.
    let mut keep = vec![false; sections.len()];
    let mut used = 0usize;
    for &i in &order {
        let len = sections[i].1.chars().count();
        if used + len <= max_chars {
            keep[i] = true;
            used += len;
        }
    }

    // Fail-safe: if nothing fit (a single file larger than the whole budget),
    // keep the best-ranked section so we never return empty — the caller's
    // safety clamp trims it to the hard cap.
    if !keep.iter().any(|&k| k) {
        if let Some(&best) = order.first() {
            keep[best] = true;
        }
    }

    // Re-emit kept sections in original order; report dropped (named) paths.
    let mut out = String::new();
    let mut dropped: Vec<String> = Vec::new();
    for (i, (path, text)) in sections.iter().enumerate() {
        if keep[i] {
            out.push_str(text);
        } else if !path.is_empty() {
            dropped.push(path.clone());
        }
    }

    (out, dropped)
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

    let sections = split_diff_sections(diff);

    let mut out = String::new();
    let mut dropped: Vec<String> = Vec::new();
    for (path, text) in &sections {
        // Empty path = preamble / undetermined section -> fail-open, keep it.
        let keep = path.is_empty()
            || ((include.is_empty() || include_set.is_match(path)) && !exclude_set.is_match(path));
        if keep {
            out.push_str(text);
        } else {
            dropped.push(path.clone());
        }
    }

    // Nothing dropped -> return the original untouched (preserve exact bytes).
    if dropped.is_empty() {
        return (diff.to_string(), Vec::new());
    }

    (out, dropped)
}

#[cfg(test)]
mod tests {
    use super::{filter_diff_by_globs, pack_diff, parse_valid_lines, split_diff_sections};

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

    #[test]
    fn split_sections_roundtrips_multi_file_diff() {
        let d = "diff --git a/src/a.rs b/src/a.rs\n\
                 +++ b/src/a.rs\n\
                 @@ -1 +1 @@\n\
                 +a\n\
                 diff --git a/README.md b/README.md\n\
                 +++ b/README.md\n\
                 @@ -1 +1 @@\n\
                 +docs\n";
        let secs = split_diff_sections(d);
        assert_eq!(secs.len(), 2);
        assert_eq!(secs[0].0, "src/a.rs");
        assert_eq!(secs[1].0, "README.md");
        // Concatenating the section texts reconstructs the diff.
        let joined: String = secs.iter().map(|(_, t)| t.as_str()).collect();
        assert_eq!(joined, d);
    }

    #[test]
    fn pack_under_budget_returns_unchanged() {
        let d = "diff --git a/src/a.rs b/src/a.rs\n+++ b/src/a.rs\n@@ -1 +1 @@\n+a\n";
        let (packed, dropped) = pack_diff(d, 10_000);
        assert_eq!(packed, d);
        assert!(dropped.is_empty());
    }

    #[test]
    fn pack_drops_low_priority_large_file_keeps_small_source() {
        // A big low-priority docs file and a small source file. Budget only fits
        // the small source section, so README.md must be dropped.
        let big_docs = "x".repeat(500);
        let d = format!(
            "diff --git a/README.md b/README.md\n\
             +++ b/README.md\n\
             @@ -1 +1 @@\n\
             +{big_docs}\n\
             diff --git a/src/a.rs b/src/a.rs\n\
             +++ b/src/a.rs\n\
             @@ -1 +1 @@\n\
             +small\n"
        );
        let (packed, dropped) = pack_diff(&d, 120);
        assert_eq!(dropped, vec!["README.md".to_string()]);
        assert!(packed.contains("src/a.rs"));
        assert!(!packed.contains("README.md"));
    }

    #[test]
    fn pack_preserves_original_file_order() {
        // Small source file first, small test file second. A tiny docs file is
        // dropped. Kept sections must come back in original (source, test) order
        // even though the ranking would sort the test section differently.
        let d = "diff --git a/src/a.rs b/src/a.rs\n\
                 +++ b/src/a.rs\n\
                 @@ -1 +1 @@\n\
                 +alpha\n\
                 diff --git a/src/a.spec.rs b/src/a.spec.rs\n\
                 +++ b/src/a.spec.rs\n\
                 @@ -1 +1 @@\n\
                 +beta\n\
                 diff --git a/README.md b/README.md\n\
                 +++ b/README.md\n\
                 @@ -1 +1 @@\n\
                 +gammagammagammagammagammagamma\n";
        let (packed, dropped) = pack_diff(d, 160);
        assert_eq!(dropped, vec!["README.md".to_string()]);
        let a = packed.find("src/a.rs").expect("source kept");
        let b = packed.find("src/a.spec.rs").expect("spec kept");
        assert!(a < b, "kept sections should be in original diff order");
    }

    #[test]
    fn pack_keeps_single_oversized_file_rather_than_empty() {
        // One file, larger than the budget: must still be returned (safety clamp
        // downstream trims it) — never empty, nothing reported as dropped.
        let big = "y".repeat(400);
        let d = format!("diff --git a/src/a.rs b/src/a.rs\n+++ b/src/a.rs\n@@ -1 +1 @@\n+{big}\n");
        let (packed, dropped) = pack_diff(&d, 50);
        assert!(!packed.is_empty());
        assert!(packed.contains("src/a.rs"));
        assert!(dropped.is_empty());
    }
}
