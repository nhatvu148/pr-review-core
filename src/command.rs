//! PR comment commands (T3.9): `/review`, `/ask <question>`, and `/describe`.
//!
//! A single entry point — [`run_command`] — lets a bot binary route any
//! recognized comment command through the core without wiring each one itself.
//! [`parse_command`] turns a raw comment body into a [`Command`]; the caller is
//! responsible for the gating that's cheap to do provider-side (the event is a
//! newly-created comment on a PR).

use anyhow::Result;

use crate::config::Config;
use crate::providers::{PrMeta, Provider};
use crate::review::{load_repo_config, run_review, RunReviewInput};

/// HTML-comment delimiters wrapping the AI-generated section of a PR description
/// so `/describe` can regenerate idempotently while preserving human-written
/// content around it. (GitHub/GitLab hide these; Bitbucket renders them literally
/// — a minor cosmetic quirk on that provider.)
const DESC_START: &str = "<!-- prbot:describe:start -->";
const DESC_END: &str = "<!-- prbot:describe:end -->";

/// A recognized PR comment command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// `/review` — (re)run the full review.
    Review,
    /// `/ask <question>` — answer a question about the PR.
    Ask(String),
    /// `/describe` — (re)generate the PR description from the diff.
    Describe,
}

/// What a command run did, for the caller to log.
#[derive(Debug, Clone)]
pub struct CommandOutcome {
    /// `"review"`, `"ask"`, or `"describe"`.
    pub command: &'static str,
    /// URL of the comment posted (or the review summary), when available.
    pub comment_url: Option<String>,
}

/// Parse a comment body into a [`Command`], or `None` if it isn't one.
///
/// The command must be the first token of the comment. `/ask` takes the rest of
/// the comment (which may span multiple lines) as its question; an empty question
/// yields `None`. Surrounding whitespace is ignored.
///
/// # Examples
/// ```
/// # use pr_review_core::command::{parse_command, Command};
/// assert_eq!(parse_command("/review"), Some(Command::Review));
/// assert_eq!(parse_command("  /describe \n"), Some(Command::Describe));
/// assert_eq!(parse_command("/ask why is this safe?"), Some(Command::Ask("why is this safe?".into())));
/// assert_eq!(parse_command("/ask"), None);            // no question
/// assert_eq!(parse_command("please /review"), None);  // not the first token
/// assert_eq!(parse_command("/reviews"), None);        // no fuzzy match
/// ```
pub fn parse_command(body: &str) -> Option<Command> {
    let trimmed = body.trim();
    let mut lines = trimmed.lines();
    let first = lines.next().unwrap_or("").trim();
    let (cmd, rest) = match first.split_once(char::is_whitespace) {
        Some((c, r)) => (c, r.trim()),
        None => (first, ""),
    };
    match cmd {
        "/review" => Some(Command::Review),
        "/describe" => Some(Command::Describe),
        "/ask" => {
            // The question is the remainder of the first line plus any following
            // lines, so a multi-line question survives intact.
            let mut q = rest.to_string();
            let tail: Vec<&str> = lines.collect();
            if !tail.is_empty() {
                if !q.is_empty() {
                    q.push('\n');
                }
                q.push_str(&tail.join("\n"));
            }
            let q = q.trim().to_string();
            if q.is_empty() {
                None
            } else {
                Some(Command::Ask(q))
            }
        }
        _ => None,
    }
}

/// Execute a parsed command end-to-end.
///
/// `/review` delegates to [`run_review`] (with an instant "Reviewing…"
/// placeholder). `/ask` and `/describe` fetch the diff, respect a per-repo
/// `.prbot.toml`, and post their result.
///
/// # Errors
/// On unknown provider, or any provider/LLM API failure.
pub async fn run_command(
    cfg: &Config,
    provider_name: &str,
    repo: &str,
    pr: u64,
    cmd: Command,
) -> Result<CommandOutcome> {
    match cmd {
        Command::Review => {
            let out = run_review(
                cfg,
                RunReviewInput {
                    provider: provider_name.to_string(),
                    repo: repo.to_string(),
                    pr,
                    dry_run: false,
                    placeholder: true,
                },
            )
            .await?;
            Ok(CommandOutcome {
                command: "review",
                comment_url: out.comment_url,
            })
        }
        Command::Ask(question) => run_ask(cfg, provider_name, repo, pr, &question).await,
        Command::Describe => run_describe(cfg, provider_name, repo, pr).await,
    }
}

