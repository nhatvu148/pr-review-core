"use strict";

// The programmatic API: what a bot written in TypeScript actually calls.
//
// Under it is a subprocess, and that is the point rather than a compromise. The
// engine's entry point takes five scalars, reads the rest of its configuration
// from the environment, and returns one JSON-serializable struct after minutes of
// network and git work. Binding that across an FFI boundary would buy nothing a
// pipe does not already give, and would cost an ABI-pinned build matrix, a
// tokio<->libuv bridge, and a second declaration of every wire type that can
// drift from the first. The types below are generated from the binary's own
// `--schema`, so they cannot.

const { spawn } = require("node:child_process");
const { StringDecoder } = require("node:string_decoder");
const { binaryPath } = require("./binary.js");

/** Flags that take a value, mapped from the camelCase option name. */
const VALUE_FLAGS = {
  provider: "--provider",
  repo: "--repo",
  pr: "--pr",
  base: "--base",
  repoRoot: "--repo-root",
  label: "--label",
  jsonOut: "--json-out",
};

/** Boolean flags, mapped from the camelCase option name. */
const BOOL_FLAGS = {
  local: "--local",
  dryRun: "--dry-run",
};

/**
 * Turn an options object into an argv for the binary.
 *
 * Unknown keys are rejected rather than dropped. A silently ignored `dry_run`
 * (snake_case, the natural typo when porting from the Python client) would post a
 * live review to someone's PR while the caller believed it was a dry run — the
 * one mistake in this API with consequences that cannot be undone.
 */
function buildArgs(options) {
  const known = new Set([...Object.keys(VALUE_FLAGS), ...Object.keys(BOOL_FLAGS), "env", "inheritEnv", "timeoutMs", "onLog", "binary", "diff"]);
  for (const key of Object.keys(options)) {
    if (!known.has(key)) {
      throw new TypeError(`kaniscope: unknown option ${JSON.stringify(key)}`);
    }
  }

  const args = ["--json"];
  for (const [key, flag] of Object.entries(VALUE_FLAGS)) {
    const value = options[key];
    if (value === undefined || value === null) continue;
    args.push(flag, String(value));
  }
  for (const [key, flag] of Object.entries(BOOL_FLAGS)) {
    if (options[key]) args.push(flag);
  }
  return args;
}

/**
 * Run the binary and return its stdout, stderr and exit code.
 *
 * Accumulates rather than using a buffered helper on purpose: `execFile` caps
 * stdout at 1 MB by default, and a review of a large PR — every finding, every
 * rendered inline comment body — goes past that. The failure mode there is a
 * truncated JSON parse error on exactly the big PRs you most wanted reviewed.
 */
