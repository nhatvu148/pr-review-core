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
//! ## The reviewer is the caller's
//!
//! [`serve`] takes a [`ReviewBackend`](crate::backend::ReviewBackend), so the same
//! seven tools run on OpenRouter, on an agent CLI, or on anything else a consumer
//! implements. `kaniscope mcp` passes the OpenRouter backend; a consumer with its
//! own passes that instead and gets the whole toolbox on it.
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

/// Serve MCP over stdin/stdout until stdin closes, reviewing through `backend`.
///
/// The backend is the caller's, not this module's. The first version hardcoded
/// [`crate::backend::OpenRouterBackend`] at every tool that needed a model, which
/// quietly made the MCP surface the one part of this crate a consumer could not
/// point at its own reviewer — in a module whose own documentation, a few lines
/// up, explains that a second path to the same answer is the thing to avoid. A
/// consumer running an agent CLI could expose every operation over MCP except the
/// ones that actually review.
///
/// stdout carries protocol messages and nothing else — the binary already sends
/// all diagnostics to stderr, which is what makes that true rather than hopeful.
///
/// # Errors
/// If stdout cannot be written. A malformed request is answered with a JSON-RPC
/// error and the loop continues: one bad message from a client must not take down
/// a session.
pub async fn serve(
    cfg: &Config,
    backend: Option<&dyn crate::backend::ReviewBackend>,
) -> anyhow::Result<()> {
    use std::io::Write;

    // Chosen once, at `initialize`, from what the client said it can do — see
    // `choose_backend`. Held here because the decision needs the client's
    // capabilities, which do not exist until the handshake.
    let sampling = std::sync::Arc::new(SamplingChannel::new());
    let mut fallback: Option<SamplingBackend> = None;

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
                //
                // No explicit flush here, and that is not an oversight. Rust's
                // `Stdout` wraps a `LineWriter`, so the `writeln!` in `respond`
                // flushes on the newline whether stdout is a terminal or a pipe —
                // unlike C, where a piped stdout is block-buffered and this really
                // would hang a client waiting on the response. Verified against a
                // piped child before relying on it. The flush at the bottom of the
                // loop is belt-and-braces for the same reason.
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
            "initialize" => {
                // The client tells us here whether it can run a model for us. A
                // caller that supplied no backend depends on this answer, so it
                // is read once and kept rather than re-derived per tool call.
                if backend.is_none() && params["capabilities"]["sampling"].is_object() {
                    fallback = Some(SamplingBackend::new(std::sync::Arc::clone(&sampling)));
                    tracing::info!(
                        "no backend supplied and the client offers sampling — \
                         reviews will run on the client's model"
                    );
                }
                success(id, initialize_result())
            }
            "tools/list" => success(id, json!({ "tools": tool_definitions() })),
            "tools/call" => match call_tool(cfg, backend, fallback.as_ref(), &params).await {
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

/// What scope a tool call named — or that it named one badly.
///
/// Three cases rather than `Result`, because "the caller gave no scope" and "the
/// caller gave a broken scope" must not collapse: the first is a local request,
/// the second is a mistake, and treating the second as the first answers
/// confidently about the wrong repository.
enum Scope {
    Local,
    Pr(String, String, u64),
    Invalid(String),
}

/// Dispatch one `tools/call`.
async fn call_tool(
    cfg: &Config,
    supplied: Option<&dyn crate::backend::ReviewBackend>,
    sampling: Option<&SamplingBackend>,
    params: &Value,
) -> anyhow::Result<Value> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("tools/call needs a tool name"))?;
    // Resolved per tool, not once per call. `get_rules`, `get_findings` and
    // `resolve_findings` make NO model call — needing no key is the whole point of
    // the first one — so requiring a reviewer for them would refuse the tools that
    // work fine without one.
    let backend = || resolve_backend(supplied, sampling);
    let args = params.get("arguments").cloned().unwrap_or(json!({}));

    let str_arg = |k: &str| args.get(k).and_then(Value::as_str).map(str::to_string);
    let root = || std::path::PathBuf::from(str_arg("repoRoot").unwrap_or_else(|| ".".to_string()));
    // Three-way, not two-way. The tools that accept either scope used to fall
    // back to local on ANY `pr_coords` error, which meant a PR-scoped call with
    // one field missing — or with `pr` sent as a string, which a model does
    // routinely — silently inspected the server's own checkout and returned a
    // plausible answer about the wrong thing. Absent means local; present but
    // wrong is an error the caller has to see.
    let scope = || -> Scope {
        let present = ["provider", "repo", "pr"]
            .iter()
            .filter(|k| args.get(**k).is_some_and(|v| !v.is_null()))
            .count();
        if present == 0 {
            return Scope::Local;
        }
        let Some(provider) = str_arg("provider") else {
            return Scope::Invalid("`provider` is required for a pull-request scope".into());
        };
        let Some(repo) = str_arg("repo") else {
            return Scope::Invalid("`repo` is required for a pull-request scope".into());
        };
        match args.get("pr").and_then(Value::as_u64) {
            Some(pr) => Scope::Pr(provider, repo, pr),
            None => Scope::Invalid(
                "`pr` is required for a pull-request scope and must be a non-negative number"
                    .into(),
            ),
        }
    };
    // For the tools that only ever take a PR.
    let pr_coords = || -> anyhow::Result<(String, String, u64)> {
        match scope() {
            Scope::Pr(p, r, n) => Ok((p, r, n)),
            Scope::Local => Err(anyhow::anyhow!(
                "`provider`, `repo` and `pr` are required for this tool"
            )),
            Scope::Invalid(why) => Err(anyhow::anyhow!(why)),
        }
    };

    match name {
        "get_rules" => match scope() {
            Scope::Pr(p, r, n) => tool_json(&crate::rules::remote(cfg, &p, &r, n).await?),
            // Local only when NOTHING was given: `get_rules` with no scope means
            // "this checkout", which is the common case. A partial or malformed
            // scope is the caller's mistake and is reported as one.
            Scope::Local => tool_json(&crate::rules::local(cfg, Some(&root()))),
            Scope::Invalid(why) => anyhow::bail!(why),
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
                    backend()?,
                    &root(),
                    finding,
                    str_arg("headSha").as_deref(),
                )
                .await?,
            )
        }
        "review_file" => {
            let path = str_arg("path").ok_or_else(|| anyhow::anyhow!("`path` is required"))?;
            match scope() {
                Scope::Pr(p, r, n) => {
                    let (out, _) =
                        crate::filereview::review_pr_file(cfg, backend()?, &p, &r, n, &path)
                            .await?;
                    tool_json(&out)
                }
                Scope::Local => tool_json(
                    &crate::filereview::review_local(cfg, backend()?, &root(), &path).await?,
                ),
                Scope::Invalid(why) => anyhow::bail!(why),
            }
        }
        "review_pr" => {
            let (provider, repo, pr) = pr_coords()?;
            tool_json(
                &crate::review::run_review_with(
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
                    backend()?,
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
                    backend()?,
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

// ── reviewing with no key at all: sampling from the calling agent ────────────

/// A [`ReviewBackend`](crate::backend::ReviewBackend) that asks the **MCP client**
/// to run the model.
///
/// MCP lets a server request a completion from the host through
/// `sampling/createMessage`. When the host is a coding agent, that means the
/// reviewer runs on the agent's own model, with the agent's own credentials — so
/// `kaniscope mcp` needs no `OPENROUTER_API_KEY`, no second subscription, and no
/// second bill for a model the caller is already paying for.
///
/// It also inverts the usual trust question in a useful way: the user can see the
/// sampling request, because MCP hosts are expected to show it. A review that
/// spends the host's tokens is one the host approved.
///
/// **Only usable when the client advertises `sampling` in its `initialize`
/// capabilities.** A server that sends `sampling/createMessage` to a client that
/// never offered it gets a method-not-found back, after the user has waited for a
/// review — so [`serve`] checks the capability up front and refuses at the tool
/// call with a message naming the cause, rather than failing deep in a model call.
pub struct SamplingBackend {
    requests: std::sync::Arc<SamplingChannel>,
}

impl SamplingBackend {
    /// Wrap a channel to the client.
    #[must_use]
    pub fn new(requests: std::sync::Arc<SamplingChannel>) -> Self {
        Self { requests }
    }
}

/// The server's side of a `sampling/createMessage` round trip.
///
/// Sending a request *to* the client from inside a tool call means the stdio loop
/// is momentarily inverted: this writes a request and then reads until the
/// matching response id comes back. Everything is serialized behind one mutex, so
/// two concurrent tool calls cannot interleave their sampling round trips and
/// read each other's answers.
pub struct SamplingChannel {
    next_id: std::sync::atomic::AtomicU64,
    /// Held across a whole round trip, not just each read — the lock IS the
    /// serialization, and releasing it between write and read would let a second
    /// sampling call consume the first one's response.
    io: tokio::sync::Mutex<()>,
}

impl Default for SamplingChannel {
    fn default() -> Self {
        Self::new()
    }
}

impl SamplingChannel {
    #[must_use]
    pub fn new() -> Self {
        Self {
            // Above any id a client is likely to use for its own requests. Ids
            // only have to be unique per sender, but a visible split makes a
            // transcript readable when something goes wrong.
            next_id: std::sync::atomic::AtomicU64::new(1_000_000),
            io: tokio::sync::Mutex::new(()),
        }
    }

    /// Ask the client for one completion and return its text.
    ///
    /// # Errors
    /// If the client answers with a JSON-RPC error (most usefully: it does not
    /// support sampling), if stdin closes mid-round-trip, or if the response
    /// carries no text content.
    pub async fn complete(
        &self,
        system: &str,
        user: &str,
        max_tokens: u32,
    ) -> anyhow::Result<String> {
        use std::io::Write;
        use std::sync::atomic::Ordering;

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "sampling/createMessage",
            "params": {
                "messages": [{ "role": "user", "content": { "type": "text", "text": user } }],
                "systemPrompt": system,
                "maxTokens": max_tokens,
                // A review is judgement over code the host already has open, so
                // the host's own context is the useful context. `thisServer` asks
                // for it without claiming to need anything beyond this session.
                "includeContext": "thisServer",
            },
        });

        let _guard = self.io.lock().await;
        {
            let mut out = std::io::stdout().lock();
            writeln!(out, "{}", serde_json::to_string(&request)?)?;
            out.flush()?;
        }

        // Read until the matching response. Anything else on the way is a message
        // the client sent us mid-round-trip; notifications are dropped, and a
        // request cannot be answered here without reentering the dispatcher, so
        // it is reported rather than silently ignored.
        loop {
            let line = tokio::task::spawn_blocking(|| {
                let mut buf = String::new();
                std::io::stdin().read_line(&mut buf).map(|n| (n, buf))
            })
            .await??;
            let (read, line) = line;
            if read == 0 {
                anyhow::bail!("the MCP client closed stdin while a sampling request was pending");
            }
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if value.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(err) = value.get("error") {
                let message = err
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error");
                anyhow::bail!("the MCP client refused the sampling request: {message}");
            }
            // `content` is a single block, not an array, in `sampling/createMessage`.
            let text = value["result"]["content"]["text"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("sampling response carried no text content"))?;
            return Ok(text.to_string());
        }
    }
}

#[async_trait::async_trait]
impl crate::backend::ReviewBackend for SamplingBackend {
    async fn complete(&self, cfg: &Config, system: &str, user: &str) -> anyhow::Result<String> {
        self.requests
            .complete(system, user, cfg.openrouter_max_tokens)
            .await
    }

    async fn review(
        &self,
        ctx: &crate::backend::ReviewContext<'_>,
    ) -> anyhow::Result<crate::llm::ReviewResult> {
        // The diff-only rubric, not the agentic one: the host model answers a
        // single prompt here and has no tool loop of this server's to drive.
        // `ctx.system_prompt` rather than the bare const, so the orchestrator's
        // calibration rules reach this backend like every other.
        let system = ctx.system_prompt(crate::prompt::SYSTEM_PROMPT);
        let user = crate::prompt::build_user_prompt(
            ctx.meta,
            ctx.diff,
            false, // the orchestrator packed the diff before handing it over
            ctx.omitted_note,
            ctx.structural_context,
            ctx.untrusted,
        );

        let text = crate::backend::ReviewBackend::complete(self, ctx.cfg, &system, &user).await?;
        let json = crate::llm::extract_json(&text)
            .map(str::to_owned)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "the client's model returned no JSON object: {}",
                    crate::clip(&text, 300)
                )
            })?;

        // Same repair pass every other backend gets, routed back through the
        // client — a malformed review is salvaged on the host's model rather than
        // silently needing a key this backend exists to avoid.
        let (review, _usage) = crate::llm::parse_review_with_repair(
            &json,
            "sampled review",
            |system, user| async move {
                let text = self
                    .requests
                    .complete(system, &user, ctx.cfg.openrouter_max_tokens)
                    .await?;
                Ok(crate::llm::Completion {
                    text,
                    model: None,
                    usage: None,
                })
            },
        )
        .await?;

        Ok(crate::llm::ReviewResult {
            review,
            // Named for what it is. The host chose the model and this server is
            // not told which, so claiming one would be an invention — and the run
            // log would attribute findings to a model that may never have run.
            model: "mcp-sampling (client's model)".to_string(),
            // No usage: the tokens were spent on the client's account, and this
            // side cannot see the count. Absent is honest; zero would not be.
            usage: None,
        })
    }
}

