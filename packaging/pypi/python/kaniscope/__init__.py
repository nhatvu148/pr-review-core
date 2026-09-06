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


def _environment(env: Optional[Mapping[str, str]], inherit_env: bool) -> Dict[str, str]:
    base = dict(os.environ) if inherit_env else {}
    if env:
        base.update({k: str(v) for k, v in env.items() if v is not None})
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
    result = subprocess.run(
        [binary or binary_path(), *_build_args(options)],
        capture_output=True,
        text=True,
        env=_environment(env, inherit_env),
        timeout=timeout,
    )
    return _parse(result.stdout, result.stderr, result.returncode)


async def review_async(
    *,
    on_log: Optional[Callable[[str], None]] = None,
    **kwargs: Any,
) -> RunReviewOutput:
    """:func:`review`, without blocking the event loop.

    A review takes minutes, so the synchronous call is unusable from inside a
    webhook handler — which is the shape most Python bots have. ``on_log`` is
    called per stderr line as it arrives, so a handler can report progress rather
    than going silent for the whole run.
    """
    binary = kwargs.pop("binary", None)
    env = kwargs.pop("env", None)
    inherit_env = kwargs.pop("inherit_env", True)
    timeout = kwargs.pop("timeout", None)

    proc = await asyncio.create_subprocess_exec(
        binary or binary_path(),
        *_build_args(kwargs),
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
        env=_environment(env, inherit_env),
    )

    async def drain(stream: Any, sink: list) -> None:
        async for raw in stream:
            line = raw.decode(errors="replace")
            sink.append(line)
            if on_log and line.strip():
                on_log(line.rstrip("\n"))

    out_lines: list = []
    err_lines: list = []
    try:
        await asyncio.wait_for(
            asyncio.gather(
                drain(proc.stdout, out_lines), drain(proc.stderr, err_lines), proc.wait()
            ),
            timeout=timeout,
        )
    except asyncio.TimeoutError:
        # Kill it rather than leaving an orphan holding a clone and an API key.
        proc.kill()
        await proc.wait()
        raise KaniscopeError(f"kaniscope timed out after {timeout}s", stderr="".join(err_lines))

    return _parse("".join(out_lines), "".join(err_lines), proc.returncode or 0)


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
