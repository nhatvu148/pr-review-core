"""Kaniscope — an AI pull-request reviewer, as a Python API.

Under this module is a native binary, and that is the point rather than a
compromise. The engine's entry point takes five scalars, reads the rest of its
configuration from the environment, and returns one JSON document after minutes
of network and git work. Binding that through an extension module would buy
nothing a pipe does not already give, and would cost an ABI-pinned wheel per
Python version, a tokio/asyncio bridge, and a second declaration of every wire
type that can drift from the first. The types in ``_types`` are generated from
the binary's own ``--schema``, so they cannot.

This is the arrangement ``uv`` and ``ruff`` use to ship a Rust program on PyPI:
the wheel carries a binary, not an extension, so it is independent of the Python
version it installs under.

    from kaniscope import review

    out = review(provider="github", repo="me/app", pr=12, dry_run=True)
    for f in out["findingsDetail"]:
        print(f["severity"], f["file"], f["body"])
"""

from __future__ import annotations

import asyncio
import json
import os
import shutil
import subprocess
import sysconfig
from typing import Any, Callable, Dict, Mapping, Optional

from ._config import CONFIG_ENV, ReviewConfig, config_to_env
from ._types import (
    EffectiveRules,
    ExplainOutput,
    FindingsOutput,
    ResolveOutput,
    FileReviewOutput,
    Finding,
    InlineComment,
    ReviewSettings,
    RunReviewOutput,
    Usage,
)

__all__ = [
    "review",
    "review_async",
    "review_file",
    "get_rules",
    "get_findings",
    "resolve_findings",
    "explain_finding",
    "schema",
    "version",
    "binary_path",
    "KaniscopeError",
    "Finding",
    "InlineComment",
    "RunReviewOutput",
    "Usage",
    "EffectiveRules",
    "ExplainOutput",
    "FileReviewOutput",
    "FindingsOutput",
    "ResolveOutput",
    "ReviewSettings",
    "ReviewConfig",
    "CONFIG_ENV",
]

_EXE = "kaniscope.exe" if os.name == "nt" else "kaniscope"


class KaniscopeError(RuntimeError):
    """The engine exited non-zero, or produced something that is not a review."""

    def __init__(self, message: str, *, exit_code: Optional[int] = None, stderr: str = "") -> None:
        super().__init__(message)
        self.exit_code = exit_code
        self.stderr = stderr


def binary_path() -> str:
    """Absolute path to the ``kaniscope`` executable this wheel installed.

    Looks in the environment's own scripts directory *before* ``PATH``. In a
    virtualenv those are usually the same file, but not always: an activated venv
    inside a shell that also has a global install on ``PATH`` would otherwise run
    whichever came first, so ``pip install -U kaniscope`` could appear to change
    nothing. Preferring the scripts directory ties the binary to the wheel that
    is actually imported.
    """
    override = os.environ.get("KANISCOPE_BINARY_PATH")
    if override:
        return override

    scripts = sysconfig.get_path("scripts")
    if scripts:
        candidate = os.path.join(scripts, _EXE)
        if os.path.isfile(candidate):
            return candidate

    found = shutil.which(_EXE)
    if found:
        return found

    raise KaniscopeError(
        f"could not find the {_EXE} binary. It ships inside the kaniscope wheel; "
        "reinstall with `pip install --force-reinstall kaniscope`, or set "
        "KANISCOPE_BINARY_PATH to a binary you built yourself "
        "(`cargo install pr-review-core --features cli`)."
    )


_VALUE_FLAGS = {
    "provider": "--provider",
    "repo": "--repo",
    "pr": "--pr",
    "base": "--base",
    "repo_root": "--repo-root",
    "label": "--label",
    "intent": "--intent",
    "intent_file": "--intent-file",
    "json_out": "--json-out",
}

_BOOL_FLAGS = {"local": "--local", "dry_run": "--dry-run"}


def _build_args(options: Mapping[str, Any]) -> list:
    args = ["--json"]
    for key, flag in _VALUE_FLAGS.items():
        value = options.get(key)
        if value is None:
            continue
        args += [flag, str(value)]
    for key, flag in _BOOL_FLAGS.items():
        if options.get(key):
            args.append(flag)
    return args


