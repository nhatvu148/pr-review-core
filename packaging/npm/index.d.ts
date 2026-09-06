import type { RunReviewOutput } from "./types";

export type { RunReviewOutput, Finding, InlineComment, Usage } from "./types";

/** Options common to every call: how the binary is found and run. */
export interface SpawnOptions {
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

/** The JSON Schema of a {@link review} result. Needs no key and no network. */
export function schema(options?: SpawnOptions): Promise<Record<string, unknown>>;

/** The engine version this package's binary was built from. */
export function version(options?: SpawnOptions): Promise<string>;

/** Absolute path to the bundled native binary. Throws if none matches the host. */
export function binaryPath(): string;
