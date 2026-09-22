//! PR comment commands (T3.9): `/review`, `/ask <question>`, and `/describe`.
//!
//! A single entry point — [`run_command`] — lets a bot binary route any
//! recognized comment command through the core without wiring each one itself.
//! [`parse_command`] turns a raw comment body into a [`Command`]; the caller is
//! responsible for the gating that's cheap to do provider-side (the event is a
//! newly-created comment on a PR).

use anyhow::Result;

use crate::backend::{OpenRouterBackend, ReviewBackend};
use crate::config::Config;
use crate::providers::{PrMeta, Provider};
use crate::review::{load_repo_config, run_review_with, RunReviewInput};

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
    /// `/review-file <path>` — deep-review an entire file at the PR head.
    ReviewFile(String),
}

/// What a command run did, for the caller to log.
#[derive(Debug, Clone)]
pub struct CommandOutcome {
    /// `"review"`, `"ask"`, `"describe"`, or `"review-file"`.
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
        "/review-file" => {
            let path = rest.trim();
            (!path.is_empty()).then(|| Command::ReviewFile(path.to_string()))
        }
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
    run_command_with(cfg, provider_name, repo, pr, cmd, &OpenRouterBackend).await
}

/// Like [`run_command`] but with a caller-supplied [`ReviewBackend`], so `/review`,
/// `/ask`, and `/describe` all run on the same backend (e.g. an agent CLI) instead
/// of always OpenRouter.
///
/// # Errors
/// On unknown provider, or any provider/backend failure.
pub async fn run_command_with(
    cfg: &Config,
    provider_name: &str,
    repo: &str,
    pr: u64,
    cmd: Command,
    backend: &dyn ReviewBackend,
) -> Result<CommandOutcome> {
    match cmd {
        Command::Review => {
            let out = run_review_with(
                cfg,
                RunReviewInput {
                    provider: provider_name.to_string(),
                    repo: repo.to_string(),
                    pr,
                    dry_run: false,
                    placeholder: true,
                },
                backend,
            )
            .await?;
            Ok(CommandOutcome {
                command: "review",
                comment_url: out.comment_url,
            })
        }
        Command::Ask(question) => run_ask(cfg, backend, provider_name, repo, pr, &question).await,
        Command::Describe => run_describe(cfg, backend, provider_name, repo, pr).await,
        Command::ReviewFile(path) => {
            run_review_file(cfg, backend, provider_name, repo, pr, &path).await
        }
    }
}

/// `/review-file <path>`: deep-review an entire file at the PR head and post the
/// findings as a summary comment. Findings can anchor to any line in the file, so
/// they're reported as text (a PR only accepts inline comments on diff lines).
///
/// The reviewing is [`crate::filereview::review_pr_file`]; this is the posting
/// adapter around it. Split that way because everything before the post — the
/// glob authorization, the head fetch, the confidence/sort/cap policy, the
/// rendering — is wanted by callers that have no PR to write to, and a second
/// implementation of the authorization step is the one copy nobody can afford.
async fn run_review_file(
    cfg: &Config,
    backend: &dyn ReviewBackend,
    provider_name: &str,
    repo: &str,
    pr: u64,
    path: &str,
) -> Result<CommandOutcome> {
    let (out, ctx) =
        crate::filereview::review_pr_file(cfg, backend, provider_name, repo, pr, path).await?;

    // Posted for every outcome, refusals included: the command was issued in a
    // comment thread, and silence there reads as the bot being broken rather than
    // as a considered refusal.
    //
    // Reusing the provider, client and merged config the review already resolved
    // — a fresh `reqwest::Client` here would open a second connection pool and
    // repeat the TLS handshake to post one comment.
    let url = ctx
        .provider
        .post_comment(&ctx.client, &ctx.cfg, repo, pr, &out.summary_markdown)
        .await?;
    Ok(CommandOutcome {
        command: "review-file",
        comment_url: url,
    })
}

/// What to say when the filters left nothing to reason over.
///
/// "There are no reviewable source changes" is true of the diff and false of the
/// pull request. On a lockfile-only PR every file is removed by `EXCLUDE_GLOBS`,
/// so `/ask "was the lockfile updated?"` answered that nothing changed — about a
/// change that consisted entirely of the file being asked about. That is the
/// same false statement this branch exists to stop, arriving in the reply a
/// human reads directly rather than in a prompt.
///
/// Naming the withheld files turns it into an answer: nothing was *reviewed*,
/// and here is what was set aside.
fn nothing_to_review_body(omitted_note: Option<&str>) -> String {
    match omitted_note {
        Some(note) => format!(
            "There are no reviewable source changes in this PR — every changed file was \
             withheld from the diff.\n\n{note}"
        ),
        None => "There are no reviewable source changes in this PR.".to_string(),
    }
}

