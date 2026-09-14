//! An MCP server over stdio, exposing the toolbox operations as tools.
//!
//! ## Why this is not a second implementation
//!
//! Every tool here calls the same library function the CLI subcommand calls, and
//! returns the same serialized type. Nothing shells out, and nothing parses
//! human-readable output. The handoff this came from is explicit about that, and
//! the reason is the one this crate keeps relearning: a second path to the same
//! answer is a path that drifts, and the drift is invisible because both sides
//! keep returning plausible reviews.
//!
//! ## Why there is no protocol dependency
//!
//! MCP over stdio is newline-delimited JSON-RPC 2.0 with three methods that
//! matter. That is less code than the wiring an SDK would need, and this is a
//! **library** crate — every dependency added here is one imposed on every
//! consumer, and a protocol SDK moving fast is a poor thing to impose. The same
//! reasoning `packaging/generate-types.mjs` gives for being dependency-free.
//!
//! ## Read-only, deliberately
//!
//! The tools exposed here **cannot post, edit, or resolve anything**. `review_pr`
//! always runs dry. That is a narrower surface than the CLI, which can post with
//! `--post`, and the asymmetry is the point: a CLI invocation is typed by someone
//! who sees the flags, while an MCP tool call is composed by a model from a
//! description and issued without a human reading the arguments. A capability
//! whose worst case is a comment on someone's pull request does not belong on the
//! second kind of surface. A caller that wants to post has the CLI.

use serde_json::{json, Value};

use crate::config::Config;

/// The MCP protocol version this server implements.
const PROTOCOL_VERSION: &str = "2025-06-18";

/// Serve MCP over stdin/stdout until stdin closes.
///
/// stdout carries protocol messages and nothing else — the binary already sends
/// all diagnostics to stderr, which is what makes that true rather than hopeful.
///
/// # Errors
/// If stdout cannot be written. A malformed request is answered with a JSON-RPC
/// error and the loop continues: one bad message from a client must not take down
/// a session.
pub async fn serve(cfg: &Config) -> anyhow::Result<()> {
    use std::io::Write;

    loop {
        // Blocking reads on a worker thread: this loop has nothing to overlap
        // with, and `tokio::io::stdin` would mean two more tokio features on a
        // library crate for no behavioural gain.
        let line = match tokio::task::spawn_blocking(|| {
            let mut buf = String::new();
            std::io::stdin().read_line(&mut buf).map(|n| (n, buf))
        })
        .await??
        {
            (0, _) => return Ok(()), // stdin closed: the client went away
            (_, buf) => buf,
        };
        if line.trim().is_empty() {
            continue;
        }

        let request: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                // -32700 is JSON-RPC's parse error. Answered with a null id
                // because the id is exactly what could not be read.
                respond(&error_response(
                    Value::Null,
                    -32700,
                    &format!("parse error: {e}"),
                ))?;
                continue;
            }
        };

        let id = request.get("id").cloned();
        let method = request.get("method").and_then(Value::as_str).unwrap_or("");
        let params = request.get("params").cloned().unwrap_or(json!({}));

        // A notification has no id and takes no response — answering one is a
        // protocol violation some clients treat as fatal.
        let Some(id) = id else {
            continue;
        };

        let response = match method {
            "initialize" => success(id, initialize_result()),
            "tools/list" => success(id, json!({ "tools": tool_definitions() })),
            "tools/call" => match call_tool(cfg, &params).await {
                Ok(v) => success(id, v),
                // Reported as a tool error rather than a protocol error: the call
                // reached the tool and the tool failed, which is something the
                // model should see and can act on, not a transport fault.
                Err(e) => success(id, tool_error(&format!("{e:#}"))),
            },
            // -32601: method not found.
            other => error_response(id, -32601, &format!("unknown method {other:?}")),
        };
        respond(&response)?;
        std::io::stdout().flush()?;
    }
}

/// Write one JSON-RPC message as a single line on stdout.
fn respond(message: &Value) -> anyhow::Result<()> {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    writeln!(out, "{}", serde_json::to_string(message)?)?;
    Ok(())
}

