//! Agentic reviewer: an OpenRouter tool-calling loop. The model is given the PR
//! diff plus read-only tools (`read_file`, `list_dir`, `grep`) over a clone of
//! the repo, investigates cross-file context on its own, then returns the same
//! structured review the diff-only path produces.

use anyhow::Result;
use reqwest::Client;
use serde_json::{json, Value};

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

/// Tool schemas advertised to the model (OpenAI function-calling format).
fn tool_defs() -> Value {
    json!([
        {"type":"function","function":{
            "name":"grep","description":"Regex search across the repository.",
            "parameters":{"type":"object","properties":{"pattern":{"type":"string"}},"required":["pattern"]}}},
        {"type":"function","function":{
            "name":"read_file","description":"Read a file, optionally a 1-indexed inclusive line range.",
            "parameters":{"type":"object","properties":{
                "path":{"type":"string"},"start":{"type":"integer"},"end":{"type":"integer"}},"required":["path"]}}},
        {"type":"function","function":{
            "name":"list_dir","description":"List entries directly under a directory.",
            "parameters":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}}}
    ])
}

/// Run a single tool call against the workspace, returning a text result the
/// model can read (errors are returned as text so the loop continues).
fn run_tool(ws: &Workspace, name: &str, args: &str) -> String {
    let args: Value = serde_json::from_str(args).unwrap_or_else(|_| json!({}));
    let result: Result<String> = match name {
        "grep" => {
            let pat = args["pattern"].as_str().unwrap_or("");
            ws.grep(pat, 50).map(|hits| {
                if hits.is_empty() {
                    "(no matches)".to_string()
                } else {
                    hits.join("\n")
                }
            })
        }
        "read_file" => {
            let path = args["path"].as_str().unwrap_or("");
            let start = args["start"].as_u64().map(|v| v as usize);
            let end = args["end"].as_u64().map(|v| v as usize);
            ws.read_file(path, start, end)
        }
        "list_dir" => {
            let path = args["path"].as_str().unwrap_or("");
            ws.list_dir(path).map(|e| e.join("\n"))
        }
        other => Ok(format!("unknown tool: {other}")),
    };
    match result {
        Ok(s) => clip(&s, 6_000),
        Err(e) => format!("Error: {e}"),
    }
}

/// Cap the total size of tool-result content carried in the conversation: keep
/// the newest results in full up to `budget_chars`, elide older ones. Bounds the
/// per-request token count so context can't snowball or overflow the model window.
fn trim_history(messages: &mut [Value], budget_chars: usize) {
    let mut used = 0usize;
    for m in messages.iter_mut().rev() {
        if m["role"].as_str() != Some("tool") {
            continue; // only the file dumps in tool results are worth eliding
        }
        let len = m["content"]
            .as_str()
            .map(|c| c.chars().count())
            .unwrap_or(0);
        if used + len > budget_chars {
            m["content"] = json!("[earlier tool result elided to save context]");
        } else {
            used += len;
        }
    }
}

/// One OpenRouter chat-completions call. Returns the assistant message and the
/// call's `total_tokens` (0 if the field is absent).
async fn chat(
    client: &Client,
    cfg: &Config,
    model: &str,
    messages: &[Value],
    tools: &Value,
    tool_choice: &str,
) -> Result<(Value, u32)> {
    let req = json!({
        "model": model,
        "max_tokens": cfg.openrouter_max_tokens,
        "temperature": cfg.openrouter_temperature,
        "messages": messages,
        "tools": tools,
        "tool_choice": tool_choice,
    });

    let res = client
        .post(format!("{}/chat/completions", cfg.openrouter_base_url))
        .bearer_auth(&cfg.openrouter_api_key)
        .header("HTTP-Referer", &cfg.http_referer)
        .header("X-Title", &cfg.x_title)
        .json(&req)
        .send()
        .await?;

    let status = res.status();
    let text = res.text().await?;
    let data: Value = serde_json::from_str(&text).map_err(|e| {
        anyhow::anyhow!("OpenRouter {status}: non-JSON ({e}): {}", clip(&text, 300))
    })?;
    if !status.is_success() || data.get("error").is_some() {
        let msg = data["error"]["message"]
            .as_str()
            .unwrap_or(&clip(&text, 400))
            .to_string();
        anyhow::bail!("OpenRouter {status}: {msg}");
    }

    let tokens = data["usage"]["total_tokens"].as_u64().unwrap_or(0) as u32;
    let message = data["choices"][0]["message"].clone();
    Ok((message, tokens))
}

