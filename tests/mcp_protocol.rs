//! The MCP server, driven over its real transport.
//!
//! Every other test of [`pr_review_core::mcp`] calls its functions directly, which
//! covers the decisions — which tool, which backend, what refusal — and cannot
//! cover the thing those decisions travel over. A renamed JSON field, a response
//! shape no client sends, a lost flush: all of those keep the unit tests green and
//! break every real client.
//!
//! So this spawns the actual binary and speaks newline-delimited JSON-RPC to it,
//! the way a host would. Two bugs found by hand during development — a client that
//! hung when it sent a request mid-sampling, and a parse error that was written but
//! not flushed — were both invisible to the unit tests and would have been caught
//! here.
//!
//! **No key, no network, no billed call.** `OPENROUTER_API_KEY` is set to the empty
//! string rather than unset: the binary loads a `.env` through `dotenvy`, which does
//! not override a variable that is already present, so unsetting it lets a developer's
//! local `.env` supply a real key and turn this suite into a live, billed review.
//! That is not hypothetical — it happened while writing this file.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};

use serde_json::{json, Value};

/// How long a test waits for the server before calling it a hang.
///
/// Generous for a local process doing no I/O of its own — every legitimate answer
/// here is immediate — and short enough that a regression fails the run rather
/// than occupying a CI worker until something else kills it.
const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// A fake MCP host: writes requests, reads messages, and can answer a sampling
/// request the way a real client's model would.
struct Host {
    child: Child,
    stdin: ChildStdin,
    /// Lines the reader thread has pulled off the server's stdout.
    ///
    /// A channel rather than reading inline, because the bug this suite exists to
    /// catch is a server that never answers — and a blocking `read_line` cannot be
    /// abandoned. The first cut wrapped one in `thread::scope` with a timeout,
    /// which does not work: the scope JOINS the reader before returning, so a
    /// timed-out read still waited forever on the thread it had given up on.
    /// Found by reintroducing the bug and watching the run wedge anyway.
    ///
    /// One detached reader for the host's lifetime has no such problem: it ends
    /// when `Drop` kills the child and closes stdout.
    lines: std::sync::mpsc::Receiver<String>,
}

impl Host {
    /// Start the server with `capabilities`, and no model key of its own.
    fn start(capabilities: Value) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_kaniscope"))
            .arg("mcp")
            // Empty, not absent — see the module docs. This is what keeps the
            // suite offline.
            .env("OPENROUTER_API_KEY", "")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("the kaniscope binary is built for integration tests");
        // Taken, not cloned: `ChildStdin`/`ChildStdout` are not cloneable, and
        // owning them here is what lets `Drop` still reach `child` to reap it.
        let stdin = child.stdin.take().expect("piped");
        let mut stdout = BufReader::new(child.stdout.take().expect("piped"));
        let (tx, lines) = std::sync::mpsc::channel();
        std::thread::spawn(move || loop {
            let mut line = String::new();
            match stdout.read_line(&mut line) {
                Ok(0) | Err(_) => return, // stdout closed: the child is gone
                Ok(_) => {
                    if tx.send(line).is_err() {
                        return; // the test finished
                    }
                }
            }
        });
        let mut host = Self {
            child,
            stdin,
            lines,
        };
        host.send(json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": { "capabilities": capabilities },
        }));
        host.read();
        host
    }

    fn send(&mut self, message: Value) {
        writeln!(self.stdin, "{message}").expect("the server is still accepting input");
        self.stdin.flush().expect("flush");
    }

    /// Read one message, or fail after [`READ_TIMEOUT`].
    ///
    /// The timeout is the point, not a safety net. The bug this suite exists to
    /// catch — a message the server consumes without answering — makes a client
    /// wait forever, so a plain blocking read turns that regression into a wedged
    /// CI job rather than a red test. Verified by reintroducing the bug: without
    /// this, the run hangs; with it, the test fails in seconds and names what it
    /// was waiting for.
    fn read(&mut self) -> Value {
        let line = self.lines.recv_timeout(READ_TIMEOUT).unwrap_or_else(|_| {
            panic!(
                "the server sent nothing within {READ_TIMEOUT:?} — a message was \
                 consumed without a reply, which is exactly the hang a real client \
                 would suffer"
            )
        });
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("not JSON-RPC: {e}: {line}"))
    }

    fn call(&mut self, id: u64, name: &str, arguments: Value) {
        self.send(json!({
            "jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": { "name": name, "arguments": arguments },
        }));
    }

    /// The JSON document a tool result carries, parsed.
    fn tool_json(result: &Value) -> Value {
        let text = result["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("no text content in {result}"));
        serde_json::from_str(text).unwrap_or_else(|e| panic!("tool text was not JSON: {e}: {text}"))
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        // Closing stdin is how `serve` is meant to end; kill only if it will not.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A git repository with one committed file and one uncommitted change, so
/// `review_local` has something real to review.
fn repo_with_a_change() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let run = |args: &[&str]| {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir.path())
            .output()
            .expect("git runs");
        assert!(out.status.success(), "git {args:?} failed: {out:?}");
    };
    run(&["init", "-q", "."]);
    run(&["config", "user.email", "t@example.com"]);
    run(&["config", "user.name", "t"]);
    std::fs::write(
        dir.path().join("a.rs"),
        "fn total(v: &[u32]) -> u32 { v.iter().sum() }\n",
    )
    .expect("write");
    run(&["add", "a.rs"]);
    run(&["commit", "-qm", "init"]);
    std::fs::write(
        dir.path().join("a.rs"),
        "fn total(v: &[u32]) -> u32 {\n    let mut t = 0;\n    for i in v { t += i; }\n    t\n}\n",
    )
    .expect("write");
    dir
}