/// What `/ask` and `/describe` reason over.
///
/// `omitted_note` exists because the parity this function claims was not real:
/// both dropped lists were discarded here (`let (diff, _dropped) = ...`), so a
/// command could be asked "was the lockfile updated?" about a diff the lockfile
/// had been filtered out of, and answer from the gap. The review path was given
/// the note in the previous release; these two were left behind.
struct CommandDiff {
    diff: String,
    structural: String,
    /// Names the files withheld from `diff`, and why. `None` when nothing was.
    omitted_note: Option<String>,
}

/// Fetch the PR diff and prepare it exactly as the review path does — glob
/// filter, size packing, omission note, and (optionally) structural context — so
/// `/ask` and `/describe` reason over the same trimmed, budgeted diff the
/// reviewer sees, and are told the same things about what is missing from it.
async fn prepared_diff(
    provider: &Provider,
    client: &reqwest::Client,
    cfg: &Config,
    repo: &str,
    meta: &PrMeta,
) -> Result<CommandDiff> {
    let raw = provider.get_diff(client, cfg, repo, meta.pr).await?;
    let (diff, glob_dropped) =
        crate::diff::filter_diff_by_globs(&raw, &cfg.include_globs, &cfg.exclude_globs);
    let (diff, packed_dropped) = crate::diff::pack_diff(&diff, cfg.max_diff_chars);
    let omitted_note = crate::review::omission_note(&glob_dropped, &packed_dropped);
    let structural = if cfg.structural_context && !diff.trim().is_empty() {
        crate::structure::structural_context(provider, client, cfg, repo, meta, &diff).await
    } else {
        String::new()
    };
    Ok(CommandDiff {
        diff,
        structural,
        omitted_note,
    })
}

