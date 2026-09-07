# kaniscope

AI pull-request reviewer: fetches a PR's diff, reviews it, and posts line-anchored inline comments plus a summary. Works with **GitHub**, **GitLab** and **Bitbucket**.

This package ships a native binary — there is no Rust toolchain to install, no compile step, and no postinstall download. The binary lives in a small per-platform package that npm picks by `os`/`cpu`, so you download one, not five.

```bash
npm install -g kaniscope                 # want the `kaniscope` command on PATH
kaniscope --provider github --repo me/app --pr 12 --dry-run
```

```bash
npm install kaniscope                    # building a bot? this is the one you want
npx kaniscope --provider github --repo me/app --pr 12 --dry-run
```

A plain `npm install kaniscope` is a **local** install, so it does not put `kaniscope` on your `PATH` — running it bare gives `command not found`. That is ordinary npm behaviour, not a broken install: reach it with `npx kaniscope`, or `./node_modules/.bin/kaniscope`, or install with `-g`.

For a bot, the local install is the right one anyway — `require("kaniscope")` works from it immediately and never touches `PATH`.

## As a library

Build a bot in TypeScript and drive the engine from it:

```ts
import { review } from "kaniscope";

const out = await review({
  provider: "github",
  repo: "me/app",
  pr: 12,
  dryRun: true,
  env: { OPENROUTER_API_KEY: key, GH_TOKEN: token, MODEL: "anthropic/claude-sonnet-5" },
  onLog: (line) => console.error(line),
});

console.log(out.recommendation, `${out.findings} finding(s)`);
for (const f of out.findingsDetail ?? []) {
  console.log(`${f.severity} ${f.file}:${f.line ?? "-"} — ${f.body}`);
}
```

`review()` runs the binary and parses its JSON. It rejects when the engine exits non-zero, with `exitCode`, `signal` and the full `stderr` on the error.

## Configuration

Everything beyond the flags above — the model, the API key, provider tokens, file globs, confidence floors, bot identity — is read from the environment, exactly as it is for the Rust library. Pass overrides in `env` (merged over `process.env`, unless `inheritEnv: false`).

The essentials: `OPENROUTER_API_KEY`, plus `GH_TOKEN` / `GITLAB_TOKEN` / Bitbucket credentials for the provider you use. See the [engine README](https://github.com/nhatvu148/pr-review-core#injecting-identity-and-prompt) for the full list, and `.prbot.toml` for per-repo settings.

## API

| Export | What it does |
| --- | --- |
| `review(options)` | Review a PR, or a local diff with `{ local: true }`. |
| `schema()` | The JSON Schema of a review result. No key, no network. |
| `version()` | The engine version this package's binary was built from. |
| `binaryPath()` | Absolute path to the bundled binary. |

`RunReviewOutput`, `Finding`, `InlineComment` and `Usage` are exported as types. They are **generated** from the binary's own `--schema`, so they cannot drift from what the engine actually emits.

## Reviewing without a PR

```bash
kaniscope --local --base main          # the working tree against a ref
git diff --staged | kaniscope --local  # any diff, from anywhere
```

Through the API, `base` picks the ref and `diff` is the stdin form:

```ts
await review({ local: true, base: "main" });
await review({ local: true, diff: await readFile("change.patch", "utf8") });
```

Same reviewer, same prompts, same anchoring — it just has nowhere to post.

## Why a subprocess and not native bindings

The engine takes five scalars, reads its configuration from the environment, and returns one JSON document after minutes of network and git work. A native addon would buy nothing a pipe does not already give, and would cost an ABI-pinned build per Node release plus a second declaration of every wire type. `uv` and `ruff` reach this ecosystem the same way.

## License

MIT OR Apache-2.0. Source: [nhatvu148/pr-review-core](https://github.com/nhatvu148/pr-review-core).