def _environment(
    env: Optional[Mapping[str, str]],
    inherit_env: bool,
    config: Optional[Mapping[str, Any]] = None,
) -> Dict[str, str]:
    """Merge ``config`` then ``env`` over the inherited environment.

    Order is the contract: inherited, then ``config``, then ``env``. The typed
    layer is a convenience over the same variables, so anything it does not
    model — or models wrongly — must stay reachable, and an escape hatch is only
    an escape hatch if it wins.

    ``None`` in ``env`` means **unset**.

    Skipping a ``None`` instead of removing the key made the two clients
    disagree about the same gesture: Node drops an ``undefined`` env value, so
    ``env={"FOO": undefined}`` unsets ``FOO`` there, while here the inherited
    value silently survived. Verified against both runtimes before choosing which
    way to converge — unsetting is the useful reading, since "inherit everything
    except this one secret" has no other spelling.
    """
    base = dict(os.environ) if inherit_env else {}
    base.update(config_to_env(config))
    for key, value in (env or {}).items():
        if value is None:
            base.pop(key, None)
        else:
            base[key] = str(value)
    return base


def _tail(text: str, lines: int = 20) -> str:
    return "\n".join(text.rstrip().splitlines()[-lines:])


def _parse(stdout: str, stderr: str, code: int) -> RunReviewOutput:
    if code != 0:
        raise KaniscopeError(
            f"kaniscope exited {code}\n{_tail(stderr)}", exit_code=code, stderr=stderr
        )
    try:
        return json.loads(stdout)
    except json.JSONDecodeError as exc:
        # "The review failed" and "the client and the binary disagree about the
        # protocol" need different fixes, and a bare JSONDecodeError at char 0
        # reads as neither.
        raise KaniscopeError(
            "kaniscope exited 0 but stdout was not JSON — is KANISCOPE_BINARY_PATH "
            f"pointing at a different program?\n{_tail(stdout, 5)}",
            exit_code=code,
            stderr=stderr,
        ) from exc


def _run_operation(op: str, args: list, options: Mapping[str, Any]) -> Any:
    """Run one explicit toolbox operation and parse its single JSON document.

    Shared so the operations below cannot diverge in how they report a crash
    versus a stdout that is not JSON — the two failures a caller most needs told
    apart, and the two a bare ``JSONDecodeError`` conflates.
    """
    result = subprocess.run(
        [options.get("binary") or binary_path(), op, *args],
        capture_output=True,
        text=True,
        env=_environment(options.get("env"), options.get("inherit_env", True), options.get("config")),
        timeout=options.get("timeout"),
        stdin=subprocess.DEVNULL,
    )
    return _parse(result.stdout, result.stderr, result.returncode)


def _scope_args(
    repo_root: Optional[str],
    provider: Optional[str],
    repo: Optional[str],
    pr: Optional[int],
) -> list:
    """Flags for the operations that take a checkout OR a pull request."""
    if provider or repo or pr is not None:
        return ["--provider", str(provider), "--repo", str(repo), "--pr", str(pr)]
    return ["--repo-root", str(repo_root)] if repo_root else []


def get_rules(
    *,
    repo_root: Optional[str] = None,
    provider: Optional[str] = None,
    repo: Optional[str] = None,
    pr: Optional[int] = None,
    config: Optional[ReviewConfig] = None,
    env: Optional[Mapping[str, str]] = None,
    inherit_env: bool = True,
    timeout: Optional[float] = None,
    binary: Optional[str] = None,
) -> EffectiveRules:
    """The effective review rules for a checkout or a pull request.

    Merged settings, which ``.prbot.toml`` was read (or why none was), what it
    overrode, and the exact instructions injected into the system prompt.

    Makes no model call, so it needs no ``OPENROUTER_API_KEY``. Credentials never
    appear in the result: it is built from an explicit allowlist of settings, so
    a secret added to the engine's configuration cannot leak into it by default.
    """
    return _run_operation("get-rules", _scope_args(repo_root, provider, repo, pr), locals())


def _pr_args(provider: Optional[str], repo: Optional[str], pr: Optional[int]) -> list:
    """PR coordinates, all three required by the findings operations."""
    missing = [n for n, v in (("provider", provider), ("repo", repo), ("pr", pr)) if v is None]
    if missing:
        raise TypeError(f"kaniscope: this operation needs {', '.join(missing)}")
    return ["--provider", str(provider), "--repo", str(repo), "--pr", str(pr)]


