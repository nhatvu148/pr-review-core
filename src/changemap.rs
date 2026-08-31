//! Change map: the structured form of what a PR touched, and the two renderings
//! built from it — a **walkthrough table** and a **mermaid diagram**.
//!
//! Both are deliberately *derived*, never generated. [`crate::structure`] already
//! parses every changed file with tree-sitter to build the prompt's structural
//! context, and [`crate::complexity`] already grades the functions a change
//! touches from that same tree. Until now all of it was flattened into a prompt
//! string and thrown away. A [`ChangeMap`] keeps it, so the summary comment can
//! show the reader what the reviewer already knows — at no extra parse, no extra
//! fetch, and no extra token.
//!
//! **Why nothing here calls a model.** A diagram a model draws from a diff cannot
//! be checked by the reader: a plausible-looking arrow that does not exist in the
//! code is indistinguishable from a real one, and a wrong diagram is worse than no
//! diagram — it is a confident claim about control flow that the reader will
//! believe. Every edge drawn here comes from a call-shaped occurrence of one
//! changed symbol's name inside another changed symbol's span, in a file the
//! parser resolved. That is a narrow claim, and it is the claim the diagram makes.
//!
//! Scope, stated plainly so the picture is not over-read:
//!
//! - Nodes are only symbols **this PR changed**. This is a map of the change, not
//!   of the system.
//! - Edges are only references **between two changed symbols**. An edge to
//!   untouched code is invisible here; that is what
//!   [`blast radius`](crate::blast) is for.
//! - Detection is textual within a resolved span (`name(`, `<Name`), matching
//!   [`crate::blast`]'s grep-based call discovery. It is a *candidate* edge, and
//!   the render says so.

use std::collections::{HashMap, HashSet};

use crate::llm::Finding;
use crate::review::severity_emoji;

/// One changed definition, with the complexity of that definition when it is a
/// function the metrics pass graded.
#[derive(Debug, Clone)]
pub struct SymbolNode {
    /// Language-idiomatic kind label (`fn`, `class`, `func`, `interface`, …).
    pub label: &'static str,
    pub name: String,
    /// Path of the file the definition lives in (new side).
    pub file: String,
    /// 1-indexed inclusive line span of the definition.
    pub start: u64,
    pub end: u64,
    /// Cyclomatic / cognitive complexity, when this symbol is a function the
    /// complexity pass measured. `None` for a class/interface/type, or when
    /// `COMPLEXITY_METRICS` is off.
    pub cyclomatic: Option<u32>,
    pub cognitive: Option<u32>,
}

impl SymbolNode {
    /// A–F grade, using the same bands as [`crate::complexity::FnComplexity`].
    pub fn grade(&self) -> Option<char> {
        self.cyclomatic.map(grade_for)
    }
}

/// A–F from cyclomatic complexity: A ≤5 · B ≤10 · C ≤15 · D ≤25 · F otherwise.
fn grade_for(cyclomatic: u32) -> char {
    match cyclomatic {
        0..=5 => 'A',
        6..=10 => 'B',
        11..=15 => 'C',
        16..=25 => 'D',
        _ => 'F',
    }
}

/// How much of the map a caller wants built. The map is not free, and the two
/// halves have different prices: resolving symbols is a read of a parse that
/// already happened, while linking edges is a pairwise span scan. A caller that
/// renders only the walkthrough needs the first and never reads the second.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapDetail {
    /// No map at all — the prompt block only.
    None,
    /// Files, symbols, and grades. No edge linking.
    Symbols,
    /// Everything, including the edges the diagram draws.
    Full,
}

/// The most complex function a change touched in one file.
#[derive(Debug, Clone)]
pub struct WorstFn {
    pub name: String,
    pub cyclomatic: u32,
    pub cognitive: u32,
}

impl WorstFn {
    /// A–F grade, same bands as [`crate::complexity::FnComplexity`].
    pub fn grade(&self) -> char {
        grade_for(self.cyclomatic)
    }
}

/// One changed file: its diff line counts and the symbols resolved inside it.
#[derive(Debug, Clone)]
pub struct FileEntry {
    pub path: String,
    pub added: usize,
    pub removed: usize,
    /// Indices into [`ChangeMap::symbols`], in definition order.
    pub symbols: Vec<usize>,
    /// The file's most complex changed function, taken from the complexity pass
    /// **directly** rather than by joining through [`FileEntry::symbols`].
    ///
    /// The join was the bug. Tier B resolves a symbol by walking up to the nearest
    /// node `def_label` recognises, and for TS/JS that list has no
    /// `arrow_function` or `function_expression` — while the complexity pass's
    /// `is_function` has both, precisely because `const handleSubmit = () => {}`
    /// is the dominant modern TS/React style. So a React PR got a graded function
    /// in the prompt block and a bare `—` in the table describing the same change.
    /// A file's worst complexity is a fact about the file, and needs no symbol.
    pub worst: Option<WorstFn>,
}

/// How one changed symbol names another.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum EdgeKind {
    /// A call through a receiver (`x.name()`) whose type this pass cannot resolve,
    /// so the target may be a same-named method on something else entirely.
    ///
    /// Kept apart from [`EdgeKind::Type`], which used to share a variant with it,
    /// because the two have opposite reliability and only one of them needs
    /// guarding. See [`link_edges`] for the same-file rule this kind is subject to.
    Receiver,
    /// A type position — `Vec<T>`, `: T`, `Foo<T>`. Unambiguous: a type name in
    /// type position is that type.
    Type,
    /// A JSX element: `<Name/>`.
    Jsx,
    /// A call: `name(`.
    Call,
}

