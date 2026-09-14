---
name: kaniscope
description: Run an independent, advisory code review with the kaniscope engine — review uncommitted or unpushed local changes, review a pull request, deep-review one file, show the review rules in effect, list the findings still open on a PR, or investigate one finding against the code. Use when asked to review a change, get a second opinion on a diff before pushing or opening a PR, check work against what it was supposed to do, work through a PR bot's open findings, or explain why the reviewer flagged something. Read-only: it never edits code and never posts without being told to.
---

# Kaniscope

An independent reviewer you call as a subprocess. It reads code and reports findings; **you** decide what to change. It has no opinion about your edit loop and no permission to touch the checkout.

Use it when a second, differently-calibrated opinion is worth having: before a push, before opening a PR, or when the user asks for a review. It is not a linter — do not run it on every edit.

## Finding the binary

```sh
command -v kaniscope || echo "not installed"
```

If absent, tell the user rather than installing anything: `npm install -g kaniscope`, `pip install kaniscope`, or `cargo install pr-review-core --features cli`.

## Operations

Every operation prints **exactly one JSON document on stdout**; logs go to stderr. Parse stdout, never scrape the logs.

### Review local changes — the common case

```sh
kaniscope review-local --base main --intent "<what this change is meant to do>"
```

Diff modes, which do not overlap — pick the one that matches what you want reviewed:

| flag | reviews |
|---|---|
| `--base <ref>` | everything that differs from the ref, through the working tree (committed + staged + unstaged) |
| `--staged` | staged changes only |
| `--working-tree` | unstaged changes only |
| *(none)* | a diff piped in on stdin |

`--intent` is the point of this operation. The reviewer checks the diff against what you say the change was *for*, and reports where they disagree — which it cannot do otherwise, because a branch carries no description. Pass the task you were given, in one or two sentences. Use `--intent-file <path>` when it is long.

Read-only. There is nowhere to post.

### Review a pull request

```sh
kaniscope review-pr --provider github --repo owner/name --pr 12
```

**Posts nothing by default.** Add `--post` only when the user has asked for the review to go on the PR. Do not add it because the surrounding task mentioned a PR — posting writes a comment under their name that cannot be un-sent. If you are unsure, run without it and show them the findings.

### Deep-review one file

```sh
kaniscope review-file --path src/auth.rs                       # in this checkout
kaniscope review-file --path src/auth.rs --provider github --repo owner/name --pr 12
```

Reviews the whole file, not a diff, so findings can land anywhere in it. Never posts.

Check `outcome.status` before reading findings: `reviewed`, or `excluded` (the repository's own filters refuse that path — report it, do not retry), or `notFound`.

### Show the effective rules

```sh
kaniscope get-rules --repo-root .
kaniscope get-rules --provider github --repo owner/name --pr 12
```

Needs no model key. Answers "why did it flag that?" and "why did it *not*?": the merged settings, which `.prbot.toml` was read, what that file overrode, and the exact instructions injected into the reviewer's system prompt.

Read `warnings` first — a repository config that failed to parse is applied silently as *nothing*, and this is the only place that says so.

### Work through a PR's open findings

```sh
kaniscope get-findings --provider github --repo owner/name --pr 12
```

Read `outcome.status` first:

- `listed` — `active` is what is still open, `resolved` what the PR already closed, `unparseable` bot comments that carry no fingerprint and need a person.
- `unsupported` — **this provider cannot answer**, and the `reason` says why. GitLab reposts its inline discussions every run and Bitbucket never carries the fingerprint marker, so neither has a lifecycle to report. Treat this as "unknown", never as "nothing is open".

Then take the ones you intend to work on:

```sh
kaniscope resolve-findings --provider github --repo owner/name --pr 12   --fingerprint <fp> --fingerprint <fp>          # omit for every active finding
```

Despite the name it **resolves nothing** — no edits, no posts, no threads closed. It hands you each finding with an `action`:

| action | what it means |
|---|---|
| `investigate` | open on the current head — read the code and decide |
| `reverifyAgainstHead` | written against an earlier commit; the line number refers to *that* commit, so re-read before touching anything |
| `alreadyResolved` | the PR already closed it |
| `notFound` | no finding on this PR has that fingerprint |
| `needsHumanJudgement` | unmatchable comment — escalate, do not guess |

Apply a fix only if the user's request already authorized you to change code. Otherwise report and stop.

### Investigate one finding

```sh
echo '{"file":"src/auth.rs","line":42,"body":"<what the reviewer said>"}'   | kaniscope explain-finding --finding @- --repo-root . --head-sha "$(git rev-parse HEAD)"
```

Reads the real file and returns a `verdict` of `holds`, `doesNotHold` or `inconclusive`, with `evidence`, `uncertainty` and a `suggestedVerification`.

`doesNotHold` is a real and useful answer — reviewers produce false positives. Do not "fix" a finding this pass says does not hold; report that it does not. `inconclusive` means go look yourself.

Pass `--head-sha` whenever you have it. Without it a revision mismatch cannot be detected, and you may be reading line numbers from a different commit.

### Schemas

`kaniscope schema` lists the operations; `kaniscope schema get-rules` prints one operation's output schema.

JSON comes in through `@-` (stdin) or `@path` wherever a flag takes it — a finding body is prose full of quotes and backticks, and a shell will mangle it inline.

## Reading the result

A review returns `recommendation`, `findings` (count) and `findingsDetail`. Each finding has `severity` (`BLOCKING` / `HIGH` / `MEDIUM` / `LOW`), `file`, `line`, `body`, `confidence`, and sometimes `suggestion` — replacement code already checked against the diff.

Present findings grouped by severity, with the file and line. Say plainly when there are none.

## Rules

- **It advises; it does not edit.** Apply a finding only when the user's request already authorized you to change code. Otherwise report and stop.
- **Weigh the findings — do not launder them.** You have the repository in front of you and the reviewer had a diff. If a finding is wrong, say so and why. Repeating a false positive because a tool emitted it is worse than not running the tool.
- **Never pass credentials on the command line.** The engine reads `OPENROUTER_API_KEY` and provider tokens from the environment. Never echo them, and never paste a result into a public place without checking it first.
- **A review describes one revision.** If the working tree or the PR head has moved since, say so rather than implying the findings still hold.
- **A non-zero exit is a failure, not an empty review.** Report the stderr tail. Zero findings is a successful review and comes back as valid JSON.
