//! Agentic reviewer: the model is given the PR diff plus read-only tools
//! (`read_file`, `list_dir`, `grep`) over a clone of the repo, investigates
//! cross-file context on its own, then returns the same structured review the
//! diff-only path produces.
//!
//! The tool-calling loop itself lives in the shared `agent-core` crate (a cheap
//! model explores with tools, a strong model synthesizes the review). This
//! module supplies the three repo tools, frames the diff, and parses the result
//! back into a [`Review`]. The migration also fixed a live bug: the old
//! hand-rolled `reqwest::Client` had no timeout, so a stalled provider hung the
//! whole review — `agent-core`'s transport carries a timeout and 429 retry.

use std::sync::Arc;
use std::time::Duration;

use agent_loop_core::{
    Backend, ChatBackend, ChatClient, EventSink, ModelPolicy, ProviderConfig, RetryPolicy,
    RunRequest, Tool, ToolError, ToolOutput, ToolRegistry,
};
use anyhow::Result;
use async_trait::async_trait;
use reqwest::Client;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::clip;
use crate::config::{require, Config};
use crate::llm::{extract_json, Review, ReviewResult, Usage};
use crate::providers::PrMeta;
use crate::repo::Workspace;

const AGENT_SYSTEM_PROMPT: &str = r#"You are an expert software engineer reviewing a pull request. You are given the PR's unified diff and READ-ONLY tools to explore the rest of the repository (a clone at the PR head):
- grep(pattern): regex search across the repo
- read_file(path, start?, end?): read a file (optionally a 1-indexed line range)
- list_dir(path): list a directory

Investigate the change in context: look up the definitions and CALLERS of changed functions, the types they use, related tests, and config. Be economical: grep to find the relevant line, then read_file with a NARROW line range (start/end) rather than whole files. Aim to finish in a few focused lookups, not exhaustive crawling.

When done, respond with ONLY a JSON object (no tools, no prose) of this shape:
{
  "summary": "<1-2 sentence overall summary>",
  "recommendation": "BLOCK" | "APPROVE WITH CHANGES" | "APPROVE",
  "findings": [
    { "severity": "BLOCKING"|"HIGH"|"MEDIUM"|"LOW",
      "file": "<path exactly as in the diff>",
      "line": <new-side line number shown in the diff, or null>,
      "body": "<one sentence problem, then ' Fix: ' and the fix>",
      "confidence": <integer 0-100 — your confidence a senior reviewer would flag this> }
  ]
}
Rules: only raise findings on lines shown in the diff (set line=null if you can't pin one — it folds into the summary). Use the repo context to catch cross-file issues (a change that breaks a caller, a wrong type, a missing update elsewhere). Don't invent problems."#;

// ---- repo tools -----------------------------------------------------------
//
// The three read-only tools, as typed `agent-core` tools. Each carries a
// lightweight `Workspace` handle sharing the clone's root (the caller's
// `Workspace` owns the TempDir and outlives the run). Output is clipped to the
// same 6 KB the hand-rolled loop used, and a workspace error is returned as
// `Error: ...` text — identical to what the model saw before — so it keeps
// exploring rather than aborting.

const TOOL_CLIP: usize = 6_000;

fn ws_result(r: Result<String>) -> ToolOutput {
    match r {
        Ok(s) => ToolOutput::ok(clip(&s, TOOL_CLIP)),
        Err(e) => ToolOutput::ok(format!("Error: {e}")),
    }
}

struct GrepTool {
    ws: Arc<Workspace>,
}

#[derive(Deserialize, JsonSchema)]
struct GrepArgs {
    /// Regex to search for across the repository.
    pattern: String,
}

#[async_trait]
impl Tool for GrepTool {
    type Args = GrepArgs;
    fn name(&self) -> &'static str {
        "grep"
    }
    fn description(&self) -> &'static str {
        "Regex search across the repository."
    }
    async fn call(&self, args: Self::Args) -> std::result::Result<ToolOutput, ToolError> {
        Ok(ws_result(self.ws.grep(&args.pattern, 50).map(|hits| {
            if hits.is_empty() {
                "(no matches)".to_string()
            } else {
                hits.join("\n")
            }
        })))
    }
}