/// A candidate reference from one changed symbol to another. Indices into
/// [`ChangeMap::symbols`].
#[derive(Debug, Clone, Copy)]
pub struct Edge {
    pub from: usize,
    pub to: usize,
    pub kind: EdgeKind,
}

/// Everything structurally known about a change, kept rather than flattened.
#[derive(Debug, Clone, Default)]
pub struct ChangeMap {
    /// Changed files in diff order.
    pub files: Vec<FileEntry>,
    /// All resolved symbols, flat; [`FileEntry::symbols`] indexes into this.
    pub symbols: Vec<SymbolNode>,
    /// Candidate references between two changed symbols.
    pub edges: Vec<Edge>,
    /// Edge linking considered only the highest-complexity subset of the symbols
    /// because the change exceeded `DIAGRAM_MAX_NODES`. The diagram says so: a
    /// picture that silently dropped half the change would read as a claim that
    /// the missing arrows don't exist.
    pub edges_truncated: bool,
}

impl ChangeMap {
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Building
// ---------------------------------------------------------------------------

/// Per-file added/removed line counts, parsed from the same unified diff the
/// reviewer saw. Counts the diff as packed, so a file trimmed by the packer
/// reports what was actually reviewed rather than what the PR contains.
pub(crate) fn diff_line_counts(diff: &str) -> HashMap<String, (usize, usize)> {
    let mut out: HashMap<String, (usize, usize)> = HashMap::new();
    for (path, body) in crate::diff::split_diff_sections(diff) {
        if path.is_empty() {
            continue;
        }
        let e = out.entry(path).or_insert((0, 0));
        for line in body.lines() {
            if let Some(rest) = line.strip_prefix('+') {
                if !rest.starts_with("++") {
                    e.0 += 1;
                }
            } else if let Some(rest) = line.strip_prefix('-') {
                if !rest.starts_with("--") {
                    e.1 += 1;
                }
            }
        }
    }
    out
}

/// Is `hay[at..]` a standalone occurrence of a name — i.e. not the tail of a
/// longer identifier?
fn boundary_before(hay: &str, at: usize) -> bool {
    hay[..at]
        .chars()
        .next_back()
        .is_none_or(|c| !c.is_alphanumeric() && c != '_')
}

/// Classify a standalone occurrence of `name` at `at` by what follows (and, for
/// JSX, what precedes) it. Returns `None` when the occurrence is a bare mention
/// in prose-like position that we do not want to draw an arrow for.
fn classify_occurrence(hay: &str, at: usize, name: &str, js: bool) -> Option<EdgeKind> {
    let after = &hay[at + name.len()..];
    // Reject the tail of a longer identifier (`fooBar` matching `foo`).
    if after
        .chars()
        .next()
        .is_some_and(|c| c.is_alphanumeric() || c == '_')
    {
        return None;
    }
    // `<Name` is a JSX render in a JS-family file and a generic argument
    // (`Vec<LoggedFinding>`) everywhere else. Labelling a Rust type parameter
    // "renders" is the kind of confidently-wrong caption a derived diagram exists
    // to avoid, so the language decides which it is.
    if hay[..at].ends_with('<') {
        return Some(if js { EdgeKind::Jsx } else { EdgeKind::Type });
    }
    // `name(` / `name (` / `name::<T>(` — a call.
    let t = after.trim_start();
    if t.starts_with('(') || t.starts_with("::<") {
        // `x.name(` calls *a* `name`, but through a receiver whose type is not
        // resolved here — it may be a same-named method on an unrelated type
        // (`OpenOptions::append` vs this crate's `append`). Real enough to draw,
        // not certain enough to draw solid.
        return Some(if hay[..at].ends_with('.') {
            EdgeKind::Receiver
        } else {
            EdgeKind::Call
        });
    }
    // Deliberately NOT `name {`. It looks like a Rust struct literal (`Config {`)
    // and it is — but it is also `if cfg.structural_context {`, `-> Config {`,
    // `match x {` and every other block opener. The first demo run drew
    // `run_review_with -> structural_context` off an `if` condition and
    // `change_map_tests -> Config` off a return type: two arrows a reader would
    // have taken as call edges. Losing real struct literals is the cheaper error.
    None
}

/// The strongest edge kind from `body` to `name`, or `None` when `name` never
/// appears there in a referencing position.
fn strongest_reference(body: &str, name: &str, js: bool) -> Option<EdgeKind> {
    if name.is_empty() {
        return None;
    }
    let mut best: Option<EdgeKind> = None;
    for (at, _) in body.match_indices(name) {
        if !boundary_before(body, at) {
            continue;
        }
        if let Some(k) = classify_occurrence(body, at, name, js) {
            best = Some(best.map_or(k, |b: EdgeKind| b.max(k)));
        }
    }
    best
}

/// The text of a symbol's definition span within its file's `content`.
fn span_text(content: &str, start: u64, end: u64) -> &str {
    if start == 0 || start > end {
        return "";
    }
    let mut begin = (start == 1).then_some(0usize);
    let mut finish = content.len();
    for (i, (offset, _)) in content.match_indices('\n').enumerate() {
        let lineno = i as u64 + 1; // this newline ends line `lineno`
        if begin.is_none() && lineno + 1 == start {
            begin = Some(offset + 1);
        }
        if lineno == end {
            finish = offset;
            break;
        }
    }
    // A start line past the end of the file resolves to *nothing*, never to the
    // whole file. Falling back to offset 0 here would hand every symbol in the
    // file to the edge scan and manufacture a fan of arrows that don't exist —
    // the precise failure this render is built to avoid.
    let Some(begin) = begin else {
        return "";
    };
    content.get(begin..finish.max(begin)).unwrap_or("")
}

/// Fill in [`ChangeMap::edges`]: for every pair of changed symbols, does one name
/// the other inside its own definition span?
///
/// `contents` maps path → the new-side file text the parser already read. A file
/// missing from it simply contributes no outgoing edges — fail-open, like every
/// other structural step.
///
/// Cost is bounded by `max_nodes`: with the default 25 symbols this is ~600 span
/// scans, all substring matching over text already in memory. No parse, no I/O.
///
/// Past that ceiling the scan **narrows rather than gives up**. A 70-symbol PR is
/// ordinary, and an all-or-nothing ceiling meant the diagram never appeared on a
/// real change; ranking by complexity keeps the arrows that are worth looking at
/// and flags the narrowing on the map.
pub(crate) fn link_edges(map: &mut ChangeMap, contents: &HashMap<&str, &str>, max_nodes: usize) {
    let considered = rank_for_linking(map, max_nodes);
    map.edges_truncated = considered.len() < map.symbols.len();

    let mut seen: HashSet<(usize, usize)> = HashSet::new();
    let mut new_edges: Vec<Edge> = Vec::new();
    for &from in &considered {
        let a = &map.symbols[from];
        if is_container(a.label) {
            continue;
        }
        let Some(content) = contents.get(a.file.as_str()) else {
            continue;
        };
        let body = span_text(content, a.start, a.end);
        let js = crate::structure::is_js_family(&a.file);
        for &to in &considered {
            let b = &map.symbols[to];
            // No self-edges, and no edge between two symbols that share a name —
            // a same-name match inside the span is the definition, not a call.
            if from == to || a.name == b.name {
                continue;
            }
            // B nested inside A (a method of a changed class, a helper inside a
            // changed module) — the occurrence in A's span is B's *definition*.
            // Drawing that as a call would be a false arrow, and a false arrow is
            // the one thing this render cannot afford.
            if b.file == a.file && b.start >= a.start && b.end <= a.end {
                continue;
            }
            if let Some(kind) = strongest_reference(body, &b.name, js) {
                // A receiver-call names a method on a value whose type this pass
                // never resolved, so across files it is a name collision as often
                // as a reference — and method names collide constantly in OO code.
                //
                // The case that forced this: an Angular widget's
                // `this.loadKpis(token)` was drawn as an arrow to a *different*
                // widget's same-named `loadKpis` in another file. The call was to
                // its own class's method all along. Restricting the kind to
                // same-file targets makes that draw nothing, which is the right
                // answer — a false arrow is the one thing this render cannot
                // afford, and it is worth losing true cross-file method calls to
                // be rid of it.
                if kind == EdgeKind::Receiver && a.file != b.file {
                    continue;
                }
                if seen.insert((from, to)) {
                    new_edges.push(Edge { from, to, kind });
                }
            }
        }
    }
    map.edges.extend(new_edges);
}

/// Is this symbol test scaffolding rather than the code under change?
///
/// Two ways to be: it lives in a test *file*, or it is nested inside a changed
/// `mod tests` in a source file — the Rust convention this very crate uses, which
/// path-based detection alone cannot see. Without this the diagram of a
/// well-tested PR is a picture of its test suite: the first demo run ranked
/// fourteen `fn the_orchestrator_does_x` test cases above the functions they
/// exercise, because a long test body outranks a short production one on every
/// proxy that isn't this.
fn is_test_scope(map: &ChangeMap, i: usize) -> bool {
    let s = &map.symbols[i];
    if crate::blast::is_test_path(&s.file) {
        return true;
    }
    if is_test_mod(s) {
        return true;
    }
    map.symbols.iter().enumerate().any(|(j, outer)| {
        j != i
            && outer.file == s.file
            && is_test_mod(outer)
            && outer.start <= s.start
            && outer.end >= s.end
    })
}

/// A `mod tests` / `mod foo_tests` declaration.
fn is_test_mod(s: &SymbolNode) -> bool {
    s.label == "mod" && (s.name == "tests" || s.name.ends_with("_tests"))
}

/// Is this symbol a *container* — a definition whose span is mostly other
/// definitions' bodies?
///
/// Such a symbol is never an edge **source**. Everything a `mod` or an `impl`
/// block "references" is really referenced by a function nested inside it, and
/// attributing it to the container produces arrows like
/// `mod orchestrator_tests --> struct Config` that are true of the text and
/// false of the code. Containers remain valid *targets*: `Config {`-style uses
/// name them for real.
fn is_container(label: &str) -> bool {
    matches!(label, "mod" | "impl" | "class" | "trait" | "namespace")
}

/// The symbols edge linking will consider, most interesting first.
///
/// "Interesting" is the complexity the metrics pass already measured, then the
/// size of the definition — both proxies for *where a reviewer's attention is
/// worth spending*. Under the ceiling this is every symbol and the order is
/// irrelevant; over it, this is what decides which arrows get drawn.
fn rank_for_linking(map: &ChangeMap, max_nodes: usize) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..map.symbols.len()).collect();
    if idx.len() <= max_nodes {
        return idx;
    }
    idx.sort_by_key(|&i| {
        let s = &map.symbols[i];
        (
            // Test scaffolding sorts last, whatever it scores on the rest.
            is_test_scope(map, i),
            std::cmp::Reverse(s.cyclomatic.unwrap_or(0)),
            std::cmp::Reverse(s.end.saturating_sub(s.start)),
            i,
        )
    });
    idx.truncate(max_nodes);
    // Back to definition order so the diagram's node ids read top-to-bottom.
    idx.sort_unstable();
    idx
}

