//! Probe whether the reviewer actually produces committable suggestions, and show
//! exactly what would post — without a PR, a provider, or a merge.
//!
//! Two questions the unit tests structurally cannot answer, because they stub the
//! model:
//!
//! 1. Does the model populate `suggestion` at all, under the deliberately narrow
//!    contract in `REVIEW_RULES`? A gate that never fires closes no gap.
//! 2. For the ones it does populate, does `suggest::sanitize` accept them? Every
//!    rejection is a fix the author has to retype, so the rejects are as
//!    interesting as the accepts — and each is printed with its reason.
//!
//! It also fills the preview hole: nothing in `RunReviewOutput` carries a rendered
//! inline body, so this is the only way to read the block before it posts.
//!
//! Usage:
//!   cargo run --example suggest_probe                 # built-in planted-bug diff
//!   cargo run --example suggest_probe -- my.diff      # your own unified diff
//!
//! Requires the review env (`OPENROUTER_API_KEY`, `OPENROUTER_MODEL`). One model
//! call per run — this is a probe, not a benchmark.

use pr_review_core::config::Config;
use pr_review_core::llm::review_diff;
use pr_review_core::providers::PrMeta;
use pr_review_core::suggest;

/// A diff whose bug is exactly the shape the contract asks for: wrong on one
/// line, fixable by rewriting that line. If the model declines to suggest here,
/// it will decline everywhere.
const PLANTED: &str = "\
diff --git a/src/session.ts b/src/session.ts
--- a/src/session.ts
+++ b/src/session.ts
@@ -40,3 +40,6 @@ import { issueSession } from './issue';
 export async function createSession(payload: Payload) {
+  const user = await lookup(payload.userId);
+  if (user.roles.includes('admin')) grantAdmin(user);
+  return issueSession(payload.userId);
 }
";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = Config::from_env();
    let diff = match std::env::args().nth(1) {
        Some(p) => std::fs::read_to_string(&p)?,
        None => PLANTED.to_string(),
    };

    // The line texts the orchestrator validates against, from the same parse.
    let texts = pr_review_core::diff::diff_line_texts(&diff);
    let valid = pr_review_core::diff::parse_valid_lines(&diff);

    let meta = PrMeta {
        repo: "probe/local".to_string(),
        pr: 0,
        title: Some("suggestion probe".to_string()),
        base_branch: None,
        head_sha: None,
        body: None,
        ci_status: None,
    };

    let system = format!(
        "{}\n\n{}",
        pr_review_core::prompt::SYSTEM_PROMPT,
        pr_review_core::prompt::REVIEW_RULES
    );
    let out = review_diff(
        &reqwest::Client::new(),
        &cfg,
        &meta,
        &diff,
        None,
        None,
        None,
        &system,
    )
    .await?;

    println!(
        "model: {}\nfindings: {}\n",
        out.model,
        out.review.findings.len()
    );

    let (mut proposed, mut accepted) = (0usize, 0usize);
    for f in &out.review.findings {
        let anchored = f
            .line
            .filter(|l| valid.get(&f.file).is_some_and(|s| s.contains(l)));
        println!("── {} {}:{:?}", f.severity, f.file, f.line);
        println!("   {}", f.body.trim());

        let Some(raw) = f.suggestion.as_deref() else {
            println!("   suggestion: none proposed\n");
            continue;
        };
        proposed += 1;

        // Exactly the orchestrator's gate: an anchor the model named itself, and
        // the current text of that line.
        let Some(line) = anchored else {
            println!("   suggestion: PROPOSED but the finding does not anchor — withheld\n");
            continue;
        };
        let current = texts.get(&f.file).and_then(|t| t.get(&line));
        match current.and_then(|c| suggest::sanitize(raw, c)) {
            Some(s) => {
                accepted += 1;
                println!("   suggestion: ACCEPTED — this is what posts:\n");
                for l in suggest::render("github", "", &s).trim().lines() {
                    println!("   | {l}");
                }
                println!();
            }
            None => {
                println!("   suggestion: REJECTED by sanitize. Raw model text was:");
                for l in raw.lines() {
                    println!("   ! {l}");
                }
                println!("   against line {line}: {:?}\n", current);
            }
        }
    }

    println!(
        "proposed {proposed} / {} findings, {accepted} survived validation",
        out.review.findings.len()
    );
    Ok(())
}
