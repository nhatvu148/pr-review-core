//! Minimal unified-diff parser. Produces the set of line numbers (on the new
//! side of each file) that appear in the diff, so we only anchor inline comments
//! to lines the provider will accept — anchoring outside the diff is rejected
//! (GitHub 422 / Bitbucket 400).

use std::collections::{HashMap, HashSet};

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

#[cfg(test)]
mod tests {
    use super::parse_valid_lines;

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
}