// ---------------------------------------------------------------------------
// Rendering — walkthrough table
// ---------------------------------------------------------------------------

/// Escape a cell so a symbol or path containing `|` can't break the table.
fn cell(s: &str) -> String {
    s.replace('|', "\\|")
}

/// Findings per file, as a severity-ordered emoji run (`🚨 ⚠️⚠️`).
fn findings_cell(findings: &[&Finding]) -> String {
    if findings.is_empty() {
        return "—".to_string();
    }
    let mut by_sev: Vec<&&Finding> = findings.iter().collect();
    by_sev.sort_by_key(|f| std::cmp::Reverse(crate::review::severity_rank(&f.severity)));
    by_sev
        .iter()
        .map(|f| severity_emoji(&f.severity))
        .collect::<Vec<_>>()
        .join("")
}

/// One file's symbol cell: definition order, de-duplicated by name, and capped.
///
/// The cap is not cosmetic. A one-line edit to a `lib.rs` resolves to every `mod`
/// declaration in it, and a change inside a test module resolves to every test
/// function — the first version of this table produced a 30-symbol cell that no
/// one would read, which is the same failure as no table at all. The count is
/// kept honest with a `(+N more)`, and the row's complexity column already
/// reports the worst symbol whether or not it is one of the ones shown.
fn render_symbol_list(syms: &[&SymbolNode], max: usize) -> String {
    if syms.is_empty() {
        return "—".to_string();
    }
    let mut seen: HashSet<(&str, &str)> = HashSet::new();
    let unique: Vec<&&SymbolNode> = syms
        .iter()
        .filter(|s| seen.insert((s.label, s.name.as_str())))
        .collect();
    let mut out: Vec<String> = unique
        .iter()
        .take(max)
        .map(|s| format!("`{} {}`", s.label, cell(&s.name)))
        .collect();
    if unique.len() > max {
        out.push(format!("_(+{} more)_", unique.len() - max));
    }
    out.join(", ")
}