/// Fetch the PR diff and prepare it exactly as the review path does — glob
/// filter, size packing, and (optionally) structural context — so `/ask` and
/// `/describe` reason over the same trimmed, budgeted diff the reviewer sees.
async fn prepared_diff(
    provider: &Provider,
    client: &reqwest::Client,
    cfg: &Config,
    repo: &str,
    meta: &PrMeta,
) -> Result<(String, String)> {
    let raw = provider.get_diff(client, cfg, repo, meta.pr).await?;
    let (diff, _dropped) =
        crate::diff::filter_diff_by_globs(&raw, &cfg.include_globs, &cfg.exclude_globs);
    let (diff, _packed) = crate::diff::pack_diff(&diff, cfg.max_diff_chars);
    let structural = if cfg.structural_context && !diff.trim().is_empty() {
        crate::structure::structural_context(provider, client, cfg, repo, meta, &diff).await
    } else {
        String::new()
    };
    Ok((diff, structural))
}

/// `/ask`: answer a question about the PR and post it as a reply comment.
async fn run_ask(
    cfg: &Config,
    provider_name: &str,
    repo: &str,
    pr: u64,
    question: &str,
) -> Result<CommandOutcome> {
    let provider = Provider::from_name(provider_name)?;
    let client = reqwest::Client::new();
    let meta = provider.get_meta(&client, cfg, repo, pr).await?;
    let effective = load_repo_config(&provider, &client, cfg, repo, &meta).await;
    let cfg = &effective;

    let (diff, structural) = prepared_diff(&provider, &client, cfg, repo, &meta).await?;
    if diff.trim().is_empty() {
        let body = format!(
            "> **/ask** {question}\n\nThere are no reviewable source changes in this PR to answer against."
        );
        let url = provider.post_comment(&client, cfg, repo, pr, &body).await?;
        return Ok(CommandOutcome {
            command: "ask",
            comment_url: url,
        });
    }

    let structural_opt = (!structural.is_empty()).then_some(structural.as_str());
    let answer =
        crate::llm::answer_question(&client, cfg, &meta, &diff, question, structural_opt).await?;
    // Echo the question so the thread reads as a Q&A exchange.
    let body = format!("> **/ask** {question}\n\n{answer}");
    let url = provider.post_comment(&client, cfg, repo, pr, &body).await?;
    Ok(CommandOutcome {
        command: "ask",
        comment_url: url,
    })
}

/// `/describe`: generate a PR description, merge it into the existing body
/// (preserving human-written content), update the PR, and confirm in a comment.
async fn run_describe(
    cfg: &Config,
    provider_name: &str,
    repo: &str,
    pr: u64,
) -> Result<CommandOutcome> {
    let provider = Provider::from_name(provider_name)?;
    let client = reqwest::Client::new();
    let meta = provider.get_meta(&client, cfg, repo, pr).await?;
    let effective = load_repo_config(&provider, &client, cfg, repo, &meta).await;
    let cfg = &effective;

    let (diff, structural) = prepared_diff(&provider, &client, cfg, repo, &meta).await?;
    if diff.trim().is_empty() {
        let url = provider
            .post_comment(
                &client,
                cfg,
                repo,
                pr,
                "No reviewable source changes to describe.",
            )
            .await?;
        return Ok(CommandOutcome {
            command: "describe",
            comment_url: url,
        });
    }

    let structural_opt = (!structural.is_empty()).then_some(structural.as_str());
    let generated = crate::llm::describe_pr(&client, cfg, &meta, &diff, structural_opt).await?;
    let merged = merge_description(meta.body.as_deref().unwrap_or(""), &generated);
    provider
        .update_pr_description(&client, cfg, &meta, &merged)
        .await?;
    let url = provider
        .post_comment(&client, cfg, repo, pr, "📝 Updated the PR description.")
        .await?;
    Ok(CommandOutcome {
        command: "describe",
        comment_url: url,
    })
}