function run(bin, args, options) {
  return new Promise((resolve, reject) => {
    // stdin is `pipe` only when a diff was supplied. The CLI's most general door
    // is `git diff | kaniscope --local`, and with stdin hard-wired to "ignore"
    // that mode was unreachable through this API: the child saw EOF immediately
    // and bailed with "empty diff". Inheriting the parent's stdin instead — what
    // the Python client used to do by omission — is worse, because a server has
    // no diff on stdin and would block or read junk. Explicit both ways.
    const wantsStdin = options.diff !== undefined && options.diff !== null;
    const child = spawn(bin, args, {
      env: options.inheritEnv === false ? { ...options.env } : { ...process.env, ...options.env },
      stdio: [wantsStdin ? "pipe" : "ignore", "pipe", "pipe"],
    });

    if (wantsStdin) {
      // A child that exits before reading it all (bad flags, missing key) makes
      // this write fail with EPIPE. That is the child's error to report, not a
      // crash in the parent, so swallow it and let the exit code speak.
      child.stdin.on("error", () => {});
      child.stdin.end(options.diff);
    }

    const stdout = [];
    const stderr = [];
    child.stdout.on("data", (c) => stdout.push(c));

    // The engine logs progress here (tool calls, warnings). A caller that wants
    // to surface them live gets them line by line; otherwise they are still
    // kept, because they are what makes a failure diagnosable.
    //
    // `carry` holds the incomplete tail of the last chunk. A `data` event is a
    // chunk of a byte stream, NOT a line: a log line straddling two events was
    // being delivered as two fragments, which breaks the "called once per line"
    // contract in index.d.ts and, worse, hands a caller half a message to log.
    // The StringDecoder is the same bug one level down — it holds a partial
    // UTF-8 sequence split across chunks instead of turning it into U+FFFD.
    const decoder = new StringDecoder("utf8");
    let carry = "";
    const emitLines = (text, flush) => {
      if (!options.onLog) return;
      carry += text;
      const lines = carry.split("\n");
      carry = lines.pop(); // the incomplete tail; more bytes may still be coming
      for (const line of lines) if (line.trim()) options.onLog(line);
      // At EOF nothing more is coming, so the tail is a whole line after all —
      // which is the common case, since the last log line has no trailing \n.
      if (flush && carry.trim()) {
        options.onLog(carry);
        carry = "";
      }
    };

    child.stderr.on("data", (c) => {
      stderr.push(c);
      emitLines(decoder.write(c), false);
    });
    child.stderr.on("end", () => emitLines(decoder.end(), true));

    let timer;
    if (options.timeoutMs) {
      timer = setTimeout(() => {
        child.kill("SIGKILL");
        // Carry the same diagnostics as every other rejection from this file.
        // A review that ran the full timeout usually printed the reason it was
        // stuck, and dropping stderr here threw away the only evidence of it —
        // on the one failure path where there is no exit code to look at either.
        // `timedOut` because "was it killed by the timeout, or did it die on its
        // own?" is otherwise only answerable by matching on the message string.
        const err = new Error(`kaniscope: timed out after ${options.timeoutMs}ms`);
        err.timedOut = true;
        err.exitCode = null;
        err.signal = "SIGKILL";
        err.stderr = Buffer.concat(stderr).toString();
        reject(err);
      }, options.timeoutMs);
    }

    child.on("error", (err) => {
      clearTimeout(timer);
      reject(new Error(`kaniscope: could not run ${bin}: ${err.message}`));
    });
    child.on("close", (code, signal) => {
      clearTimeout(timer);
      resolve({
        code,
        signal,
        stdout: Buffer.concat(stdout).toString(),
        stderr: Buffer.concat(stderr).toString(),
      });
    });
  });
}

/** Trim stderr for an error message — the tail is where the cause is. */
function tail(text, lines = 20) {
  const all = text.trimEnd().split("\n");
  return all.slice(-lines).join("\n");
}

/**
 * Review a pull request, or a local diff with `{ local: true }`.
 *
 * Configuration beyond these flags — the API key, the model, globs, confidence
 * floors, bot identity — comes from the environment, exactly as it does for a
 * Rust consumer. Pass overrides in `env`; they are merged over `process.env`.
 *
 * @param {import("./index").ReviewOptions} options
 * @returns {Promise<import("./types").RunReviewOutput>}
 */
async function review(options = {}) {
  const args = buildArgs(options);
  const bin = options.binary || binaryPath();
  const result = await run(bin, args, options);

  if (result.code !== 0) {
    const how = result.signal ? `killed by ${result.signal}` : `exited ${result.code}`;
    const err = new Error(`kaniscope ${how}\n${tail(result.stderr)}`);
    err.exitCode = result.code;
    err.signal = result.signal;
    err.stderr = result.stderr;
    throw err;
  }

  try {
    return JSON.parse(result.stdout);
  } catch (cause) {
    // Distinguish "the review failed" from "the wrapper and the binary disagree
    // about the protocol". They need different fixes, and a bare SyntaxError
    // pointing at position 0 reads as neither.
    const err = new Error(
      `kaniscope exited 0 but stdout was not JSON — is KANISCOPE_BINARY_PATH ` +
        `pointing at a different program?\n${tail(result.stdout, 5)}`
    );
    err.cause = cause;
    throw err;
  }
}

/** The JSON Schema of a {@link review} result. Needs no key and no network. */
async function schema(options = {}) {
  const bin = options.binary || binaryPath();
  const result = await run(bin, ["--schema"], options);
  if (result.code !== 0) throw new Error(`kaniscope --schema failed\n${tail(result.stderr)}`);
  return JSON.parse(result.stdout);
}

/** The engine version this package's binary was built from. */
async function version(options = {}) {
  const bin = options.binary || binaryPath();
  const result = await run(bin, ["--version"], options);
  if (result.code !== 0) throw new Error(`kaniscope --version failed\n${tail(result.stderr)}`);
  return result.stdout.trim().replace(/^kaniscope\s+/, "");
}

module.exports = { review, schema, version, binaryPath };