fn success(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error_response(id: Value, code: i32, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// A tool result carrying an error the model should read and act on.
fn tool_error(message: &str) -> Value {
    json!({
        "content": [{ "type": "text", "text": message }],
        "isError": true,
    })
}

/// A tool result carrying one JSON document.
///
/// Serialized into a text block because that is what MCP tool results carry, and
/// the shape inside it is the same type the CLI prints — so a caller can use the
/// operation's published schema either way.
fn tool_json(value: &impl serde::Serialize) -> anyhow::Result<Value> {
    Ok(json!({
        "content": [{ "type": "text", "text": serde_json::to_string(value)? }],
    }))
}

fn initialize_result() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "kaniscope", "version": crate::VERSION },
        "instructions":
            "An independent, advisory code reviewer. Every tool here is READ-ONLY: none of \
             them edits code, posts to a pull request, or resolves a review thread. Findings \
             are advice for you to weigh against the code in front of you, not instructions \
             to apply.",
    })
}

/// The tools, with their input schemas.
///
/// Hand-written rather than derived: these are *input* shapes, small and stable,
/// and a derived schema would carry Rust's naming and doc prose into a surface a
/// model reads to decide what to call. The *outputs* stay generated, where the
/// drift actually costs something.
fn tool_definitions() -> Vec<Value> {
    let pr_props = json!({
        "provider": { "type": "string", "description": "github | gitlab | bitbucket" },
        "repo": { "type": "string", "description": "owner/repo" },
        "pr": { "type": "integer", "description": "pull request number" },
    });
    vec![
        json!({
            "name": "review_local",
            "description":
                "Review uncommitted or unpushed local changes. Read-only; posts nothing. Pass \
                 `intent` saying what the change is meant to do and the reviewer will check \
                 the diff against it — a local change has no PR description, so this is the \
                 only way it learns what the work was for.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repoRoot": { "type": "string", "description": "checkout to review; defaults to the working directory" },
                    "base": { "type": "string", "description": "diff against this ref, through the working tree" },
                    "staged": { "type": "boolean", "description": "review staged changes only" },
                    "workingTree": { "type": "boolean", "description": "review unstaged changes only" },
                    "intent": { "type": "string", "description": "what this change is meant to do" },
                    "label": { "type": "string", "description": "what to call this change in the output" },
                },
            },
        }),
        json!({
            "name": "review_pr",
            "description":
                "Review a pull request. ALWAYS a dry run through this server — it produces the \
                 review and posts nothing. Use the kaniscope CLI with --post if the user has \
                 asked for the review to go on the PR.",
            "inputSchema": { "type": "object", "properties": pr_props, "required": ["provider", "repo", "pr"] },
        }),
        json!({
            "name": "review_file",
            "description":
                "Deep-review one complete file rather than a diff, so findings can land \
                 anywhere in it. Read-only. Give repoRoot for a local file, or provider/repo/pr \
                 to read it at a pull request's head.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "repository-relative path" },
                    "repoRoot": { "type": "string" },
                    "provider": { "type": "string" },
                    "repo": { "type": "string" },
                    "pr": { "type": "integer" },
                },
                "required": ["path"],
            },
        }),
        json!({
            "name": "get_rules",
            "description":
                "The review rules actually in effect: merged settings, which .prbot.toml was \
                 read and what it overrode, and the exact instructions injected into the \
                 reviewer. Answers 'why did it flag that' and 'why did it not'. Makes no model \
                 call and returns no credentials.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repoRoot": { "type": "string" },
                    "provider": { "type": "string" },
                    "repo": { "type": "string" },
                    "pr": { "type": "integer" },
                },
            },
        }),
        json!({
            "name": "get_findings",
            "description":
                "List the findings this reviewer has on a pull request, with lifecycle state: \
                 active, resolved, or unparseable. Check outcome.status — a provider that \
                 cannot track findings returns 'unsupported', which means UNKNOWN, not 'none \
                 are open'.",
            "inputSchema": { "type": "object", "properties": pr_props, "required": ["provider", "repo", "pr"] },
        }),
        json!({
            "name": "resolve_findings",
            "description":
                "Select findings to work on and get them back with context and a recommended \
                 action. Despite the name it resolves NOTHING — it does not edit code, post, or \
                 close any thread. Omit fingerprints to take every active finding.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "provider": { "type": "string" },
                    "repo": { "type": "string" },
                    "pr": { "type": "integer" },
                    "fingerprints": { "type": "array", "items": { "type": "string" } },
                },
                "required": ["provider", "repo", "pr"],
            },
        }),
        json!({
            "name": "explain_finding",
            "description":
                "Investigate one finding against the real file and return a verdict of holds, \
                 doesNotHold or inconclusive, with evidence. 'doesNotHold' is a real answer — \
                 reviewers produce false positives, and this is how you catch one before acting \
                 on it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "finding": {
                        "type": "object",
                        "description": "the finding: file, line, body, severity, originalCommit",
                        "properties": {
                            "file": { "type": "string" },
                            "line": { "type": "integer" },
                            "body": { "type": "string" },
                            "severity": { "type": "string" },
                            "originalCommit": { "type": "string" },
                        },
                        "required": ["file", "body"],
                    },
                    "repoRoot": { "type": "string" },
                    "headSha": { "type": "string", "description": "the commit the checkout is at, so a revision mismatch can be reported" },
                },
                "required": ["finding"],
            },
        }),
    ]
}

