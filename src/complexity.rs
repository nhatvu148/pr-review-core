//! Deterministic complexity metrics for a PR's changed functions — a cheap,
//! LLM-free risk signal (idea borrowed from `trusty-tools`). For each function a
//! change touches, we compute **cyclomatic complexity** (McCabe: 1 + decision
//! points) and an approximate **cognitive complexity** (nesting-weighted count of
//! control-flow structures, à la SonarSource), and grade it A–F.
//!
//! Fully fail-open, matching [`crate::structure`]: an unsupported language or a
//! parse error yields no metrics and never fails the review. Nested functions are
//! attributed separately (a callback's complexity isn't folded into its parent).
//!
//! The numbers are a *signal*, not a spec — the grammar-specific node sets are
//! pragmatic approximations, and the model still owns the judgement.

use std::collections::HashSet;

use tree_sitter::{Node, Parser};

/// Languages we can measure (mirrors [`crate::structure`]'s Tier-B set).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Lang {
    Rust,
    Ts,
    Tsx,
    Js,
    Py,
    Go,
}

fn lang_for(path: &str) -> Option<Lang> {
    let ext = path.rsplit('.').next()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "rs" => Lang::Rust,
        "ts" | "mts" | "cts" => Lang::Ts,
        "tsx" => Lang::Tsx,
        "js" | "jsx" | "mjs" | "cjs" => Lang::Js,
        "py" | "pyi" => Lang::Py,
        "go" => Lang::Go,
        _ => return None,
    })
}

impl Lang {
    fn grammar(self) -> tree_sitter::Language {
        match self {
            Lang::Rust => tree_sitter_rust::LANGUAGE.into(),
            Lang::Ts => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Lang::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Lang::Js => tree_sitter_javascript::LANGUAGE.into(),
            Lang::Py => tree_sitter_python::LANGUAGE.into(),
            Lang::Go => tree_sitter_go::LANGUAGE.into(),
        }
    }

    /// Node kinds that define a function/method — a complexity unit. Nested ones
    /// are measured on their own, not folded into the enclosing function.
    fn is_function(self, kind: &str) -> bool {
        match self {
            Lang::Rust => matches!(kind, "function_item"),
            Lang::Ts | Lang::Tsx | Lang::Js => matches!(
                kind,
                "function_declaration"
                    | "generator_function_declaration"
                    | "method_definition"
                    | "function_expression"
                    | "arrow_function"
            ),
            Lang::Py => matches!(kind, "function_definition"),
            Lang::Go => matches!(kind, "function_declaration" | "method_declaration"),
        }
    }

    /// The function's declared name. Most functions carry a `name` field; a JS/TS
    /// **arrow / function-expression never does** — its name lives on the binding
    /// (`const handleSubmit = () => {}`, `obj.fn = () => {}`, `{ key: () => {} }`,
    /// a class field), which is the dominant style in modern TS/React. We recover
    /// that so those functions get a complexity signal. `None` only for truly
    /// anonymous inline callbacks (`arr.map(x => x + 1)`).
    fn fn_name(self, node: Node, src: &[u8]) -> Option<String> {
        if let Some(n) = node
            .child_by_field_name("name")
            .and_then(|n| n.utf8_text(src).ok())
        {
            return Some(n.to_string());
        }
        if matches!(node.kind(), "arrow_function" | "function_expression") {
            let parent = node.parent()?;
            let name_node = match parent.kind() {
                "variable_declarator" => parent.child_by_field_name("name"),
                "assignment_expression" => parent.child_by_field_name("left"),
                "pair" => parent.child_by_field_name("key"),
                "public_field_definition" | "field_definition" => {
                    parent.child_by_field_name("name")
                }
                _ => None,
            }?;
            return name_node.utf8_text(src).ok().map(str::to_string);
        }
        None
    }

