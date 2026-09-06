"""Regression tests for the Python client.

Every case here is a bug this file actually shipped and a pre-push review caught
before it reached anyone. They are written against a fake binary rather than the
real one so they need no API key, no network and no PR — which is also what makes
them cheap enough to run on every push.

    PYTHONPATH=python python3 test_client.py
"""

from __future__ import annotations

import asyncio
import json
import os
import subprocess
import sys
import tempfile

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "python"))

import kaniscope  # noqa: E402

FAILURES = []


def check(name: str, ok: bool, detail: str = "") -> None:
    print(f"{'ok  ' if ok else 'FAIL'} {name}{f' — {detail}' if detail else ''}")
    if not ok:
        FAILURES.append(name)


def fake_binary(script: str) -> str:
    """A throwaway executable standing in for the engine."""
    handle = tempfile.NamedTemporaryFile("w", suffix=".sh", delete=False)
    handle.write("#!/bin/sh\n" + script + "\n")
    handle.close()
    os.chmod(handle.name, 0o755)
    return handle.name


def test_unknown_kwarg_is_rejected() -> None:
    """An unknown argument must raise, not be dropped on the way to the binary.

    `dryRun=True` is the natural typo when porting from the TypeScript client,
    where that IS the spelling. Dropped silently, it posts a live review to
    someone's PR while the caller believes they asked for a dry run — the one
    mistake in this API whose consequences cannot be undone.
    """
    for call in (kaniscope.review, kaniscope.review_async):
        try:
            result = call(provider="github", repo="a/b", pr=1, dryRun=True)
            if asyncio.iscoroutine(result):
                result.close()
            check(f"{call.__name__} rejects an unknown kwarg", False, "it was accepted")
        except TypeError:
            check(f"{call.__name__} rejects an unknown kwarg", True)


def test_cancellation_reaps_the_child() -> None:
    """Cancelling the caller must not leave the review running.

    A client disconnect or a shutdown cancels the task. An unreaped child keeps
    holding a clone, spending the API quota, and can still POST to the pull
    request minutes after whoever asked for it went away.
    """
    binary = fake_binary("sleep 30")

    async def run() -> tuple:
        task = asyncio.create_task(kaniscope.review_async(binary=binary))
        await asyncio.sleep(0.4)
        before = subprocess.run(["pgrep", "-f", binary], capture_output=True, text=True)
        task.cancel()
        try:
            await task
        except asyncio.CancelledError:
            pass
        await asyncio.sleep(0.3)
        after = subprocess.run(["pgrep", "-f", binary], capture_output=True, text=True)
        return before.stdout.split(), after.stdout.split()

    try:
        before, after = asyncio.run(run())
        check(
            "cancelling review_async reaps the subprocess",
            bool(before) and not after,
            f"{len(before)} before, {len(after)} after",
        )
    finally:
        os.unlink(binary)


def test_timeout_reaps_the_child() -> None:
    """Same requirement on the timeout path, which is the one that fires in CI."""
    binary = fake_binary("sleep 30")
    try:
        asyncio.run(kaniscope.review_async(binary=binary, timeout=0.5))
        check("a timed-out review is killed", False, "no error raised")
    except kaniscope.KaniscopeError:
        left = subprocess.run(["pgrep", "-f", binary], capture_output=True, text=True)
        check("a timed-out review is killed", not left.stdout.split())
    finally:
        os.unlink(binary)


def test_large_single_line_stdout() -> None:
    """`--json` emits the whole review as ONE line, of unbounded length.

    asyncio's stream reader raises ValueError past its 64 KiB line limit, so
    reading stdout by line failed on exactly the large PRs whose reviews were
    most expensive to produce — after all the work was already done.
    """
    payload = json.dumps(
        {
            "model": "m",
            "findings": 0,
            "inlinePosted": 0,
            "posted": False,
            "pr": 0,
            "provider": "p",
            "recommendation": "r",
            "repo": "r",
            "summaryMarkdown": "y" * 300_000,
        }
    )
    binary = fake_binary("cat <<'EOF'\n" + payload + "\nEOF")
    try:
        out = asyncio.run(kaniscope.review_async(binary=binary))
        check(
            "a review far past the 64 KiB line limit parses",
            len(out["summaryMarkdown"]) == 300_000,
            f"{len(payload) // 1024} KiB single line",
        )
    finally:
        os.unlink(binary)


def test_nonzero_exit_carries_the_reason() -> None:
    """A failed review must surface its exit code and stderr, not just fail."""
    binary = fake_binary("echo 'the cause' >&2\nexit 3")
    try:
        asyncio.run(kaniscope.review_async(binary=binary))
        check("a non-zero exit raises with its stderr", False, "no error raised")
    except kaniscope.KaniscopeError as err:
        check(
            "a non-zero exit raises with its stderr",
            err.exit_code == 3 and "the cause" in err.stderr,
            f"exit={err.exit_code}",
        )
    finally:
        os.unlink(binary)


if __name__ == "__main__":
    test_unknown_kwarg_is_rejected()
    test_cancellation_reaps_the_child()
    test_timeout_reaps_the_child()
    test_large_single_line_stdout()
    test_nonzero_exit_carries_the_reason()
    print()
    if FAILURES:
        print(f"{len(FAILURES)} failure(s): {', '.join(FAILURES)}")
        sys.exit(1)
    print("python client: all checks passed")
