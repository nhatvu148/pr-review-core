# Contributing

Thanks for taking an interest. This is a small, opinionated crate, and the bar is
that a reviewer can tell in one pass *why* a change is right. That matters more
here than volume.

## Before you write code

**Comment on the issue to claim it, and wait for a reply.** Two reasons: the fix
may already be half-written locally — that has happened — and for anything
touching review behaviour, the design question is usually harder than the code. A
PR that arrives unannounced against shipped behaviour is likely to be turned down
for a reason a two-line comment would have surfaced first.

No issue yet? Open one. For a bug, the fastest path to a merge is a failing test
or a reproducible diff.

## The local loop

CI runs exactly these three, and they must be clean:

```sh
cargo fmt --check
cargo clippy --all-features --all-targets -- -D warnings
cargo test --all-features
```

`clippy` runs with `-D warnings`, so a warning is a failure.

## What the code should look like

Match the file you are editing rather than any general style guide.

- **Tests live beside the code**, in the module's `mod tests`. Name them as a
  sentence stating the behaviour, not the function under test:
  `an_empty_diff_is_an_error_not_an_empty_review`, not `test_review`.
- **Public items get doc comments**, and the examples in them run as doctests.
- **Comment the why, not the what** — the surprising constraint, the reason the
  simpler version does not work, the format trap you hit. Not a restatement of
  the line below it.
- **Enrichment fails open.** The scan and context paths never block a review when
  a service is down or a parse fails.

## The CHANGELOG

Any user-visible change gets an entry in [CHANGELOG.md](CHANGELOG.md) under a
`## Unreleased` heading — add the heading if it is not currently there. Write why
the old behaviour was wrong, not only what changed. The existing entries are the
model.

`## Unreleased` is promoted to a version number at release time, along with the
`Cargo.toml` bump — see [Releasing](README.md#releasing). That is a maintainer
step, so **do not choose a version number or bump `Cargo.toml` in a
contribution**: releasing here requires compiling private downstream consumers
that CI cannot reach, and that check gates the version.

This is also why you will not find an `Unreleased` section sitting in the
CHANGELOG most of the time. It is always promoted before a release lands, so its
absence means the last change was released — not that the convention is unused.

If the change has a known limit or leaves a gap, say so in the entry. An
overclaim discovered later costs more than the gap ever did.

## Pull requests

- One concern per PR. A drive-by refactor inside a bugfix makes both harder to
  review.
- Say how you verified it. "Tests pass" is weaker than "ran it against X, and
  here is what came back".
- Found something real but out of scope? File it separately rather than widening
  the PR.

## AI-assisted contributions

Fine, and used here too. But you are the author: read what you are submitting,
verify it actually runs, and be able to say why it works. A PR the submitter
cannot explain will be closed regardless of how it was produced.

## Licensing

Unless you state otherwise, contributions are dual licensed MIT / Apache-2.0, as
described in the [README](README.md#license).