struct ReadFileTool {
    ws: Arc<Workspace>,
}

#[derive(Deserialize, JsonSchema)]
struct ReadFileArgs {
    /// Path relative to the repository root.
    path: String,
    /// 1-indexed inclusive start line.
    start: Option<usize>,
    /// 1-indexed inclusive end line.
    end: Option<usize>,
}

#[async_trait]
impl Tool for ReadFileTool {
    type Args = ReadFileArgs;
    fn name(&self) -> &'static str {
        "read_file"
    }
    fn description(&self) -> &'static str {
        "Read a file, optionally a 1-indexed inclusive line range."
    }
    async fn call(&self, args: Self::Args) -> std::result::Result<ToolOutput, ToolError> {
        Ok(ws_result(
            self.ws.read_file(&args.path, args.start, args.end),
        ))
    }
}

struct ListDirTool {
    ws: Arc<Workspace>,
}

#[derive(Deserialize, JsonSchema)]
struct ListDirArgs {
    /// Directory path relative to the repository root.
    path: String,
}

#[async_trait]
impl Tool for ListDirTool {
    type Args = ListDirArgs;
    fn name(&self) -> &'static str {
        "list_dir"
    }
    fn description(&self) -> &'static str {
        "List entries directly under a directory."
    }
    async fn call(&self, args: Self::Args) -> std::result::Result<ToolOutput, ToolError> {
        Ok(ws_result(
            self.ws.list_dir(&args.path).map(|e| e.join("\n")),
        ))
    }
}

/// Build the tool registry over a shallow workspace handle. `Workspace::from_dir`
/// shares the clone's root without owning the TempDir, so the caller's borrowed
/// `Workspace` keeps the directory alive for the run's duration.
fn build_registry(ws: &Workspace) -> ToolRegistry {
    let handle = Arc::new(Workspace::from_dir(ws.root()));
    ToolRegistry::new()
        .with(GrepTool {
            ws: Arc::clone(&handle),
        })
        .with(ReadFileTool {
            ws: Arc::clone(&handle),
        })
        .with(ListDirTool { ws: handle })
}

