//! Committable suggestions: turning a finding's proposed fix into a block the
//! host renders with an *Apply* button, or withholding it when it cannot be
//! applied safely.
//!
//! A suggestion is not a snippet. The fenced content **replaces exactly the
//! lines the comment is anchored to**, so a block that reads fine as prose —
//! an excerpt, a patch, a differently-indented fragment — becomes a one-click
//! commit that corrupts the branch. Prose degrades gracefully when it is
//! slightly wrong; a suggestion does not.
//!
//! So every suggestion passes [`sanitize`] against the line it would replace,
//! and anything that fails is dropped. Dropping costs nothing: the finding
//! still posts, with the fix described in its body exactly as before. That
//! asymmetry — a withheld button is invisible, a wrong button is a bad commit —
//! is why every rule here fails closed.

/// Whether a provider renders a committable suggestion block natively.
///
/// GitHub and GitLab do, with different fence syntax (see [`fence_info`]).
/// Bitbucket Cloud has no equivalent: the block would render as an ordinary
/// code fence with a misleading `suggestion` label and no way to apply it.
///
/// `local` is excluded for a different reason — not that it cannot render one,
/// but that nothing in its output contract carries one. A local review returns
/// `summary_markdown` (built from the *unanchored* findings) and
/// `findings_detail`; the rendered inline bodies never leave `finish_review`, so
/// a block built here would be discarded unread. A local consumer that wants to
/// display the fix as code has [`Finding::suggestion`] in `findings_detail`, and
/// can run it through [`sanitize`] and [`render`] itself.
///
/// [`Finding::suggestion`]: crate::llm::Finding::suggestion
pub fn supports_suggestions(provider: &str) -> bool {
    matches!(provider, "github" | "gitlab")
}

/// The fence info string that makes a block committable on `provider`.
///
/// GitLab requires an explicit line range on the info string: `-0+0` means
/// "replace zero lines above and zero lines below the anchored line", i.e. that
/// line alone. GitHub infers the same range from the comment's own anchor.
/// Emitting GitHub's bare `suggestion` on GitLab renders an inert code block,
/// which is the failure this function exists to prevent.
fn fence_info(provider: &str) -> &'static str {
    match provider {
        "gitlab" => "suggestion:-0+0",
        _ => "suggestion",
    }
}

/// Render a sanitized suggestion as a fenced block for `provider`.
///
/// `body` is the finding's prose, which is kept above the block: the button
/// answers *what to write*, never *why*, and a suggestion with no reasoning
/// above it is a diff with no review attached.
pub fn render(provider: &str, body: &str, suggestion: &str) -> String {
    format!(
        "{}\n\n```{}\n{}\n```",
        body.trim_end(),
        fence_info(provider),
        suggestion
    )
}

/// Validate a model-proposed suggestion against the line it would replace,
/// returning the text to fence or `None` to withhold the button.
///
/// `current` is the exact new-side text of the anchored line, from
/// [`crate::diff::diff_line_texts`] — the same map the re-anchor step reads.
///
/// The suggestion may span several lines even though it replaces one: adding a
/// guard above a kept statement is the common case, and the host expands the
/// single anchored line into all of them.
pub fn sanitize(suggestion: &str, current: &str) -> Option<String> {
    let s = unwrap_fence(suggestion);
    // Trailing blank lines are invisible in the model's output and would commit
    // as real ones. Leading blank lines are load-bearing far less often than
    // they are drift, and stripping them keeps the indent check below reading
    // the line the model actually wrote.
    let s = s.trim_matches('\n');
    if s.trim().is_empty() {
        return None;
    }

    // Diff furniture: the model answered with a patch rather than a
    // replacement. Applying it would commit the markers verbatim. Only the
    // unambiguous forms are matched — a bare leading `-` or `+` is ordinary
    // code (a YAML list item, a negative literal) far more often than it is a
    // diff marker, and rejecting on it would withhold most real suggestions.
    if s.lines()
        .any(|l| l.starts_with("@@ ") || l.starts_with("+++ ") || l.starts_with("--- "))
    {
        return None;
    }

    // A fence inside the content would close ours early and spill the rest of
    // the block into the comment as prose.
    if s.contains("```") {
        return None;
    }

    let s = reindent(s, current);

    // A first line indented differently from the line it replaces is drift, not
    // a fix: the model wrote against a mental copy of the code rather than the
    // real one, and everything below it is suspect too. (`reindent` has already
    // supplied the indent in the common case where the model omitted it, so
    // what reaches here is a genuine mismatch.)
    if leading_ws(s.lines().next()?) == leading_ws(current) {
        return None;
    }

    // A button that commits the line already there is pure noise — and it is
    // what a model produces when it has flagged something real but cannot
    // express the fix, which is exactly when the prose matters most.
    if s == current.trim_end() {
        return None;
    }

    Some(s)
}

/// Strip a ```` ``` ```` fence the model wrapped its suggestion in.
///
/// Asked for replacement text, models routinely return it fenced — sometimes
/// already tagged `suggestion`. Left in place it would nest inside our own
/// fence and commit the backticks.
fn unwrap_fence(s: &str) -> &str {
    let t = s.trim_matches('\n');
    let Some(rest) = t.strip_prefix("```") else {
        return s;
    };
    // Drop the info string (```rust, ```suggestion, or nothing) and the closing
    // fence. A missing closing fence means this is not a wrapper at all.
    let Some(nl) = rest.find('\n') else {
        return s;
    };
    let after_info = &rest[nl + 1..];
    match after_info.rfind("```") {
        Some(end) => after_info[..end].trim_end_matches('\n'),
        None => s,
    }
}

