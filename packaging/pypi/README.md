# kaniscope

AI pull-request reviewer: fetches a PR's diff, reviews it, and posts line-anchored inline comments plus a summary. Works with **GitHub**, **GitLab** and **Bitbucket**.

The wheel carries a native binary, not an extension module — so it is independent of your Python version, needs no compiler, and adds no runtime dependencies. Same arrangement `uv` and `ruff` use.

```bash
pip install kaniscope          # or: uv tool install kaniscope
kaniscope --provider github --repo me/app --pr 12 --dry-run
```

## As a library

Build a bot in Python and drive the engine from it:

```python
from kaniscope import review

out = review(
    provider="github",
    repo="me/app",
    pr=12,
    dry_run=True,
    env={"OPENROUTER_API_KEY": key, "GH_TOKEN": token, "MODEL": "anthropic/claude-sonnet-5"},
)

print(out["recommendation"], out["findings"], "finding(s)")
for f in out.get("findingsDetail", []):
    print(f"{f['severity']} {f['file']}:{f.get('line', '-')} — {f['body']}")
```

A review takes minutes, so inside a webhook handler use the async form, which also streams the engine's progress:

```python
from kaniscope import review_async

out = await review_async(
    provider="github", repo="me/app", pr=12,
    on_log=lambda line: log.info(line),
)
```

Both raise `KaniscopeError` when the engine exits non-zero, carrying `exit_code` and the full `stderr`.

## Configuration

Everything beyond these arguments — the model, the API key, provider tokens, file globs, confidence floors, bot identity — is read from the environment, exactly as it is for the Rust library. Pass overrides in `env` (merged over `os.environ`, unless `inherit_env=False`).

The essentials: `OPENROUTER_API_KEY`, plus `GH_TOKEN` / `GITLAB_TOKEN` / Bitbucket credentials for the provider you use. See the [engine README](https://github.com/nhatvu148/pr-review-core#injecting-identity-and-prompt) for the full list, and `.prbot.toml` for per-repo settings.

## API

| Function | What it does |
| --- | --- |
| `review(**kwargs)` | Review a PR, or a local diff with `local=True`. |
| `review_async(**kwargs)` | The same, without blocking the event loop. |
| `schema()` | The JSON Schema of a review result. No key, no network. |
| `version()` | The engine version this wheel's binary was built from. |
| `binary_path()` | Absolute path to the installed binary. |

`RunReviewOutput`, `Finding`, `InlineComment` and `Usage` are `TypedDict`s **generated** from the binary's own `--schema`, so they cannot drift from what the engine actually emits.

## Reviewing without a PR

```bash
kaniscope --local --base main            # the working tree against a ref
git diff --staged | kaniscope --local    # any diff, from anywhere
```

Through the API, `base` picks the ref and `diff` is the stdin form:

```python
review(local=True, base="main")
review(local=True, diff=pathlib.Path("change.patch").read_text())
```

Same reviewer, same prompts, same anchoring — it just has nowhere to post.

## Why a binary and not a pyo3 extension

The engine takes five scalars, reads its configuration from the environment, and returns one JSON document after minutes of network and git work. Binding that through an extension module would buy nothing a pipe does not already give, and would cost a wheel per interpreter ABI plus a second declaration of every wire type. This way there is one artifact per platform, valid for every Python that can install it.

## License

MIT OR Apache-2.0. Source: [nhatvu148/pr-review-core](https://github.com/nhatvu148/pr-review-core).
