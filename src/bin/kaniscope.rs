//! `kaniscope` — the review engine as one command, so a bot does not have to be
//! written in Rust to use it.
//!
//! The library's entry point already takes five scalars and returns one
//! `Serialize` struct that [`RunReviewOutput`] itself documents as "the HTTP/CLI
//! response", and every knob is read from the environment by [`Config::from_env`].
//! That is a wire contract in everything but name. This binary makes it one: set
//! env, pass five flags, read one JSON object off stdout. A bot in Python or
//! TypeScript spawns it (see `packaging/`) and needs no FFI, no bindings, and no
//! ABI-pinned prebuilds — the same way `uv` and `ruff` reach ecosystems they are
//! not written in.
//!
//! ```text
//! kaniscope --provider github --repo me/app --pr 12 --dry-run
//! kaniscope --provider github --repo me/app --pr 12 --json
//!
//! # --local: review uncommitted or unpushed work, before it is a PR. Same
//! # reviewer, same prompts, same anchoring — it just has nowhere to post.
//! kaniscope --local --base main
//! git diff --staged | kaniscope --local
//!
//! # --intent says what the change is MEANT to do, so the reviewer can check the
//! # diff against it — the local stand-in for a PR description.
//! kaniscope --local --base main --intent "Retry 5xx with backoff; leave 4xx alone."
//! kaniscope --local --base main --intent-file task.md
//!
//! # The JSON Schema of the output, for generating typed clients.
//! kaniscope --schema
//! ```
//!
//! Identity stays injected, exactly as it is for a library consumer: `BOT_NAME`,
//! `EXTRA_PROMPT` and the rest are read from the environment, so a bot spawning
//! this brands its own reviews. The binary has a name; the reviews do not.

use std::path::PathBuf;

use clap::Parser;

use pr_review_core::backend::OpenRouterBackend;
use pr_review_core::config::Config;
use pr_review_core::review::{
    run_review, run_review_local, LocalReviewInput, RunReviewInput, RunReviewOutput,
};

