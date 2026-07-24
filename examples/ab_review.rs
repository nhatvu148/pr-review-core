//! A/B harness: run the SAME pull request through the agentic reviewer twice —
//! once with the blast radius OFF, once ON — in dry-run (nothing is posted), and
//! diff the findings + token cost. Use it to judge whether precomputed
//! callers/tests actually surface cross-file findings the old path missed.
//!
//! Usage:
//!   cargo run --example ab_review -- <provider> <owner/repo> <pr>
//!   cargo run --example ab_review -- https://github.com/owner/repo/pull/123
//!
//! Requires the same env as a real review (OPENROUTER_API_KEY, a provider token,
//! e.g. GH_TOKEN). It FORCES `agentic=true` (blast radius only exists on the
//! agentic path) and toggles only `blast_radius` between the two runs.
//!
//! Caveat: the model runs at a non-zero temperature, so a single A/B pair is
//! directional, not proof. For a real signal, run a handful of PRs (ideally ones
//! with a KNOWN cross-file bug) and look for findings that appear only with blast
//! ON. Token cost is the cheaper, more stable signal — blast should keep agent
//! turns (hence tokens) flat or lower by removing hand-rolled caller hunts.

use pr_review_core::config::Config;
use pr_review_core::review::{run_review, RunReviewInput, RunReviewOutput};

/// Parse `https://github.com/owner/repo/pull/123` → (provider, "owner/repo", 123).
/// Only GitHub URLs are auto-detected; other providers use the 3-arg form.
fn parse_url(url: &str) -> Option<(String, String, u64)> {
    let rest = url.split("github.com/").nth(1)?;
    let parts: Vec<&str> = rest.split('/').collect();
    if parts.len() >= 4 && parts[2] == "pull" {
        let pr: u64 = parts[3].parse().ok()?;
        return Some(("github".into(), format!("{}/{}", parts[0], parts[1]), pr));
    }
    None
}

async fn run(cfg: &Config, provider: &str, repo: &str, pr: u64, blast: bool) -> RunReviewOutput {
    let mut c = cfg.clone();
    c.agentic = true; // blast radius is agentic-only
    c.blast_radius = blast;
    run_review(
        &c,
        RunReviewInput {
            provider: provider.to_string(),
            repo: repo.to_string(),
            pr,
            dry_run: true,
            placeholder: false,
        },
    )
    .await
    .unwrap_or_else(|e| panic!("review (blast={blast}) failed: {e:#}"))
}

fn tokens(out: &RunReviewOutput) -> u64 {
    out.usage
        .as_ref()
        .and_then(|u| u.total_tokens)
        .map(u64::from)
        .unwrap_or(0)
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (provider, repo, pr) = match args.as_slice() {
        [url] => parse_url(url).unwrap_or_else(|| {
            eprintln!("could not parse URL: {url}\nuse: <provider> <owner/repo> <pr>");
            std::process::exit(2);
        }),
        [p, r, n] => (p.clone(), r.clone(), n.parse().expect("pr must be a number")),
        _ => {
            eprintln!("usage: cargo run --example ab_review -- <provider> <owner/repo> <pr>");
            eprintln!("   or: cargo run --example ab_review -- <github-pr-url>");
            std::process::exit(2);
        }
    };

    let cfg = Config::from_env();
    eprintln!("A/B on {provider} {repo}#{pr} — running blast OFF then ON (dry-run)…\n");

    // We can't read the post-critique/cap findings back out of RunReviewOutput
    // (it returns counts + summary), so compare on the summary + counts + tokens
    // the output DOES expose. For a finding-level diff, run with SELF_CRITIQUE=false
    // and MIN_CONFIDENCE=0 and read the two summary_markdown blocks side by side.
    let off = run(&cfg, &provider, &repo, pr, false).await;
    let on = run(&cfg, &provider, &repo, pr, true).await;

    let bar = "─".repeat(64);
    println!("{bar}");
    println!("{:<28}{:>16}{:>16}", "metric", "blast OFF", "blast ON");
    println!("{bar}");
    println!("{:<28}{:>16}{:>16}", "recommendation", off.recommendation, on.recommendation);
    println!("{:<28}{:>16}{:>16}", "findings", off.findings, on.findings);
    println!("{:<28}{:>16}{:>16}", "inline posted", off.inline_posted, on.inline_posted);
    println!(
        "{:<28}{:>16}{:>16}",
        "total tokens",
        tokens(&off),
        tokens(&on)
    );
    let dt = tokens(&on) as i64 - tokens(&off) as i64;
    println!("{:<28}{:>32}", "Δ tokens (ON − OFF)", format!("{dt:+}"));
    println!("{bar}\n");

    println!("── blast OFF summary ──\n{}\n", off.summary_markdown);
    println!("── blast ON summary ──\n{}\n", on.summary_markdown);

    println!("Interpretation:");
    println!("  • findings ON > OFF, and the extra ones are cross-file (broken caller,");
    println!("    stale test) → blast is earning its keep.");
    println!("  • findings roughly equal but tokens ON ≤ OFF → same quality, cheaper.");
    println!("  • findings ON < OFF or noisier → investigate; blast may be misleading the model.");
    println!("\nRun several PRs (esp. ones with a known cross-file bug) before concluding.");
}
