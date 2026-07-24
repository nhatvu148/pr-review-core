//! Blast radius: for the definitions a PR changes, find who *calls* them and
//! which *tests* reference them, computed from the agent's clone. This seeds the
//! agentic reviewer with the cross-file context it would otherwise have to
//! rediscover by hand (grep → read → repeat), and backs the `references` tool.
//!
//! Deliberately lightweight and fail-open, matching [`crate::structure`]: a
//! parse/read/grep hiccup only drops that symbol and never fails the review.
//! Call-site discovery is grep-based (`\bNAME\s*\(`), so it catches free
//! functions, methods (`.foo(`), and call-like constructors, at the cost of
//! being a *candidate* list — the reviewer confirms with `read_file`. It is still
//! strictly better than the model hand-rolling its own regex: deterministic,
//! test-aware, definition-site excluded, capped, and labeled.

use std::collections::HashSet;

use crate::config::Config;
use crate::diff::parse_valid_lines;
use crate::repo::Workspace;
use crate::structure::{
    changed_symbols, diff_file_order, is_js_family, references_in_source, RefKind,
};

/// A single call/reference site found for a symbol.
struct Ref {
    path: String,
    line: u64,
}

/// One changed symbol together with its discovered callers, tests, and type uses.
struct SymbolBlast {
    label: &'static str,
    name: String,
    def_file: String,
    callers: Vec<Ref>,
    tests: Vec<Ref>,
    /// Type-position references (`: T`, `Foo<T>`) — TS/TSX only.
    types: Vec<Ref>,
    /// The clone-wide search hit its ceiling, so a bucket may be incomplete — an
    /// empty one must not be read as a real "no callers/tests" signal.
    capped: bool,
}

/// Result of a clone-wide reference search, split by bucket. `callers` mixes plain
/// calls and JSX renders (`<Comp/>`); `types` is type-position uses; `tests` is any
/// reference living in a test file. Fetched with a generous ceiling
/// ([`RAW_FETCH_CAP`]) so a heavily-used symbol's callers can't crowd its tests out
/// of the result window; buckets are truncated to `blast_max_refs` at render time.
struct CallSites {
    callers: Vec<Ref>,
    tests: Vec<Ref>,
    types: Vec<Ref>,
    capped: bool,
}

/// Ceiling on raw grep hits fetched per symbol. Far above the per-bucket display
/// cap (`blast_max_refs`, default 8) so both buckets are reached in practice; the
/// pathological overflow past this is flagged via [`CallSites::capped`].
const RAW_FETCH_CAP: usize = 600;

/// Heuristic: does this path look like a test file? Covers the common conventions
/// across Rust/TS/JS/Python/Go (dir-based and filename-based).
fn is_test_path(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    let file = p.rsplit('/').next().unwrap_or(&p);
    p.contains("/tests/")
        || p.contains("/test/")
        || p.contains("__tests__")
        || p.contains("/spec/")
        || file.contains(".test.")
        || file.contains(".spec.")
        || file.ends_with("_test.go")
        || file.ends_with("_test.py")
        || file.ends_with("_test.rs")
        || file.ends_with("_spec.rb")
        || file.starts_with("test_")
}

/// Parse one `path:line: text` line from [`Workspace::grep`] into (path, line).
/// Returns `None` for a line that doesn't match that shape.
fn parse_grep_hit(hit: &str) -> Option<(String, u64)> {
    let (path, rest) = hit.split_once(':')?;
    let (line_str, _text) = rest.split_once(':')?;
    let line: u64 = line_str.trim().parse().ok()?;
    Some((path.to_string(), line))
}

/// Whether a matched line is itself a *definition* of `name` (so we don't report
/// the definition as one of its own callers). Cheap keyword check on the source
/// text portion of a grep hit.
fn looks_like_definition(text: &str, name: &str) -> bool {
    let t = text.trim_start();
    for kw in ["fn ", "func ", "def ", "function ", "class ", "interface "] {
        if let Some(rest) = t.strip_prefix(kw) {
            if rest.trim_start().starts_with(name) {
                return true;
            }
        }
        // `pub fn`, `async fn`, `export function`, `public function`, …
        if let Some(idx) = t.find(kw) {
            if idx <= 16 && t[idx + kw.len()..].trim_start().starts_with(name) {
                return true;
            }
        }
    }
    false
}