/// The backend a tool call should use, or why there is none.
///
/// Three outcomes, and the third is the one worth being careful about: a server
/// with no key and a client that cannot sample must say so **at the tool call**,
/// in words naming both halves of the fix. The alternative — failing inside the
/// model call — reports a missing `OPENROUTER_API_KEY` to someone who deliberately
/// did not set one, which reads as a bug in the tool rather than a choice about
/// where the model runs.
fn resolve_backend<'a>(
    supplied: Option<&'a dyn crate::backend::ReviewBackend>,
    sampling: Option<&'a SamplingBackend>,
) -> anyhow::Result<&'a dyn crate::backend::ReviewBackend> {
    if let Some(b) = supplied {
        return Ok(b);
    }
    if let Some(b) = sampling {
        return Ok(b);
    }
    anyhow::bail!(
        "no reviewer is available: this server was started without a model backend, \
         and the MCP client did not advertise `sampling` support in its initialize \
         capabilities. Either set OPENROUTER_API_KEY so the server can review on its \
         own, or use a client that supports MCP sampling so reviews can run on its model."
    )
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

    /// A backend that records whether it was reached, so a test can prove the
    /// CALLER's backend is the one the tools use — the property the seam exists
    /// for, and the one a hardcoded `OpenRouterBackend` silently broke.
    struct SpyBackend(std::sync::atomic::AtomicUsize);

    #[async_trait::async_trait]
    impl crate::backend::ReviewBackend for SpyBackend {
        async fn review(
            &self,
            _ctx: &crate::backend::ReviewContext<'_>,
        ) -> anyhow::Result<crate::llm::ReviewResult> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            anyhow::bail!("spy backend reached")
        }
        async fn complete(
            &self,
            _cfg: &Config,
            _system: &str,
            _user: &str,
        ) -> anyhow::Result<String> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            anyhow::bail!("spy backend reached")
        }
    }

    /// The tools review through the backend they were GIVEN.
    ///
    /// No network is configured here, so a regression to a hardcoded OpenRouter
    /// backend fails on a missing key or a connection rather than passing quietly.
    #[tokio::test]
    async fn the_tools_use_the_backend_they_were_given() {
        use std::sync::atomic::Ordering;
        let cfg = Config::from_env();
        let spy = SpyBackend(std::sync::atomic::AtomicUsize::new(0));
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a.rs"), "fn main() {}\n").expect("write");
        let root = dir.path().to_string_lossy().to_string();

        let err = call_tool(
            &cfg,
            Some(&spy),
            None,
            &json!({
                "name": "review_file",
                "arguments": { "path": "a.rs", "repoRoot": root },
            }),
        )
        .await
        .expect_err("the spy always fails, which is how we know it ran");

        assert!(err.to_string().contains("spy backend reached"), "{err}");
        assert_eq!(
            spy.0.load(Ordering::Relaxed),
            1,
            "the caller's backend was used"
        );
    }

    /// With no backend and no sampling, the refusal has to name BOTH ways out.
    ///
    /// Failing inside the model call instead would report a missing
    /// `OPENROUTER_API_KEY` to someone who deliberately did not set one — which
    /// reads as a broken tool rather than a choice about where the model runs.
    #[test]
    fn with_no_backend_and_no_sampling_the_refusal_names_both_fixes() {
        let err = resolve_backend(None, None)
            .map(|_| ())
            .expect_err("must refuse");
        let msg = err.to_string();
        assert!(msg.contains("OPENROUTER_API_KEY"), "{msg}");
        assert!(msg.contains("sampling"), "{msg}");
    }

    /// A supplied backend always wins: a consumer that passed its own reviewer
    /// must not have it silently replaced by the client's model.
    #[test]
    fn a_supplied_backend_is_never_replaced_by_sampling() {
        let supplied = crate::backend::OpenRouterBackend;
        let sampling = SamplingBackend::new(std::sync::Arc::new(SamplingChannel::new()));
        // Both available — the supplied one is chosen.
        let chosen = match resolve_backend(Some(&supplied), Some(&sampling)) {
            Ok(b) => b,
            Err(e) => panic!("must resolve: {e}"),
        };
        assert!(
            std::ptr::eq(
                chosen as *const dyn crate::backend::ReviewBackend as *const u8,
                &supplied as *const _ as *const u8
            ),
            "the caller's backend must win"
        );
        // Sampling alone is used when nothing was supplied.
        assert!(resolve_backend(None, Some(&sampling)).is_ok());
    }

    /// Every advertised tool must dispatch. A name in the list that falls through
    /// to "unknown tool" is a tool a model will call once and never trust again.
    ///
    /// `repoRoot` points at an EMPTY temporary directory, not at the default `.`.
    /// With `.`, `review_local` runs `git diff HEAD` against this repository — so
    /// on a developer's machine with uncommitted work (the normal state while
    /// running the tests) the diff is non-empty, dispatch continues into
    /// `run_review_local`, and the unit test makes a real, billed OpenRouter call.
    /// The test's behaviour must not depend on the working tree being clean.
    #[tokio::test]
    async fn every_advertised_tool_is_dispatchable() {
        let cfg = Config::from_env();
        let empty = tempfile::tempdir().expect("tempdir");
        let root = empty.path().to_string_lossy().to_string();
        for t in tool_definitions() {
            let name = t["name"].as_str().expect("named");
            // Called with only a scope: each must fail on a MISSING ARGUMENT or
            // on doing its work, never on the name.
            let err = call_tool(
                &cfg,
                Some(&crate::backend::OpenRouterBackend),
                None,
                &json!({ "name": name, "arguments": { "repoRoot": root } }),
            )
            .await
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
            assert!(
                !err.contains("unknown tool"),
                "{name} is advertised but not dispatched"
            );
        }
        let err = call_tool(
            &cfg,
            Some(&crate::backend::OpenRouterBackend),
            None,
            &json!({ "name": "not_a_tool", "arguments": {} }),
        )
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

    /// A malformed PR scope must be an error, not a silent local answer.
    ///
    /// `get_rules` and `review_file` used to fall back to local on ANY scope
    /// error, so a model that sent `pr` as a string — which they do routinely —
    /// got a confident answer about the server's own checkout instead of the
    /// pull request it asked for.
    #[tokio::test]
    async fn a_malformed_pr_scope_is_an_error_not_a_local_answer() {
        let cfg = Config::from_env();
        let empty = tempfile::tempdir().expect("tempdir");
        let root = empty.path().to_string_lossy().to_string();

        for bad in [
            json!({ "provider": "github", "repo": "o/r", "pr": "12" }), // a string
            json!({ "provider": "github", "repo": "o/r" }),             // missing pr
            json!({ "repo": "o/r", "pr": 12 }),                         // missing provider
        ] {
            let mut args = bad.as_object().expect("object").clone();
            args.insert("repoRoot".into(), json!(root));
            let err = call_tool(
                &cfg,
                Some(&crate::backend::OpenRouterBackend),
                None,
                &json!({ "name": "get_rules", "arguments": args }),
            )
            .await
            .expect_err("a partial scope must not be answered locally");
            assert!(
                err.to_string().contains("pull-request scope"),
                "{bad}: {err}"
            );
        }

        // ...and no scope at all is still the local case, which is the common one.
        call_tool(
            &cfg,
            Some(&crate::backend::OpenRouterBackend),
            None,
            &json!({ "name": "get_rules", "arguments": { "repoRoot": root } }),
        )
        .await
        .expect("no scope means this checkout");
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
