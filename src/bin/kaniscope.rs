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

/// The toolbox surface: one explicit operation per subcommand.
///
/// Added alongside the original flat flags rather than replacing them. Those
/// flags are what the npm and PyPI clients build today and what runs in
/// consumers' CI, so breaking them to tidy up an argv would break deployments to
/// buy nothing. `kaniscope --local --base main` and `kaniscope review-local
/// --base main` are the same review; the second says which operation it is
/// without the reader having to know that `--local` selects a mode.
///
/// **Subcommands emit JSON on stdout by default**, because they exist to be
/// called by a program — the handoff's rule that every machine-facing operation
/// emits exactly one JSON document. `--human` opts back out. The flat flags keep
/// the opposite default, human unless `--json`, because that is what they have
/// always done.
#[derive(clap::Subcommand)]
enum Op {
    /// Review a local diff — a branch, a worktree, staged work. Nothing is posted.
    ReviewLocal(LocalArgs),
    /// Review a pull request. Posts nothing unless `--post` is given.
    ReviewPr(PrArgs),
    /// Deep-review one complete file, locally or at a PR head. Never posts.
    ReviewFile(FileArgs),
    /// Print the effective review rules. No model call, so no model key.
    GetRules(RulesArgs),
    /// List the findings currently on a pull request. Read-only.
    GetFindings(FindingsArgs),
    /// Package selected findings for a coding agent to investigate. Changes
    /// nothing: no edits, no posts, no thread resolution.
    ResolveFindings(ResolveArgs),
    /// Investigate one finding against a local checkout. Never posts.
    ExplainFinding(ExplainArgs),
    /// Serve these operations as MCP tools over stdio.
    ///
    /// Configure it per project, never user-globally, by pointing an `.mcp.json`
    /// at `kaniscope mcp`. The tools are read-only — this surface cannot post,
    /// edit or resolve anything, whatever the CLI can do.
    Mcp,
    /// Print the JSON Schema of an operation's output.
    Schema(SchemaArgs),
}

#[derive(clap::Args)]
struct FindingsArgs {
    #[arg(long)]
    provider: String,
    #[arg(long)]
    repo: String,
    #[arg(long)]
    pr: u64,
}

#[derive(clap::Args)]
struct ResolveArgs {
    #[arg(long)]
    provider: String,
    #[arg(long)]
    repo: String,
    #[arg(long)]
    pr: u64,
    /// Fingerprints to hand over, repeatable. Omit to take every ACTIVE finding,
    /// which is what "work through this PR's findings" means and saves a caller
    /// round-tripping the list it just received.
    #[arg(long = "fingerprint", value_name = "FP")]
    fingerprints: Vec<String>,
}

#[derive(clap::Args)]
struct ExplainArgs {
    /// The finding as JSON — `{"file":...,"line":...,"body":...}`. Read from a
    /// file with `@path`, or from stdin with `@-`.
    ///
    /// JSON rather than a flag per field because the input IS a finding, and a
    /// caller almost always has one already: from a review's `findingsDetail`,
    /// or from `get-findings`. Retyping it into five flags invites transcription
    /// errors in the one field that matters, the body.
    #[arg(long)]
    finding: String,
    /// The checkout to read the file from. Defaults to `.`.
    #[arg(long = "repo-root")]
    repo_root: Option<PathBuf>,
    /// The commit the checkout is at, so a revision mismatch can be reported.
    #[arg(long = "head-sha")]
    head_sha: Option<String>,
}