/// Find references to `name` across the clone, split into `callers` (calls + JSX
/// renders), `tests` (any reference in a test file), and `types` (TS type
/// positions). The symbol's own definition span (`def` = `(file, start, end)`) and
/// self-definition lines are excluded. Deterministic (sorted + de-duplicated by
/// `(path, line)` per bucket).
///
/// Two engines, each with a grep sized to its own need so neither dilutes the
/// other's [`RAW_FETCH_CAP`] window:
/// - **Non-JS** (Rust/Go/Python/…): a narrow `\bNAME\s*\(` grep — every hit is
///   already a call, so the cap is spent only on real calls (pre-JSX behaviour).
/// - **JS/TS family** (`.ts/.tsx/.js/.jsx`): a broad `\bNAME\b` grep locates
///   candidate *files*, each parsed once by [`references_in_source`] for precise
///   JSX / type / call classification (JSX and type uses aren't call syntax, so
///   the narrow grep can't see them).
///
/// Either grep hitting its ceiling sets [`CallSites::capped`] so an empty bucket
/// isn't misread as absence. Fully fail-open — a bad regex or grep error yields
/// empty buckets.
fn call_sites(ws: &Workspace, name: &str, def: Option<(&str, u64, u64)>) -> CallSites {
    if name.is_empty() {
        return CallSites {
            callers: Vec::new(),
            tests: Vec::new(),
            types: Vec::new(),
            capped: false,
        };
    }
    let esc = regex::escape(name);
    // Narrow call grep — the only source for non-JS files, so their candidate list
    // keeps its full cap and pre-PR precision (no dilution by bare mentions).
    let call_hits = ws
        .grep(&format!(r"\b{esc}\s*\("), RAW_FETCH_CAP)
        .unwrap_or_default();
    // Broad mention grep — mined ONLY for JS-family file paths to AST-classify.
    let mention_hits = ws
        .grep(&format!(r"\b{esc}\b"), RAW_FETCH_CAP)
        .unwrap_or_default();
    let capped = call_hits.len() >= RAW_FETCH_CAP || mention_hits.len() >= RAW_FETCH_CAP;

    let mut callers: Vec<Ref> = Vec::new();
    let mut tests: Vec<Ref> = Vec::new();
    let mut types: Vec<Ref> = Vec::new();
    let in_def_span = |path: &str, line: u64| {
        matches!(def, Some((df, s, e)) if path == df && line >= s && line <= e)
    };

    // JS-family: parse each distinct candidate file once, classify every reference.
    let mut seen_js: HashSet<String> = HashSet::new();
    for hit in &mention_hits {
        let Some((path, _)) = parse_grep_hit(hit) else {
            continue;
        };
        if !is_js_family(&path) || !seen_js.insert(path.clone()) {
            continue;
        }
        let Ok(src) = ws.read_raw(&path) else { continue };
        let is_test = is_test_path(&path);
        for r in references_in_source(&path, &src, name) {
            if in_def_span(&path, r.line) {
                continue;
            }
            let bucket = if is_test {
                &mut tests
            } else if r.kind == RefKind::Type {
                &mut types
            } else {
                &mut callers // Call | Jsx
            };
            bucket.push(Ref {
                path: path.clone(),
                line: r.line,
            });
        }
    }

    // Non-JS: every narrow-grep hit is already a `NAME(` call; drop only the
    // definition span and lines that are themselves a definition of `name`.
    for hit in &call_hits {
        let Some((path, line)) = parse_grep_hit(hit) else {
            continue;
        };
        if is_js_family(&path) {
            continue; // handled by the AST pass above
        }
        let text = hit.splitn(3, ':').nth(2).unwrap_or("");
        if in_def_span(&path, line) || looks_like_definition(text, name) {
            continue;
        }
        let bucket = if is_test_path(&path) {
            &mut tests
        } else {
            &mut callers
        };
        bucket.push(Ref { path, line });
    }

    // Sort + de-dup by (path, line): a line carrying two ref kinds (e.g. a Call and
    // a Jsx to the same name) must list once, not twice.
    let finish = |v: &mut Vec<Ref>| {
        v.sort_by(|a, b| a.path.cmp(&b.path).then(a.line.cmp(&b.line)));
        v.dedup_by(|a, b| a.path == b.path && a.line == b.line);
    };
    finish(&mut callers);
    finish(&mut tests);
    finish(&mut types);
    CallSites {
        callers,
        tests,
        types,
        capped,
    }
}