/// The handshake and the tool list, over the wire.
///
/// `initialize` is the first thing any client sends and the last thing a unit
/// test can vouch for: it asserts the shape a host parses.
#[test]
fn the_handshake_and_tool_list_are_well_formed() {
    let mut host = Host::start(json!({}));

    host.send(json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }));
    let listed = host.read();

    let tools = listed["result"]["tools"]
        .as_array()
        .expect("tools/list returns an array");
    assert_eq!(tools.len(), 7, "{tools:#?}");
    for tool in tools {
        assert!(tool["name"].is_string(), "every tool is named: {tool}");
        assert!(
            tool["description"].as_str().is_some_and(|d| d.len() > 40),
            "a model picks tools by description; {} has none worth reading",
            tool["name"]
        );
        assert_eq!(
            tool["inputSchema"]["type"], "object",
            "{} must publish an object input schema",
            tool["name"]
        );
    }
}

/// A notification carries no id and must get NO response.
///
/// Answering one is a protocol violation some hosts treat as fatal, and nothing
/// in the unit tests looks at the wire, so only this can say it does not happen.
#[test]
fn a_notification_is_not_answered() {
    let mut host = Host::start(json!({}));

    host.send(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
    // Then a request whose answer proves the server is alive and in step: if the
    // notification HAD been answered, this read would return that answer instead.
    host.send(json!({ "jsonrpc": "2.0", "id": 7, "method": "tools/list" }));

    let next = host.read();
    assert_eq!(next["id"], 7, "the notification was answered: {next}");
}

/// Malformed input is answered and FLUSHED, not left in a buffer.
///
/// The flush question is invisible to a unit test and fatal over a pipe: a client
/// that waits for this error before sending anything else would hang forever.
#[test]
fn malformed_input_is_answered_rather_than_swallowed() {
    let mut host = Host::start(json!({}));

    host.send(json!("this is valid JSON but not a JSON-RPC object"));
    host.stdin.write_all(b"{ not json at all\n").expect("write");
    host.stdin.flush().expect("flush");

    host.send(json!({ "jsonrpc": "2.0", "id": 9, "method": "tools/list" }));

    // Parse errors may arrive first; the server must still reach our request.
    let mut seen_parse_error = false;
    loop {
        let message = host.read();
        if message["id"] == json!(9) {
            break;
        }
        if message["error"]["code"] == json!(-32700) {
            seen_parse_error = true;
        }
    }
    assert!(seen_parse_error, "the unparseable line was never reported");
}

/// `get_rules` answers with no model key and no sampling — that is its whole
/// point, and an earlier cut of the backend gating refused it.
#[test]
fn get_rules_answers_with_no_reviewer_available() {
    let dir = repo_with_a_change();
    let mut host = Host::start(json!({}));

    host.call(
        3,
        "get_rules",
        json!({ "repoRoot": dir.path().to_string_lossy() }),
    );
    let result = host.read();

    assert!(
        result["result"]["isError"] != json!(true),
        "get_rules needs no reviewer: {result}"
    );
    let rules = Host::tool_json(&result);
    assert_eq!(rules["scope"]["kind"], "local");
    assert!(
        rules["injectedRules"]
            .as_str()
            .is_some_and(|r| !r.is_empty()),
        "the injected rules are the answer this tool exists to give"
    );
    assert!(
        !serde_json::to_string(&rules).unwrap().contains("sk-"),
        "no credential may appear in a rules document"
    );
}

/// With neither a key nor a sampling-capable client, a review refuses in words
/// naming both ways out — rather than reporting a missing key to someone who
/// deliberately did not set one.
#[test]
fn a_review_refuses_when_there_is_no_reviewer_at_all() {
    let dir = repo_with_a_change();
    let mut host = Host::start(json!({})); // no sampling capability

    host.call(
        4,
        "review_local",
        json!({ "repoRoot": dir.path().to_string_lossy(), "base": "HEAD" }),
    );
    let result = host.read();

    assert_eq!(result["result"]["isError"], json!(true), "{result}");
    let text = result["result"]["content"][0]["text"]
        .as_str()
        .expect("text");
    assert!(text.contains("OPENROUTER_API_KEY"), "{text}");
    assert!(text.contains("sampling"), "{text}");
}

/// The whole no-key path, end to end: the server asks the HOST for a completion
/// and turns the answer into a review.
///
/// This is the one that cannot be faked from inside the crate — the request has
/// to leave the process and the answer has to come back in.
#[test]
fn a_sampling_client_gets_a_review_with_no_api_key() {
    let dir = repo_with_a_change();
    let mut host = Host::start(json!({ "sampling": {} }));

    host.call(
        5,
        "review_local",
        json!({
            "repoRoot": dir.path().to_string_lossy(),
            "base": "HEAD",
            "intent": "replace the fold with an accumulator",
        }),
    );

    // The server should now be asking US to run the model.
    let ask = host.read();
    assert_eq!(ask["method"], "sampling/createMessage", "{ask}");
    let params = &ask["params"];
    assert!(
        params["systemPrompt"]
            .as_str()
            .is_some_and(|p| p.contains("severity")),
        "the orchestrator's calibration rules must reach the sampled prompt"
    );
    let user = params["messages"][0]["content"]["text"]
        .as_str()
        .expect("a user message");
    assert!(
        user.contains("t += i"),
        "the diff must reach the prompt: {user}"
    );
    assert!(
        user.contains("replace the fold with an accumulator"),
        "the stated intent must reach the prompt"
    );

    // Answer as a host model would.
    let review = json!({
        "summary": "an accumulator replaced a fold",
        "recommendation": "APPROVE WITH CHANGES",
        "findings": [{
            "severity": "MEDIUM", "file": "a.rs", "line": 3,
            "body": "`t` can overflow on a long list. Fix: use checked_add.",
            "confidence": 90, "suggestion": null,
        }],
    });
    host.send(json!({
        "jsonrpc": "2.0", "id": ask["id"],
        "result": {
            "role": "assistant",
            "content": { "type": "text", "text": review.to_string() },
            "model": "the-host-model", "stopReason": "endTurn",
        },
    }));

    // The self-critique pass runs on the same backend, so a sampled review costs
    // TWO round trips on the host's model, not one. Asserted rather than tolerated:
    // it is a real cost characteristic of this path, a caller paying for the host's
    // tokens should know about it, and turning `SELF_CRITIQUE` off must show up here
    // as a changed number rather than silently.
    let critique = host.read();
    assert_eq!(
        critique["method"], "sampling/createMessage",
        "the critique pass must also sample: {critique}"
    );
    assert!(
        critique["params"]["systemPrompt"]
            .as_str()
            .is_some_and(|p| p.contains("skeptical senior reviewer")),
        "the second round trip should be the critique, not a repeat of the review"
    );
    host.send(json!({
        "jsonrpc": "2.0", "id": critique["id"],
        "result": {
            "role": "assistant",
            // Keep the finding: this pass prunes, and dropping it here would make
            // the assertions below test the critique rather than the review.
            "content": { "type": "text", "text": json!([{
                "severity": "MEDIUM", "file": "a.rs", "line": 3,
                "body": "`t` can overflow on a long list. Fix: use checked_add.",
                "confidence": 90, "suggestion": null,
            }]).to_string() },
            "model": "the-host-model", "stopReason": "endTurn",
        },
    }));

    let result = host.read();
    assert_eq!(
        result["id"],
        json!(5),
        "the original tool call must answer: {result}"
    );
    let out = Host::tool_json(&result);
    assert_eq!(out["recommendation"], "APPROVE WITH CHANGES");
    assert_eq!(out["findings"], json!(1));
    assert!(
        out["model"]
            .as_str()
            .is_some_and(|m| m.contains("sampling")),
        "the model must be named as the client's, not invented: {}",
        out["model"]
    );
}

/// A request arriving MID-sampling is answered, not swallowed.
///
/// The first version dropped every message with a non-matching id, so a client
/// that sent anything during a review waited forever for a reply that was never
/// coming. Found in review; this is the test that would have found it first.
#[test]
fn a_request_during_sampling_is_answered_rather_than_dropped() {
    let dir = repo_with_a_change();
    let mut host = Host::start(json!({ "sampling": {} }));

    host.call(
        6,
        "review_local",
        json!({ "repoRoot": dir.path().to_string_lossy(), "base": "HEAD" }),
    );
    let ask = host.read();
    assert_eq!(ask["method"], "sampling/createMessage");

    // The thing that used to hang.
    host.send(json!({ "jsonrpc": "2.0", "id": 42, "method": "tools/list" }));
    let interjection = host.read();
    assert_eq!(
        interjection["id"],
        json!(42),
        "the stray request was dropped: {interjection}"
    );
    assert!(
        interjection["error"].is_object(),
        "a request mid-round-trip should be refused, not served: {interjection}"
    );

    // And the review still completes afterwards.
    let review = json!({ "summary": "s", "recommendation": "APPROVE", "findings": [] });
    host.send(json!({
        "jsonrpc": "2.0", "id": ask["id"],
        "result": {
            "role": "assistant",
            "content": { "type": "text", "text": review.to_string() },
            "model": "m", "stopReason": "endTurn",
        },
    }));
    // Zero findings, so the critique pass has nothing to review and does not run —
    // which is itself worth pinning: the second round trip is conditional on the
    // review producing something to critique.
    let result = host.read();
    assert_eq!(
        result["id"],
        json!(6),
        "the original call must still return: {result}"
    );
}