def get_findings(
    *,
    provider: Optional[str] = None,
    repo: Optional[str] = None,
    pr: Optional[int] = None,
    config: Optional[ReviewConfig] = None,
    env: Optional[Mapping[str, str]] = None,
    inherit_env: bool = True,
    timeout: Optional[float] = None,
    binary: Optional[str] = None,
) -> FindingsOutput:
    """The findings currently on a pull request, with their lifecycle state.

    Read-only. Check ``outcome["status"]``: a provider that cannot track findings
    returns ``unsupported`` rather than an empty list, because "no open findings"
    is a conclusion a caller acts on and must never come from a question that was
    never asked.
    """
    return _run_operation("get-findings", _pr_args(provider, repo, pr), locals())


def resolve_findings(
    *,
    provider: Optional[str] = None,
    repo: Optional[str] = None,
    pr: Optional[int] = None,
    fingerprints: Optional[list] = None,
    config: Optional[ReviewConfig] = None,
    env: Optional[Mapping[str, str]] = None,
    inherit_env: bool = True,
    timeout: Optional[float] = None,
    binary: Optional[str] = None,
) -> ResolveOutput:
    """Package findings for your own edit loop.

    Changes nothing — no edits, no posts, no provider thread resolution. The name
    is the operation's; the returned ``disclaimer`` says so in the payload.

    Omit ``fingerprints`` to take every active finding.
    """
    fps = [a for fp in (fingerprints or []) for a in ("--fingerprint", str(fp))]
    return _run_operation("resolve-findings", [*_pr_args(provider, repo, pr), *fps], locals())


def explain_finding(
    *,
    finding: Mapping[str, Any],
    repo_root: Optional[str] = None,
    head_sha: Optional[str] = None,
    config: Optional[ReviewConfig] = None,
    env: Optional[Mapping[str, str]] = None,
    inherit_env: bool = True,
    timeout: Optional[float] = None,
    binary: Optional[str] = None,
) -> ExplainOutput:
    """Investigate one finding against a local checkout. Never posts.

    The finding goes in on stdin rather than as a flag: a finding body is
    multi-line prose containing quotes and backticks, which is exactly what an
    argv mangles.
    """
    args = ["--finding", "@-"]
    if repo_root:
        args += ["--repo-root", str(repo_root)]
    if head_sha:
        args += ["--head-sha", str(head_sha)]
    result = subprocess.run(
        [binary or binary_path(), "explain-finding", *args],
        capture_output=True,
        text=True,
        env=_environment(env, inherit_env, config),
        timeout=timeout,
        input=json.dumps(finding),
    )
    return _parse(result.stdout, result.stderr, result.returncode)


def review_file(
    *,
    path: str,
    repo_root: Optional[str] = None,
    provider: Optional[str] = None,
    repo: Optional[str] = None,
    pr: Optional[int] = None,
    config: Optional[ReviewConfig] = None,
    env: Optional[Mapping[str, str]] = None,
    inherit_env: bool = True,
    timeout: Optional[float] = None,
    binary: Optional[str] = None,
) -> FileReviewOutput:
    """Deep-review one complete file, in a checkout or at a pull request's head.

    Posts nothing. A path excluded by the repository's review filters comes back
    as an ``excluded`` outcome rather than raising, so a caller can report it
    without retrying it forever.
    """
    return _run_operation(
        "review-file",
        ["--path", str(path), *_scope_args(repo_root, provider, repo, pr)],
        locals(),
    )


