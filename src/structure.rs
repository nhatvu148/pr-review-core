//! Structural context: tell the review model WHICH functions/symbols a PR's
//! changed lines belong to, computed cheaply from the diff (and, when possible,
//! the full new-side files) without the agentic clone.
//!
//! Two tiers, both fully fail-open — a parse/fetch/language error only skips that
//! file and never fails the review:
//! - **Tier A** ([`hunk_context`]): reads the enclosing definition git already
//!   prints after the second `@@` on a hunk header. Pure string parsing, no deps.
//! - **Tier B** ([`structural_context`]): fetches each changed file at the PR head
//!   and uses tree-sitter to name the smallest enclosing definition of every
//!   changed line. Falls back to Tier A for any file it can't cover.

use std::collections::HashSet;

use reqwest::Client;
use tree_sitter::{Node, Parser, Point};

use crate::config::Config;
use crate::diff::{parse_valid_lines, split_diff_sections};
use crate::providers::{PrMeta, Provider};

// ---------------------------------------------------------------------------
// Tier A — hunk-header context (no heavy deps)
// ---------------------------------------------------------------------------

/// Extract the trailing text after the second `@@` of a hunk header, e.g. for
/// `@@ -10,7 +10,8 @@ func processOrder(o Order) {` this returns
/// `func processOrder(o Order) {`. Returns `None` when there's no trailing text.
fn hunk_trailing(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("@@")?;
    let idx = rest.find("@@")?;
    let trailing = rest[idx + 2..].trim();
    (!trailing.is_empty()).then_some(trailing)
}

/// Parse the new-side path from a `+++ ` marker line (`+++ b/PATH`). `None` for a
/// `/dev/null` deletion.
fn plusplus_path(line: &str) -> Option<String> {
    let rest = line.strip_prefix("+++ ")?;
    let p = rest.trim();
    let p = p.strip_prefix("b/").unwrap_or(p);
    (p != "/dev/null").then(|| p.to_string())
}

/// Group each file's hunk-header trailing signatures (deduplicated, first-seen
/// order preserved), in the order the files appear in the diff. This is the
/// structured form both [`hunk_context`] and [`structural_context`] build on.
fn hunk_regions(diff: &str) -> Vec<(String, Vec<String>)> {
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    let mut cur: Option<String> = None;

    for line in diff.lines() {
        if let Some(path) = line.strip_prefix("+++ ").and_then(|_| plusplus_path(line)) {
            cur = Some(path);
        } else if line.starts_with("@@") {
            let (Some(path), Some(sig)) = (cur.as_ref(), hunk_trailing(line)) else {
                continue;
            };
            let sig = sig.to_string();
            match out.iter_mut().find(|(p, _)| p == path) {
                Some((_, sigs)) => {
                    if !sigs.contains(&sig) {
                        sigs.push(sig);
                    }
                }
                None => out.push((path.clone(), vec![sig])),
            }
        }
    }
    out
}

/// Render a set of per-file region groups as a compact `Changed regions:` block.
fn format_regions(regions: &[(String, Vec<String>)]) -> String {
    let mut s = String::from("Changed regions:");
    for (path, sigs) in regions {
        s.push_str(&format!("\n- {}: {}", path, sigs.join("; ")));
    }
    s
}

/// Tier A structural context: for every hunk header carrying an enclosing
/// definition after the second `@@`, group those signatures per file into a
/// compact `Changed regions:` block. Returns an empty string when the diff has no
/// such headers. Pure string parsing — no fetch, no dependencies, never fails.
///
/// # Examples
/// ```
/// # use pr_review_core::structure::hunk_context;
/// let d = "+++ b/orders.go\n@@ -10,7 +10,8 @@ func processOrder(o Order) {\n+x\n";
/// let ctx = hunk_context(d);
/// assert!(ctx.contains("orders.go"));
/// assert!(ctx.contains("func processOrder(o Order) {"));
/// ```
pub fn hunk_context(diff: &str) -> String {
    let regions = hunk_regions(diff);
    if regions.is_empty() {
        return String::new();
    }
    format_regions(&regions)
}

// ---------------------------------------------------------------------------
// Tier B — tree-sitter enclosing symbols (precise)
// ---------------------------------------------------------------------------

/// A resolved definition enclosing one or more changed lines.
struct Sym {
    /// Language-idiomatic kind label (`fn`, `class`, `func`, `interface`, …).
    label: &'static str,
    name: String,
    /// 1-indexed inclusive line span of the definition on the new side.
    start: u64,
    end: u64,
}