/// Compute the blast radius for every changed symbol in the diff, using files
/// read from the clone. Capped at `cfg.blast_max_symbols` symbols. Fail-open.
fn compute(ws: &Workspace, diff: &str, cfg: &Config) -> Vec<SymbolBlast> {
    let valid = parse_valid_lines(diff);
    let mut out: Vec<SymbolBlast> = Vec::new();
    let mut seen: HashSet<(String, String)> = HashSet::new();

    for path in diff_file_order(diff) {
        if out.len() >= cfg.blast_max_symbols {
            break;
        }
        let Some(lines) = valid.get(&path).filter(|s| !s.is_empty()) else {
            continue;
        };
        let Ok(source) = ws.read_raw(&path) else {
            continue;
        };
        for sym in changed_symbols(&path, &source, lines) {
            if out.len() >= cfg.blast_max_symbols {
                break;
            }
            // De-dupe a symbol name that spans several changed hunks in one file.
            if !seen.insert((path.clone(), sym.name.clone())) {
                continue;
            }
            let cs = call_sites(ws, &sym.name, Some((&path, sym.start, sym.end)));
            out.push(SymbolBlast {
                label: sym.label,
                name: sym.name,
                def_file: path.clone(),
                callers: cs.callers,
                tests: cs.tests,
                types: cs.types,
                capped: cs.capped,
            });
        }
    }
    out
}

/// Render a compact `caller: file:line` list, truncated to `max` with a `(+N more)`
/// suffix so a hot symbol can't blow the token budget.
fn render_refs(refs: &[Ref], max: usize) -> String {
    let shown: Vec<String> = refs
        .iter()
        .take(max)
        .map(|r| format!("{}:{}", r.path, r.line))
        .collect();
    let mut s = shown.join(", ");
    if refs.len() > max {
        s.push_str(&format!(" (+{} more)", refs.len() - max));
    }
    s
}

/// Build the `## Blast radius` prompt block: for each changed symbol, its callers
/// and tests found across the clone. Returns an empty string when the feature is
/// off or nothing was derived (so the caller can `Option`-gate it like structural
/// context). Symbols with no callers/tests are still listed — "no in-repo callers"
/// is itself a signal (new API, entrypoint, or dead code).
pub fn blast_seed(ws: &Workspace, diff: &str, cfg: &Config) -> String {
    if !cfg.blast_radius {
        return String::new();
    }
    let syms = compute(ws, diff, cfg);
    if syms.is_empty() {
        return String::new();
    }

    let mut s = String::from(
        "## Blast radius\nPrecomputed references to the definitions this PR changes — callers \
(calls + JSX `<Comp/>` renders), tests, and type uses (candidate list from a clone-wide \
search; confirm with read_file). \"no in-repo callers\" often means a new/public API, an \
entrypoint, or dead code.",
    );
    for b in &syms {
        s.push_str(&format!("\n- {} {} ({})", b.label, b.name, b.def_file));
        if b.callers.is_empty() && b.tests.is_empty() && b.types.is_empty() {
            // Only claim "none found" when the search was exhaustive; a capped
            // search that returned nothing after filtering is inconclusive, not
            // evidence of dead code.
            s.push_str(if b.capped {
                ": search capped — references could not be fully enumerated"
            } else {
                ": no in-repo callers, tests, or type uses found"
            });
            continue;
        }
        for (label, refs) in [
            ("callers", &b.callers),
            ("tests", &b.tests),
            ("type uses", &b.types),
        ] {
            if !refs.is_empty() {
                s.push_str(&format!(
                    "\n    {label} ({}): {}",
                    refs.len(),
                    render_refs(refs, cfg.blast_max_refs)
                ));
            }
        }
        // When the raw search was capped, an empty bucket may be truncation, not
        // absence — say so, so the model doesn't read "no tests" as a signal.
        if b.capped {
            s.push_str("\n    (search capped — a missing bucket may be incomplete, not absent)");
        }
    }
    s
}