#[derive(Parser)]
#[command(
    name = "kaniscope",
    version = pr_review_core::VERSION,
    about = "Review a pull request (or a local diff) with an AI reviewer and optionally post the comments"
)]
struct Args {
    /// Review a local diff instead of a PR: no provider, no fetch, nothing posted.
    /// The diff comes from `--base`, or from stdin when `--base` is absent.
    #[arg(long, default_value_t = false)]
    local: bool,
    /// With --local: diff the working tree against this ref (e.g. `main`,
    /// `origin/main`, `HEAD`). Omit to read a diff from stdin instead.
    #[arg(long)]
    base: Option<String>,
    /// With --local: the checkout to read new-side files from for structural
    /// context. Defaults to the current directory.
    #[arg(long = "repo-root")]
    repo_root: Option<PathBuf>,
    /// With --local: what to call this change in the output. Defaults to the
    /// current branch name.
    #[arg(long)]
    label: Option<String>,
    /// With --local: what this change is MEANT to do — the task you set out to
    /// do, or the instruction you gave a coding agent. The reviewer checks the
    /// diff against it, the way it checks a PR against its description.
    ///
    /// Treated as untrusted data: it is fenced and labelled before it reaches the
    /// model, states what the change should do, and cannot direct the review.
    #[arg(long, conflicts_with = "intent_file")]
    intent: Option<String>,
    /// With --local: read --intent from this file.
    ///
    /// A task statement worth writing is usually longer than a comfortable shell
    /// argument and often already exists as a file — an issue body, a plan, an
    /// agent's task description. Mutually exclusive with `--intent`: two sources
    /// for one input is a silent-precedence bug waiting to happen.
    ///
    /// No `-` for stdin, deliberately: stdin is already how `--local` receives a
    /// diff, and one pipe cannot carry both.
    #[arg(long = "intent-file", value_name = "PATH", conflicts_with = "intent")]
    intent_file: Option<PathBuf>,
    /// github | gitlab | bitbucket (PR mode)
    #[arg(long)]
    provider: Option<String>,
    /// owner/repo (GitHub, GitLab) or workspace/repo (Bitbucket) (PR mode)
    #[arg(long)]
    repo: Option<String>,
    /// PR / MR number (PR mode)
    #[arg(long)]
    pr: Option<u64>,
    /// Generate the review but do not post it
    #[arg(long = "dry-run", default_value_t = false)]
    dry_run: bool,
    /// Emit the whole `RunReviewOutput` as one JSON object on stdout instead of
    /// the human-readable block, so a caller can consume it.
    ///
    /// This is the flag the wrapper packages use. Everything diagnostic goes to
    /// stderr, so stdout carries exactly one JSON document and nothing else — a
    /// caller can `json.loads(stdout)` without filtering.
    #[arg(long = "json", default_value_t = false)]
    json: bool,
    /// Also write the whole `RunReviewOutput` as JSON to this path. The
    /// human-readable block still prints; parent directories are created.
    ///
    /// `--json` moves the review *into* stdout, which makes it unreadable at the
    /// moment it is produced. A git hook wants both halves: findings a person
    /// reads while pushing, and a file a later run can pick up instead of
    /// re-deriving the same findings.
    #[arg(long = "json-out", value_name = "PATH")]
    json_out: Option<PathBuf>,
    /// Print the engine's full environment-variable surface as a markdown table
    /// and exit. Needs no key, no token and no network.
    ///
    /// The README's table is generated from this, and CI fails if the committed
    /// copy drifts — the previous hand-written one documented 41 of 61 names.
    #[arg(long = "config-docs", default_value_t = false)]
    config_docs: bool,
    /// Print the environment-variable surface as JSON and exit — the source the
    /// npm and PyPI clients generate their typed `config` options from.
    ///
    /// Separate from `--config-docs`, which renders the same spec as a markdown
    /// table for humans. One spec, two renderings, so the table a reader sees and
    /// the types a client ships cannot disagree.
    #[arg(long = "config-json", default_value_t = false)]
    config_json: bool,
    /// Print the JSON Schema of the `--json` output and exit. Needs no key, no
    /// token and no network.
    ///
    /// This is what keeps a non-Rust client honest. `Finding`'s shape is already
    /// declared in several places, and a field added on one side and missed on
    /// another ships the feature dark with everything still green. Generating the
    /// wrapper packages' types from this schema makes that a build error in the
    /// wrapper instead of a silently absent field at runtime.
    #[arg(long, default_value_t = false)]
    schema: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();
    // Diagnostics on stderr, always — `--json` promises stdout is one JSON
    // document, and a tracing line on stdout would break every caller that parses
    // it rather than only the ones that look at it.
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();

    let args = Args::parse();

    // Before any config read or network call: a schema dump is a pure function of
    // the binary, and demanding a key for it would make the typed-client
    // generation step need production credentials.
    if args.config_json {
        println!("{}", pr_review_core::config_spec::as_json());
        return Ok(());
    }

    if args.config_docs {
        print!("{}", pr_review_core::config_spec::markdown_table());
        return Ok(());
    }

    if args.schema {
        println!(
            "{}",
            serde_json::to_string_pretty(&schemars::schema_for!(RunReviewOutput))?
        );
        return Ok(());
    }

    let cfg = Config::from_env();

    // What this run will do, in one line, before the first network call. "Did it
    // write to the PR?" is the question a reader most needs when a run ends badly,
    // and it has no answer in a silent log.
    tracing::info!(
        "kaniscope {}: mode={} {}",
        pr_review_core::VERSION,
        if args.local { "local" } else { "pr" },
        // Derived from `local` first, not from `dry_run` alone: `--local` has no
        // host to post to whatever `--dry-run` says, and reporting LIVE for a mode
        // that cannot post defeats the one line whose job is to say otherwise.
        if args.local {
            "(local review — nowhere to post)"
        } else if args.dry_run {
            "dry-run (nothing will be posted)"
        } else {
            "LIVE (will post to the PR)"
        }
    );