    /// Cyclomatic decision points contributed by `node` (1 per branch; `&&`/`||`
    /// each add a path). Everything else is 0.
    fn decision_points(self, node: Node, src: &[u8]) -> u32 {
        let k = node.kind();
        match self {
            Lang::Rust => match k {
                "if_expression" | "while_expression" | "while_let_expression"
                | "for_expression" | "loop_expression" | "match_arm" => 1,
                "binary_expression" => bool_ops(node, src),
                _ => 0,
            },
            Lang::Ts | Lang::Tsx | Lang::Js => match k {
                "if_statement" | "for_statement" | "for_in_statement" | "while_statement"
                | "do_statement" | "switch_case" | "catch_clause" | "ternary_expression" => 1,
                "binary_expression" => bool_ops(node, src),
                _ => 0,
            },
            Lang::Py => match k {
                "if_statement" | "elif_clause" | "for_statement" | "while_statement"
                | "except_clause" | "conditional_expression" => 1,
                "boolean_operator" => 1,
                _ => 0,
            },
            Lang::Go => match k {
                "if_statement" | "for_statement" | "expression_case" | "type_case"
                | "communication_case" => 1,
                "binary_expression" => bool_ops(node, src),
                _ => 0,
            },
        }
    }

    /// Control-flow structures that both count toward cognitive complexity and
    /// increase the nesting level for what they contain.
    fn nests(self, kind: &str) -> bool {
        match self {
            Lang::Rust => matches!(
                kind,
                "if_expression"
                    | "while_expression"
                    | "while_let_expression"
                    | "for_expression"
                    | "loop_expression"
                    | "match_expression"
            ),
            Lang::Ts | Lang::Tsx | Lang::Js => matches!(
                kind,
                "if_statement"
                    | "for_statement"
                    | "for_in_statement"
                    | "while_statement"
                    | "do_statement"
                    | "switch_statement"
                    | "catch_clause"
            ),
            Lang::Py => matches!(
                kind,
                "if_statement" | "for_statement" | "while_statement" | "except_clause"
            ),
            Lang::Go => matches!(
                kind,
                "if_statement"
                    | "for_statement"
                    | "expression_switch_statement"
                    | "type_switch_statement"
                    | "select_statement"
            ),
        }
    }
}

/// Count `&&` / `||` operators on a binary-expression node (each adds a path).
fn bool_ops(node: Node, src: &[u8]) -> u32 {
    match node
        .child_by_field_name("operator")
        .and_then(|o| o.utf8_text(src).ok())
    {
        Some("&&" | "||") => 1,
        _ => 0,
    }
}

/// Complexity metrics for one changed function.
pub struct FnComplexity {
    pub label: &'static str,
    pub name: String,
    pub cyclomatic: u32,
    pub cognitive: u32,
    /// 1-indexed start line of the definition (used to order the report).
    pub start: u64,
}

impl FnComplexity {
    /// A–F grade from cyclomatic complexity (McCabe bands, slightly relaxed):
    /// A ≤5 · B ≤10 · C ≤15 · D ≤25 · F otherwise.
    pub fn grade(&self) -> char {
        match self.cyclomatic {
            0..=5 => 'A',
            6..=10 => 'B',
            11..=15 => 'C',
            16..=25 => 'D',
            _ => 'F',
        }
    }
}

/// Cyclomatic complexity of a function subtree: 1 + decision points, not
/// descending into nested functions.
fn cyclomatic(func: Node, lang: Lang, src: &[u8]) -> u32 {
    let mut count = 1;
    let mut stack = vec![func];
    while let Some(n) = stack.pop() {
        let mut cur = n.walk();
        for ch in n.children(&mut cur) {
            if lang.is_function(ch.kind()) {
                continue; // nested function — measured on its own
            }
            count += lang.decision_points(ch, src);
            stack.push(ch);
        }
    }
    count
}

/// Approximate cognitive complexity: each control-flow structure adds `1 + its
/// nesting depth`, and nesting increases inside it. Boolean operators are left to
/// cyclomatic. Doesn't descend into nested functions.
///
/// Known divergence from SonarSource: an `else if` chain counts as increasing
/// nesting (its `else` branch is itself a nested `if`), so a flat N-way else-if
/// scores `1+2+…+N` rather than a flat `N`. This can overstate a common,
/// unremarkable pattern — fine for a risk *signal*, but don't read the cognitive
/// number as an exact SonarSource score. (Cyclomatic, the primary grade driver,
/// is unaffected: it counts each branch once regardless of nesting.)
fn cognitive(func: Node, lang: Lang) -> u32 {
    fn walk(n: Node, lang: Lang, nesting: u32) -> u32 {
        let mut total = 0;
        let mut cur = n.walk();
        for ch in n.children(&mut cur) {
            if lang.is_function(ch.kind()) {
                continue;
            }
            if lang.nests(ch.kind()) {
                total += 1 + nesting;
                total += walk(ch, lang, nesting + 1);
            } else {
                total += walk(ch, lang, nesting);
            }
        }
        total
    }
    walk(func, lang, 0)
}

