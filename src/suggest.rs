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
/// All three do, and each was confirmed by posting this crate's own output to a
/// live pull request and watching the host render an *Apply* button — not read off
/// a documentation page. Only the fence differs (see [`fence_info`]).
///
/// Bitbucket was nearly excluded on a false premise. Atlassian's docs describe
/// only a UI path for suggestions — a toolbar button, `/suggestcode` — and publish
/// no raw-markdown form, which reads like the feature is unavailable to an API
/// client. It is not: an ordinary ```` ```suggestion ```` fence in a comment's
/// `content.raw` renders as a *Suggested change* with an *Apply suggestion*
/// button, exactly as on GitHub. Absent documentation was not absent capability,
/// and one posted comment settled what no amount of reading could.
///
/// `local` is included again. It was dropped when a local review's rendered
/// bodies died inside `finish_review` and a block built for it would have been
/// discarded unread — a true statement about the output contract, not about the
/// renderer. `RunReviewOutput::inline_detail` now carries them on every path, so
/// the block reaches the caller, and the local path is where you *want* it: it is
/// the one place a suggestion can be read before any of it touches a PR.
///
/// [`RunReviewOutput::inline_detail`]: crate::review::RunReviewOutput::inline_detail
pub fn supports_suggestions(provider: &str) -> bool {
    matches!(
        provider,
        "github" | "gitlab" | "bitbucket" | crate::review::LOCAL_PROVIDER
    )
}

