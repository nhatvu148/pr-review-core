//! Print the ranked PR queue from a run log — the layer above one PR.
//!
//! A review ends when its comment posts, and the reviewer's memory of it ended
//! there too: every record needed to answer "which of our open PRs most needs a
//! human?" was already being written and never read. This reads it.
//!
//! Usage:
//!   cargo run --example queue -- ~/.local/share/pr-review/runs.jsonl
//!   cat runs.jsonl | cargo run --example queue
//!
//! Needs no key, no token, and no network: it is a fold over records you already
//! have. Set `PRBOT_RUN_LOG` to start producing them.
//!
//! ## What it cannot tell you
//!
//! A run log knows only about PRs the reviewer *ran on*. A PR nobody reviewed is
//! absent, and one whose review died before logging is too — so this is the queue
//! of what has been looked at, not the queue of what is open. Reconciling against
//! the provider's list of open PRs needs a provider token, which is a bot's job,
//! not this crate's.

use std::io::Read;

use pr_review_core::queue::{parse_jsonl, rank, render_queue, Priority};
use pr_review_core::runlog::RunLog;

fn main() -> anyhow::Result<()> {
    let text = match std::env::args().nth(1) {
        Some(path) => std::fs::read_to_string(&path)
            .map_err(|e| anyhow::anyhow!("cannot read {path}: {e}"))?,
        None => {
            let mut s = String::new();
            std::io::stdin().read_to_string(&mut s)?;
            s
        }
    };

    let records = parse_jsonl(&text);
    if records.is_empty() {
        eprintln!(
            "No run-log records found. Records carry `\"_kind\":\"{}\"`; set PRBOT_RUN_LOG to produce them.",
            pr_review_core::runlog::KIND
        );
        return Ok(());
    }

    let rows = rank(&records);
    println!("{}\n", render_queue(&rows, RunLog::now_unix()));

    let count = |p: Priority| rows.iter().filter(|r| r.priority == p).count();
    println!(
        "{} PR(s) from {} record(s) — P0 {}, P1 {}, P2 {}",
        rows.len(),
        records.len(),
        count(Priority::P0),
        count(Priority::P1),
        count(Priority::P2),
    );

    // Said out loud because a queue invites being read as complete, and this one
    // structurally is not.
    let skipped = records.len() - records.iter().filter(|r| !r.dry_run).count();
    if skipped > 0 {
        println!("({skipped} dry run(s) excluded — they post nothing to act on.)");
    }
    println!("Only PRs this reviewer has run on appear here.");
    Ok(())
}