/// Back the agent's `references` tool: references to `name` across the clone,
/// split into callers (calls + JSX renders), tests, and type uses, as a compact
/// text block the model can read. No definition span is known here (the model
/// supplies a bare name), so only self-definition *lines* are filtered.
pub fn references(ws: &Workspace, name: &str, max_refs: usize) -> String {
    let cs = call_sites(ws, name, None);
    if cs.callers.is_empty() && cs.tests.is_empty() && cs.types.is_empty() {
        return if cs.capped {
            format!("(search capped — could not enumerate references for `{name}`)")
        } else {
            format!("(no references found for `{name}`)")
        };
    }
    let mut lines: Vec<String> = Vec::new();
    for (label, refs) in [
        ("callers", &cs.callers),
        ("tests", &cs.tests),
        ("type uses", &cs.types),
    ] {
        if !refs.is_empty() {
            lines.push(format!(
                "{label} ({}): {}",
                refs.len(),
                render_refs(refs, max_refs)
            ));
        }
    }
    if cs.capped {
        lines.push("(search capped — list may be incomplete)".to_string());
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn cfg() -> Config {
        let mut c = Config::from_env();
        c.blast_radius = true;
        c.blast_max_symbols = 12;
        c.blast_max_refs = 8;
        c
    }

    /// A repo where `orders.rs` defines `process`, called from a handler and a test.
    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::create_dir_all(dir.path().join("tests")).unwrap();
        fs::write(
            dir.path().join("src/orders.rs"),
            "pub fn process(o: Order) -> i32 {\n    o.total\n}\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("src/handler.rs"),
            "fn handle(o: Order) {\n    let n = process(o);\n    println!(\"{n}\");\n}\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("tests/orders_test.rs"),
            "#[test]\nfn it_processes() {\n    assert_eq!(process(mk()), 0);\n}\n",
        )
        .unwrap();
        dir
    }

    const DIFF: &str = "\
diff --git a/src/orders.rs b/src/orders.rs
--- a/src/orders.rs
+++ b/src/orders.rs
@@ -1,3 +1,3 @@ pub fn process(o: Order) -> i32 {
-    o.total
+    o.total + 1
";

    #[test]
    fn is_test_path_covers_conventions() {
        assert!(is_test_path("tests/orders_test.rs"));
        assert!(is_test_path("src/foo_test.go"));
        assert!(is_test_path("app/user.spec.ts"));
        assert!(is_test_path("pkg/__tests__/x.js"));
        assert!(is_test_path("test_users.py"));
        assert!(!is_test_path("src/orders.rs"));
        assert!(!is_test_path("src/handler.rs"));
    }

    #[test]
    fn parse_grep_hit_extracts_path_and_line() {
        assert_eq!(
            parse_grep_hit("src/handler.rs:2:     let n = process(o);"),
            Some(("src/handler.rs".to_string(), 2))
        );
        assert_eq!(parse_grep_hit("no-colons-here"), None);
    }

    #[test]
    fn looks_like_definition_flags_defs_not_calls() {
        assert!(looks_like_definition("pub fn process(o: Order) -> i32 {", "process"));
        assert!(looks_like_definition("def process(order):", "process"));
        assert!(!looks_like_definition("    let n = process(o);", "process"));
    }

    #[test]
    fn call_sites_finds_caller_and_test_excludes_definition() {
        let d = fixture();
        let ws = Workspace::from_dir(d.path());
        // Exclude the definition span in orders.rs (lines 1..=3).
        let cs = call_sites(&ws, "process", Some(("src/orders.rs", 1, 3)));
        // The caller lands in the callers bucket, the test in the tests bucket —
        // buckets are populated independently so one can't crowd out the other.
        assert!(
            cs.callers.iter().any(|r| r.path == "src/handler.rs"),
            "caller found: {:?}",
            cs.callers.iter().map(|r| &r.path).collect::<Vec<_>>()
        );
        assert!(
            cs.tests.iter().any(|r| r.path == "tests/orders_test.rs"),
            "test found: {:?}",
            cs.tests.iter().map(|r| &r.path).collect::<Vec<_>>()
        );
        // The definition line itself must not appear in either bucket.
        assert!(
            !cs.callers.iter().chain(cs.tests.iter()).any(|r| r.path == "src/orders.rs"),
            "definition excluded"
        );
        // A small fixture never hits the fetch ceiling.
        assert!(!cs.capped);
    }

    #[test]
    fn blast_seed_renders_callers_and_tests() {
        let d = fixture();
        let ws = Workspace::from_dir(d.path());
        let seed = blast_seed(&ws, DIFF, &cfg());
        assert!(seed.contains("## Blast radius"), "{seed}");
        assert!(seed.contains("fn process"), "{seed}");
        assert!(seed.contains("src/handler.rs:2"), "{seed}");
        assert!(seed.contains("tests/orders_test.rs:3"), "{seed}");
        assert!(seed.contains("callers (1)"), "{seed}");
        assert!(seed.contains("tests (1)"), "{seed}");
    }

    #[test]
    fn blast_seed_empty_when_disabled() {
        let d = fixture();
        let ws = Workspace::from_dir(d.path());
        let mut c = cfg();
        c.blast_radius = false;
        assert!(blast_seed(&ws, DIFF, &c).is_empty());
    }

    #[test]
    fn references_tool_reports_buckets() {
        let d = fixture();
        let ws = Workspace::from_dir(d.path());
        let out = references(&ws, "process", 8);
        assert!(out.contains("callers"), "{out}");
        assert!(out.contains("tests"), "{out}");
        assert!(out.contains("src/handler.rs:2"), "{out}");
    }

    #[test]
    fn references_tool_reports_nothing_for_unknown() {
        let d = fixture();
        let ws = Workspace::from_dir(d.path());
        let out = references(&ws, "nonexistent_symbol_xyz", 8);
        assert!(out.contains("no references found"), "{out}");
    }

    // ----- TS/TSX: JSX renders + type uses (the #9 capability) -----

    /// A tiny TSX repo: `Card` rendered as JSX, `Finding` used as a type, and
    /// `analyze` called from a `.test.ts`.
    fn ts_fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        let w = |rel: &str, body: &str| fs::write(dir.path().join(rel), body).unwrap();
        w("src/Card.tsx", "export function Card() {\n  return null;\n}\n");
        w(
            "src/App.tsx",
            "import { Card } from './Card';\nexport function App() {\n  return <Card />;\n}\n",
        );
        w("src/types.ts", "export interface Finding {\n  id: string;\n}\n");
        w(
            "src/analyze.ts",
            "import { Finding } from './types';\nexport function analyze(f: Finding): Finding {\n  return f;\n}\n",
        );
        w(
            "src/analyze.test.ts",
            "import { analyze } from './analyze';\ntest('runs', () => {\n  analyze({ id: '1' } as any);\n});\n",
        );
        dir
    }

    #[test]
    fn references_finds_jsx_render_as_a_caller() {
        let d = ts_fixture();
        let ws = Workspace::from_dir(d.path());
        // `<Card />` in App.tsx is a caller even though there's no `Card(` call.
        let out = references(&ws, "Card", 8);
        assert!(out.contains("callers"), "{out}");
        assert!(out.contains("src/App.tsx"), "JSX render found: {out}");
    }

    #[test]
    fn references_finds_type_uses_bucket() {
        let d = ts_fixture();
        let ws = Workspace::from_dir(d.path());
        // `Finding` is used only in type positions — a call grep would miss it.
        let out = references(&ws, "Finding", 8);
        assert!(out.contains("type uses"), "type bucket present: {out}");
        assert!(out.contains("src/analyze.ts"), "{out}");
        assert!(!out.contains("callers ("), "no call sites for a pure type: {out}");
    }

    #[test]
    fn call_sites_dedups_same_line_call_and_jsx() {
        // A line that references `Card` as BOTH a JSX element and a call yields two
        // SymbolRefs (different kinds); they must collapse to ONE caller entry.
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(
            dir.path().join("src/Card.tsx"),
            "export function Card() { return null; }\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("src/use.tsx"),
            "export const U = (c: boolean) => (c ? <Card /> : Card());\n",
        )
        .unwrap();
        let ws = Workspace::from_dir(dir.path());
        let out = references(&ws, "Card", 8);
        assert_eq!(out.matches("src/use.tsx:1").count(), 1, "listed once: {out}");
        assert!(out.contains("callers (1)"), "{out}");
    }

    #[test]
    fn blast_seed_ts_splits_callers_and_tests() {
        let d = ts_fixture();
        let ws = Workspace::from_dir(d.path());
        // A diff editing `analyze` (line 2 of analyze.ts, inside the function).
        let diff = "\
diff --git a/src/analyze.ts b/src/analyze.ts
--- a/src/analyze.ts
+++ b/src/analyze.ts
@@ -2,2 +2,2 @@ export function analyze(f: Finding): Finding {
-  return f;
+  return f;
";
        let seed = blast_seed(&ws, diff, &cfg());
        assert!(seed.contains("analyze"), "{seed}");
        // The only in-repo call to analyze is in the test file → tests bucket.
        assert!(seed.contains("tests ("), "{seed}");
        assert!(seed.contains("src/analyze.test.ts"), "{seed}");
    }
}
