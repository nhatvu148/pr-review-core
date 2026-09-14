import type {
  RunReviewOutput,
  FileReviewOutput,
  EffectiveRules,
  FindingsOutput,
  ResolveOutput,
  ExplainOutput,
  ExplainInput,
} from "./types";

export type {
  RunReviewOutput,
  Finding,
  InlineComment,
  Usage,
  FileReviewOutput,
  FileReviewOutcome,
  FileSource,
  EffectiveRules,
  ReviewSettings,
  RepoConfigSource,
  RulesScope,
  FindingsOutput,
  FindingsOutcome,
  OutstandingFinding,
  FindingState,
  ResolveOutput,
  FindingHandoff,
  HandoffAction,
  ExplainOutput,
  ExplainInput,
  Explanation,
  ExplanationVerdict,
} from "./types";

export type { ReviewConfig } from "./config";

/** Options common to every call: how the binary is found and run. */
export interface SpawnOptions {
  /**
   * Typed overrides for the engine's configuration — the same variables `env`
   * carries, with names, types and documentation generated from the engine's
   * own spec.
   *
   * `env` is applied *after* this and therefore wins, so the raw escape hatch
   * stays authoritative for anything not modelled.
   */
  config?: import("./config").ReviewConfig;
  /**
   * Environment overrides, merged over `process.env` (see {@link inheritEnv}).
   * This is where the engine's configuration lives — `OPENROUTER_API_KEY`,
   * `GH_TOKEN`, `MODEL`, `BOT_NAME`, globs, confidence floors — exactly as it
   * does for a Rust consumer.
   */
  env?: Record<string, string | undefined>;
  /** Set `false` to pass only {@link env}, ignoring `process.env`. Default `true`. */
  inheritEnv?: boolean;
  /** Kill the review after this many milliseconds and reject. */
  timeoutMs?: number;
  /** Called once per line the engine logs to stderr, as it happens. */
  onLog?: (line: string) => void;
  /** Override the binary to run. Defaults to the bundled per-platform one. */
  binary?: string;
}

export interface ReviewOptions extends SpawnOptions {
  /** `github` | `gitlab` | `bitbucket`. Required unless {@link local}. */
  provider?: string;
  /** `owner/repo` (GitHub, GitLab) or `workspace/repo` (Bitbucket). */
  repo?: string;
  /** PR / MR number. */
  pr?: number;
  /** Produce the review but post nothing. */
  dryRun?: boolean;
  /** Review a diff instead of a PR. Nothing is posted; there is nowhere to post. */
  local?: boolean;
  /** With {@link local}: diff the working tree against this ref. */
  base?: string;
  /** With {@link local}: the checkout to read files from. Defaults to cwd. */
  repoRoot?: string;
  /** With {@link local}: what to call this change. Defaults to the branch name. */
  label?: string;
  /**
   * With {@link local}: what this change is MEANT to do — the task, or the
   * instruction given to a coding agent. The reviewer checks the diff against
   * it, the way it checks a PR against its description.
   *
   * Treated as untrusted data: fenced and labelled before it reaches the model,
   * and unable to direct the review. Mutually exclusive with {@link intentFile}.
   */
  intent?: string;
  /** With {@link local}: read {@link intent} from this file instead. */
  intentFile?: string;
  /** Also write the result as JSON to this path, atomically. */
  jsonOut?: string;
  /**
   * A unified diff to feed the engine on stdin — the way to use
   * `{ local: true }` without a {@link base}, matching
   * `git diff --staged | kaniscope --local`.
   *
   * Without it the child's stdin is closed rather than inherited: a server has
   * no diff on its own stdin, so inheriting would block on a read that never
   * returns.
   */
  diff?: string;
}

/**
 * Review a pull request, or a local diff with `{ local: true }`.
 *
 * Rejects when the engine exits non-zero; the error carries `exitCode`,
 * `signal` and the full `stderr`.
 */
export function review(options?: ReviewOptions): Promise<RunReviewOutput>;

/** Selects a checkout, or a pull request. Give one or the other, not both. */
export interface ScopeOptions extends SpawnOptions {
  /** The checkout to resolve against. Defaults to the current directory. */
  repoRoot?: string;
  /** `github` | `gitlab` | `bitbucket`. With {@link repo} and {@link pr}. */
  provider?: string;
  repo?: string;
  pr?: number;
}

export interface ReviewFileOptions extends ScopeOptions {
  /** Repository-relative path of the file to review. Required. */
  path: string;
}

/**
 * The effective review rules: merged settings, which `.prbot.toml` was read,
 * what it overrode, and the exact instructions injected into the system prompt.
 *
 * Makes no model call, so it needs no `OPENROUTER_API_KEY`. Never includes
 * credentials — the result is built from an explicit allowlist of settings.
 */
export function getRules(options?: ScopeOptions): Promise<EffectiveRules>;

/**
 * Deep-review one complete file, in a checkout or at a PR head.
 *
 * Posts nothing. A path excluded by the repository's review filters comes back
 * as an `excluded` outcome rather than an error, so it can be reported without
 * being retried.
 */
export function reviewFile(options: ReviewFileOptions): Promise<FileReviewOutput>;

/** PR coordinates. All three are required. */
export interface PrOptions extends SpawnOptions {
  provider: string;
  repo: string;
  pr: number;
}

/**
 * The findings currently on a pull request, with lifecycle state.
 *
 * Read-only. Check `outcome.status`: a provider that cannot track findings
 * returns `unsupported` rather than an empty list, because "no open findings" is
 * a conclusion and must never come from a question that was never asked.
 */
export function getFindings(options: PrOptions): Promise<FindingsOutput>;

export interface ResolveOptions extends PrOptions {
  /** Fingerprints to hand over. Omit for every active finding. */
  fingerprints?: string[];
}

/**
 * Package findings for your own edit loop.
 *
 * Changes nothing — no edits, no posts, no thread resolution. The name is the
 * operation's, and the returned `disclaimer` says so in the payload.
 */
export function resolveFindings(options: ResolveOptions): Promise<ResolveOutput>;

export interface ExplainOptions extends SpawnOptions {
  /** The finding to investigate. */
  finding: ExplainInput;
  /** The checkout to read the file from. Defaults to the current directory. */
  repoRoot?: string;
  /** The commit the checkout is at, so a revision mismatch can be reported. */
  headSha?: string;
}

/** Investigate one finding against a local checkout. Never posts. */
export function explainFinding(options: ExplainOptions): Promise<ExplainOutput>;

export interface SchemaOptions extends SpawnOptions {
  /**
   * Which operation's output schema to fetch — `review-file`, `get-rules`,
   * `get-findings`, `resolve-findings`, `explain-finding`, `review-local`,
   * `review-pr`. Omit for the review output's schema.
   */
  operation?: string;
}

/** The JSON Schema of an operation's result. Needs no key and no network. */
export function schema(options?: SchemaOptions): Promise<Record<string, unknown>>;

/** The engine version this package's binary was built from. */
export function version(options?: SpawnOptions): Promise<string>;

/** Absolute path to the bundled native binary. Throws if none matches the host. */
export function binaryPath(): string;