/// Supported source languages (a subset of what the grammar crates provide).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Lang {
    Rust,
    TypeScript,
    Tsx,
    JavaScript,
    Python,
    Go,
}

/// Whether a path is JS/TS-family (`.ts/.tsx/.js/.jsx` and their variants) — the
/// files [`references_in_source`] can AST-classify for JSX + type references.
pub(crate) fn is_js_family(path: &str) -> bool {
    matches!(
        language_for_path(path),
        Some(Lang::TypeScript | Lang::Tsx | Lang::JavaScript)
    )
}

/// Classify a path to a supported [`Lang`] by extension, or `None` if unsupported.
fn language_for_path(path: &str) -> Option<Lang> {
    let ext = path.rsplit('.').next()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "rs" => Lang::Rust,
        "ts" | "mts" | "cts" => Lang::TypeScript,
        "tsx" => Lang::Tsx,
        "js" | "jsx" | "mjs" | "cjs" => Lang::JavaScript,
        "py" | "pyi" => Lang::Python,
        "go" => Lang::Go,
        _ => return None,
    })
}

impl Lang {
    /// The tree-sitter grammar for this language.
    fn ts_language(self) -> tree_sitter::Language {
        match self {
            Lang::Rust => tree_sitter_rust::LANGUAGE.into(),
            Lang::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Lang::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Lang::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Lang::Python => tree_sitter_python::LANGUAGE.into(),
            Lang::Go => tree_sitter_go::LANGUAGE.into(),
        }
    }

    /// Map a grammar node kind to a human, language-idiomatic label if the node is
    /// a definition worth reporting; `None` for everything else.
    fn def_label(self, kind: &str) -> Option<&'static str> {
        match self {
            Lang::Rust => match kind {
                "function_item" => Some("fn"),
                "struct_item" => Some("struct"),
                "enum_item" => Some("enum"),
                "union_item" => Some("union"),
                "trait_item" => Some("trait"),
                "impl_item" => Some("impl"),
                "mod_item" => Some("mod"),
                "type_item" => Some("type"),
                "const_item" => Some("const"),
                "static_item" => Some("static"),
                "macro_definition" => Some("macro"),
                _ => None,
            },
            Lang::TypeScript | Lang::Tsx | Lang::JavaScript => match kind {
                "function_declaration" | "generator_function_declaration" => Some("function"),
                "method_definition" => Some("method"),
                "class_declaration" | "abstract_class_declaration" => Some("class"),
                "interface_declaration" => Some("interface"),
                "type_alias_declaration" => Some("type"),
                "enum_declaration" => Some("enum"),
                _ => None,
            },
            Lang::Python => match kind {
                "function_definition" => Some("function"),
                "class_definition" => Some("class"),
                _ => None,
            },
            Lang::Go => match kind {
                "function_declaration" | "method_declaration" => Some("func"),
                "type_spec" => Some("type"),
                _ => None,
            },
        }
    }
}

/// Unwrap a JS/TS `export ...` wrapper to the definition it exports, so a change
/// on the `export` line still resolves to the class/function/etc. Returns `node`
/// unchanged for every other case.
fn unwrap_export(lang: Lang, node: Node) -> Node {
    if matches!(lang, Lang::TypeScript | Lang::Tsx | Lang::JavaScript)
        && node.kind() == "export_statement"
    {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if lang.def_label(child.kind()).is_some() {
                return child;
            }
        }
    }
    node
}

/// Resolve a definition node's name from the source. Uses the grammar's `name`
/// field, falling back to a Rust `impl` block's `type` field. `None` when no name
/// can be extracted (the caller then keeps walking up to an outer definition).
fn node_name(lang: Lang, node: Node, src: &[u8]) -> Option<String> {
    if let Some(n) = node.child_by_field_name("name") {
        if let Ok(t) = n.utf8_text(src) {
            return Some(t.to_string());
        }
    }
    if lang == Lang::Rust && node.kind() == "impl_item" {
        if let Some(t) = node.child_by_field_name("type") {
            if let Ok(s) = t.utf8_text(src) {
                return Some(s.to_string());
            }
        }
    }
    None
}