/// Review a PR agentically with a two-tier model split: a cheap `explore` model
/// drives the tool loop (grep/read_file/list_dir) to gather cross-file context,
/// then the main (quality) model writes the final review from what was gathered.
/// This keeps the bulk of the token volume on the cheap model while the findings
/// — where judgment matters — come from the stronger model. Usage is summed
/// across every call.
///
/// The loop, transport (with the timeout + 429 retry the old code lacked), and
/// history compaction all live in `agent-core`; this function frames the diff,
/// supplies the tools, and parses the synthesized JSON back into a [`Review`].
///
/// # Errors
/// On missing API key, OpenRouter failure, or if the synthesis model doesn't
/// return a parseable review.
pub async fn agentic_review(
    _client: &Client,
    cfg: &Config,
    meta: &PrMeta,
    diff: &str,
    omitted_note: Option<&str>,
    structural_context: Option<&str>,
    ws: &Workspace,
) -> Result<ReviewResult> {
    require(&cfg.openrouter_api_key, "OPENROUTER_API_KEY")?;

    // The diff is already packed to fit; this is a SAFETY clamp for a lone
    // oversized file that couldn't be packed under the budget.
    let truncated = diff.chars().count() > cfg.max_diff_chars;
    let clipped: String = diff.chars().take(cfg.max_diff_chars).collect();
    let omitted = omitted_note
        .map(|n| format!("\n[NOTE: {n}]"))
        .unwrap_or_default();
    let structural = structural_context
        .filter(|c| !c.trim().is_empty())
        .map(|c| format!("\n\n## Structural context\n{c}"))
        .unwrap_or_default();
    let user = format!(
        "Repository: {}\nPull request: #{}{}{omitted}{structural}\n\n--- BEGIN DIFF ---\n{clipped}\n--- END DIFF ---{}",
        meta.repo,
        meta.pr,
        meta.title.as_deref().map(|t| format!(" — {t}")).unwrap_or_default(),
        if truncated { "\n[diff truncated]" } else { "" },
    );

    let system_prompt = if cfg.extra_system_prompt.is_empty() {
        AGENT_SYSTEM_PROMPT.to_string()
    } else {
        format!("{AGENT_SYSTEM_PROMPT}\n{}", cfg.extra_system_prompt)
    };

    let chat = ChatClient::new(ProviderConfig {
        base_url: cfg.openrouter_base_url.clone(),
        api_key: cfg.openrouter_api_key.clone(),
        timeout: Duration::from_secs(cfg.openrouter_timeout_secs),
        retry: RetryPolicy {
            max_retries: cfg.openrouter_max_retries,
            ..RetryPolicy::default()
        },
        extra_headers: vec![
            ("HTTP-Referer".to_string(), cfg.http_referer.clone()),
            ("X-Title".to_string(), cfg.x_title.clone()),
        ],
        ..ProviderConfig::default()
    })
    .map_err(|e| anyhow::anyhow!(e))?;

    let policy = ModelPolicy {
        explore: cfg.openrouter_model_explore.clone(),
        synthesize: cfg.openrouter_model.clone(),
        max_turns: cfg.max_turns as u32,
        // pr-review-core bounded the run by turns, not tokens.
        stop_after_tokens: None,
        // The transport carries the per-request timeout; there was no
        // whole-run wall-clock cap in the hand-rolled loop.
        timeout_secs: None,
        max_tokens: cfg.openrouter_max_tokens,
        temperature: cfg.openrouter_temperature,
        max_history_chars: cfg.max_history_chars,
        initial_tool_choice: "auto".to_string(),
        continue_on_tool_error: true,
        // pr-review-core always runs the final tools-forbidden synthesis turn —
        // even single-model — because that turn is where the clean review JSON
        // is demanded.
        final_synthesis: true,
    };

    let backend = ChatBackend::new(chat, Arc::new(build_registry(ws)), policy);
    let outcome = backend
        .run(
            RunRequest {
                system_prompt,
                user_prompt: user,
                messages: Vec::new(),
                tool_scope: Vec::new(),
            },
            EventSink::none(),
        )
        .await
        .map_err(|e| anyhow::anyhow!(e))?;

    // Parse with this crate's own extractor so the error text stays stable.
    let content = outcome.content.as_deref().unwrap_or_default();
    let parsed = extract_json(content)
        .ok_or_else(|| anyhow::anyhow!("agent returned no JSON: {}", clip(content, 300)))?;
    let review: Review = serde_json::from_str(parsed).map_err(|e| {
        anyhow::anyhow!("could not parse agent review ({e}): {}", clip(parsed, 300))
    })?;

    let usage = (outcome.total_tokens > 0).then_some(Usage {
        prompt_tokens: None,
        completion_tokens: None,
        total_tokens: Some(outcome.total_tokens),
    });
    // `outcome.model` is agent-core's honest attribution — "main (explore:
    // cheap)" when tiered, collapsing to one name when explore == synthesize.
    Ok(ReviewResult {
        review,
        model: outcome.model,
        usage,
    })
}

#[cfg(test)]
mod tests {
    //! Characterisation tests: these pin the loop's behaviour **as it is today**,
    //! including two known defects (see `malformed_tool_args_*` and
    //! `unknown_tool_*`). They exist so the `agent-core` extraction can be proved
    //! behaviour-preserving. A test failing here after a refactor means the
    //! refactor changed semantics — decide deliberately, don't just update it.
    //!
    //! `Config::openrouter_base_url` is injectable, so the real loop runs
    //! end-to-end over HTTP against a queued mock. `Seq` is the seam the future
    //! `RecordingBackend` formalises.

    use super::*;
    // json!/Value moved out of the production imports when the loop was swapped;
    // the mock harness still needs them.
    use serde_json::{json, Value};
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    /// Replays one queued response per call and records every request body.
    #[derive(Clone)]
    struct Seq(Arc<SeqInner>);

    struct SeqInner {
        queued: Mutex<VecDeque<Value>>,
        seen: Mutex<Vec<Value>>,
    }