    let out = if args.local {
        // Silently ignoring flags the user typed is worse than refusing them: the
        // two modes take different inputs, and a `--local --pr 12` that quietly
        // reviewed the working tree would look like it had reviewed PR 12.
        if args.provider.is_some() || args.repo.is_some() || args.pr.is_some() {
            anyhow::bail!(
                "--local reviews a diff, not a PR — drop --provider/--repo/--pr \
                 (or drop --local to review the PR)"
            );
        }
        run_local(&cfg, &args).await?
    } else {
        // Refused rather than ignored, for the same reason as the check above. A
        // pull request states its intent in its description, which the reviewer
        // already reads; accepting `--intent` here would look like it worked and
        // change nothing about the review.
        if args.intent.is_some() || args.intent_file.is_some() {
            anyhow::bail!(
                "--intent/--intent-file describe a local change — a PR's intent is \
                 its description, which the reviewer already reads (PR_BODY)"
            );
        }
        let (provider, repo, pr) = match (&args.provider, &args.repo, args.pr) {
            (Some(p), Some(r), Some(n)) => (p.clone(), r.clone(), n),
            _ => anyhow::bail!(
                "PR mode needs --provider, --repo and --pr; \
                 for a diff with no PR use --local (see --help)"
            ),
        };
        run_review(
            &cfg,
            RunReviewInput {
                provider,
                repo,
                pr,
                dry_run: args.dry_run,
                placeholder: false,
            },
        )
        .await?
    };

    if args.json {
        // Everything the caller needs, and nothing else on stdout.
        println!("{}", serde_json::to_string(&out)?);
        if let Some(path) = &args.json_out {
            write_json_out(path, &out)?;
        }
        return Ok(());
    }

    print_human(&args, &out);

    // Last on purpose. The review is the product; a full disk or a bad
    // `--json-out` path must not destroy output already generated. Printing first
    // means a failed write still leaves the findings on screen — and still exits
    // non-zero, so a caller never reads a missing file as a clean run that found
    // nothing.
    if let Some(path) = &args.json_out {
        write_json_out(path, &out)?;
    }
    Ok(())
}

/// The human-readable block: what would post, then the run's totals.
fn print_human(args: &Args, out: &RunReviewOutput) {
    let rule = "─".repeat(60);
    println!("\n{rule}\n{}\n{rule}", out.summary_markdown);

    // `summary_markdown` is the *comment body*, which on the posting path omits
    // the findings that were anchored as inline comments — so a dry run could
    // print 2 of 7 findings and silently drop the rest, which makes the CLI
    // useless for checking whether a rule actually fires on a given PR. Print
    // every finding here; the summary above stays as-is so what you see still
    // matches what would be posted.
    //
    // Dry-run and local only: on a posting run these were just anchored onto the
    // PR, so reprinting them duplicates data that already has a durable home.
    if (args.dry_run || args.local) && !out.findings_detail.is_empty() {
        println!(
            "\nAll {} finding(s), including inline-anchored:",
            out.findings
        );
        for f in &out.findings_detail {
            let loc = match f.line {
                Some(l) => format!("{}:{}", f.file, l),
                None => f.file.clone(),
            };
            let conf = f
                .confidence
                .map(|c| format!(" [confidence {c}]"))
                .unwrap_or_default();
            println!("\n  {} — {loc}{conf}\n  {}", f.severity, f.body);
        }
    }

    // The findings above are the model's prose. What actually lands on the PR is
    // the *rendered* comment, which can carry a committable suggestion block —
    // code applied to the branch on one click. Printing the prose alone would show
    // a dry run of everything except the part with consequences.
    if (args.dry_run || args.local) && !out.inline_detail.is_empty() {
        println!(
            "\nInline comment(s) as they would post ({}):",
            out.inline_detail.len()
        );
        for c in &out.inline_detail {
            println!("\n  ── {}:{}", c.path, c.line);
            for l in c.body.lines() {
                println!("  | {l}");
            }
        }
    }

    let tokens = out
        .usage
        .as_ref()
        .and_then(|u| u.total_tokens)
        .map(|t| format!(" · tokens: {t}"))
        .unwrap_or_default();
    println!(
        "\nmodel: {}{tokens}\nfindings: {} ({} inline-anchored)\nrecommendation: {}",
        out.model, out.findings, out.inline_posted, out.recommendation
    );
    println!(
        "posted: {}",
        if out.posted {
            out.comment_url.clone().unwrap_or_else(|| "yes".into())
        } else if args.local {
            // A local review has no host to post to — that is the mode, not a
            // withheld action, so it must not read as "dry-run".
            "no (local review — nowhere to post)".into()
        } else {
            "no (dry-run)".into()
        }
    );
}