/// Parse `source` with tree-sitter and, for each changed line, collect the
/// smallest enclosing named definition. Returns unique symbols. Fully fail-open:
/// a language/parse error yields an empty vec.
fn symbols_for_file(lang: Lang, source: &str, changed: &HashSet<u64>) -> Vec<Sym> {
    let mut parser = Parser::new();
    if parser.set_language(&lang.ts_language()).is_err() {
        return Vec::new();
    }
    let tree = match parser.parse(source, None) {
        Some(t) => t,
        None => return Vec::new(),
    };
    let src = source.as_bytes();
    let root = tree.root_node();

    // Deterministic output: visit changed lines in ascending order.
    let mut lines: Vec<u64> = changed.iter().copied().collect();
    lines.sort_unstable();

    let mut seen: HashSet<(&'static str, String, u64, u64)> = HashSet::new();
    let mut out: Vec<Sym> = Vec::new();

    for line in lines {
        if line == 0 {
            continue;
        }
        let pt = Point {
            row: (line - 1) as usize,
            column: 0,
        };
        let mut node = match root.descendant_for_point_range(pt, pt) {
            Some(n) => n,
            None => continue,
        };
        // Walk up to the smallest enclosing definition that has a resolvable name.
        loop {
            let def = unwrap_export(lang, node);
            if let Some(label) = lang.def_label(def.kind()) {
                if let Some(name) = node_name(lang, def, src) {
                    let start = def.start_position().row as u64 + 1;
                    let end = def.end_position().row as u64 + 1;
                    if seen.insert((label, name.clone(), start, end)) {
                        out.push(Sym {
                            label,
                            name,
                            start,
                            end,
                        });
                    }
                    break;
                }
            }
            match node.parent() {
                Some(p) => node = p,
                None => break,
            }
        }
    }
    out
}

/// A definition a PR changed, exposed for cross-file analysis (see
/// [`crate::blast`]). The `&'static str` label mirrors [`Sym::label`].
pub(crate) struct ChangedSymbol {
    pub label: &'static str,
    pub name: String,
    pub start: u64,
    pub end: u64,
}

/// The smallest enclosing named definitions of `changed` lines in `source`, keyed
/// by `path`'s language. Returns an empty vec for an unsupported language or any
/// parse failure — the tree-sitter wrapper [`crate::blast`] builds a blast radius
/// on. Reuses the exact enclosing-definition logic Tier B renders for the prompt.
pub(crate) fn changed_symbols(
    path: &str,
    source: &str,
    changed: &HashSet<u64>,
) -> Vec<ChangedSymbol> {
    let Some(lang) = language_for_path(path) else {
        return Vec::new();
    };
    symbols_for_file(lang, source, changed)
        .into_iter()
        .map(|s| ChangedSymbol {
            label: s.label,
            name: s.name,
            start: s.start,
            end: s.end,
        })
        .collect()
}

/// How a symbol is referenced, so [`crate::blast`] can bucket JSX renders and
/// type usages that a `\bNAME\s*\(` call grep never matches.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum RefKind {
    /// `f(...)`, `obj.f(...)`, or `new F(...)`.
    Call,
    /// A JSX element: `<Comp/>` or `<Ns.Comp/>`.
    Jsx,
    /// A type position: `: T`, `extends T`, `Foo<T>`.
    Type,
}

/// One classified reference to a symbol in a source file (1-indexed line).
pub(crate) struct SymbolRef {
    pub line: u64,
    pub kind: RefKind,
}

/// Text of a callee/tag/constructor node that names the referenced symbol:
/// a bare `identifier` yields its text; a `member_expression`
/// (`foo.bar` / `Ns.Comp`) yields the `property` (last segment). `None` otherwise.
fn ref_name<'a>(node: Node, src: &'a [u8]) -> Option<&'a str> {
    match node.kind() {
        "identifier" | "type_identifier" | "property_identifier" => node.utf8_text(src).ok(),
        "member_expression" | "nested_identifier" => node
            .child_by_field_name("property")
            .and_then(|p| p.utf8_text(src).ok())
            .or_else(|| node.utf8_text(src).ok().and_then(|t| t.rsplit('.').next())),
        _ => None,
    }
}