/// Where a local review's diff comes from.
///
/// Named modes rather than one `--base`-or-stdin guess, because "committed and
/// uncommitted changes" is ambiguous in a way that loses work silently: a review
/// that omitted staged changes would look like a clean review of everything.
/// Each mode pins one git invocation, and the tests pin the invocations.
#[derive(clap::Args)]
struct LocalArgs {
    /// Everything that differs from this ref, through the working tree.
    /// `git diff <ref> --`.
    #[arg(long, conflicts_with_all = ["staged", "working_tree"])]
    base: Option<String>,
    /// Staged changes only. `git diff --cached --`.
    #[arg(long, conflicts_with_all = ["base", "working_tree"])]
    staged: bool,
    /// Unstaged working-tree changes only. `git diff --`.
    #[arg(long = "working-tree", conflicts_with_all = ["base", "staged"])]
    working_tree: bool,
    /// The checkout to read files and repository rules from. Defaults to `.`.
    #[arg(long = "repo-root")]
    repo_root: Option<PathBuf>,
    /// What to call this change in the output. Defaults to the branch name.
    #[arg(long)]
    label: Option<String>,
    /// What this change is meant to do, for the reviewer to check it against.
    #[arg(long, conflicts_with = "intent_file")]
    intent: Option<String>,
    /// Read `--intent` from a file.
    #[arg(long = "intent-file", value_name = "PATH", conflicts_with = "intent")]
    intent_file: Option<PathBuf>,
    /// Also write the JSON result to this path, atomically.
    #[arg(long = "json-out", value_name = "PATH")]
    json_out: Option<PathBuf>,
    /// Print the readable block instead of JSON.
    #[arg(long, default_value_t = false)]
    human: bool,
}

#[derive(clap::Args)]
struct PrArgs {
    #[arg(long)]
    provider: String,
    #[arg(long)]
    repo: String,
    #[arg(long)]
    pr: u64,
    /// Post the review to the pull request.
    ///
    /// Opt-in, which is the opposite of the flat `--dry-run` flag's default, and
    /// deliberately so. This surface is driven by coding agents, where the cost
    /// of a wrong default is asymmetric: not posting is a re-run, and posting is
    /// a comment on someone's pull request that cannot be un-sent.
    #[arg(long, default_value_t = false)]
    post: bool,
    /// Accepted and ignored — posting is already off unless `--post` is given.
    /// Kept so a caller porting from the flat flags cannot be surprised by it
    /// being rejected, or worse, by it being read as a request TO post.
    #[arg(long = "dry-run", default_value_t = false)]
    dry_run: bool,
    #[arg(long = "json-out", value_name = "PATH")]
    json_out: Option<PathBuf>,
    #[arg(long, default_value_t = false)]
    human: bool,
}

#[derive(clap::Args)]
struct FileArgs {
    /// Repository-relative path of the file to review.
    #[arg(long)]
    path: String,
    /// Review the file in this checkout. Mutually exclusive with `--provider`.
    #[arg(long = "repo-root", conflicts_with_all = ["provider", "repo", "pr"])]
    repo_root: Option<PathBuf>,
    // `requires_all` on `provider` alone only enforces one direction, so
    // `--repo o/r --pr 5` with no `--provider` parsed fine and then fell through
    // to the local branch — silently reviewing the checkout instead of the pull
    // request the caller named. Each field requires the other two, so an
    // incomplete scope is refused rather than quietly redirected.
    #[arg(long, requires_all = ["repo", "pr"])]
    provider: Option<String>,
    #[arg(long, requires_all = ["provider", "pr"])]
    repo: Option<String>,
    #[arg(long, requires_all = ["provider", "repo"])]
    pr: Option<u64>,
    #[arg(long, default_value_t = false)]
    human: bool,
}

#[derive(clap::Args)]
struct RulesArgs {
    /// Resolve the rules for this checkout. Mutually exclusive with `--provider`.
    #[arg(long = "repo-root", conflicts_with_all = ["provider", "repo", "pr"])]
    repo_root: Option<PathBuf>,
    // Each requires the other two — see the note on `FileArgs`.
    #[arg(long, requires_all = ["repo", "pr"])]
    provider: Option<String>,
    #[arg(long, requires_all = ["provider", "pr"])]
    repo: Option<String>,
    #[arg(long, requires_all = ["provider", "repo"])]
    pr: Option<u64>,
}

#[derive(clap::Args)]
struct SchemaArgs {
    /// Which operation's output to describe. Omit to list the names.
    #[arg(value_name = "OPERATION")]
    operation: Option<String>,
}

