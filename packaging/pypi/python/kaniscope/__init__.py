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
from ._types import Finding, InlineComment, RunReviewOutput, Usage

__all__ = [
    "review",
    "review_async",
    "schema",
    "version",
    "binary_path",
    "KaniscopeError",
    "Finding",
    "InlineComment",
    "RunReviewOutput",
    "Usage",
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
    json_out: Optional[str] = None,
    diff: Optional[str] = None,
    config: Optional[ReviewConfig] = None,
    env: Optional[Mapping[str, str]] = None,
    inherit_env: bool = True,
    timeout: Optional[float] = None,
    binary: Optional[str] = None,
) -> RunReviewOutput:
    """Review a pull request, or a local diff with ``local=True``.

    Everything beyond these arguments — the API key, the model, globs, confidence
    floors, bot identity — comes from the environment, exactly as it does for a
    Rust consumer. Pass overrides in ``env``; they are merged over ``os.environ``
    unless ``inherit_env=False``.

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


def schema(*, binary: Optional[str] = None) -> Dict[str, Any]:
    """The JSON Schema of a :func:`review` result. Needs no key and no network."""
    result = subprocess.run(
        [binary or binary_path(), "--schema"], capture_output=True, text=True
    )
    if result.returncode != 0:
        raise KaniscopeError(
            f"kaniscope --schema failed\n{_tail(result.stderr)}",
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