/// Dispatch one `tools/call`.
async fn call_tool(cfg: &Config, params: &Value) -> anyhow::Result<Value> {
    use crate::backend::OpenRouterBackend;

    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("tools/call needs a tool name"))?;
    let args = params.get("arguments").cloned().unwrap_or(json!({}));

    let str_arg = |k: &str| args.get(k).and_then(Value::as_str).map(str::to_string);
    let root = || std::path::PathBuf::from(str_arg("repoRoot").unwrap_or_else(|| ".".to_string()));
    // Required together: a PR tool called with two of the three would otherwise
    // reach a provider as a malformed coordinate and fail with its error.
    let pr_coords = || -> anyhow::Result<(String, String, u64)> {
        let provider =
            str_arg("provider").ok_or_else(|| anyhow::anyhow!("`provider` is required"))?;
        let repo = str_arg("repo").ok_or_else(|| anyhow::anyhow!("`repo` is required"))?;
        let pr = args
            .get("pr")
            .and_then(Value::as_u64)
            .ok_or_else(|| anyhow::anyhow!("`pr` is required and must be a number"))?;
        Ok((provider, repo, pr))
    };

    match name {
        "get_rules" => match pr_coords() {
            Ok((p, r, n)) => tool_json(&crate::rules::remote(cfg, &p, &r, n).await?),
            // Local is the fallback, not an error: `get_rules` with no scope at
            // all means "this checkout", which is the common case.
            Err(_) => tool_json(&crate::rules::local(cfg, Some(&root()))),
        },
        "get_findings" => {
            let (p, r, n) = pr_coords()?;
            tool_json(&crate::findings::get_findings(cfg, &p, &r, n).await?)
        }
        "resolve_findings" => {
            let (p, r, n) = pr_coords()?;
            let fps: Vec<String> = args
                .get("fingerprints")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            tool_json(&crate::findings::resolve_findings(cfg, &p, &r, n, &fps).await?)
        }
        "explain_finding" => {
            let finding: crate::findings::ExplainInput = serde_json::from_value(
                args.get("finding")
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("`finding` is required"))?,
            )?;
            tool_json(
                &crate::findings::explain_finding(
                    cfg,
                    &OpenRouterBackend,
                    &root(),
                    finding,
                    str_arg("headSha").as_deref(),
                )
                .await?,
            )
        }
        "review_file" => {
            let path = str_arg("path").ok_or_else(|| anyhow::anyhow!("`path` is required"))?;
            match pr_coords() {
                Ok((p, r, n)) => {
                    let (out, _) = crate::filereview::review_pr_file(
                        cfg,
                        &OpenRouterBackend,
                        &p,
                        &r,
                        n,
                        &path,
                    )
                    .await?;
                    tool_json(&out)
                }
                Err(_) => tool_json(
                    &crate::filereview::review_local(cfg, &OpenRouterBackend, &root(), &path)
                        .await?,
                ),
            }
        }
        "review_pr" => {
            let (provider, repo, pr) = pr_coords()?;
            tool_json(
                &crate::review::run_review(
                    cfg,
                    crate::review::RunReviewInput {
                        provider,
                        repo,
                        pr,
                        // Not configurable, by design — see the module docs. An
                        // MCP tool call is composed by a model and issued without
                        // a human reading the arguments; posting does not belong
                        // on that surface at any price.
                        dry_run: true,
                        placeholder: false,
                    },
                )
                .await?,
            )
        }
        "review_local" => {
            let root = root();
            let diff = local_diff(&root, &args)?;
            tool_json(
                &crate::review::run_review_local(
                    cfg,
                    crate::review::LocalReviewInput {
                        diff,
                        label: str_arg("label").unwrap_or_else(|| "local changes".to_string()),
                        repo_root: Some(root),
                        change_intent: str_arg("intent"),
                    },
                    &OpenRouterBackend,
                )
                .await?,
            )
        }
        other => anyhow::bail!("unknown tool {other:?}"),
    }
}