#[derive(Parser)]
#[command(
    name = "kaniscope",
    version = pr_review_core::VERSION,
    about = "Review a pull request (or a local diff) with an AI reviewer and optionally post the comments"
)]
struct Args {
    /// An explicit toolbox operation. When absent, the flat flags below select
    /// the mode exactly as they always have.
    #[command(subcommand)]
    op: Option<Op>,
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

    // An explicit operation short-circuits the flat-flag path entirely. The two
    // never interleave: a subcommand carries its own arguments, so there is no
    // case where a stray top-level flag silently changes what an operation does.
    if let Some(op) = args.op {
        return run_op(&cfg, op).await;
    }

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

/// Run one explicit operation and print exactly one JSON document on stdout.
///
/// Every arm ends in `emit` or `emit_human`, so the "one document, nothing else"
/// promise is kept in one place rather than by each arm remembering to.
async fn run_op(cfg: &Config, op: Op) -> anyhow::Result<()> {
    match op {
        Op::Schema(a) => {
            print!("{}", operation_schema(a.operation.as_deref())?);
            Ok(())
        }
        Op::GetRules(a) => {
            // No model call anywhere on this path, so no key is required — the
            // whole point of the operation. Say so if it turns out otherwise.
            let rules = match (&a.provider, &a.repo, a.pr) {
                (Some(p), Some(r), Some(n)) => pr_review_core::rules::remote(cfg, p, r, n).await?,
                _ => {
                    let root = a.repo_root.unwrap_or_else(|| PathBuf::from("."));
                    pr_review_core::rules::local(cfg, Some(&root))
                }
            };
            emit(&rules)
        }
        Op::ReviewFile(a) => {
            // Clap now requires provider/repo/pr together, so this really is a
            // two-way choice: all three, or none.
            let out = match (&a.provider, &a.repo, a.pr) {
                (Some(p), Some(r), Some(n)) => {
                    pr_review_core::filereview::review_pr_file(
                        cfg,
                        &OpenRouterBackend,
                        p,
                        r,
                        n,
                        &a.path,
                    )
                    .await?
                    .0
                }
                _ => {
                    let root = a.repo_root.unwrap_or_else(|| PathBuf::from("."));
                    pr_review_core::filereview::review_local(
                        cfg,
                        &OpenRouterBackend,
                        &root,
                        &a.path,
                    )
                    .await?
                }
            };
            if a.human {
                println!("{}", out.summary_markdown);
                return Ok(());
            }
            emit(&out)
        }
        Op::Mcp => pr_review_core::mcp::serve(cfg).await,
        Op::GetFindings(a) => {
            let out =
                pr_review_core::findings::get_findings(cfg, &a.provider, &a.repo, a.pr).await?;
            emit(&out)
        }
        Op::ResolveFindings(a) => {
            let out = pr_review_core::findings::resolve_findings(
                cfg,
                &a.provider,
                &a.repo,
                a.pr,
                &a.fingerprints,
            )
            .await?;
            emit(&out)
        }
        Op::ExplainFinding(a) => {
            let finding: pr_review_core::findings::ExplainInput =
                serde_json::from_str(&read_arg(&a.finding)?)
                    .map_err(|e| anyhow::anyhow!("--finding must be a finding JSON object: {e}"))?;
            let root = a.repo_root.unwrap_or_else(|| PathBuf::from("."));
            let out = pr_review_core::findings::explain_finding(
                cfg,
                &OpenRouterBackend,
                &root,
                finding,
                a.head_sha.as_deref(),
            )
            .await?;
            emit(&out)
        }
        Op::ReviewPr(a) => {
            let out = run_review(
                cfg,
                RunReviewInput {
                    provider: a.provider,
                    repo: a.repo,
                    pr: a.pr,
                    // Inverted from the flat flag on purpose: this surface posts
                    // only when asked. `--dry-run` is accepted for familiarity and
                    // changes nothing, because it would already be a dry run.
                    dry_run: !a.post,
                    placeholder: false,
                },
            )
            .await?;
            finish_review_output(&out, a.human, false, !a.post, a.json_out.as_deref())
        }
        Op::ReviewLocal(a) => {
            let root = a.repo_root.clone().unwrap_or_else(|| PathBuf::from("."));
            let diff = local_diff(&root, &a)?;
            let label = a.label.clone().unwrap_or_else(|| default_label(&root, &a));
            let out = run_review_local(
                cfg,
                LocalReviewInput {
                    diff,
                    repo_root: Some(root),
                    label,
                    change_intent: local_intent(&a)?,
                },
                &OpenRouterBackend,
            )
            .await?;
            // A local review has nowhere to post, so it is never a dry run.
            finish_review_output(&out, a.human, true, false, a.json_out.as_deref())
        }
    }
}

/// Print a review result, honouring `--human` and `--json-out`.
fn finish_review_output(
    out: &RunReviewOutput,
    human: bool,
    local: bool,
    dry_run: bool,
    json_out: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    if human {
        print_review(out, local, dry_run);
    } else {
        println!("{}", serde_json::to_string(out)?);
    }
    // Last, for the same reason as the flat path: a failed write must not destroy
    // output already produced, and must still exit non-zero.
    if let Some(path) = json_out {
        write_json_out(path, out)?;
    }
    Ok(())
}

/// A value given inline, or `@path` to read a file, or `@-` for stdin.
///
/// A finding body is multi-line prose containing quotes and backticks, which is
/// exactly the kind of argument a shell mangles. `@-` is safe here because no
/// operation taking this reads a diff from stdin, so the two cannot collide.
fn read_arg(value: &str) -> anyhow::Result<String> {
    use anyhow::Context;
    match value.strip_prefix('@') {
        None => Ok(value.to_string()),
        Some("-") => {
            use std::io::Read;
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf)?;
            Ok(buf)
        }
        Some(path) => std::fs::read_to_string(path).with_context(|| format!("reading {path}")),
    }
}