/// Find references to `name` in a JS/TS-family `source` (parsed as `path`'s
/// language), classified as [`RefKind::Call`], [`RefKind::Jsx`], or
/// [`RefKind::Type`]. This catches the references a call-only grep misses: JSX
/// component usage (`<Name/>`) and type positions (`: Name`, `extends Name`,
/// `Foo<Name>`).
///
/// Returns 1-indexed lines. Fully fail-open: a non-JS-family path, a parse error,
/// or no matches all yield an empty vec. Deduplicated by `(line, kind)`.
pub(crate) fn references_in_source(path: &str, source: &str, name: &str) -> Vec<SymbolRef> {
    let lang = match language_for_path(path) {
        Some(l @ (Lang::TypeScript | Lang::Tsx | Lang::JavaScript)) => l,
        _ => return Vec::new(),
    };
    if name.is_empty() {
        return Vec::new();
    }
    let mut parser = Parser::new();
    if parser.set_language(&lang.ts_language()).is_err() {
        return Vec::new();
    }
    let tree = match parser.parse(source, None) {
        Some(t) => t,
        None => return Vec::new(),
    };
    let src = source.as_bytes();

    let mut seen: HashSet<(u64, RefKind)> = HashSet::new();
    let mut out: Vec<SymbolRef> = Vec::new();
    let mut push = |node: Node, kind: RefKind, out: &mut Vec<SymbolRef>| {
        let line = node.start_position().row as u64 + 1;
        if seen.insert((line, kind)) {
            out.push(SymbolRef { line, kind });
        }
    };

    // Manual pre-order walk (no tree-sitter query, to keep the grammar coupling
    // in one place and stay fail-open on grammar drift).
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        match node.kind() {
            "call_expression" => {
                if node
                    .child_by_field_name("function")
                    .and_then(|f| ref_name(f, src))
                    == Some(name)
                {
                    push(node, RefKind::Call, &mut out);
                }
            }
            "new_expression" => {
                if node
                    .child_by_field_name("constructor")
                    .and_then(|c| ref_name(c, src))
                    == Some(name)
                {
                    push(node, RefKind::Call, &mut out);
                }
            }
            // Match the opening/self-closing tag only (not the closing tag) so a
            // paired element `<Comp>…</Comp>` counts once.
            "jsx_opening_element" | "jsx_self_closing_element" => {
                if node.child_by_field_name("name").and_then(|n| ref_name(n, src)) == Some(name) {
                    push(node, RefKind::Jsx, &mut out);
                }
            }
            "type_identifier" if node.utf8_text(src) == Ok(name) => {
                push(node, RefKind::Type, &mut out);
            }
            _ => {}
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            stack.push(child);
        }
    }
    out.sort_by_key(|r| r.line);
    out
}

/// Render one file's resolved symbols as a `Changed symbols:` block entry.
fn format_symbols(path: &str, syms: &[Sym]) -> String {
    let parts: Vec<String> = syms
        .iter()
        .map(|s| {
            format!(
                "{} {} (lines {}\u{2013}{})",
                s.label, s.name, s.start, s.end
            )
        })
        .collect();
    format!("- {}: {}", path, parts.join(", "))
}

/// Ordered, de-duplicated list of new-side file paths in the diff (empty/preamble
/// sections dropped), preserving first-appearance order.
pub(crate) fn diff_file_order(diff: &str) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut order: Vec<String> = Vec::new();
    for (path, _) in split_diff_sections(diff) {
        if !path.is_empty() && seen.insert(path.clone()) {
            order.push(path);
        }
    }
    order
}