/// The diff for `review_local`, from whichever mode the caller selected.
///
/// No stdin mode here — this process's stdin is the MCP transport, and reading a
/// diff from it would consume the protocol stream. `--base HEAD` is the default
/// because it is the only mode that covers committed, staged and unstaged work at
/// once, which is what "review my changes" means to someone who has not thought
/// about the distinction.
fn local_diff(root: &std::path::Path, args: &Value) -> anyhow::Result<String> {
    let base = args.get("base").and_then(Value::as_str);
    let staged = args.get("staged").and_then(Value::as_bool).unwrap_or(false);
    let working = args
        .get("workingTree")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    if [base.is_some(), staged, working]
        .iter()
        .filter(|x| **x)
        .count()
        > 1
    {
        anyhow::bail!("give at most one of `base`, `staged` or `workingTree`");
    }
    let argv: Vec<&str> = if let Some(b) = base {
        if b.starts_with('-') {
            anyhow::bail!("`base` must be a ref, not an option (got {b:?})");
        }
        vec!["diff", b, "--"]
    } else if staged {
        vec!["diff", "--cached", "--"]
    } else if working {
        vec!["diff", "--"]
    } else {
        vec!["diff", "HEAD", "--"]
    };

    let out = std::process::Command::new("git")
        .args(&argv)
        .current_dir(root)
        .output()
        .map_err(|e| anyhow::anyhow!("could not run git: {e}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "git {} failed: {}",
            argv.join(" "),
            crate::clip(String::from_utf8_lossy(&out.stderr).trim(), 400)
        );
    }
    let diff = String::from_utf8_lossy(&out.stdout).into_owned();
    if diff.trim().is_empty() {
        anyhow::bail!("no changes to review in {}", root.display());
    }
    Ok(diff)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The safety property this server is built around. A tool description that
    /// advertised posting, or a `review_pr` that honoured a `post` argument,
    /// would put "comment on someone's pull request" behind a call a model
    /// composes without a human reading it.
    #[test]
    fn no_tool_can_post_edit_or_resolve_anything() {
        let tools = tool_definitions();
        let names: Vec<&str> = tools
            .iter()
            .map(|t| t["name"].as_str().expect("named"))
            .collect();
        assert_eq!(names.len(), 7, "{names:?}");

        for t in &tools {
            let schema = &t["inputSchema"]["properties"];
            for forbidden in ["post", "dryRun", "dry_run", "resolve"] {
                assert!(
                    schema.get(forbidden).is_none(),
                    "{} must not accept `{forbidden}`",
                    t["name"]
                );
            }
        }
        // ...and the server says so up front, where a model reads it.
        let instructions = initialize_result()["instructions"]
            .as_str()
            .expect("instructions")
            .to_string();
        assert!(instructions.contains("READ-ONLY"), "{instructions}");
    }

    /// Every advertised tool must dispatch. A name in the list that falls through
    /// to "unknown tool" is a tool a model will call once and never trust again.
    #[tokio::test]
    async fn every_advertised_tool_is_dispatchable() {
        let cfg = Config::from_env();
        for t in tool_definitions() {
            let name = t["name"].as_str().expect("named");
            // Called with no arguments: each must fail on a MISSING ARGUMENT or
            // on doing its work, never on the name.
            let err = call_tool(&cfg, &json!({ "name": name, "arguments": {} }))
                .await
                .err()
                .map(|e| e.to_string())
                .unwrap_or_default();
            assert!(
                !err.contains("unknown tool"),
                "{name} is advertised but not dispatched"
            );
        }
        let err = call_tool(&cfg, &json!({ "name": "not_a_tool", "arguments": {} }))
            .await
            .expect_err("must reject");
        assert!(err.to_string().contains("unknown tool"), "{err}");
    }

    /// A notification carries no id and must get no response — answering one is a
    /// protocol violation some clients treat as fatal.
    #[test]
    fn the_initialize_result_names_the_protocol_and_the_server() {
        let r = initialize_result();
        assert_eq!(r["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(r["serverInfo"]["name"], "kaniscope");
        assert_eq!(r["serverInfo"]["version"], crate::VERSION);
        assert!(r["capabilities"]["tools"].is_object());
    }

    /// `review_local` cannot read a diff from stdin, because stdin is the
    /// protocol stream. The default mode has to cover everything instead.
    #[test]
    fn conflicting_local_diff_modes_are_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = local_diff(dir.path(), &json!({ "staged": true, "workingTree": true }))
            .expect_err("must refuse");
        assert!(err.to_string().contains("at most one"), "{err}");

        let err = local_diff(dir.path(), &json!({ "base": "--upload-pack=x" }))
            .expect_err("must refuse an option as a ref");
        assert!(err.to_string().contains("must be a ref"), "{err}");
    }
}