/// The fence info string that makes a block committable on `provider`.
///
/// GitLab requires an explicit line range on the info string: `-0+0` means
/// "replace zero lines above and zero lines below the anchored line", i.e. that
/// line alone. GitHub and Bitbucket both infer the same range from the comment's
/// own anchor and take the bare `suggestion`. Emitting that bare form on GitLab
/// renders an inert code block, which is the failure this function exists to
/// prevent.
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
/// `prev` and `next` are the new-side texts of the lines immediately above and
/// below the anchor, when the diff shows them. They are not context for judging
/// the fix — they are how an echoed line is caught; see the check below.
pub fn sanitize(
    suggestion: &str,
    current: &str,
    prev: Option<&str>,
    next: Option<&str>,
) -> Option<String> {
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
    if leading_ws(s.lines().next()?) != leading_ws(current) {
        return None;
    }

    // A button that commits the line already there is pure noise — and it is
    // what a model produces when it has flagged something real but cannot
    // express the fix, which is exactly when the prose matters most.
    if s == current.trim_end() {
        return None;
    }

    // An echoed neighbour. A multi-line suggestion replaces the anchored line
    // ALONE, so a model that helpfully includes the line above or below — a very
    // common habit, since that is how one writes a code sample — produces a block
    // that commits a duplicate of a line already in the file. Nothing downstream
    // catches it: the result parses, compiles in many languages, and reads as
    // correct in the review. This is the one-click failure the module exists to
    // prevent, and it is invisible in the rendered block unless the reader has
    // the surrounding file in mind.
    let lines: Vec<&str> = s.lines().collect();
    if lines.len() > 1 {
        let echoes = |a: &str, b: Option<&str>| b.is_some_and(|t| a.trim() == t.trim());
        if echoes(lines[0], prev) || echoes(lines[lines.len() - 1], next) {
            return None;
        }
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

    /// `sanitize` for a line with no diff neighbours — the shape most rules are
    /// about. The neighbour rule has its own tests below.
    fn sanitize_alone(suggestion: &str, current: &str) -> Option<String> {
        sanitize(suggestion, current, None, None)
    }

    #[test]
    fn keeps_a_clean_single_line_replacement() {
        let got = sanitize_alone("    return null;", "    return x;");
        assert_eq!(got.as_deref(), Some("    return null;"));
    }

    #[test]
    fn one_line_may_expand_into_several() {
        let got = sanitize_alone(
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
        let got = sanitize_alone("return null;", "        return x;");
        assert_eq!(got.as_deref(), Some("        return null;"));
    }

    #[test]
    fn reindents_every_line_of_a_flush_left_block() {
        let got = sanitize_alone("if (a) return;\nreturn b;", "    return b;");
        assert_eq!(got.as_deref(), Some("    if (a) return;\n    return b;"));
    }

    #[test]
    fn declines_a_differently_indented_first_line() {
        // The model tracked indentation (line 2 has some) but got line 1 wrong.
        assert_eq!(
            sanitize_alone("  if (a) return;\n    return b;", "    return b;"),
            None
        );
    }

    #[test]
    fn declines_a_no_op() {
        assert_eq!(sanitize_alone("    return x;", "    return x;"), None);
        // Trailing whitespace on the original must not manufacture a difference.
        assert_eq!(sanitize_alone("    return x;", "    return x;  "), None);
    }

    #[test]
    fn declines_a_patch() {
        assert_eq!(
            sanitize_alone("@@ -1,2 +1,3 @@\n     return x;", "    return x;"),
            None
        );
        assert_eq!(
            sanitize_alone("--- a/x.ts\n+++ b/x.ts", "    return x;"),
            None
        );
    }

    #[test]
    fn declines_an_inner_fence() {
        assert_eq!(sanitize_alone("    x();\n```\nnote", "    y();"), None);
    }

    #[test]
    fn declines_empty() {
        assert_eq!(sanitize_alone("", "    return x;"), None);
        assert_eq!(sanitize_alone("\n  \n", "    return x;"), None);
    }

    #[test]
    fn unwraps_a_fence_the_model_added() {
        let got = sanitize_alone("```ts\n    return null;\n```", "    return x;");
        assert_eq!(got.as_deref(), Some("    return null;"));
        let got = sanitize_alone("```suggestion\n    return null;\n```", "    return x;");
        assert_eq!(got.as_deref(), Some("    return null;"));
    }

    #[test]
    fn a_yaml_list_item_is_not_a_diff_marker() {
        let got = sanitize_alone("  - name: build", "  - name: biuld");
        assert_eq!(got.as_deref(), Some("  - name: build"));
    }

    /// A multi-line suggestion that echoes the line below the anchor would
    /// commit a duplicate of it — the failure is silent, since the result parses
    /// and reads as correct in the rendered block.
    #[test]
    fn declines_a_suggestion_that_echoes_the_next_line() {
        assert_eq!(
            sanitize(
                "    let t = compute();\n    return t;",
                "    let t = old();",
                None,
                Some("    return t;"),
            ),
            None
        );
    }

    #[test]
    fn declines_a_suggestion_that_echoes_the_previous_line() {
        assert_eq!(
            sanitize(
                "    let t = compute();\n    return t;",
                "    return old();",
                Some("    let t = compute();"),
                None,
            ),
            None
        );
    }

    /// The rule is about *echoed* neighbours, not about multi-line suggestions.
    #[test]
    fn a_multi_line_suggestion_with_no_echo_is_kept() {
        let got = sanitize(
            "    if (!t) return;\n    return t;",
            "    return t;",
            Some("    let t = compute();"),
            Some("}"),
        );
        assert_eq!(got.as_deref(), Some("    if (!t) return;\n    return t;"));
    }

    /// A single-line suggestion equal to a neighbour is a legitimate fix — moving
    /// a line, or making this line match the one above it.
    #[test]
    fn a_one_line_suggestion_may_equal_a_neighbour() {
        let got = sanitize(
            "    return t;",
            "    return old();",
            None,
            Some("    return t;"),
        );
        assert_eq!(got.as_deref(), Some("    return t;"));
    }

    #[test]
    fn gitlab_needs_an_explicit_range_on_the_fence() {
        assert_eq!(fence_info("gitlab"), "suggestion:-0+0");
        assert_eq!(fence_info("github"), "suggestion");
        assert_eq!(fence_info("bitbucket"), "suggestion");
    }

    /// Every host this crate posts to renders a suggestion; all three were
    /// verified on a live pull request rather than from documentation.
    #[test]
    fn every_host_gets_suggestions() {
        assert!(supports_suggestions("github"));
        assert!(supports_suggestions("gitlab"));
        assert!(supports_suggestions("bitbucket"));
        // Local reads its blocks out of `inline_detail` — see the doc comment.
        assert!(supports_suggestions(crate::review::LOCAL_PROVIDER));
        assert!(!supports_suggestions("nonesuch"));
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