/// Leading whitespace of a line.
fn leading_ws(l: &str) -> &str {
    &l[..l.len() - l.trim_start().len()]
}

/// Supply the anchored line's indentation when the model omitted it entirely.
///
/// Models write replacement text the way they write a code sample — flush left,
/// with the indentation implied by context. Committed verbatim that de-indents
/// the line. Repairing it here is what makes the feature fire at all; the
/// alternative is rejecting the majority of otherwise-correct suggestions.
///
/// Only the wholesale case is repaired: if *any* line already carries
/// indentation, the model was tracking indentation and its choices are left
/// alone for the check above to judge.
fn reindent(s: &str, current: &str) -> String {
    let indent = leading_ws(current);
    if indent.is_empty() || s.lines().any(|l| !leading_ws(l).is_empty()) {
        return s.to_string();
    }
    s.lines()
        .map(|l| {
            if l.trim().is_empty() {
                l.to_string()
            } else {
                format!("{indent}{l}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_a_clean_single_line_replacement() {
        let got = sanitize("    return null;", "    return x;");
        assert_eq!(got.as_deref(), Some("    return null;"));
    }

    #[test]
    fn one_line_may_expand_into_several() {
        let got = sanitize(
            "    if (p.expiresAt <= Date.now()) return null;\n    return issue(p.id);",
            "    return issue(p.id);",
        );
        assert_eq!(
            got.as_deref(),
            Some("    if (p.expiresAt <= Date.now()) return null;\n    return issue(p.id);")
        );
    }

    #[test]
    fn supplies_the_indent_the_model_omitted() {
        let got = sanitize("return null;", "        return x;");
        assert_eq!(got.as_deref(), Some("        return null;"));
    }

    #[test]
    fn reindents_every_line_of_a_flush_left_block() {
        let got = sanitize("if (a) return;\nreturn b;", "    return b;");
        assert_eq!(got.as_deref(), Some("    if (a) return;\n    return b;"));
    }

    #[test]
    fn declines_a_differently_indented_first_line() {
        // The model tracked indentation (line 2 has some) but got line 1 wrong.
        assert_eq!(
            sanitize("  if (a) return;\n    return b;", "    return b;"),
            None
        );
    }

    #[test]
    fn declines_a_no_op() {
        assert_eq!(sanitize("    return x;", "    return x;"), None);
        // Trailing whitespace on the original must not manufacture a difference.
        assert_eq!(sanitize("    return x;", "    return x;  "), None);
    }

    #[test]
    fn declines_a_patch() {
        assert_eq!(
            sanitize("@@ -1,2 +1,3 @@\n     return x;", "    return x;"),
            None
        );
        assert_eq!(sanitize("--- a/x.ts\n+++ b/x.ts", "    return x;"), None);
    }

    #[test]
    fn declines_an_inner_fence() {
        assert_eq!(sanitize("    x();\n```\nnote", "    y();"), None);
    }

    #[test]
    fn declines_empty() {
        assert_eq!(sanitize("", "    return x;"), None);
        assert_eq!(sanitize("\n  \n", "    return x;"), None);
    }

    #[test]
    fn unwraps_a_fence_the_model_added() {
        let got = sanitize("```ts\n    return null;\n```", "    return x;");
        assert_eq!(got.as_deref(), Some("    return null;"));
        let got = sanitize("```suggestion\n    return null;\n```", "    return x;");
        assert_eq!(got.as_deref(), Some("    return null;"));
    }

    #[test]
    fn a_yaml_list_item_is_not_a_diff_marker() {
        let got = sanitize("  - name: build", "  - name: biuld");
        assert_eq!(got.as_deref(), Some("  - name: build"));
    }

    #[test]
    fn gitlab_needs_an_explicit_range_on_the_fence() {
        assert_eq!(fence_info("gitlab"), "suggestion:-0+0");
        assert_eq!(fence_info("github"), "suggestion");
    }

    /// Only the two hosts that both render the block *and* receive a rendered
    /// comment body from this crate.
    ///
    /// `local` is the trap: it renders markdown fine, but `run_review_local`
    /// returns `summary_markdown` and `findings_detail` — never the inline
    /// bodies — so a block built for it is discarded unread, and claiming
    /// support would put a number in `funnel.suggested` for output nobody gets.
    #[test]
    fn only_hosts_that_receive_a_rendered_body_get_suggestions() {
        assert!(supports_suggestions("github"));
        assert!(supports_suggestions("gitlab"));
        assert!(!supports_suggestions("bitbucket"));
        assert!(!supports_suggestions(crate::review::LOCAL_PROVIDER));
    }

    #[test]
    fn render_keeps_the_prose_above_the_block() {
        let out = render("github", "⚠️ **HIGH** — Off by one.", "    i += 1;");
        assert_eq!(
            out,
            "⚠️ **HIGH** — Off by one.\n\n```suggestion\n    i += 1;\n```"
        );
    }
}