def review(
    *,
    provider: Optional[str] = None,
    repo: Optional[str] = None,
    pr: Optional[int] = None,
    dry_run: bool = False,
    local: bool = False,
    base: Optional[str] = None,
    repo_root: Optional[str] = None,
    label: Optional[str] = None,
    intent: Optional[str] = None,
    intent_file: Optional[str] = None,
    json_out: Optional[str] = None,
    diff: Optional[str] = None,
    config: Optional[ReviewConfig] = None,
    env: Optional[Mapping[str, str]] = None,
    inherit_env: bool = True,
    timeout: Optional[float] = None,
    binary: Optional[str] = None,
) -> RunReviewOutput:
    """Review a pull request, or a local diff with ``local=True``.

    Everything beyond these arguments is engine configuration. Pass it typed in
    ``config`` — a ``ReviewConfig`` generated from the engine's own spec, so its
    names, types and defaults cannot drift from what the binary reads::

        review(provider="github", repo=repo, pr=pr,
               config={"openrouter_model": "anthropic/claude-sonnet-5",
                       "min_confidence": 70})

    ``env`` still takes raw strings and is applied **after** ``config``, so it
    wins. That ordering is deliberate: ``config`` cannot model everything and can
    model something wrongly, and an escape hatch is only an escape hatch if it
    wins. Both are merged over ``os.environ`` unless ``inherit_env=False``.

    ``intent`` (or ``intent_file``, which is mutually exclusive with it) says
    what a ``local=True`` change is MEANT to do, so the reviewer can check the
    diff against it the way it checks a PR against its description. It is treated
    as untrusted data: fenced and labelled before it reaches the model, and unable
    to direct the review.

    Keyword-only on purpose: ``provider``/``repo``/``pr`` are three adjacent
    values of which two are strings, and a positional call that swapped them
    would review a repository named after a provider rather than fail.
    """
    options = locals()
    try:
        result = subprocess.run(
            [binary or binary_path(), *_build_args(options)],
            capture_output=True,
            text=True,
            env=_environment(env, inherit_env, config),
            timeout=timeout,
            # Explicit, never inherited. `local=True` without `base` reads the
            # diff from stdin, and leaving stdin to default meant a caller got
            # whatever the parent had — in a webhook server, nothing, so the
            # process would block on a read that never returns. `diff` feeds that
            # mode properly; its absence closes the door rather than leaving it
            # ajar. The Node client does the same thing for the same reason.
            input=diff,
            stdin=None if diff is not None else subprocess.DEVNULL,
        )
    except subprocess.TimeoutExpired as exc:
        # Re-raised as KaniscopeError so both APIs fail the same way. A caller
        # that wraps this in `except KaniscopeError` — which the README and these
        # docstrings both tell them to — would otherwise have a raw
        # `subprocess.TimeoutExpired` escape from the sync path only, and find out
        # in production that the two functions disagree about their own contract.
        #
        # `subprocess.run` has already killed and reaped the child by the time it
        # raises, so unlike the async path there is nothing to clean up here; what
        # is wrong is only the exception type. Partial output is preserved,
        # because a review that ran for the full timeout usually printed the
        # reason it was stuck.
        stderr = exc.stderr or b""
        raise KaniscopeError(
            f"kaniscope timed out after {timeout}s",
            stderr=stderr.decode(errors="replace") if isinstance(stderr, bytes) else stderr,
        ) from None
    return _parse(result.stdout, result.stderr, result.returncode)