/// Review a local diff: no provider, no PR, nothing posted.
///
/// The diff comes from `--base` (a `git diff <ref>` this runs itself) or from
/// stdin. stdin is the more general door — it takes a diff from any source at all:
/// `git diff --staged`, a saved patch, or a caller that generated one itself.
async fn run_local(cfg: &Config, args: &Args) -> anyhow::Result<RunReviewOutput> {
    let root = args.repo_root.clone().unwrap_or_else(|| PathBuf::from("."));

    let diff = match &args.base {
        Some(base) => {
            // A leading `-` makes git read this as an OPTION rather than a ref
            // (`--upload-pack=...` and friends), so refuse it. Not shell injection
            // — `Command` takes an argv and spawns no shell — but argument
            // injection is real once the base comes from a hook, from CI, or from
            // a wrapper package passing a caller's string through.
            if base.starts_with('-') {
                anyhow::bail!("--base must be a ref, not an option (got {base:?})");
            }
            let out = std::process::Command::new("git")
                // `--` ends the ref list, so a ref that also names a file cannot
                // be re-read as a pathspec.
                .args(["diff", base, "--"])
                .current_dir(&root)
                .output()
                .map_err(|e| anyhow::anyhow!("could not run git: {e}"))?;
            if !out.status.success() {
                anyhow::bail!(
                    "git diff {base} failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
            String::from_utf8_lossy(&out.stdout).into_owned()
        }
        None => {
            use std::io::Read;
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf)?;
            buf
        }
    };
    if diff.trim().is_empty() {
        anyhow::bail!(
            "empty diff — nothing to review. Pass --base <ref>, or pipe one in \
             (`git diff --staged | kaniscope --local`)."
        );
    }

    // Default the label to the branch, since that is what the reader is looking
    // at. Purely descriptive: it names the change in the prompt and the output.
    let label = args.label.clone().unwrap_or_else(|| {
        std::process::Command::new("git")
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .current_dir(&root)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| usable_branch(&String::from_utf8_lossy(&o.stdout)))
            .unwrap_or_else(|| "local changes".to_string())
    });

    run_review_local(
        cfg,
        LocalReviewInput {
            diff,
            repo_root: Some(root),
            label,
            change_intent: change_intent(args)?,
        },
        &OpenRouterBackend,
    )
    .await
}

/// The caller's stated intent for this change: `--intent` verbatim, `--intent-file`
/// read from disk, or `None`.
///
/// Clap enforces that at most one is set (`conflicts_with`), so this does not
/// re-check it; what it does own is the failure mode of the file. An unreadable
/// `--intent-file` is an ERROR rather than a silent `None`: the intent shapes what
/// the review checks the diff against, so a typo'd path that degraded to "no
/// intent" would return a review that looks complete and quietly answered a
/// different question. The one case where losing it is invisible is the one case
/// worth being loud about.
///
/// Blank content is not an error — an empty file is a legible way to say "no
/// intent", and `change_intent_block` drops whitespace-only text anyway.
fn change_intent(args: &Args) -> anyhow::Result<Option<String>> {
    use anyhow::Context;

    if let Some(text) = &args.intent {
        return Ok(Some(text.clone()));
    }
    match &args.intent_file {
        Some(path) => {
            Ok(Some(std::fs::read_to_string(path).with_context(|| {
                format!("reading --intent-file {}", path.display())
            })?))
        }
        None => Ok(None),
    }
}

/// A branch name from `git rev-parse --abbrev-ref HEAD`, or `None` when there
/// isn't a usable one.
///
/// Detached HEAD prints the literal `HEAD` and **exits 0**, so an exit-status
/// check passes it straight through and the review gets labelled "HEAD" — which
/// names nothing. A rebase, a bisect, and a CI checkout of a commit are all
/// detached, so this is the normal case in exactly the places a label matters.
fn usable_branch(raw: &str) -> Option<String> {
    let name = raw.trim();
    if name.is_empty() || name == "HEAD" {
        return None;
    }
    Some(name.to_string())
}