/// `/ask`: answer a question about the PR and post it as a reply comment.
async fn run_ask(
    cfg: &Config,
    backend: &dyn ReviewBackend,
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

    let CommandDiff {
        diff,
        structural,
        omitted_note,
    } = prepared_diff(&provider, &client, cfg, repo, &meta).await?;
    if diff.trim().is_empty() {
        let body = format!(
            "> **/ask** {question}\n\n{}",
            nothing_to_review_body(omitted_note.as_deref())
        );
        let url = provider.post_comment(&client, cfg, repo, pr, &body).await?;
        return Ok(CommandOutcome {
            command: "ask",
            comment_url: url,
        });
    }

    let structural_opt = (!structural.is_empty()).then_some(structural.as_str());
    let answer = crate::llm::answer_question(
        cfg,
        backend,
        &meta,
        &diff,
        question,
        omitted_note.as_deref(),
        structural_opt,
    )
    .await?;
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
    backend: &dyn ReviewBackend,
    provider_name: &str,
    repo: &str,
    pr: u64,
) -> Result<CommandOutcome> {
    let provider = Provider::from_name(provider_name)?;
    let client = reqwest::Client::new();
    let meta = provider.get_meta(&client, cfg, repo, pr).await?;
    let effective = load_repo_config(&provider, &client, cfg, repo, &meta).await;
    let cfg = &effective;

    let CommandDiff {
        diff,
        structural,
        omitted_note,
    } = prepared_diff(&provider, &client, cfg, repo, &meta).await?;
    if diff.trim().is_empty() {
        let url = provider
            .post_comment(
                &client,
                cfg,
                repo,
                pr,
                &nothing_to_review_body(omitted_note.as_deref()),
            )
            .await?;
        return Ok(CommandOutcome {
            command: "describe",
            comment_url: url,
        });
    }

    let structural_opt = (!structural.is_empty()).then_some(structural.as_str());
    let generated = crate::llm::describe_pr(
        cfg,
        backend,
        &meta,
        &diff,
        omitted_note.as_deref(),
        structural_opt,
    )
    .await?;
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
    fn parses_the_core_commands() {
        assert_eq!(parse_command("/review"), Some(Command::Review));
        assert_eq!(parse_command("/describe"), Some(Command::Describe));
        assert_eq!(
            parse_command("/ask does this leak memory?"),
            Some(Command::Ask("does this leak memory?".into()))
        );
    }

    #[test]
    fn parses_review_file_with_path() {
        assert_eq!(
            parse_command("/review-file src/auth.rs"),
            Some(Command::ReviewFile("src/auth.rs".into()))
        );
        // Extra surrounding whitespace is trimmed off the path.
        assert_eq!(
            parse_command("  /review-file   src/lib.rs  "),
            Some(Command::ReviewFile("src/lib.rs".into()))
        );
    }

    #[test]
    fn review_file_without_path_is_none() {
        assert_eq!(parse_command("/review-file"), None);
        assert_eq!(parse_command("/review-file    "), None);
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

#[cfg(test)]
mod command_omission_tests {
    use super::*;

    /// `/ask` and `/describe` must be told what was withheld from their diff.
    ///
    /// They used to discard both dropped lists (`let (diff, _dropped) = ...`)
    /// and pass `None` for `omitted_note`, while claiming in a doc comment to
    /// prepare the diff "exactly as the review path does". So `/ask "was the
    /// lockfile updated?"` reasoned over a diff the lockfile had been filtered
    /// out of, with nothing saying so — the same gap that made the reviewer
    /// report a committed lockfile as missing, on the two commands whose whole
    /// job is answering questions about the change.
    #[test]
    fn the_note_reaches_the_ask_and_describe_prompts() {
        let meta = PrMeta {
            repo: "o/r".to_string(),
            pr: 1,
            title: None,
            base_branch: None,
            head_sha: None,
            body: None,
            ci_status: None,
        };
        let note = "1 file(s) are excluded from the diff by this repository's \
                    configuration and are NOT shown: Cargo.lock.";

        let with = crate::prompt::build_user_prompt(
            &meta,
            "diff --git a/src/lib.rs b/src/lib.rs\n",
            false,
            Some(note),
            None,
            crate::prompt::UntrustedContext::default(),
        );
        assert!(
            with.contains("Cargo.lock"),
            "the prompt must name the withheld file: {with}"
        );

        let without = crate::prompt::build_user_prompt(
            &meta,
            "diff --git a/src/lib.rs b/src/lib.rs\n",
            false,
            None,
            None,
            crate::prompt::UntrustedContext::default(),
        );
        assert!(
            !without.contains("Cargo.lock"),
            "and must not invent one when nothing was withheld: {without}"
        );
    }

    /// A lockfile-only PR must not be told that nothing changed.
    ///
    /// Every file is removed by `EXCLUDE_GLOBS`, so the diff is empty and both
    /// commands took an early return reading "There are no reviewable source
    /// changes in this PR". Asked `/ask "was the lockfile updated?"` on a PR
    /// that is nothing but that lockfile, the honest answer is "it was withheld
    /// from review", not "nothing changed" — the same false statement this
    /// branch exists to stop, arriving in the reply a human reads directly.
    /// Caught by the pre-push review at 94 confidence.
    #[test]
    fn an_all_filtered_pr_says_what_was_withheld_not_that_nothing_changed() {
        let note = crate::review::omission_note(&["Cargo.lock".to_string()], &[]).unwrap();
        let body = nothing_to_review_body(Some(&note));

        assert!(
            body.contains("Cargo.lock"),
            "must name what was set aside: {body}"
        );
        assert!(
            body.contains("withheld"),
            "must say the files were withheld, not absent: {body}"
        );

        // A genuinely empty change keeps the plain sentence — claiming files
        // were withheld when none were is the mirror-image lie.
        let empty = nothing_to_review_body(None);
        assert!(!empty.contains("withheld"), "{empty}");
        assert!(empty.contains("no reviewable source changes"), "{empty}");
    }

    /// The note is built from BOTH drop routes, exactly as the review path
    /// builds it — the two must not drift apart again.
    #[test]
    fn the_command_note_is_the_same_function_the_review_path_uses() {
        let globbed = vec!["Cargo.lock".to_string()];
        let packed = vec!["big/generated.rs".to_string()];

        let note =
            crate::review::omission_note(&globbed, &packed).expect("both routes dropped something");
        assert!(note.contains("configuration") && note.contains("Cargo.lock"));
        assert!(note.contains("size limit") && note.contains("big/generated.rs"));

        assert!(
            crate::review::omission_note(&[], &[]).is_none(),
            "nothing withheld, nothing said"
        );
    }
}
