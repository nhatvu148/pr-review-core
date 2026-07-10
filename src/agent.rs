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
    let user =
        format!(
        "Repository: {}\nPull request: #{}{}{omitted}\n\n--- BEGIN DIFF ---\n{clipped}\n--- END DIFF ---{}",
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