/// Write the review as one JSON object to `path`, atomically.
///
/// Atomically because this file is a handoff between processes: a git hook writes
/// it during a push while a later run reads it. A reader that catches a partial
/// write sees invalid JSON and has no way to tell that from a review that
/// genuinely produced nothing. Writing beside the target and renaming makes the
/// file appear complete or not at all.
///
/// Atomic *visibility*, deliberately not durability: there is no fsync before the
/// rename. A machine that dies mid-run has no review to preserve — the next run
/// regenerates the file. Paying two syncs per review to protect a regenerable
/// cache would be the wrong trade.
fn write_json_out<T: serde::Serialize>(path: &std::path::Path, value: &T) -> anyhow::Result<()> {
    use anyhow::Context;

    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
    }
    let tmp = tmp_sibling(path, &run_token());
    let write = (|| -> anyhow::Result<()> {
        // `create_new`, not `create`: the scratch name is already unique per run,
        // so an existing one means an assumption is wrong, and truncating someone
        // else's file is exactly the corruption this function exists to prevent.
        let mut f = std::fs::File::create_new(&tmp)
            .with_context(|| format!("creating {}", tmp.display()))?;
        std::io::Write::write_all(&mut f, serde_json::to_string(value)?.as_bytes())
            .with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path)
            .with_context(|| format!("renaming {} onto {}", tmp.display(), path.display()))
    })();
    if write.is_err() {
        // Do not leave scratch litter beside the target on the failure path.
        let _ = std::fs::remove_file(&tmp);
    }
    write
}

/// The scratch path [`write_json_out`] renames from — the target's name, a token
/// unique to this run, and `.tmp`, in the target's own directory.
///
/// Same directory because `rename` is only atomic within one filesystem, and the
/// system temp dir is routinely a different one. Appending rather than replacing
/// the extension keeps `a.json` and `a.json.bak` from colliding on one scratch
/// file. The token is what makes the atomicity claim true rather than nearly
/// true: a fixed `.tmp` name is shared state, so two runs writing the same target
/// would take turns truncating one scratch file and one could publish a file the
/// other is still writing.
fn tmp_sibling(path: &std::path::Path, unique: &str) -> std::path::PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    path.with_file_name(format!("{name}.{unique}.tmp"))
}

