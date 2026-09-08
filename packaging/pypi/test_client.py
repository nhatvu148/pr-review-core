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


def test_sync_timeout_raises_kaniscope_error() -> None:
    """Both APIs must fail the same way on a timeout.

    The sync path let a raw `subprocess.TimeoutExpired` escape while the async
    path raised `KaniscopeError`, so a caller following the documented
    `except KaniscopeError` would have caught one and not the other.
    """
    binary = fake_binary("sleep 30")
    try:
        kaniscope.review(binary=binary, timeout=0.5)
        check("a sync timeout raises KaniscopeError", False, "no error raised")
    except kaniscope.KaniscopeError:
        check("a sync timeout raises KaniscopeError", True)
    except BaseException as err:  # noqa: BLE001 - the point is which type escapes
        check("a sync timeout raises KaniscopeError", False, type(err).__name__)
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


def test_env_none_unsets() -> None:
    """``None`` in ``env`` must UNSET, matching the Node client.

    Node drops an ``undefined`` env value, so the same gesture unset the variable
    there and silently left the inherited one in place here. Verified against
    both runtimes before choosing which way to converge.
    """
    os.environ["KANISCOPE_TEST_VAR"] = "inherited"
    unset = kaniscope._environment({"KANISCOPE_TEST_VAR": None}, True)
    kept = kaniscope._environment({"KANISCOPE_TEST_VAR": "override"}, True)
    check(
        "env None unsets an inherited variable",
        "KANISCOPE_TEST_VAR" not in unset and kept["KANISCOPE_TEST_VAR"] == "override",
    )
    del os.environ["KANISCOPE_TEST_VAR"]


def test_diff_reaches_stdin() -> None:
    """``local=True`` with no ``base`` reads the diff from stdin.

    Unreachable through this API until ``diff`` existed, and stdin must not be
    inherited either: a webhook server has no diff on its stdin, so the child
    would block on a read that never returns.
    """
    # A heredoc, not a quoted printf: inside an unquoted heredoc a `"` is
    # literal, so the JSON survives the shell intact. One binary echoes what it
    # read; the other only counts it, because the big-diff case must not embed
    # raw newlines in a JSON string field.
    body = (
        '{"model":"m","findings":0,"inlinePosted":0,"posted":false,"pr":0,'
        '"provider":"p","recommendation":"%s","repo":"r","summaryMarkdown":""}'
    )
    echoes = fake_binary("D=$(cat)\ncat <<EOF\n" + (body % "[$D]") + "\nEOF")
    counts = fake_binary("N=$(wc -c | tr -d ' ')\ncat <<EOF\n" + (body % "$N") + "\nEOF")
    try:
        sync = kaniscope.review(binary=echoes, local=True, diff="THE DIFF")
        closed = kaniscope.review(binary=echoes, local=True)
        check("sync diff reaches stdin", sync["recommendation"] == "[THE DIFF]", sync["recommendation"])
        check("no diff means a closed stdin", closed["recommendation"] == "[]", closed["recommendation"])

        # Past the OS pipe buffer (64 KiB on Linux, 8-64 KiB on macOS), which is
        # where a write-then-read implementation deadlocks instead of finishing.
        big = "diff --git a/x b/x\n" + ("+line\n" * 40000)
        got = asyncio.run(kaniscope.review_async(binary=counts, local=True, diff=big))
        check(
            "a diff past the pipe buffer does not deadlock",
            got["recommendation"] == str(len(big.encode())),
            f"{len(big) // 1024} KiB delivered, child counted {got['recommendation']}",
        )
    finally:
        os.unlink(echoes)
        os.unlink(counts)


def test_typed_config_maps_to_env() -> None:
    """`config` is a typed façade over the same variables `env` carries."""
    e = kaniscope._environment(None, False, {"min_confidence": 70, "agentic": True,
                                             "exclude_globs": ["a/**", "b/**"]})
    check(
        "config coerces by kind",
        e == {"MIN_CONFIDENCE": "70", "AGENTIC": "true", "EXCLUDE_GLOBS": "a/**,b/**"},
        str(e),
    )


def test_env_beats_config() -> None:
    """The raw escape hatch has to win, or it is not an escape hatch.

    `config` cannot model everything and can model something wrongly; `env` is
    what remains when it does.
    """
    e = kaniscope._environment({"MIN_CONFIDENCE": "99"}, False, {"min_confidence": 70})
    check("env overrides config", e.get("MIN_CONFIDENCE") == "99", str(e))


def test_unknown_config_key_is_rejected() -> None:
    """Same reasoning as an unknown kwarg: a silently dropped `min_confidence`
    ships every nit, and nothing tells you."""
    try:
        kaniscope._environment(None, False, {"minConfidence": 70})  # camelCase typo
        check("an unknown config key raises", False, "it was accepted")
    except TypeError:
        check("an unknown config key raises", True)


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
    test_sync_timeout_raises_kaniscope_error()
    test_large_single_line_stdout()
    test_env_none_unsets()
    test_typed_config_maps_to_env()
    test_env_beats_config()
    test_unknown_config_key_is_rejected()
    test_diff_reaches_stdin()
    test_nonzero_exit_carries_the_reason()
    print()
    if FAILURES:
        print(f"{len(FAILURES)} failure(s): {', '.join(FAILURES)}")
        sys.exit(1)
    print("python client: all checks passed")