/// Review a PR agentically with a two-tier model split: a cheap `explore` model
/// drives the tool loop (grep/read_file/list_dir) to gather cross-file context,
/// then the main (quality) model writes the final review from what was gathered.
/// This keeps the bulk of the token volume on the cheap model while the findings
/// — where judgment matters — come from the stronger model. Usage is summed
/// across every call.
///
/// # Errors
/// On missing API key, OpenRouter failure, or if the synthesis model doesn't
/// return a parseable review.
pub async fn agentic_review(
    client: &Client,
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
    let user =
        format!(
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

    let mut messages: Vec<Value> = vec![
        json!({"role":"system","content": system_prompt}),
        json!({"role":"user","content": user}),
    ];
    let tools = tool_defs();
    let mut total_tokens = 0u32;

    // Phase 1 — exploration on the cheap model. Loop until it stops asking for
    // tools (it's gathered enough) or we hit the turn cap.
    for turn in 0..cfg.max_turns {
        trim_history(&mut messages, cfg.max_history_chars);

        let (message, tokens) = chat(
            client,
            cfg,
            &cfg.openrouter_model_explore,
            &messages,
            &tools,
            "auto",
        )
        .await?;
        total_tokens += tokens;
        messages.push(message.clone());

        let tool_calls = message["tool_calls"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if tool_calls.is_empty() {
            break; // explorer is done gathering — synthesize below
        }
        for tc in &tool_calls {
            let id = tc["id"].as_str().unwrap_or("");
            let name = tc["function"]["name"].as_str().unwrap_or("");
            let args = tc["function"]["arguments"].as_str().unwrap_or("{}");
            tracing::info!(
                "agent[{}#{}] turn {turn} explore: {name} {}",
                meta.repo,
                meta.pr,
                clip(args, 160)
            );
            let result = run_tool(ws, name, args);
            messages.push(json!({"role":"tool","tool_call_id": id, "content": result}));
        }
    }

    // Phase 2 — synthesis on the main (quality) model. Forbid tools and demand
    // the review JSON, using only the context the explorer already gathered.
    messages.push(json!({
        "role": "user",
        "content": "Stop investigating now. Using only what you've already gathered, output ONLY the final review JSON object — no prose, no tool calls."
    }));
    trim_history(&mut messages, cfg.max_history_chars);

    let (message, tokens) = chat(
        client,
        cfg,
        &cfg.openrouter_model,
        &messages,
        &tools,
        "none",
    )
    .await?;
    total_tokens += tokens;

    let content = message["content"].as_str().unwrap_or_default();
    let parsed = extract_json(content)
        .ok_or_else(|| anyhow::anyhow!("agent returned no JSON: {}", clip(content, 300)))?;
    let review: Review = serde_json::from_str(parsed).map_err(|e| {
        anyhow::anyhow!("could not parse agent review ({e}): {}", clip(parsed, 300))
    })?;

    let usage = (total_tokens > 0).then_some(Usage {
        prompt_tokens: None,
        completion_tokens: None,
        total_tokens: Some(total_tokens),
    });
    // Report both tiers so token cost attribution is honest — the summed usage
    // spans the explore and synthesis models. Collapse to one name when tiering
    // is disabled (explore == synthesis).
    let model = if cfg.openrouter_model_explore == cfg.openrouter_model {
        cfg.openrouter_model.clone()
    } else {
        format!(
            "{} (explore: {})",
            cfg.openrouter_model, cfg.openrouter_model_explore
        )
    };
    Ok(ReviewResult {
        review,
        model,
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

    // ---- known defects, pinned deliberately --------------------------------

    #[tokio::test]
    async fn unknown_tool_is_reported_as_text_not_an_error() {
        // DEFECT (pinned): an unroutable tool name yields a normal-looking tool
        // result. The loop cannot distinguish "tool doesn't exist" from a real
        // answer, and burns a turn either way.
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
        let tool_msg = messages(&reqs, 1)
            .iter()
            .find(|m| m["role"] == "tool")
            .unwrap();
        assert_eq!(tool_msg["content"], "unknown tool: delete_everything");
    }

    #[tokio::test]
    async fn malformed_tool_args_silently_become_an_empty_object() {
        // DEFECT (pinned): `serde_json::from_str(args).unwrap_or(json!({}))` plus
        // `args["pattern"].as_str().unwrap_or("")` turns unparseable arguments
        // into a repo-wide empty-regex grep instead of an error. The typed-args
        // work in step 3 should change this — and this test should then be
        // updated deliberately, not silently.
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
            !content.starts_with("Error:"),
            "today this degrades silently rather than erroring; got: {content}"
        );
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

        // Pinned: there is no retry today — a single 429 discards the whole run.
        // Step 2 (provider.rs) changes this; update this test when it does.
        let err = agentic_review(&Client::new(), &cfg, &meta(), DIFF, None, None, &ws)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("rate limited"), "got: {err}");
    }

    // ---- private helpers ----------------------------------------------------

    #[test]
    fn trim_history_elides_only_older_tool_results() {
        let mut msgs = vec![
            json!({"role":"system","content":"S"}),
            json!({"role":"user","content":"U"}),
            json!({"role":"tool","tool_call_id":"1","content":"old-and-long"}),
            json!({"role":"tool","tool_call_id":"2","content":"newest"}),
        ];
        trim_history(&mut msgs, 8); // fits "newest" (6) but not the older one

        assert_eq!(msgs[0]["content"], "S", "system is never touched");
        assert_eq!(msgs[1]["content"], "U", "user is never touched");
        assert_eq!(
            msgs[2]["content"],
            "[earlier tool result elided to save context]"
        );
        assert_eq!(msgs[3]["content"], "newest", "newest result kept in full");
    }

    #[test]
    fn trim_history_keeps_everything_under_budget() {
        let mut msgs = vec![
            json!({"role":"tool","tool_call_id":"1","content":"a"}),
            json!({"role":"tool","tool_call_id":"2","content":"b"}),
        ];
        trim_history(&mut msgs, 10_000);
        assert_eq!(msgs[0]["content"], "a");
        assert_eq!(msgs[1]["content"], "b");
    }

    #[test]
    fn run_tool_clips_oversized_results() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("big.txt"), "x".repeat(20_000)).unwrap();
        let ws = Workspace::from_dir(d.path());

        let out = run_tool(&ws, "read_file", r#"{"path":"big.txt"}"#);
        assert!(
            out.chars().count() <= 6_100,
            "clipped to ~6k, got {}",
            out.len()
        );
    }

    #[test]
    fn run_tool_surfaces_sandbox_escape_as_error_text() {
        let (_d, ws) = workspace();
        let out = run_tool(&ws, "read_file", r#"{"path":"../../../etc/passwd"}"#);
        assert!(out.starts_with("Error:"), "got: {out}");
    }

    #[test]
    fn tool_defs_match_the_names_run_tool_dispatches() {
        // Guards the schema/dispatch drift called out in the review: today these
        // are two independent sources of truth.
        let defs = tool_defs();
        let names: Vec<&str> = defs
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["function"]["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["grep", "read_file", "list_dir"]);

        let (_d, ws) = workspace();
        for n in names {
            let out = run_tool(&ws, n, r#"{"path":".","pattern":"alpha"}"#);
            assert!(
                !out.starts_with("unknown tool:"),
                "{n} is advertised but not dispatched"
            );
        }
    }
}