/// The smallest enclosing function node of a 1-indexed `line`, if any.
fn enclosing_function<'a>(root: Node<'a>, line: u64, lang: Lang) -> Option<Node<'a>> {
    use tree_sitter::Point;
    let pt = Point {
        row: (line.checked_sub(1)?) as usize,
        column: 0,
    };
    let mut node = root.descendant_for_point_range(pt, pt)?;
    loop {
        if lang.is_function(node.kind()) {
            return Some(node);
        }
        node = node.parent()?;
    }
}

/// Human label for a function node's kind, per language.
fn label_for(lang: Lang, kind: &str) -> &'static str {
    match lang {
        Lang::Rust => "fn",
        Lang::Go => "func",
        Lang::Py => "def",
        _ => match kind {
            "method_definition" => "method",
            "arrow_function" | "function_expression" => "fn",
            _ => "function",
        },
    }
}

/// Cyclomatic + cognitive complexity for every function a change touches in
/// `source` (parsed as `path`'s language). Deduplicated by definition span, sorted
/// by start line. Fully fail-open: unsupported language / parse error → empty vec.
///
/// Parses `source` itself; when the caller already has a tree-sitter tree for this
/// file (as [`crate::structure`] does), prefer [`changed_fn_complexity_in`] to
/// avoid a second parse.
pub fn changed_fn_complexity(
    path: &str,
    source: &str,
    changed: &HashSet<u64>,
) -> Vec<FnComplexity> {
    let Some(lang) = lang_for(path) else {
        return Vec::new();
    };
    let mut parser = Parser::new();
    if parser.set_language(&lang.grammar()).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };
    changed_fn_complexity_in(path, tree.root_node(), source, changed)
}