    impl Seq {
        fn new(responses: Vec<Value>) -> Self {
            Seq(Arc::new(SeqInner {
                queued: Mutex::new(responses.into()),
                seen: Mutex::new(Vec::new()),
            }))
        }
        fn requests(&self) -> Vec<Value> {
            self.0.seen.lock().unwrap().clone()
        }
        fn calls(&self) -> usize {
            self.0.seen.lock().unwrap().len()
        }
    }

    impl Respond for Seq {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            let body: Value = serde_json::from_slice(&req.body).expect("request body is JSON");
            self.0.seen.lock().unwrap().push(body);
            match self.0.queued.lock().unwrap().pop_front() {
                Some(b) => ResponseTemplate::new(200).set_body_json(b),
                None => ResponseTemplate::new(500).set_body_string("no queued response"),
            }
        }
    }

    const REVIEW_JSON: &str = r#"{"summary":"ok","recommendation":"APPROVE","findings":[]}"#;

    /// An assistant turn requesting tools: `(id, name, raw_json_arguments)`.
    fn tool_turn(calls: &[(&str, &str, &str)], tokens: u64) -> Value {
        let tcs: Vec<Value> = calls
            .iter()
            .map(|(id, name, args)| {
                json!({"id": id, "type": "function",
                       "function": {"name": name, "arguments": args}})
            })
            .collect();
        json!({
            "choices": [{"message": {"role": "assistant", "content": null, "tool_calls": tcs}}],
            "usage": {"total_tokens": tokens}
        })
    }

    /// An assistant turn with plain content and no tool calls.
    fn text_turn(content: &str, tokens: u64) -> Value {
        json!({
            "choices": [{"message": {"role": "assistant", "content": content}}],
            "usage": {"total_tokens": tokens}
        })
    }

    async fn server(responses: Vec<Value>) -> (MockServer, Seq) {
        let s = MockServer::start().await;
        let seq = Seq::new(responses);
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(seq.clone())
            .mount(&s)
            .await;
        (s, seq)
    }

    fn cfg_for(base_url: &str) -> Config {
        let mut c = Config::from_env();
        c.openrouter_api_key = "test-key".to_string();
        c.openrouter_base_url = base_url.to_string();
        c.openrouter_model = "main/model".to_string();
        c.openrouter_model_explore = "explore/model".to_string();
        c.extra_system_prompt = String::new(); // don't inherit a real one from env
        c.max_turns = 6;
        c.max_history_chars = 45_000;
        c.max_diff_chars = 200_000;
        // No retries in tests: keeps the transient-failure test to one attempt
        // (fast + deterministic) rather than the production default of 3.
        c.openrouter_max_retries = 0;
        c
    }

    fn workspace() -> (tempfile::TempDir, Workspace) {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("lib.rs"), "fn alpha() {}\nfn beta() {}\n").unwrap();
        std::fs::create_dir(d.path().join("sub")).unwrap();
        std::fs::write(
            d.path().join("sub").join("mod.rs"),
            "pub const X: u8 = 1;\n",
        )
        .unwrap();
        let ws = Workspace::from_dir(d.path());
        (d, ws)
    }

    fn meta() -> PrMeta {
        PrMeta {
            repo: "owner/repo".to_string(),
            pr: 7,
            title: Some("Test PR".to_string()),
            base_branch: None,
            head_sha: None,
            body: None,
        }
    }

    const DIFF: &str = "--- a/lib.rs\n+++ b/lib.rs\n@@ -1 +1,2 @@\n fn alpha() {}\n+fn beta() {}\n";

    /// Find the messages array sent on request `i`.
    fn messages(reqs: &[Value], i: usize) -> &Vec<Value> {
        reqs[i]["messages"].as_array().unwrap()
    }

    // ---- the two-phase contract -------------------------------------------

    #[tokio::test]
    async fn explore_phase_uses_cheap_model_with_tools_synthesis_uses_main_model_without() {
        let (srv, seq) = server(vec![
            tool_turn(&[("call_1", "grep", r#"{"pattern":"alpha"}"#)], 10),
            text_turn("done exploring", 10),
            text_turn(REVIEW_JSON, 25),
        ])
        .await;
        let cfg = cfg_for(&srv.uri());
        let (_d, ws) = workspace();

        let out = agentic_review(&Client::new(), &cfg, &meta(), DIFF, None, None, &ws)
            .await
            .expect("review succeeds");

        assert_eq!(seq.calls(), 3, "2 explore turns + 1 synthesis");
        let reqs = seq.requests();

        // Phase 1: cheap model, tools offered.
        assert_eq!(reqs[0]["model"], "explore/model");
        assert_eq!(reqs[0]["tool_choice"], "auto");
        assert!(reqs[0]["tools"].as_array().unwrap().len() == 3);
        assert_eq!(reqs[1]["model"], "explore/model");

        // Phase 2: strong model, tools forbidden.
        assert_eq!(reqs[2]["model"], "main/model");
        assert_eq!(reqs[2]["tool_choice"], "none");

        // Synthesis is prefixed by the hard stop-investigating instruction.
        let last = messages(&reqs, 2).last().unwrap();
        assert_eq!(last["role"], "user");
        assert!(last["content"]
            .as_str()
            .unwrap()
            .starts_with("Stop investigating now."));

        assert_eq!(out.review.recommendation, "APPROVE");
    }

    #[tokio::test]
    async fn loop_breaks_as_soon_as_a_turn_requests_no_tools() {
        let (srv, seq) = server(vec![
            text_turn("nothing to look up", 5),
            text_turn(REVIEW_JSON, 5),
        ])
        .await;
        let cfg = cfg_for(&srv.uri());
        let (_d, ws) = workspace();

        agentic_review(&Client::new(), &cfg, &meta(), DIFF, None, None, &ws)
            .await
            .unwrap();

        assert_eq!(
            seq.calls(),
            2,
            "one explore turn, then straight to synthesis"
        );
    }

    #[tokio::test]
    async fn max_turns_caps_exploration_and_still_synthesises() {
        // Model never stops asking for tools; the cap must end phase 1.
        let (srv, seq) = server(vec![
            tool_turn(&[("c1", "grep", r#"{"pattern":"a"}"#)], 1),
            tool_turn(&[("c2", "grep", r#"{"pattern":"b"}"#)], 1),
            text_turn(REVIEW_JSON, 1),
        ])
        .await;
        let mut cfg = cfg_for(&srv.uri());
        cfg.max_turns = 2;
        let (_d, ws) = workspace();

        agentic_review(&Client::new(), &cfg, &meta(), DIFF, None, None, &ws)
            .await
            .expect("hitting the turn cap is not an error");

        assert_eq!(
            seq.calls(),
            3,
            "exactly max_turns explore calls + 1 synthesis"
        );
    }

    #[tokio::test]
    async fn tool_results_are_appended_as_tool_role_with_matching_call_id() {
        let (srv, seq) = server(vec![
            tool_turn(&[("call_abc", "read_file", r#"{"path":"lib.rs"}"#)], 1),
            text_turn("ok", 1),
            text_turn(REVIEW_JSON, 1),
        ])
        .await;
        let cfg = cfg_for(&srv.uri());
        let (_d, ws) = workspace();

        agentic_review(&Client::new(), &cfg, &meta(), DIFF, None, None, &ws)
            .await
            .unwrap();

        let reqs = seq.requests();
        let second = messages(&reqs, 1);
        let tool_msg = second
            .iter()
            .find(|m| m["role"] == "tool")
            .expect("tool result carried into the next turn");
        assert_eq!(tool_msg["tool_call_id"], "call_abc");
        assert!(tool_msg["content"].as_str().unwrap().contains("fn alpha"));
    }

    #[tokio::test]
    async fn multiple_tool_calls_in_one_turn_all_execute() {
        let (srv, seq) = server(vec![
            tool_turn(
                &[
                    ("c1", "read_file", r#"{"path":"lib.rs"}"#),
                    ("c2", "list_dir", r#"{"path":"sub"}"#),
                ],
                1,
            ),
            text_turn("ok", 1),
            text_turn(REVIEW_JSON, 1),
        ])
        .await;
        let cfg = cfg_for(&srv.uri());
        let (_d, ws) = workspace();

        agentic_review(&Client::new(), &cfg, &meta(), DIFF, None, None, &ws)
            .await
            .unwrap();

        let reqs = seq.requests();
        let tool_msgs: Vec<_> = messages(&reqs, 1)
            .iter()
            .filter(|m| m["role"] == "tool")
            .collect();
        assert_eq!(tool_msgs.len(), 2);
        assert_eq!(tool_msgs[0]["tool_call_id"], "c1");
        assert_eq!(tool_msgs[1]["tool_call_id"], "c2");
    }

    // ---- tool-error handling: one defect fixed, one behaviour preserved ----

    #[tokio::test]
    async fn an_unknown_tool_name_surfaces_to_the_model_as_text() {
        // Behaviour PRESERVED, not a fix: both the old loop and agent-loop-core
        // hand the model a "unknown tool `X`" tool result rather than aborting,
        // so it keeps going. (Internally the new registry produces a typed
        // `NotFound` error, which is cleaner for the loop's own handling, but the
        // model sees the same kind of text — this test just pins that the run
        // doesn't crash and the miss is reported.)
        let (srv, seq) = server(vec![
            tool_turn(&[("c1", "delete_everything", "{}")], 1),
            text_turn("ok", 1),
            text_turn(REVIEW_JSON, 1),
        ])
        .await;
        let cfg = cfg_for(&srv.uri());
        let (_d, ws) = workspace();

        agentic_review(&Client::new(), &cfg, &meta(), DIFF, None, None, &ws)
            .await
            .expect("unknown tool does not fail the review");

        let reqs = seq.requests();
        let content = messages(&reqs, 1)
            .iter()
            .find(|m| m["role"] == "tool")
            .unwrap()["content"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(content.contains("unknown tool"), "got: {content}");
        assert!(content.contains("delete_everything"), "names it: {content}");
    }

    #[tokio::test]
    async fn malformed_tool_args_are_rejected_not_silently_defaulted() {
        // WAS a pinned defect: `unwrap_or(json!({}))` plus
        // `args["pattern"].as_str().unwrap_or("")` turned unparseable arguments
        // into a repo-wide empty-regex grep. agent-core parses once, strictly,
        // and tells the model the arguments were invalid.
        let (srv, seq) = server(vec![
            tool_turn(&[("c1", "grep", "{not valid json")], 1),
            text_turn("ok", 1),
            text_turn(REVIEW_JSON, 1),
        ])
        .await;
        let cfg = cfg_for(&srv.uri());
        let (_d, ws) = workspace();

        agentic_review(&Client::new(), &cfg, &meta(), DIFF, None, None, &ws)
            .await
            .unwrap();

        let reqs = seq.requests();
        let content = messages(&reqs, 1)
            .iter()
            .find(|m| m["role"] == "tool")
            .unwrap()["content"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(
            content.contains("invalid arguments"),
            "should reject, not run an empty-regex grep; got: {content}"
        );
        assert!(content.contains("grep"), "names the tool: {content}");
    }

    // ---- usage & model reporting -------------------------------------------

    #[tokio::test]
    async fn usage_sums_total_tokens_and_leaves_the_split_unknown() {
        let (srv, _seq) = server(vec![
            tool_turn(&[("c1", "grep", r#"{"pattern":"alpha"}"#)], 10),
            text_turn("ok", 20),
            text_turn(REVIEW_JSON, 30),
        ])
        .await;
        let cfg = cfg_for(&srv.uri());
        let (_d, ws) = workspace();

        let out = agentic_review(&Client::new(), &cfg, &meta(), DIFF, None, None, &ws)
            .await
            .unwrap();

        let usage = out.usage.expect("usage reported when tokens > 0");
        assert_eq!(usage.total_tokens, Some(60));
        // Known limitation: the agentic path cannot report the prompt/completion
        // split, which is what you'd need to reason about prompt caching.
        assert_eq!(usage.prompt_tokens, None);
        assert_eq!(usage.completion_tokens, None);
    }

    #[tokio::test]
    async fn model_reports_both_tiers_when_they_differ_and_collapses_when_equal() {
        let (srv, _s) = server(vec![text_turn("ok", 1), text_turn(REVIEW_JSON, 1)]).await;
        let cfg = cfg_for(&srv.uri());
        let (_d, ws) = workspace();
        let out = agentic_review(&Client::new(), &cfg, &meta(), DIFF, None, None, &ws)
            .await
            .unwrap();
        assert_eq!(out.model, "main/model (explore: explore/model)");

        let (srv2, _s2) = server(vec![text_turn("ok", 1), text_turn(REVIEW_JSON, 1)]).await;
        let mut same = cfg_for(&srv2.uri());
        same.openrouter_model_explore = same.openrouter_model.clone();
        let out2 = agentic_review(&Client::new(), &same, &meta(), DIFF, None, None, &ws)
            .await
            .unwrap();
        assert_eq!(out2.model, "main/model");
    }

    // ---- failure modes ------------------------------------------------------

    #[tokio::test]
    async fn synthesis_without_json_is_an_error() {
        let (srv, _s) = server(vec![
            text_turn("ok", 1),
            text_turn("I could not produce a review.", 1),
        ])
        .await;
        let cfg = cfg_for(&srv.uri());
        let (_d, ws) = workspace();

        let err = agentic_review(&Client::new(), &cfg, &meta(), DIFF, None, None, &ws)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("agent returned no JSON"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn missing_api_key_fails_before_any_network_call() {
        let (srv, seq) = server(vec![]).await;
        let mut cfg = cfg_for(&srv.uri());
        cfg.openrouter_api_key = String::new();
        let (_d, ws) = workspace();

        let err = agentic_review(&Client::new(), &cfg, &meta(), DIFF, None, None, &ws)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("OPENROUTER_API_KEY"), "got: {err}");
        assert_eq!(seq.calls(), 0);
    }

    #[tokio::test]
    async fn upstream_error_status_aborts_the_review() {
        let srv = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(429)
                    .set_body_json(json!({"error": {"message": "rate limited"}})),
            )
            .mount(&srv)
            .await;
        let cfg = cfg_for(&srv.uri());
        let (_d, ws) = workspace();

        // Retries are now real and configurable (agent-core's transport); the
        // old code had none. `cfg_for` sets max_retries = 0, so this asserts the
        // single-attempt behaviour: a 429 with retries off aborts, surfacing as
        // a rate-limit error rather than the old raw passthrough.
        let err = agentic_review(&Client::new(), &cfg, &meta(), DIFF, None, None, &ws)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("rate limited"), "got: {err}");
    }

    // ---- the repo tools -----------------------------------------------------
    //
    // `trim_history`, the tool dispatch table, and the schema/dispatch
    // drift-guard moved into agent-core with the loop and are tested there. What
    // remains this crate's own is the three repo tools; these cover their output
    // contract (clip size, error-as-text) directly.

    #[tokio::test]
    async fn read_file_tool_clips_oversized_output() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("big.txt"), "x".repeat(20_000)).unwrap();
        let tool = ReadFileTool {
            ws: Arc::new(Workspace::from_dir(d.path())),
        };
        let out = tool
            .call(ReadFileArgs {
                path: "big.txt".to_string(),
                start: None,
                end: None,
            })
            .await
            .unwrap();
        assert!(
            out.content.chars().count() <= 6_100,
            "clipped to ~6k, got {}",
            out.content.len()
        );
    }

    #[tokio::test]
    async fn read_file_tool_returns_a_sandbox_escape_as_error_text() {
        // The clone is a sandbox; a path escaping it must not read the host FS.
        // Preserved verbatim from the old loop: the error is returned as
        // "Error: ..." text the model can read, not a hard failure.
        let (_d, ws) = workspace();
        let tool = ReadFileTool {
            ws: Arc::new(Workspace::from_dir(ws.root())),
        };
        let out = tool
            .call(ReadFileArgs {
                path: "../../../etc/passwd".to_string(),
                start: None,
                end: None,
            })
            .await
            .unwrap();
        assert!(out.content.starts_with("Error:"), "got: {}", out.content);
    }

    #[test]
    fn the_registry_exposes_exactly_the_three_repo_tools() {
        let (_d, ws) = workspace();
        // Sorted by agent-core; drift between schema and dispatch is now
        // structurally impossible (same map), so this just pins the surface.
        assert_eq!(
            build_registry(&ws).names(),
            vec!["grep", "list_dir", "read_file"]
        );
    }
}