/// Compute structural context for the PR: name the smallest enclosing definition
/// of each changed line (Tier B, tree-sitter over the fetched new-side files) and
/// fall back to hunk-header regions (Tier A) for any file tree-sitter didn't
/// cover. Returns the combined human-readable block, or an empty string if
/// nothing could be derived.
///
/// Fully fail-open at every step — a missing head SHA, a fetch error, an oversized
/// file, an unsupported language, or a parse failure only skips that file and
/// never fails the review. At most `cfg.structural_max_files` supported files are
/// fetched.
pub async fn structural_context(
    provider: &Provider,
    client: &Client,
    cfg: &Config,
    repo: &str,
    meta: &PrMeta,
    diff: &str,
) -> String {
    // Without a head SHA there's nothing to fetch against — Tier A only.
    let head = match meta.head_sha.as_deref() {
        Some(s) if !s.is_empty() => s,
        _ => return hunk_context(diff),
    };

    let valid = parse_valid_lines(diff);
    let mut covered: HashSet<String> = HashSet::new();
    let mut ts_blocks: Vec<String> = Vec::new();
    let mut attempted = 0usize;

    for path in diff_file_order(diff) {
        if attempted >= cfg.structural_max_files {
            break;
        }
        let Some(lang) = language_for_path(&path) else {
            continue;
        };
        let Some(lines) = valid.get(&path).filter(|s| !s.is_empty()) else {
            continue;
        };
        attempted += 1;

        // Fetch the full new-side file; skip on empty/None/Err or if it's huge.
        let content = match provider
            .get_file_contents(client, cfg, repo, head, &path)
            .await
        {
            Ok(Some(c)) if c.len() <= 400_000 => c,
            _ => continue,
        };

        let syms = symbols_for_file(lang, &content, lines);
        if !syms.is_empty() {
            covered.insert(path.clone());
            ts_blocks.push(format_symbols(&path, &syms));
        }
    }

    let mut out = String::new();
    if !ts_blocks.is_empty() {
        out.push_str("Changed symbols:\n");
        out.push_str(&ts_blocks.join("\n"));
    }

    // Tier A for files tree-sitter didn't cover (unsupported, unfetched, or no
    // symbols found) — prefer precise symbols, append hunk regions for the rest.
    let regions: Vec<(String, Vec<String>)> = hunk_regions(diff)
        .into_iter()
        .filter(|(p, _)| !covered.contains(p))
        .collect();
    if !regions.is_empty() {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&format_regions(&regions));
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hunk_context_extracts_trailing_signature() {
        let d = "+++ b/orders.go\n\
                 @@ -10,7 +10,8 @@ func processOrder(o Order) {\n\
                 +added\n";
        let ctx = hunk_context(d);
        assert!(ctx.starts_with("Changed regions:"));
        assert!(ctx.contains("orders.go"));
        assert!(ctx.contains("func processOrder(o Order) {"));
    }

    #[test]
    fn hunk_context_empty_when_no_trailing() {
        let d = "+++ b/a.rs\n@@ -1,2 +1,3 @@\n+added\n";
        assert!(hunk_context(d).is_empty());
    }

    #[test]
    fn hunk_context_dedups_and_groups_per_file() {
        let d = "+++ b/a.ts\n\
                 @@ -1,3 +1,4 @@ class Foo {\n\
                 +a\n\
                 @@ -20,3 +21,4 @@ class Foo {\n\
                 +b\n\
                 +++ b/b.ts\n\
                 @@ -1,3 +1,4 @@ function bar() {\n\
                 +c\n";
        let ctx = hunk_context(d);
        // "class Foo {" appears twice in the diff but is grouped once.
        assert_eq!(ctx.matches("class Foo {").count(), 1);
        assert!(ctx.contains("a.ts: class Foo {"));
        assert!(ctx.contains("b.ts: function bar() {"));
    }

    #[test]
    fn hunk_context_skips_dev_null() {
        let d = "+++ /dev/null\n@@ -1,1 +0,0 @@ func gone() {\n-gone\n";
        assert!(hunk_context(d).is_empty());
    }

    // ----- Tier B (tree-sitter) -----

    #[test]
    fn ts_finds_enclosing_rust_fn_and_impl() {
        let src = "\
struct Point { x: i32 }

impl Point {
    fn clamp(v: i32) -> i32 {
        v
    }
}
";
        // Line 5 (`v`) is inside `fn clamp`, which is inside `impl Point`.
        let changed: HashSet<u64> = [5u64].into_iter().collect();
        let syms = symbols_for_file(Lang::Rust, src, &changed);
        assert_eq!(syms.len(), 1, "smallest enclosing def only");
        assert_eq!(syms[0].label, "fn");
        assert_eq!(syms[0].name, "clamp");
    }

    #[test]
    fn ts_reports_outer_def_for_line_outside_inner() {
        let src = "\
impl Point {
    fn clamp(v: i32) -> i32 {
        v
    }
}
";
        // Line 1 is on `impl Point` but outside the inner fn.
        let changed: HashSet<u64> = [1u64].into_iter().collect();
        let syms = symbols_for_file(Lang::Rust, src, &changed);
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].label, "impl");
        assert_eq!(syms[0].name, "Point");
    }

    #[test]
    fn ts_finds_typescript_class_and_method() {
        let src = "\
export class OrderService {
  processOrder(o: Order) {
    return o;
  }
}
";
        // Line 3 is inside processOrder; line 1 is on the class itself.
        let changed: HashSet<u64> = [1u64, 3u64].into_iter().collect();
        let syms = symbols_for_file(Lang::TypeScript, src, &changed);
        let names: Vec<_> = syms.iter().map(|s| (s.label, s.name.as_str())).collect();
        assert!(names.contains(&("class", "OrderService")));
        assert!(names.contains(&("method", "processOrder")));
    }

    #[test]
    fn ts_finds_python_function() {
        let src = "\
def process(order):
    total = 0
    return total
";
        let changed: HashSet<u64> = [2u64].into_iter().collect();
        let syms = symbols_for_file(Lang::Python, src, &changed);
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].label, "function");
        assert_eq!(syms[0].name, "process");
    }

    #[test]
    fn ts_finds_go_func() {
        let src = "\
package main

func processOrder(o Order) int {
    return 0
}
";
        let changed: HashSet<u64> = [4u64].into_iter().collect();
        let syms = symbols_for_file(Lang::Go, src, &changed);
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].label, "func");
        assert_eq!(syms[0].name, "processOrder");
    }

    #[test]
    fn language_for_path_maps_extensions() {
        assert!(matches!(language_for_path("a/b.rs"), Some(Lang::Rust)));
        assert!(matches!(
            language_for_path("a/b.ts"),
            Some(Lang::TypeScript)
        ));
        assert!(matches!(language_for_path("a/b.tsx"), Some(Lang::Tsx)));
        assert!(matches!(
            language_for_path("a/b.jsx"),
            Some(Lang::JavaScript)
        ));
        assert!(matches!(language_for_path("a/b.py"), Some(Lang::Python)));
        assert!(matches!(language_for_path("a/b.go"), Some(Lang::Go)));
        assert!(language_for_path("a/b.md").is_none());
        assert!(language_for_path("Makefile").is_none());
    }

    #[test]
    fn format_symbols_uses_en_dash_span() {
        let syms = vec![Sym {
            label: "fn",
            name: "clamp".to_string(),
            start: 5,
            end: 9,
        }];
        let s = format_symbols("src/util.rs", &syms);
        assert_eq!(s, "- src/util.rs: fn clamp (lines 5\u{2013}9)");
    }

    // ----- references_in_source (JSX + type + call classification) -----

    fn kinds_on(refs: &[SymbolRef]) -> Vec<(u64, RefKind)> {
        refs.iter().map(|r| (r.line, r.kind)).collect()
    }

    #[test]
    fn refs_classify_jsx_component_usage() {
        let src = "\
function App() {
  return <FindingCard title=\"x\" />;
}
function Other() {
  return <FindingCard>{y}</FindingCard>;
}
";
        let refs = references_in_source("app.tsx", src, "FindingCard");
        // Both the self-closing (line 2) and the paired opening tag (line 5) count
        // once each; the closing tag on line 5 must not double-count.
        assert_eq!(kinds_on(&refs), vec![(2, RefKind::Jsx), (5, RefKind::Jsx)]);
    }

    #[test]
    fn refs_classify_namespaced_jsx() {
        let src = "const x = <Ns.Panel/>;\n";
        let refs = references_in_source("a.tsx", src, "Panel");
        assert_eq!(kinds_on(&refs), vec![(1, RefKind::Jsx)]);
    }

    #[test]
    fn refs_classify_type_positions() {
        let src = "\
function f(x: Finding): Finding { return x; }
const a: Array<Finding> = [];
";
        // Param + return type collapse to one entry on line 1 (dedup by line+kind);
        // the generic arg on line 2 is a second type reference.
        let finding = references_in_source("a.ts", src, "Finding");
        assert_eq!(
            kinds_on(&finding),
            vec![(1, RefKind::Type), (2, RefKind::Type)]
        );
    }

    #[test]
    fn refs_ignore_class_extends_value_identifier() {
        // `class B extends Base` makes `Base` a value identifier, not a
        // type_identifier — we deliberately don't over-match bare identifiers.
        let refs = references_in_source("a.ts", "class B extends Base {}\n", "Base");
        assert!(refs.is_empty());
    }

    #[test]
    fn refs_classify_calls_and_new() {
        let src = "\
scoreColor(1);
foo.analyzeGlstat(y);
const w = new Widget();
";
        assert_eq!(
            kinds_on(&references_in_source("a.ts", src, "scoreColor")),
            vec![(1, RefKind::Call)]
        );
        assert_eq!(
            kinds_on(&references_in_source("a.ts", src, "analyzeGlstat")),
            vec![(2, RefKind::Call)]
        );
        assert_eq!(
            kinds_on(&references_in_source("a.ts", src, "Widget")),
            vec![(3, RefKind::Call)]
        );
    }

    #[test]
    fn refs_empty_for_non_js_family_and_no_match() {
        assert!(references_in_source("a.rs", "fn f() { g(); }", "g").is_empty());
        assert!(references_in_source("a.tsx", "const x = 1;\n", "Missing").is_empty());
    }
}