/// The **walkthrough**: one row per changed file — what moved, which definitions
/// it landed in, the worst complexity grade among them, and how many findings the
/// review filed there.
///
/// Every column is already computed elsewhere in the pipeline. This costs one
/// table render and zero tokens, which is the whole point: the equivalent block
/// from a hosted competitor is an LLM call whose output nobody can check.
///
/// Returns an empty string when the map holds nothing worth tabulating.
pub fn render_walkthrough(map: &ChangeMap, findings: &[Finding], max_symbols: usize) -> String {
    if map.is_empty() {
        return String::new();
    }
    let mut by_file: HashMap<&str, Vec<&Finding>> = HashMap::new();
    for f in findings {
        by_file.entry(f.file.as_str()).or_default().push(f);
    }

    let sym_total: usize = map.files.iter().map(|f| f.symbols.len()).sum();
    // Column order is load-bearing, not taste. *Changed symbols* is the one cell
    // with no width bound — a Rust name runs 40 characters and four of them clear
    // 120 — so it goes last. A PR comment is a fixed-width column and the table
    // will overflow on a big change either way; what that costs is decided here.
    // With the verdict columns on the left, overflow hides the list you can scroll
    // to. With them on the right, overflow hid *Findings* behind a scrollbar, which
    // is how this shipped and how it looked in the real UI: a header reading
    // "Findir" and the review's own conclusions off the edge of the screen.
    let mut s = format!(
        "<details>\n<summary>🗺️ <b>Walkthrough</b> — {} file(s), {} changed symbol(s)</summary>\n\n\
         | File | +/− | Worst complexity | Findings | Changed symbols |\n\
         | --- | --- | --- | --- | --- |\n",
        map.files.len(),
        sym_total
    );

    for f in &map.files {
        let syms: Vec<&SymbolNode> = f.symbols.iter().map(|i| &map.symbols[*i]).collect();
        let names = render_symbol_list(&syms, max_symbols);
        // Absent when nothing in the file was graded (no function touched, or
        // `COMPLEXITY_METRICS` off) — an empty cell, never a fake "A".
        let worst = f
            .worst
            .as_ref()
            .map(|w| format!("{} ({})", w.grade(), w.cyclomatic))
            .unwrap_or_else(|| "—".to_string());
        let empty: Vec<&Finding> = Vec::new();
        let hits = by_file.get(f.path.as_str()).unwrap_or(&empty);
        s.push_str(&format!(
            "| `{}` | +{} −{} | {} | {} | {} |\n",
            cell(&f.path),
            f.added,
            f.removed,
            worst,
            findings_cell(hits),
            names
        ));
    }
    s.push_str("\n</details>");
    s
}

// ---------------------------------------------------------------------------
// Rendering — mermaid diagram
// ---------------------------------------------------------------------------

/// Sanitize a label for a mermaid node: quotes and brackets break the parser.
fn mermaid_label(s: &str) -> String {
    s.replace('"', "'")
        .replace(['[', ']', '{', '}', '(', ')'], "")
}

