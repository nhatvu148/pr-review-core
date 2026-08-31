//! Render the walkthrough table and change diagram for a local diff, so both can
//! be eyeballed before they are ever posted on someone's PR.
//!
//! No model, no network, no key: everything printed here comes from the
//! tree-sitter parse of the working tree, which is the whole claim these two
//! blocks make. If a row or an arrow here is wrong, it is wrong deterministically
//! and you can go read the code it names.
//!
//! ```console
//! $ git diff main... > /tmp/pr.diff
//! $ cargo run --example changemap_demo -- . /tmp/pr.diff
//! ```

use pr_review_core::changemap::{render_diagram, render_walkthrough};
use pr_review_core::config::Config;
use pr_review_core::structure::structural_context_local_mapped;

fn main() {
    let mut args = std::env::args().skip(1);
    let (root, diff_path) = match (args.next(), args.next()) {
        (Some(r), Some(d)) => (r, d),
        _ => {
            eprintln!("usage: changemap_demo <repo_root> <diff_file>");
            std::process::exit(2);
        }
    };
    let diff = std::fs::read_to_string(&diff_path).expect("read the diff file");

    let mut cfg = Config::from_env();
    cfg.structural_context = true;

    let (block, map) = structural_context_local_mapped(&cfg, std::path::Path::new(&root), &diff);

    eprintln!(
        "-- {} file(s), {} symbol(s), {} edge(s); prompt block {} line(s) --",
        map.files.len(),
        map.symbols.len(),
        map.edges.len(),
        block.lines().count()
    );

    // Every edge with the span it was derived from, so a suspicious arrow can be
    // checked against the file rather than argued about.
    for e in &map.edges {
        let (a, b) = (&map.symbols[e.from], &map.symbols[e.to]);
        eprintln!(
            "   {:?}  {} {} ({}:{}-{})  ->  {} {} ({}:{})",
            e.kind, a.label, a.name, a.file, a.start, a.end, b.label, b.name, b.file, b.start
        );
    }

    let walkthrough = render_walkthrough(&map, &[], cfg.walkthrough_max_symbols);
    let diagram = render_diagram(&map);
    if walkthrough.is_empty() && diagram.is_empty() {
        eprintln!("(nothing to render — no changed files resolved, or no edges to draw)");
        return;
    }
    println!("{walkthrough}");
    if !diagram.is_empty() {
        println!("\n{diagram}");
    }
}
