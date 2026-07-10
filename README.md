# pr-review-core

Core engine behind self-hosted, advisory AI pull-request reviewers.

`pr-review-core` fetches a pull request's unified diff, reviews it with a Claude
model via [OpenRouter](https://openrouter.ai), and posts line-anchored inline
comments plus a summary comment. It works with both **GitHub** and **Bitbucket**,
and optionally runs an *agentic* pass that clones the repo and lets the model
investigate cross-file context (grep / read_file / list_dir) before writing its
findings.

This crate is a **library** — it carries no bot identity of its own. Consumers
(the actual bot binaries) depend on it and inject their branding and any extra
prompt through [`Config`].

## What's in the box

- Provider-agnostic review flow (`review::run_review`) across GitHub + Bitbucket.
- Structured JSON review from the model, anchored to diff lines that the provider
  will accept (out-of-diff findings fold into the summary).
- Optional agentic reviewer with a two-tier model split (cheap explore model +
  stronger synthesis model).
- Webhook signature verification and payload parsing helpers.
- Dedupe: the bot updates its own prior comments on re-review instead of stacking.

## Injecting identity and prompt

Nothing about the bot's identity is hardcoded. `Config::from_env()` reads:

| Field | Env var | Default |
| --- | --- | --- |
| `comment_marker` | `COMMENT_MARKER` | `🤖 ai-pr-review` |
| `user_agent` | `USER_AGENT` | `pr-review-core` |
| `http_referer` | `OPENROUTER_HTTP_REFERER` | `https://github.com/nhatvu148/pr-review-core` |
| `x_title` | `OPENROUTER_X_TITLE` | `pr-review` |
| `extra_system_prompt` | `EXTRA_SYSTEM_PROMPT` / `EXTRA_SYSTEM_PROMPT_FILE` | *(empty)* |

- `comment_marker` is the signature appended to every comment and the dedupe key
  used to find/update the bot's own comments.
- `extra_system_prompt` is appended to the built-in system prompts. Set it inline
  via `EXTRA_SYSTEM_PROMPT`, or point `EXTRA_SYSTEM_PROMPT_FILE` at a file baked
  into your Docker image to inject a large conventions block without touching the
  library.

Other operational settings (OpenRouter key/models, provider tokens, agentic mode,
size caps) are also read from the environment — see `src/config.rs`.

## License

Licensed under either of

- MIT license ([LICENSE-MIT](LICENSE-MIT))
- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))

at your option.

[`Config`]: src/config.rs