/// The **change diagram**: changed symbols grouped by file, with an arrow wherever
/// one names another.
///
/// Emitted only when there is at least one edge. A diagram of disconnected boxes
/// restates the walkthrough table in a form that is harder to read, and shipping
/// one on every PR is exactly the noise this reviewer spends `SELF_CRITIQUE` and
/// `MAX_FINDINGS` suppressing.
///
/// The caller is responsible for not calling this where mermaid does not render
/// (Bitbucket Cloud has no native support) — see [`supports_mermaid`].
pub fn render_diagram(map: &ChangeMap) -> String {
    if map.edges.is_empty() {
        return String::new();
    }
    // Only draw symbols that actually participate in an edge; a lone box adds
    // nothing to a picture whose whole subject is the relationships.
    let mut drawn: HashSet<usize> = HashSet::new();
    for e in &map.edges {
        drawn.insert(e.from);
        drawn.insert(e.to);
    }

    let mut s = String::from(
        "<details>\n<summary>🔗 <b>Change diagram</b> — how the changed symbols reference each \
         other</summary>\n\n```mermaid\nflowchart TD\n",
    );
    for (fi, f) in map.files.iter().enumerate() {
        let members: Vec<usize> = f
            .symbols
            .iter()
            .copied()
            .filter(|i| drawn.contains(i))
            .collect();
        if members.is_empty() {
            continue;
        }
        s.push_str(&format!(
            "  subgraph f{fi}[\"{}\"]\n",
            mermaid_label(&f.path)
        ));
        for i in members {
            let sym = &map.symbols[i];
            // Only annotate a grade worth reacting to. Most changed functions are
            // an A, so printing every grade puts a line of text on every box and
            // distinguishes nothing; showing only C and worse makes the risky nodes
            // stand out by contrast, with less ink rather than more.
            let grade = sym
                .grade()
                .filter(|g| matches!(g, 'C' | 'D' | 'F'))
                .map(|g| format!("<br/>grade {g}"))
                .unwrap_or_default();
            s.push_str(&format!(
                "    n{i}[\"{} {}{grade}\"]\n",
                mermaid_label(sym.label),
                mermaid_label(&sym.name)
            ));
        }
        s.push_str("  end\n");
    }
    for e in &map.edges {
        let arrow = match e.kind {
            EdgeKind::Call => "-->",
            EdgeKind::Jsx => "-.->|renders|",
            EdgeKind::Receiver => "-.->|names|",
            EdgeKind::Type => "-.->|type|",
        };
        s.push_str(&format!("  n{} {arrow} n{}\n", e.from, e.to));
    }
    s.push_str(
        "```\n\n_Derived from tree-sitter spans, not written by the model: an arrow means one \
         changed symbol names another inside its own definition. Candidate references — \
         a bare call to a name that several modules define is not disambiguated, and a \
         call through a receiver (`x.name()`) is drawn only within one file, where it is \
         likely to mean what it looks like. Only changed symbols appear; untouched callers \
         are out of scope. A grade is shown only at C or worse._",
    );
    if map.edges_truncated {
        s.push_str(
            "\n\n_This change has more symbols than `DIAGRAM_MAX_NODES`, so linking considered \
             only the most complex ones — a missing arrow here is not evidence that there \
             isn't one._",
        );
    }
    s.push_str("\n</details>");
    s
}