/// Merge a freshly-generated description into an existing PR body.
///
/// The generated text is wrapped between [`DESC_START`]/[`DESC_END`] markers. If
/// those markers already exist (a prior `/describe`), only the section between
/// them is replaced, preserving anything the author wrote around it. Otherwise
/// the marked block is prepended to the existing body (or becomes the whole body
/// when it was empty).
///
/// # Examples
/// ```
/// # use pr_review_core::command::merge_description;
/// // First run on an empty body: just the generated block.
/// let out = merge_description("", "## Summary\nDoes a thing.");
/// assert!(out.contains("Does a thing."));
/// // Re-run replaces only the generated section, keeping human notes.
/// let again = merge_description(&out, "## Summary\nUpdated.");
/// assert!(again.contains("Updated."));
/// assert!(!again.contains("Does a thing."));
/// ```
pub fn merge_description(existing: &str, generated: &str) -> String {
    let block = format!("{DESC_START}\n{}\n{DESC_END}", generated.trim());
    if let (Some(s), Some(e)) = (existing.find(DESC_START), existing.find(DESC_END)) {
        if e > s {
            let end = e + DESC_END.len();
            return format!("{}{}{}", &existing[..s], block, &existing[end..]);
        }
    }
    if existing.trim().is_empty() {
        block
    } else {
        format!("{block}\n\n{}", existing.trim())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_three_commands() {
        assert_eq!(parse_command("/review"), Some(Command::Review));
        assert_eq!(parse_command("/describe"), Some(Command::Describe));
        assert_eq!(
            parse_command("/ask does this leak memory?"),
            Some(Command::Ask("does this leak memory?".into()))
        );
    }

    #[test]
    fn ask_captures_multiline_question() {
        let cmd = parse_command("/ask first line\nsecond line").unwrap();
        assert_eq!(cmd, Command::Ask("first line\nsecond line".into()));
    }

    #[test]
    fn ask_with_no_question_is_none() {
        assert_eq!(parse_command("/ask"), None);
        assert_eq!(parse_command("/ask    "), None);
    }

    #[test]
    fn non_commands_are_ignored() {
        assert_eq!(parse_command("please /review"), None);
        assert_eq!(parse_command("/reviews"), None);
        assert_eq!(parse_command("just a comment"), None);
        assert_eq!(parse_command(""), None);
    }

    #[test]
    fn leading_and_trailing_whitespace_ok() {
        assert_eq!(parse_command("  /review  \n"), Some(Command::Review));
    }

    #[test]
    fn merge_into_empty_body() {
        let out = merge_description("", "generated text");
        assert_eq!(out, format!("{DESC_START}\ngenerated text\n{DESC_END}"));
    }

    #[test]
    fn merge_prepends_to_human_body() {
        let out = merge_description("Human notes here.", "gen");
        assert!(out.starts_with(DESC_START));
        assert!(out.ends_with("Human notes here."));
        assert!(out.contains("gen"));
    }

    #[test]
    fn merge_replaces_prior_generated_section() {
        let first = merge_description("Keep me.", "old desc");
        // Human edits above and below the block are preserved on re-run.
        let edited = format!("PREFIX\n{first}\nSUFFIX");
        let again = merge_description(&edited, "new desc");
        assert!(again.contains("new desc"));
        assert!(!again.contains("old desc"));
        assert!(again.starts_with("PREFIX"));
        assert!(again.ends_with("SUFFIX"));
        assert!(again.contains("Keep me."));
    }
}