/// Serialize one value as the operation's single stdout document.
fn emit<T: serde::Serialize>(value: &T) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string(value)?);
    Ok(())
}

/// The diff for a `review-local` run, from exactly one named source.
///
/// Each mode is one pinned git invocation. `--base` keeps the flat flag's
/// meaning — everything that differs from the ref, through the working tree — so
/// migrating from `--local --base main` changes nothing about what is reviewed.
fn local_diff(root: &std::path::Path, a: &LocalArgs) -> anyhow::Result<String> {
    let diff = if let Some(base) = &a.base {
        git_diff(root, &["diff", base, "--"], Some(base))?
    } else if a.staged {
        git_diff(root, &["diff", "--cached", "--"], None)?
    } else if a.working_tree {
        git_diff(root, &["diff", "--"], None)?
    } else {
        use std::io::Read;
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf)?;
        buf
    };
    if diff.trim().is_empty() {
        anyhow::bail!(
            "empty diff — nothing to review. Pass --base <ref>, --staged or \
             --working-tree, or pipe a diff in."
        );
    }
    Ok(diff)
}

/// Run one `git diff` invocation in `root` and return its stdout.
///
/// `ref_arg`, when present, is refused if it starts with `-`: `Command` spawns no
/// shell, so this is not shell injection, but a ref that git reads as an option
/// (`--upload-pack=...`) is real once the value arrives from a hook, from CI, or
/// through a wrapper package.
fn git_diff(
    root: &std::path::Path,
    args: &[&str],
    ref_arg: Option<&str>,
) -> anyhow::Result<String> {
    if let Some(r) = ref_arg {
        if r.starts_with('-') {
            anyhow::bail!("--base must be a ref, not an option (got {r:?})");
        }
    }
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .map_err(|e| anyhow::anyhow!("could not run git: {e}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The default label for a `review-local` run: the branch, or the diff mode when
/// there is no usable branch name.
fn default_label(root: &std::path::Path, a: &LocalArgs) -> String {
    std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(root)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| usable_branch(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or_else(|| {
            // Naming the mode beats "local changes" for a detached HEAD, which is
            // the normal state during a rebase, a bisect, or a CI commit checkout.
            if a.staged {
                "staged changes".to_string()
            } else if a.working_tree {
                "working-tree changes".to_string()
            } else {
                "local changes".to_string()
            }
        })
}

/// `--intent` verbatim, `--intent-file` from disk, or `None`. Clap has already
/// refused the pair; an unreadable file is an error rather than a silent absence,
/// for the reason given on the flat path's `change_intent`.
fn local_intent(a: &LocalArgs) -> anyhow::Result<Option<String>> {
    use anyhow::Context;
    if let Some(text) = &a.intent {
        return Ok(Some(text.clone()));
    }
    match &a.intent_file {
        Some(path) => {
            Ok(Some(std::fs::read_to_string(path).with_context(|| {
                format!("reading --intent-file {}", path.display())
            })?))
        }
        None => Ok(None),
    }
}

/// The JSON Schema of one operation's output, or the list of operation names.
///
/// Per-operation rather than one schema with every field optional: `get-rules`
/// and `review-local` have nothing in common, and a union type would make every
/// generated client's fields nullable, which is how a missing field becomes
/// indistinguishable from a field that is legitimately absent.
fn operation_schema(operation: Option<&str>) -> anyhow::Result<String> {
    use pr_review_core::{filereview::FileReviewOutput, rules::EffectiveRules};

    let schema = match operation {
        None => {
            return Ok(format!(
                "{}\n",
                serde_json::to_string_pretty(&serde_json::json!({
                    "operations": SCHEMA_OPERATIONS,
                }))?
            ))
        }
        Some("review-local" | "review-pr") => {
            serde_json::to_value(schemars::schema_for!(RunReviewOutput))?
        }
        Some("review-file") => serde_json::to_value(schemars::schema_for!(FileReviewOutput))?,
        Some("get-rules") => serde_json::to_value(schemars::schema_for!(EffectiveRules))?,
        Some("get-findings") => serde_json::to_value(schemars::schema_for!(
            pr_review_core::findings::FindingsOutput
        ))?,
        Some("resolve-findings") => serde_json::to_value(schemars::schema_for!(
            pr_review_core::findings::ResolveOutput
        ))?,
        Some("explain-finding") => serde_json::to_value(schemars::schema_for!(
            pr_review_core::findings::ExplainOutput
        ))?,
        Some(other) => anyhow::bail!(
            "unknown operation {other:?} — known: {}",
            SCHEMA_OPERATIONS.join(", ")
        ),
    };
    Ok(format!("{}\n", serde_json::to_string_pretty(&schema)?))
}

/// Operations that have a schema. Named once so `kaniscope schema` with no
/// argument, and the error for an unknown one, cannot disagree.
const SCHEMA_OPERATIONS: &[&str] = &[
    "review-local",
    "review-pr",
    "review-file",
    "get-rules",
    "get-findings",
    "resolve-findings",
    "explain-finding",
];

/// The human-readable block for the flat-flag path.
fn print_human(args: &Args, out: &RunReviewOutput) {
    print_review(out, args.local, args.dry_run);
}

/// The human-readable block: what would post, then the run's totals.
///
/// Takes the two facts it actually uses rather than the whole `Args`, so the
/// subcommand path can print exactly what the flat path prints. `local` and
/// `dry_run` are distinct on purpose: a local review has nowhere to post, which
/// is the mode, not a withheld action.
fn print_review(out: &RunReviewOutput, local: bool, dry_run: bool) {
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
    if (dry_run || local) && !out.findings_detail.is_empty() {
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
    if (dry_run || local) && !out.inline_detail.is_empty() {
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
        } else if local {
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

    /// Build a repository with a committed file, one STAGED change and one
    /// UNSTAGED change, so the three diff modes have to disagree.
    fn repo_with_staged_and_unstaged() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .output()
                .expect("git runs");
            assert!(out.status.success(), "git {args:?}: {:?}", out);
        };
        run(&["init", "-q", "."]);
        run(&["config", "user.email", "t@example.com"]);
        run(&["config", "user.name", "t"]);
        std::fs::write(dir.path().join("a.txt"), "one\n").expect("write");
        run(&["add", "a.txt"]);
        run(&["commit", "-qm", "init"]);

        std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").expect("write");
        run(&["add", "a.txt"]); // `two` is now staged
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\n").expect("write");
        dir // `three` is unstaged
    }

    fn local_args(argv: &[&str]) -> super::LocalArgs {
        let parsed = Args::try_parse_from(argv).expect("parses");
        match parsed.op {
            Some(super::Op::ReviewLocal(a)) => a,
            _ => panic!("expected review-local"),
        }
    }

    /// "Committed and uncommitted changes" is ambiguous, and the ambiguity loses
    /// work silently: a review that omitted staged changes returns a clean result
    /// for a change it never read. Each mode is pinned to one git invocation here,
    /// against a real repository, so the boundaries cannot drift.
    #[test]
    fn the_diff_modes_do_not_overlap_or_omit_changes() {
        let dir = repo_with_staged_and_unstaged();
        let root = dir.path();

        let staged = super::local_diff(
            root,
            &local_args(["kaniscope", "review-local", "--staged"].as_ref()),
        )
        .expect("staged diff");
        assert!(staged.contains("+two"), "{staged}");
        assert!(
            !staged.contains("+three"),
            "staged must exclude unstaged work: {staged}"
        );

        let working = super::local_diff(
            root,
            &local_args(["kaniscope", "review-local", "--working-tree"].as_ref()),
        )
        .expect("working-tree diff");
        assert!(working.contains("+three"), "{working}");
        assert!(
            !working.contains("+two"),
            "unstaged must exclude staged work: {working}"
        );

        // The union mode. This is the one that must not silently drop staged
        // changes — the failure the named modes exist to prevent.
        let base = super::local_diff(
            root,
            &local_args(["kaniscope", "review-local", "--base", "HEAD"].as_ref()),
        )
        .expect("base diff");
        assert!(
            base.contains("+two"),
            "--base must include staged work: {base}"
        );
        assert!(
            base.contains("+three"),
            "--base must include unstaged work: {base}"
        );
    }

    /// Two diff sources is an ambiguity, not a preference order — refuse rather
    /// than silently letting one win.
    #[test]
    fn conflicting_diff_modes_are_refused() {
        for pair in [
            ["--staged", "--working-tree"],
            ["--base", "--staged"],
            ["--base", "--working-tree"],
        ] {
            let argv = if pair[0] == "--base" {
                vec!["kaniscope", "review-local", "--base", "main", pair[1]]
            } else {
                vec!["kaniscope", "review-local", pair[0], pair[1]]
            };
            let err = Args::try_parse_from(argv.clone())
                .map(|_| ())
                .expect_err(&format!("{argv:?} must be refused"));
            assert_eq!(
                err.kind(),
                clap::error::ErrorKind::ArgumentConflict,
                "{argv:?} must be refused as a conflict, not something else"
            );
        }
    }

    /// A `--base` that git would read as an option, refused. Not shell injection
    /// — `Command` spawns no shell — but argument injection is real once the ref
    /// arrives from a hook, from CI, or through a wrapper package.
    ///
    /// Written with `--base=<value>` rather than a space, deliberately. Clap
    /// rejects `--base --upload-pack=x` itself as an unknown argument, so that
    /// spelling never reaches the guard and testing it would prove nothing. The
    /// attached form hands the leading dash straight through as the value, which
    /// is the case the guard exists for.
    #[test]
    fn a_base_that_looks_like_an_option_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let args = local_args(["kaniscope", "review-local", "--base=--upload-pack=x"].as_ref());
        assert_eq!(
            args.base.as_deref(),
            Some("--upload-pack=x"),
            "clap passed it through"
        );

        let err = super::local_diff(dir.path(), &args).expect_err("must refuse");
        assert!(err.to_string().contains("must be a ref"), "{err}");
    }

    /// The subcommands are additive: every invocation the wrapper clients build,
    /// and every one in the README, still parses to the same flat-flag mode.
    #[test]
    fn adding_subcommands_did_not_change_the_flat_flags() {
        let local = Args::try_parse_from(["kaniscope", "--json", "--local", "--base", "main"])
            .expect("flat local still parses");
        assert!(local.op.is_none(), "no subcommand was given");
        assert!(local.local && local.json);
        assert_eq!(local.base.as_deref(), Some("main"));

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
        .expect("flat PR still parses");
        assert!(pr.op.is_none());
        assert_eq!(pr.pr, Some(12));
        assert!(pr.dry_run);

        for flag in ["--schema", "--config-json", "--config-docs"] {
            let a = Args::try_parse_from(["kaniscope", flag]).expect("still parses");
            assert!(a.op.is_none(), "{flag} must not be read as a subcommand");
        }
    }

    /// `review-pr` posts only when asked. This is inverted from the flat
    /// `--dry-run` flag, and it is the one default in this binary whose wrong
    /// value cannot be undone — a comment on someone's pull request.
    #[test]
    fn review_pr_does_not_post_unless_told_to() {
        let quiet = Args::try_parse_from([
            "kaniscope",
            "review-pr",
            "--provider",
            "github",
            "--repo",
            "o/r",
            "--pr",
            "1",
        ])
        .expect("parses");
        match quiet.op {
            Some(super::Op::ReviewPr(a)) => assert!(!a.post, "posting must be opt-in"),
            _ => panic!("expected review-pr"),
        }

        let loud = Args::try_parse_from([
            "kaniscope",
            "review-pr",
            "--provider",
            "github",
            "--repo",
            "o/r",
            "--pr",
            "1",
            "--post",
        ])
        .expect("parses");
        match loud.op {
            Some(super::Op::ReviewPr(a)) => assert!(a.post),
            _ => panic!("expected review-pr"),
        }
    }

    /// A PR scope missing one of its three parts must be REFUSED, not quietly
    /// downgraded to the local checkout.
    ///
    /// `requires_all` on `provider` alone only enforced one direction, so
    /// `--repo o/r --pr 5` parsed fine, fell through the `(Some, Some, Some)`
    /// match into the local branch, and answered for the current directory — a
    /// result that looks entirely normal and is about the wrong thing. Exactly
    /// the ambiguity the diff modes are refused for.
    #[test]
    fn an_incomplete_pr_scope_is_refused_not_treated_as_local() {
        let partials: &[&[&str]] = &[
            &["--repo", "o/r", "--pr", "5"],
            &["--provider", "github", "--repo", "o/r"],
            &["--provider", "github", "--pr", "5"],
            &["--pr", "5"],
        ];
        for op in ["get-rules", "review-file"] {
            for partial in partials {
                let mut argv = vec!["kaniscope", op];
                if op == "review-file" {
                    argv.extend(["--path", "src/a.rs"]);
                }
                argv.extend(partial.iter().copied());
                let err = Args::try_parse_from(argv.clone())
                    .map(|_| ())
                    .expect_err(&format!("{argv:?} must be refused, not read as local"));
                assert_eq!(
                    err.kind(),
                    clap::error::ErrorKind::MissingRequiredArgument,
                    "{argv:?} must be refused for a MISSING argument"
                );
            }
        }

        // The two complete forms still parse.
        Args::try_parse_from(["kaniscope", "get-rules", "--repo-root", "."]).expect("local");
        Args::try_parse_from([
            "kaniscope",
            "get-rules",
            "--provider",
            "github",
            "--repo",
            "o/r",
            "--pr",
            "5",
        ])
        .expect("full PR scope");
    }

    /// Every name `schema` advertises must actually resolve, or a client
    /// generator follows the list into an error.
    #[test]
    fn every_advertised_operation_has_a_schema() {
        for op in super::SCHEMA_OPERATIONS {
            let out = super::operation_schema(Some(op)).unwrap_or_else(|e| panic!("{op}: {e}"));
            let v: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
            assert!(
                v.get("properties").is_some()
                    || v.get("oneOf").is_some()
                    || v.get("$ref").is_some(),
                "{op} produced a schema with no shape: {out}"
            );
        }
        // The bare listing names exactly those, so the two cannot drift.
        let listed = super::operation_schema(None).expect("lists");
        for op in super::SCHEMA_OPERATIONS {
            assert!(
                listed.contains(op),
                "{op} missing from the listing: {listed}"
            );
        }
        assert!(super::operation_schema(Some("not-an-op")).is_err());
    }
}