/// [`changed_fn_complexity`] over an already-parsed `root` — lets a caller share
/// one parse across structural symbols and complexity (same grammar per
/// extension). Fail-open: an unsupported `path` extension yields an empty vec.
pub(crate) fn changed_fn_complexity_in(
    path: &str,
    root: Node,
    source: &str,
    changed: &HashSet<u64>,
) -> Vec<FnComplexity> {
    let Some(lang) = lang_for(path) else {
        return Vec::new();
    };
    let src = source.as_bytes();

    let mut lines: Vec<u64> = changed.iter().copied().filter(|l| *l > 0).collect();
    lines.sort_unstable();

    let mut seen: HashSet<(u64, u64)> = HashSet::new();
    let mut out: Vec<FnComplexity> = Vec::new();
    for line in lines {
        let Some(func) = enclosing_function(root, line, lang) else {
            continue;
        };
        let start = func.start_position().row as u64 + 1;
        let end = func.end_position().row as u64 + 1;
        if !seen.insert((start, end)) {
            continue;
        }
        // Skip anonymous closures — no name to report, and they inflate noise.
        let Some(name) = lang.fn_name(func, src) else {
            continue;
        };
        out.push(FnComplexity {
            label: label_for(lang, func.kind()),
            name,
            cyclomatic: cyclomatic(func, lang, src),
            cognitive: cognitive(func, lang),
            start,
        });
    }
    out.sort_by_key(|f| f.start);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn changed(lines: &[u64]) -> HashSet<u64> {
        lines.iter().copied().collect()
    }

    fn only(path: &str, src: &str, line: u64) -> FnComplexity {
        let mut v = changed_fn_complexity(path, src, &changed(&[line]));
        assert_eq!(v.len(), 1, "expected exactly one function");
        v.pop().unwrap()
    }

    #[test]
    fn rust_cyclomatic_counts_branches_and_bool_ops() {
        let src = "\
fn score(x: i32) -> i32 {
    let mut n = 0;
    if x > 0 && x < 10 {
        n += 1;
    }
    for i in 0..x {
        if i % 2 == 0 {
            n += i;
        }
    }
    match n {
        0 => 0,
        1 => 1,
        _ => n,
    }
}
";
        let f = only("a.rs", src, 3);
        assert_eq!(f.name, "score");
        // 1 base + if + (&&) + for + if + 3 match_arms = 8
        assert_eq!(f.cyclomatic, 8, "cyclomatic");
        assert_eq!(f.grade(), 'B');
        assert!(f.cognitive >= 4, "cognitive reflects nesting: {}", f.cognitive);
    }

    #[test]
    fn typescript_counts_if_ternary_switch() {
        let src = "\
function classify(x: number): string {
  if (x < 0) return 'neg';
  const s = x === 0 ? 'zero' : 'pos';
  switch (s) {
    case 'zero': return 'z';
    case 'pos': return 'p';
    default: return s;
  }
}
";
        let f = only("a.ts", src, 2);
        assert_eq!(f.name, "classify");
        // 1 + if + ternary + 2 switch_case (default not counted) = 5
        assert_eq!(f.cyclomatic, 5);
        assert_eq!(f.grade(), 'A');
    }

    #[test]
    fn python_counts_if_elif_for_and_bool() {
        let src = "\
def pick(items):
    out = []
    for it in items:
        if it > 0 and it < 100:
            out.append(it)
        elif it == 0:
            out.append(-1)
    return out
";
        let f = only("a.py", src, 1);
        assert_eq!(f.name, "pick");
        // 1 + for + if + (and) + elif = 5
        assert_eq!(f.cyclomatic, 5);
    }

    #[test]
    fn named_arrow_function_is_measured() {
        // Arrow functions have no `name` field — recover it from the binding.
        // This is the dominant TS/React style, so it must produce a signal.
        let src = "\
const handleSubmit = (x: number) => {
  if (x > 0) { return 1; }
  return 0;
};
";
        let f = only("a.tsx", src, 2);
        assert_eq!(f.name, "handleSubmit");
        assert_eq!(f.cyclomatic, 2); // 1 + if
    }

    #[test]
    fn anonymous_inline_arrow_is_skipped() {
        // A bare callback has no binding name — no signal, no noise.
        let items = changed_fn_complexity(
            "a.ts",
            "function f(xs: number[]) { return xs.map((x) => x + 1); }\n",
            &changed(&[1]),
        );
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].name, "f");
    }

    #[test]
    fn go_counts_if_switch_cases_and_bool() {
        // Locks in the Go grammar node names (if_statement, expression_case, …).
        let src = "\
package main

func classify(x int) string {
	if x > 0 && x < 10 {
		return \"small\"
	}
	switch x {
	case 0:
		return \"zero\"
	case 1:
		return \"one\"
	default:
		return \"many\"
	}
}
";
        let f = only("a.go", src, 4);
        assert_eq!(f.name, "classify");
        // 1 + if + (&&) + 2 expression_case (default not counted) = 5
        assert_eq!(f.cyclomatic, 5);
    }

    #[test]
    fn trivial_function_grades_a() {
        let f = only("a.rs", "fn id(x: i32) -> i32 { x }\n", 1);
        assert_eq!(f.cyclomatic, 1);
        assert_eq!(f.cognitive, 0);
        assert_eq!(f.grade(), 'A');
    }

    #[test]
    fn nested_function_measured_separately() {
        // The inner closure's branches must NOT inflate the outer function.
        let src = "\
function outer(xs: number[]): number[] {
  return xs.map(function inner(x: number) {
    if (x > 0) { return x; }
    return 0;
  });
}
";
        let items = changed_fn_complexity("a.ts", src, &changed(&[1, 3]));
        let outer = items.iter().find(|f| f.name == "outer").unwrap();
        // outer itself has no branches of its own.
        assert_eq!(outer.cyclomatic, 1, "outer excludes inner's branch");
        let inner = items.iter().find(|f| f.name == "inner").unwrap();
        assert_eq!(inner.cyclomatic, 2, "inner counts its own if");
    }

    #[test]
    fn unsupported_or_unparsable_is_empty() {
        assert!(changed_fn_complexity("a.md", "# hi", &changed(&[1])).is_empty());
        assert!(changed_fn_complexity("a.rs", "", &changed(&[1])).is_empty());
    }
}