/// A token distinct across concurrent runs: pid, plus the clock in nanoseconds.
///
/// The pid alone is not enough — pids are reused, and a crash between create and
/// rename would then leave a scratch file that a later run with the same pid
/// cannot create, failing every write from then on. The clock breaks that tie
/// without needing a cleanup pass.
fn run_token() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{}.{nanos}", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::{change_intent, tmp_sibling, usable_branch, Args};
    use clap::Parser;
    use std::path::Path;

    #[test]
    fn a_real_branch_name_is_used() {
        assert_eq!(
            usable_branch("feat/binary-distribution\n").as_deref(),
            Some("feat/binary-distribution")
        );
    }

    /// Verified against git: in detached HEAD, `rev-parse --abbrev-ref HEAD`
    /// prints `HEAD` and exits 0. Labelling a review "HEAD" names nothing, and
    /// rebase / bisect / a CI commit checkout are all detached.
    #[test]
    fn detached_head_is_not_a_branch_name() {
        assert_eq!(usable_branch("HEAD\n"), None);
        assert_eq!(usable_branch(""), None);
        assert_eq!(usable_branch("  \n "), None);
    }

    /// The scratch file must be a sibling: `rename` is only atomic within one
    /// filesystem, and it must not eat the target's extension, or `a.json` and
    /// `a.json.bak` would share one scratch path.
    #[test]
    fn scratch_file_sits_beside_its_target() {
        assert_eq!(
            tmp_sibling(Path::new("/repo/.git/prbot/feat-x.json"), "77.12"),
            Path::new("/repo/.git/prbot/feat-x.json.77.12.tmp")
        );
        assert_eq!(
            tmp_sibling(Path::new("review.json.bak"), "77.12"),
            Path::new("review.json.bak.77.12.tmp")
        );
    }

    /// The schema is the contract the wrapper packages generate their types from,
    /// so an empty or property-less schema must fail here rather than silently
    /// produce a client with no fields.
    #[test]
    fn output_schema_names_the_fields_clients_read() {
        let schema = serde_json::to_value(schemars::schema_for!(
            pr_review_core::review::RunReviewOutput
        ))
        .expect("schema serializes");
        let props = schema
            .get("properties")
            .and_then(|p| p.as_object())
            .expect("schema has properties");
        for field in [
            "findingsDetail",
            "inlineDetail",
            "summaryMarkdown",
            "recommendation",
            "posted",
        ] {
            assert!(props.contains_key(field), "schema is missing {field}");
        }
    }

    /// Two sources for one input is a silent-precedence bug waiting to happen, so
    /// clap refuses the pair rather than letting one quietly win.
    #[test]
    fn intent_text_and_intent_file_are_mutually_exclusive() {
        let err = Args::try_parse_from([
            "kaniscope",
            "--local",
            "--base",
            "main",
            "--intent",
            "do the thing",
            "--intent-file",
            "task.md",
        ])
        .map(|_| ())
        .expect_err("two intent sources must be refused");
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    /// Either one alone is fine, and `--intent` takes prose containing anything —
    /// it is a sentence a developer types, not a token.
    #[test]
    fn either_intent_source_alone_parses() {
        let text = Args::try_parse_from([
            "kaniscope",
            "--local",
            "--base",
            "main",
            "--intent",
            "Retry 5xx with backoff; leave 4xx alone.",
        ])
        .expect("--intent alone is valid");
        assert_eq!(
            text.intent.as_deref(),
            Some("Retry 5xx with backoff; leave 4xx alone.")
        );
        assert!(text.intent_file.is_none());

        let file = Args::try_parse_from(["kaniscope", "--local", "--intent-file", "task.md"])
            .expect("--intent-file alone is valid");
        assert_eq!(file.intent_file.as_deref(), Some(Path::new("task.md")));
        assert!(file.intent.is_none());
    }

    /// An unreadable `--intent-file` fails the run. Degrading to "no intent" would
    /// return a review that looks complete and quietly checked the diff against
    /// nothing — the one way of losing this input that a caller cannot see.
    #[test]
    fn an_unreadable_intent_file_is_an_error_not_a_silent_absence() {
        let args = Args::try_parse_from([
            "kaniscope",
            "--local",
            "--intent-file",
            "/nonexistent/task.md",
        ])
        .expect("parses");
        let err = change_intent(&args).expect_err("a missing intent file must fail the run");
        assert!(
            err.to_string().contains("/nonexistent/task.md"),
            "the error should name the path: {err}"
        );
    }

    /// An empty file is a legible way to say "no intent" — it is read, and the
    /// blank text is dropped downstream rather than erroring here.
    #[test]
    fn an_empty_intent_file_is_read_not_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("task.md");
        std::fs::write(&path, "   \n").expect("write");

        let args = Args::try_parse_from([
            "kaniscope",
            "--local",
            "--intent-file",
            path.to_str().unwrap(),
        ])
        .expect("parses");
        assert_eq!(
            change_intent(&args).expect("reads"),
            Some("   \n".to_string())
        );
    }

    /// No intent given, nothing invented.
    #[test]
    fn no_intent_flag_means_no_intent() {
        let args =
            Args::try_parse_from(["kaniscope", "--local", "--base", "main"]).expect("parses");
        assert_eq!(change_intent(&args).expect("no error"), None);
    }

    /// The npm and PyPI clients build these argv strings, and a bot in production
    /// runs one of them. New flags must not change what the existing ones mean.
    #[test]
    fn the_invocations_the_wrapper_clients_build_still_parse() {
        let pr = Args::try_parse_from([
            "kaniscope",
            "--json",
            "--provider",
            "github",
            "--repo",
            "o/r",
            "--pr",
            "12",
            "--dry-run",
        ])
        .expect("the PR invocation still parses");
        assert_eq!(pr.provider.as_deref(), Some("github"));
        assert_eq!(pr.pr, Some(12));
        assert!(pr.dry_run && pr.json && !pr.local);
        assert!(pr.intent.is_none() && pr.intent_file.is_none());

        let local = Args::try_parse_from([
            "kaniscope",
            "--json",
            "--local",
            "--base",
            "main",
            "--repo-root",
            "/w",
            "--label",
            "feat/x",
            "--json-out",
            "out.json",
        ])
        .expect("the local invocation still parses");
        assert!(local.local);
        assert_eq!(local.base.as_deref(), Some("main"));
        assert_eq!(local.label.as_deref(), Some("feat/x"));
        assert_eq!(local.json_out.as_deref(), Some(Path::new("out.json")));

        // The no-argument doors both clients also use.
        for flag in ["--schema", "--config-json", "--config-docs", "--local"] {
            Args::try_parse_from(["kaniscope", flag])
                .unwrap_or_else(|e| panic!("{flag} must still parse: {e}"));
        }
    }
}