async def review_async(
    *,
    provider: Optional[str] = None,
    repo: Optional[str] = None,
    pr: Optional[int] = None,
    dry_run: bool = False,
    local: bool = False,
    base: Optional[str] = None,
    repo_root: Optional[str] = None,
    label: Optional[str] = None,
    intent: Optional[str] = None,
    intent_file: Optional[str] = None,
    json_out: Optional[str] = None,
    diff: Optional[str] = None,
    config: Optional[ReviewConfig] = None,
    env: Optional[Mapping[str, str]] = None,
    inherit_env: bool = True,
    timeout: Optional[float] = None,
    binary: Optional[str] = None,
    on_log: Optional[Callable[[str], None]] = None,
) -> RunReviewOutput:
    """:func:`review`, without blocking the event loop.

    A review takes minutes, so the synchronous call is unusable from inside a
    webhook handler — which is the shape most Python bots have. ``on_log`` is
    called per stderr line as it arrives, so a handler can report progress rather
    than going silent for the whole run.

    Takes ``config`` and ``env`` exactly as :func:`review` does, with the same
    precedence: ``config`` first, then ``env``, which wins.

    The signature is spelled out rather than taken as ``**kwargs`` so that an
    unknown argument is a :class:`TypeError` here, at the call, instead of being
    dropped on the way to the binary. Silently ignoring ``dryRun=True`` — the
    natural typo when porting from the TypeScript client, where that IS the
    spelling — would post a live review to someone's PR while the caller believed
    they had asked for a dry run. That is the one mistake in this API whose
    consequences cannot be undone.
    """
    options = locals()

    proc = await asyncio.create_subprocess_exec(
        binary or binary_path(),
        *_build_args(options),
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
        # Explicit, never inherited — see the note in `review`. A server has no
        # diff on its stdin, so inheriting means blocking on a read that never
        # returns; `diff` opens the door deliberately and DEVNULL keeps it shut.
        stdin=asyncio.subprocess.PIPE if diff is not None else asyncio.subprocess.DEVNULL,
        env=_environment(env, inherit_env, config),
    )

    async def read_all(stream: Any) -> str:
        """Drain a stream to EOF in bounded chunks.

        Deliberately NOT line-oriented. ``--json`` emits the whole review as ONE
        line, and asyncio's stream reader raises ``ValueError`` on any line past
        its 64 KiB limit — so iterating by line fails on exactly the large PRs
        whose reviews were most expensive to produce, after all the work is done.
        """
        chunks = []
        while True:
            chunk = await stream.read(65536)
            if not chunk:
                return b"".join(chunks).decode(errors="replace")
            chunks.append(chunk)

    async def read_lines(stream: Any, sink: list) -> None:
        """Drain stderr by line, reporting each to ``on_log`` as it arrives.

        Line-oriented is right here and wrong for stdout: these are log lines,
        they are short, and their value is in arriving during the run rather than
        after it.
        """
        while True:
            raw = await stream.readline()
            if not raw:
                return
            line = raw.decode(errors="replace")
            sink.append(line)
            if on_log and line.strip():
                on_log(line.rstrip("\n"))

    async def feed_stdin() -> None:
        """Write the diff and close, concurrently with draining the outputs.

        Concurrently, not before: a diff larger than the pipe buffer would block
        this write until the child consumes it, while the child can block writing
        its output until we consume that — a deadlock that only shows up on big
        diffs, which is the worst possible size at which to discover it.
        """
        if diff is None or proc.stdin is None:
            return
        try:
            proc.stdin.write(diff.encode())
            await proc.stdin.drain()
        except (BrokenPipeError, ConnectionResetError):
            # The child exited before reading it all — its own exit code and
            # stderr say why, and that is the more useful error of the two.
            pass
        finally:
            try:
                proc.stdin.close()
            except (BrokenPipeError, ConnectionResetError):
                pass

    err_lines: list = []
    gather = asyncio.gather(
        read_all(proc.stdout), read_lines(proc.stderr, err_lines), feed_stdin(), proc.wait()
    )
    try:
        stdout, _, _, _ = await asyncio.wait_for(gather, timeout=timeout)
    except asyncio.TimeoutError:
        await _terminate(proc)
        raise KaniscopeError(
            f"kaniscope timed out after {timeout}s", stderr="".join(err_lines)
        ) from None
    except asyncio.CancelledError:
        # The caller's task was cancelled — a client disconnect, a shutdown, an
        # `asyncio.timeout` block outside this call. Without this the review keeps
        # running unsupervised: it holds a clone, spends the API quota, and can
        # still POST to the pull request minutes after whoever asked for it went
        # away. Reap it, then let the cancellation continue.
        await _terminate(proc)
        raise

    return _parse(stdout, "".join(err_lines), proc.returncode or 0)


async def _terminate(proc: Any) -> None:
    """Kill a review subprocess and wait for it, so no orphan is left behind."""
    if proc.returncode is not None:
        return
    try:
        proc.kill()
    except ProcessLookupError:
        # It exited between the check and the kill; nothing to reap.
        return
    await proc.wait()


def schema(
    *, operation: Optional[str] = None, binary: Optional[str] = None
) -> Dict[str, Any]:
    """The JSON Schema of an operation's result. Needs no key and no network.

    ``operation`` selects a per-operation schema (``review-file``, ``get-rules``,
    ``review-local``, ``review-pr``); omit it for the review output's schema,
    which is what this has always returned.
    """
    args = ["schema", operation] if operation else ["--schema"]
    result = subprocess.run(
        [binary or binary_path(), *args], capture_output=True, text=True
    )
    if result.returncode != 0:
        raise KaniscopeError(
            f"kaniscope schema failed\n{_tail(result.stderr)}",
            exit_code=result.returncode,
            stderr=result.stderr,
        )
    return json.loads(result.stdout)


def version(*, binary: Optional[str] = None) -> str:
    """The engine version this wheel's binary was built from."""
    result = subprocess.run(
        [binary or binary_path(), "--version"], capture_output=True, text=True
    )
    if result.returncode != 0:
        raise KaniscopeError(
            f"kaniscope --version failed\n{_tail(result.stderr)}",
            exit_code=result.returncode,
            stderr=result.stderr,
        )
    return result.stdout.strip().removeprefix("kaniscope ")