/// Whether a provider renders ```` ```mermaid ```` blocks in PR comments natively.
///
/// GitHub and GitLab do. Bitbucket Cloud does not — the request was closed
/// won't-fix, and the workarounds are a browser extension or a Marketplace app.
/// Posting the block there would leave a wall of raw mermaid source in the
/// comment, so the diagram is simply skipped.
pub fn supports_mermaid(provider: &str) -> bool {
    // `local` is not a host at all: a local review posts nowhere and hands its
    // markdown straight back to the caller, so there is no renderer to be
    // incompatible with. Withholding the diagram there would be withholding it
    // from the one caller guaranteed to be able to display it.
    matches!(
        provider,
        "github" | "gitlab" | crate::review::LOCAL_PROVIDER
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sym(name: &str, file: &str, start: u64, end: u64, cyc: Option<u32>) -> SymbolNode {
        SymbolNode {
            label: "fn",
            name: name.to_string(),
            file: file.to_string(),
            start,
            end,
            cyclomatic: cyc,
            cognitive: cyc,
        }
    }

    fn finding(file: &str, sev: &str) -> Finding {
        Finding {
            severity: sev.to_string(),
            file: file.to_string(),
            line: Some(1),
            body: "b".to_string(),
            confidence: None,
        }
    }

    #[test]
    fn span_text_slices_the_inclusive_line_range() {
        let c = "one\ntwo\nthree\nfour\n";
        assert_eq!(span_text(c, 2, 3), "two\nthree");
        assert_eq!(span_text(c, 1, 1), "one");
    }

    #[test]
    fn span_text_survives_a_span_past_the_end_of_the_file() {
        // A stale/again-edited file must never panic the render.
        assert_eq!(span_text("a\nb", 1, 99), "a\nb");
        assert_eq!(span_text("a\nb", 9, 99), "");
    }

    #[test]
    fn a_call_beats_a_bare_mention_and_a_substring_is_not_a_reference() {
        assert_eq!(
            strongest_reference("let x = parse(y);", "parse", false),
            Some(EdgeKind::Call)
        );
        assert_eq!(
            strongest_reference("<Widget />", "Widget", true),
            Some(EdgeKind::Jsx)
        );
        // `parse` is a prefix of `parser` — not a reference to `parse`.
        assert_eq!(
            strongest_reference("let p = parser(y);", "parse", false),
            None
        );
        // ...and a suffix match must not count either.
        assert_eq!(
            strongest_reference("let p = reparse(y);", "parse", false),
            None
        );
        // Named in prose or in a comment, never called: no arrow.
        assert_eq!(
            strongest_reference("// see parse for details", "parse", false),
            None
        );
    }

    #[test]
    fn edges_link_two_changed_symbols_but_never_a_symbol_to_itself() {
        let mut map = ChangeMap {
            symbols: vec![
                sym("caller", "a.rs", 1, 3, None),
                sym("callee", "b.rs", 1, 2, None),
            ],
            files: vec![],
            edges: vec![],
            edges_truncated: false,
        };
        let a = "fn caller() {\n    callee();\n}\n";
        let b = "fn callee() {\n}\n";
        let contents: HashMap<&str, &str> = [("a.rs", a), ("b.rs", b)].into_iter().collect();
        link_edges(&mut map, &contents, 25);
        assert_eq!(map.edges.len(), 1, "{:?}", map.edges);
        assert_eq!(map.edges[0].from, 0);
        assert_eq!(map.edges[0].to, 1);
        assert_eq!(map.edges[0].kind, EdgeKind::Call);
    }

    #[test]
    fn a_nested_definition_is_not_drawn_as_a_call_to_itself() {
        // `helper` is defined *inside* `outer`'s span. The occurrence is the
        // definition, not a call — an arrow there would be a lie.
        let mut map = ChangeMap {
            symbols: vec![
                sym("outer", "a.rs", 1, 5, None),
                sym("helper", "a.rs", 2, 3, None),
            ],
            files: vec![],
            edges: vec![],
            edges_truncated: false,
        };
        let a = "fn outer() {\n    fn helper() {\n    }\n    0\n}\n";
        let contents: HashMap<&str, &str> = [("a.rs", a)].into_iter().collect();
        link_edges(&mut map, &contents, 25);
        assert!(map.edges.is_empty(), "{:?}", map.edges);
    }

    /// Past the ceiling the scan narrows to the most complex symbols instead of
    /// giving up — the bug the first demo run exposed: a 71-symbol PR produced
    /// zero edges and therefore no diagram, ever.
    #[test]
    fn past_the_ceiling_linking_narrows_to_the_most_complex_symbols() {
        let mut map = ChangeMap {
            symbols: vec![
                sym("trivial", "a.rs", 1, 2, Some(1)),
                sym("hot", "a.rs", 4, 6, Some(30)),
                sym("callee", "b.rs", 1, 2, Some(20)),
            ],
            files: vec![],
            edges: vec![],
            edges_truncated: false,
        };
        let a = "fn trivial() {\n    callee();\n}\nfn hot() {\n    callee();\n}\n";
        let contents: HashMap<&str, &str> = [("a.rs", a)].into_iter().collect();
        link_edges(&mut map, &contents, 2);

        assert!(
            map.edges_truncated,
            "narrowing must be recorded, not silent"
        );
        // `hot` (30) and `callee` (20) survive the ranking; `trivial` (1) does not,
        // so its call is not drawn.
        assert_eq!(map.edges.len(), 1, "{:?}", map.edges);
        assert_eq!(map.symbols[map.edges[0].from].name, "hot");
    }

    /// A `mod tests` in a source file is not a test *path*, so only the nesting
    /// rule catches it — and without it the diagram becomes a picture of the
    /// test suite.
    #[test]
    fn test_scaffolding_loses_to_production_code_at_the_ceiling() {
        let mut map = ChangeMap {
            symbols: vec![
                SymbolNode {
                    label: "mod",
                    name: "tests".into(),
                    file: "a.rs".into(),
                    start: 10,
                    end: 40,
                    cyclomatic: None,
                    cognitive: None,
                },
                sym("a_long_and_thorough_test_case", "a.rs", 11, 39, Some(2)),
                sym("modest_production_fn", "a.rs", 1, 4, Some(2)),
            ],
            files: vec![],
            edges: vec![],
            edges_truncated: false,
        };
        // Ceiling of 1: exactly one symbol survives, and it must not be the test.
        let kept = rank_for_linking(&map, 1);
        assert_eq!(map.symbols[kept[0]].name, "modest_production_fn");
        // Sanity: the mod itself is test scope too.
        assert!(is_test_scope(&map, 1));
        map.edges_truncated = false;
    }

    /// The two false-arrow classes the first demo run produced, pinned.
    #[test]
    fn a_block_opener_is_never_read_as_a_reference() {
        // `if cfg.structural_context {` — a condition, not a call.
        assert_eq!(
            strongest_reference("if cfg.structural_context {", "structural_context", false),
            None
        );
        // `-> Config {` — a return type, not a struct literal.
        assert_eq!(
            strongest_reference("fn conf() -> Config {", "Config", false),
            None
        );
        // A genuine call still lands.
        assert_eq!(
            strongest_reference("Config::from_env()", "from_env", false),
            Some(EdgeKind::Call)
        );
    }

    #[test]
    fn a_container_is_never_an_edge_source() {
        let mut map = ChangeMap {
            symbols: vec![
                SymbolNode {
                    label: "mod",
                    name: "inner".into(),
                    file: "a.rs".into(),
                    start: 1,
                    end: 4,
                    cyclomatic: None,
                    cognitive: None,
                },
                sym("target", "b.rs", 1, 2, None),
            ],
            files: vec![],
            edges: vec![],
            edges_truncated: false,
        };
        let a = "mod inner {\n    fn x() { target(); }\n}\n";
        let contents: HashMap<&str, &str> = [("a.rs", a)].into_iter().collect();
        link_edges(&mut map, &contents, 25);
        // The call belongs to `fn x`, which this diff did not resolve as a changed
        // symbol — so there is no arrow to draw, not an arrow from the module.
        assert!(map.edges.is_empty(), "{:?}", map.edges);
    }

    /// `<Name` means two different things in two different languages, and the
    /// wrong one produced a Rust `struct RunLog -.->|renders| LoggedFinding`.
    #[test]
    fn an_angle_bracket_is_jsx_only_in_a_js_family_file() {
        assert_eq!(
            strongest_reference(
                "let v: Vec<LoggedFinding> = vec![];",
                "LoggedFinding",
                false
            ),
            Some(EdgeKind::Type)
        );
        assert_eq!(
            strongest_reference("return <LoggedFinding />;", "LoggedFinding", true),
            Some(EdgeKind::Jsx)
        );
    }

    /// `.append(true)` on an `OpenOptions` is not a call to this crate's
    /// `append` — real enough to draw, not certain enough to draw solid.
    #[test]
    fn a_receiver_call_is_weaker_than_a_bare_call() {
        assert_eq!(
            strongest_reference("f.append(true)", "append", false),
            Some(EdgeKind::Receiver)
        );
        assert_eq!(
            strongest_reference("append(&path, &rec)", "append", false),
            Some(EdgeKind::Call)
        );
    }

    /// The simcel-saas#75 case. An Angular widget's `this.loadKpis(token)` was
    /// drawn as an arrow to a *different* widget's same-named `loadKpis` in
    /// another file; the call was to its own class's method all along. A
    /// receiver-call only links within one file now, so this draws nothing.
    #[test]
    fn a_receiver_call_never_reaches_across_files() {
        let mut map = ChangeMap {
            symbols: vec![
                sym("loadKpisForScenario", "kpi-table-widget.ts", 1, 3, None),
                sym("loadKpis", "pl-chart-widget.ts", 1, 2, None),
            ],
            files: vec![],
            edges: vec![],
            edges_truncated: false,
        };
        let a = "loadKpisForScenario() {\n    this.loadKpis(token);\n}\n";
        let contents: HashMap<&str, &str> = [("kpi-table-widget.ts", a)].into_iter().collect();
        link_edges(&mut map, &contents, 25);
        assert!(map.edges.is_empty(), "{:?}", map.edges);
    }

    /// ...but within one file `this.helper()` is very likely that file's helper,
    /// so the weaker edge is still worth drawing there.
    #[test]
    fn a_receiver_call_inside_one_file_still_links() {
        let mut map = ChangeMap {
            symbols: vec![
                sym("caller", "w.ts", 1, 3, None),
                sym("helper", "w.ts", 5, 6, None),
            ],
            files: vec![],
            edges: vec![],
            edges_truncated: false,
        };
        let a = "caller() {\n    this.helper();\n}\n\nhelper() {\n}\n";
        let contents: HashMap<&str, &str> = [("w.ts", a)].into_iter().collect();
        link_edges(&mut map, &contents, 25);
        assert_eq!(map.edges.len(), 1, "{:?}", map.edges);
        assert_eq!(map.edges[0].kind, EdgeKind::Receiver);
    }

    /// A type-position reference is unambiguous and crosses files freely — it was
    /// only ever guarded because it shared a variant with the receiver case.
    #[test]
    fn a_type_reference_still_crosses_files() {
        let mut map = ChangeMap {
            symbols: vec![
                sym("holder", "a.rs", 1, 3, None),
                sym("LoggedFinding", "b.rs", 1, 2, None),
            ],
            files: vec![],
            edges: vec![],
            edges_truncated: false,
        };
        let a = "struct holder {\n    items: Vec<LoggedFinding>,\n}\n";
        let contents: HashMap<&str, &str> = [("a.rs", a)].into_iter().collect();
        link_edges(&mut map, &contents, 25);
        assert_eq!(map.edges.len(), 1, "{:?}", map.edges);
        assert_eq!(map.edges[0].kind, EdgeKind::Type);
    }

    #[test]
    fn under_the_ceiling_nothing_is_marked_truncated() {
        let mut map = ChangeMap {
            symbols: vec![sym("a", "a.rs", 1, 2, None)],
            files: vec![],
            edges: vec![],
            edges_truncated: false,
        };
        link_edges(&mut map, &HashMap::new(), 25);
        assert!(!map.edges_truncated);
    }

    #[test]
    fn a_long_symbol_cell_collapses_and_says_how_many_it_hid() {
        let syms: Vec<SymbolNode> = (0..10)
            .map(|i| sym(&format!("f{i}"), "a.rs", 1, 2, None))
            .collect();
        let refs: Vec<&SymbolNode> = syms.iter().collect();
        let out = render_symbol_list(&refs, 3);
        assert!(out.contains("`fn f0`") && out.contains("`fn f2`"), "{out}");
        assert!(!out.contains("`fn f3`"), "{out}");
        assert!(out.contains("(+7 more)"), "{out}");
    }

    /// Two `impl` blocks in a file both yield `fn review`; listing it twice tells
    /// the reader nothing and crowds out a symbol that would.
    #[test]
    fn a_repeated_symbol_name_is_listed_once() {
        let syms = [
            sym("review", "a.rs", 1, 2, None),
            sym("review", "a.rs", 8, 9, None),
            sym("other", "a.rs", 12, 13, None),
        ];
        let refs: Vec<&SymbolNode> = syms.iter().collect();
        let out = render_symbol_list(&refs, 6);
        assert_eq!(out.matches("`fn review`").count(), 1, "{out}");
        assert!(out.contains("`fn other`"), "{out}");
    }

    #[test]
    fn diff_line_counts_ignore_the_file_headers() {
        let d = "diff --git a/x.rs b/x.rs\n--- a/x.rs\n+++ b/x.rs\n@@ -1,2 +1,3 @@\n+added\n-gone\n ctx\n";
        let c = diff_line_counts(d);
        assert_eq!(c.get("x.rs").copied(), Some((1, 1)));
    }

    #[test]
    fn the_walkthrough_tabulates_every_file_and_grades_the_worst_function() {
        let map = ChangeMap {
            symbols: vec![
                sym("small", "a.rs", 1, 3, Some(4)),
                sym("big", "a.rs", 5, 9, Some(30)),
            ],
            files: vec![
                FileEntry {
                    path: "a.rs".into(),
                    added: 9,
                    removed: 2,
                    symbols: vec![0, 1],
                    // The grade comes from the complexity pass, never from a join
                    // through `symbols` — see `FileEntry::worst`.
                    worst: Some(WorstFn {
                        name: "big".into(),
                        cyclomatic: 30,
                        cognitive: 30,
                    }),
                },
                FileEntry {
                    path: "README.md".into(),
                    added: 1,
                    removed: 0,
                    symbols: vec![],
                    worst: None,
                },
            ],
            edges: vec![],
            edges_truncated: false,
        };
        let out = render_walkthrough(&map, &[finding("a.rs", "HIGH")], 6);
        assert!(out.contains("| `a.rs` | +9 −2 |"), "{out}");
        // Verdict columns before the unbounded one, so overflow can't hide them.
        let header = out
            .lines()
            .find(|l| l.starts_with("| File"))
            .expect("header");
        assert!(
            header.find("Findings") < header.find("Changed symbols"),
            "{header}"
        );
        assert!(
            out.contains("`fn small`") && out.contains("`fn big`"),
            "{out}"
        );
        // Worst grade wins: cyclomatic 30 is an F, not the 4's A.
        assert!(out.contains("F (30)"), "{out}");
        // An unparsed file still gets a row, with no invented grade.
        assert!(out.contains("| `README.md` | +1 −0 | — | — | — |"), "{out}");
        assert!(out.contains("⚠️"), "{out}");
    }

    #[test]
    fn a_pipe_in_a_name_cannot_break_the_table() {
        let map = ChangeMap {
            symbols: vec![sym("a|b", "x.rs", 1, 2, None)],
            files: vec![FileEntry {
                path: "x.rs".into(),
                added: 1,
                removed: 0,
                symbols: vec![0],
                worst: None,
            }],
            edges: vec![],
            edges_truncated: false,
        };
        let out = render_walkthrough(&map, &[], 6);
        assert!(out.contains("a\\|b"), "{out}");
    }

    #[test]
    fn no_edges_means_no_diagram() {
        let map = ChangeMap {
            symbols: vec![sym("lonely", "a.rs", 1, 2, None)],
            files: vec![FileEntry {
                path: "a.rs".into(),
                added: 1,
                removed: 0,
                symbols: vec![0],
                worst: None,
            }],
            edges: vec![],
            edges_truncated: false,
        };
        assert!(render_diagram(&map).is_empty());
    }

    #[test]
    fn the_diagram_groups_by_file_and_draws_only_linked_symbols() {
        let map = ChangeMap {
            symbols: vec![
                sym("caller", "a.rs", 1, 3, Some(12)),
                sym("callee", "b.rs", 1, 2, None),
                sym("unrelated", "b.rs", 5, 6, None),
            ],
            files: vec![
                FileEntry {
                    path: "a.rs".into(),
                    added: 3,
                    removed: 0,
                    symbols: vec![0],
                    worst: None,
                },
                FileEntry {
                    path: "b.rs".into(),
                    added: 2,
                    removed: 0,
                    symbols: vec![1, 2],
                    worst: None,
                },
            ],
            edges: vec![Edge {
                from: 0,
                to: 1,
                kind: EdgeKind::Call,
            }],
            edges_truncated: false,
        };
        let out = render_diagram(&map);
        assert!(out.contains("```mermaid"), "{out}");
        // TD, not LR: a PR comment is a narrow column, and LR sprawled past it.
        assert!(out.contains("flowchart TD"), "{out}");
        assert!(out.contains("subgraph f0[\"a.rs\"]"), "{out}");
        assert!(out.contains("n0 --> n1"), "{out}");
        // The graded node carries its grade; the unlinked one isn't drawn at all.
        assert!(out.contains("grade C"), "{out}");
        assert!(!out.contains("unrelated"), "{out}");
    }

    /// A grade on every box is a line of text that distinguishes nothing, since
    /// most changed functions are an A. Only C and worse earn the annotation.
    #[test]
    fn only_a_grade_worth_reacting_to_is_drawn() {
        let map = ChangeMap {
            symbols: vec![
                sym("calm", "a.rs", 1, 2, Some(3)),   // A
                sym("risky", "b.rs", 1, 2, Some(30)), // F
            ],
            files: vec![
                FileEntry {
                    path: "a.rs".into(),
                    added: 1,
                    removed: 0,
                    symbols: vec![0],
                    worst: None,
                },
                FileEntry {
                    path: "b.rs".into(),
                    added: 1,
                    removed: 0,
                    symbols: vec![1],
                    worst: None,
                },
            ],
            edges: vec![Edge {
                from: 0,
                to: 1,
                kind: EdgeKind::Call,
            }],
            edges_truncated: false,
        };
        let out = render_diagram(&map);
        assert!(out.contains("grade F"), "{out}");
        assert!(!out.contains("grade A"), "{out}");
    }

    #[test]
    fn mermaid_labels_lose_the_characters_that_break_the_parser() {
        let map = ChangeMap {
            symbols: vec![
                sym("f(x)", "a.rs", 1, 2, None),
                sym("g[y]", "b.rs", 1, 2, None),
            ],
            files: vec![
                FileEntry {
                    path: "a.rs".into(),
                    added: 1,
                    removed: 0,
                    symbols: vec![0],
                    worst: None,
                },
                FileEntry {
                    path: "b.rs".into(),
                    added: 1,
                    removed: 0,
                    symbols: vec![1],
                    worst: None,
                },
            ],
            edges: vec![Edge {
                from: 0,
                to: 1,
                kind: EdgeKind::Call,
            }],
            edges_truncated: false,
        };
        let out = render_diagram(&map);
        assert!(out.contains("n0[\"fn fx\"]"), "{out}");
        assert!(out.contains("n1[\"fn gy\"]"), "{out}");
    }

    #[test]
    fn bitbucket_gets_no_mermaid_because_it_would_not_render_it() {
        assert!(supports_mermaid("github"));
        assert!(supports_mermaid("gitlab"));
        assert!(supports_mermaid(crate::review::LOCAL_PROVIDER));
        assert!(!supports_mermaid("bitbucket"));
    }
}
