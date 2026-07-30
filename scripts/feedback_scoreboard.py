#!/usr/bin/env python3
"""Turn the hand-written review-feedback entries into a scoreboard.

`docs/feedback/*.md` records, per reviewed PR, how many findings were confirmed,
how many were false positives, and what was missed. That is precision and recall
data — but it currently only exists as prose, so "is the reviewer getting better?"
can only be answered by re-reading eight files and trusting the reader's memory.

This reads the verdict lines and writes `docs/SCOREBOARD.md`: one row per PR-round,
a running precision figure, and the worst severity any false positive was filed at
(the number that matters most, since a false BLOCKING stops a merge and a false
MEDIUM moves the recommendation).

Deliberately tolerant: entries are written by hand and their headers vary. A file
whose verdict cannot be parsed is REPORTED, never skipped silently — an entry that
falls out of the scoreboard would quietly flatter the numbers.

Usage:
    python3 scripts/feedback_scoreboard.py [--feedback-dir DIR] [--out FILE]
"""

from __future__ import annotations

import argparse
import re
import sys
from dataclasses import dataclass, field
from pathlib import Path

SEVERITIES = ("BLOCKING", "HIGH", "MEDIUM", "LOW")

ROUND = re.compile(r"^round[\s\-]*(\d+)", re.I)
TITLE = re.compile(r"^#\s+(?P<repo>[\w.\-]+/[\w.\-]+)#(?P<pr>\d+)")

COUNTS = {
    "confirmed": re.compile(r"(\d+)\s+confirmed", re.I),
    "false_positive": re.compile(r"(\d+)\s+false[\s\-]?positive", re.I),
    "missed": re.compile(r"(\d+)\s+(?:in[\s\-]reach\s+)?(?:missed|misses|miss)\b", re.I),
    "out_of_scope": re.compile(r"(\d+)\s+out[\s\-]of[\s\-]scope", re.I),
}


@dataclass
class Row:
    date: str
    repo: str
    pr: str
    round: str
    confirmed: int = 0
    false_positive: int = 0
    missed: int = 0
    out_of_scope: int = 0
    worst_fp_severity: str = ""
    source: str = ""


@dataclass
class Parsed:
    rows: list[Row] = field(default_factory=list)
    unparsed: list[tuple[str, str]] = field(default_factory=list)  # (file, why)


def strip_md(text: str) -> str:
    """Drop bold/inline-code markers so counts read the same either way."""
    return text.replace("**", "").replace("`", "")


def is_verdict_line(line: str) -> bool:
    """Does this line state a verdict?

    Entries are hand-written and their headers vary, so this keys on meaning rather
    than a fixed shape. Every form seen so far:

        Reviewed: 2026-07-27 · Verdict: 3 confirmed, 0 false positive, 0 misses
        Round 1 verdict: 0 confirmed, 2 false positive (both BLOCKING), 0 missed
        Round 2: 0 confirmed, 0 false positive, 7 out-of-scope (all LOW), 0 missed

    Requiring at least one count is what keeps prose out — a section heading like
    "Round 2 — re-review after the refutation" announces a round but scores nothing.
    """
    low = line.lower()
    looks_like_a_header = (
        low.startswith("reviewed:") or low.startswith("round") or "verdict" in low
    )
    return looks_like_a_header and any(p.search(line) for p in COUNTS.values())


def fp_severity(line: str) -> str:
    """Severity a false positive was filed at, e.g. '(both BLOCKING)' -> BLOCKING.

    Read from the clause *after* the false-positive count so a severity attached to
    some other clause ('7 out-of-scope (all LOW)') is not misattributed.
    """
    m = COUNTS["false_positive"].search(line)
    if not m or int(m.group(1)) == 0:
        return ""  # no false positive to have a severity
    # Stop at the next clause: "0 false positive, 7 out-of-scope (all LOW)" must not
    # report LOW as a false-positive severity.
    tail = re.split(r"[,·]", line[m.end() :], maxsplit=1)[0]
    for sev in SEVERITIES:  # most severe first
        if sev in tail.upper():
            return sev
    return ""


def parse_entry(path: Path) -> tuple[list[Row], str | None]:
    text = path.read_text(encoding="utf-8")
    lines = text.splitlines()

    title = next((TITLE.match(l) for l in lines if TITLE.match(l)), None)
    repo = title.group("repo") if title else path.stem
    pr = title.group("pr") if title else "?"
    date_m = re.match(r"(\d{4}-\d{2}-\d{2})", path.stem)
    date = date_m.group(1) if date_m else ""

    rows: list[Row] = []
    for raw in lines:
        line = strip_md(raw).strip()
        if not is_verdict_line(line):
            continue
        rnd = ROUND.match(line)
        row = Row(
            date=date,
            repo=repo,
            pr=pr,
            round=f"r{rnd.group(1)}" if rnd else "—",
            worst_fp_severity=fp_severity(line),
            source=path.name,
        )
        for field_name, pattern in COUNTS.items():
            m = pattern.search(line)
            if m:
                setattr(row, field_name, int(m.group(1)))
        rows.append(row)

    if not rows:
        return [], "no parseable verdict line"
    return rows, None


def sort_key(r: Row):
    """Order rows without assuming the PR number parsed.

    `r.pr` is `"?"` when the entry's heading didn't match `TITLE` — a hand-written
    file is one typo away from that, and `int("?")` used to take the whole script
    down with it. Unparsed PRs sort last within their repo, and are called out in
    the output rather than passing as ordinary rows.
    """
    numeric = int(r.pr) if r.pr.isdigit() else 0
    return (r.date, r.repo, 0 if r.pr.isdigit() else 1, numeric, r.round)


def render(parsed: Parsed) -> str:
    rows = sorted(parsed.rows, key=sort_key)

    out = [
        "# Reviewer scoreboard",
        "",
        "Generated by `scripts/feedback_scoreboard.py` from `docs/feedback/*.md`.",
        "Do not edit by hand — edit the entries and re-run.",
        "",
        "Precision = confirmed / (confirmed + false positives). Recall is NOT here:",
        "a miss only enters an entry when someone found it later, so the misses column",
        "is a floor, never a rate.",
        "",
        "| date | PR | round | confirmed | false pos | worst FP | out of scope | missed | precision |",
        "|---|---|---|---|---|---|---|---|---|",
    ]

    tc = tfp = tmiss = toos = 0
    for r in rows:
        judged = r.confirmed + r.false_positive
        prec = f"{100 * r.confirmed / judged:.0f}%" if judged else "—"
        out.append(
            f"| {r.date} | {r.repo}#{r.pr} | {r.round} | {r.confirmed} | {r.false_positive} "
            f"| {r.worst_fp_severity or '—'} | {r.out_of_scope} | {r.missed} | {prec} |"
        )
        tc += r.confirmed
        tfp += r.false_positive
        tmiss += r.missed
        toos += r.out_of_scope

    judged = tc + tfp
    overall = f"{100 * tc / judged:.0f}%" if judged else "—"
    out += [
        f"| **total** | {len(rows)} rounds | | **{tc}** | **{tfp}** | | {toos} | {tmiss} | **{overall}** |",
        "",
        "## Where the false positives landed",
        "",
    ]

    by_sev = {s: sum(r.false_positive for r in rows if r.worst_fp_severity == s) for s in SEVERITIES}
    unlabelled = sum(r.false_positive for r in rows if not r.worst_fp_severity)
    for sev in SEVERITIES:
        if by_sev[sev]:
            out.append(f"- **{sev}**: {by_sev[sev]}")
    if unlabelled:
        out.append(f"- unlabelled: {unlabelled}")
    if not any(by_sev.values()) and not unlabelled:
        out.append("- none recorded")

    # Scored, but the heading didn't parse — say so rather than let a row with a
    # bare "?" for its PR pass as ordinary.
    headless = sorted({r.source for r in rows if not r.pr.isdigit()})
    if headless:
        out += [
            "",
            "## Entries with an unreadable heading",
            "",
            "Their verdicts ARE counted above, but the `# owner/repo#N — title` heading",
            "could not be parsed, so the PR column shows `?`. Fix the heading to get a",
            "real row.",
            "",
        ]
        out += [f"- `{name}`" for name in headless]

    if parsed.unparsed:
        out += [
            "",
            "## Entries not scored",
            "",
            "These carry no parseable verdict line. They are listed so the totals above",
            "cannot silently omit an entry.",
            "",
        ]
        out += [f"- `{name}` — {why}" for name, why in parsed.unparsed]

    return "\n".join(out) + "\n"


def main() -> int:
    root = Path(__file__).resolve().parent.parent
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--feedback-dir", type=Path, default=root / "docs" / "feedback")
    ap.add_argument("--out", type=Path, default=root / "docs" / "SCOREBOARD.md")
    args = ap.parse_args()

    if not args.feedback_dir.is_dir():
        print(f"no feedback directory at {args.feedback_dir}", file=sys.stderr)
        return 1

    parsed = Parsed()
    for path in sorted(args.feedback_dir.glob("*.md")):
        rows, why = parse_entry(path)
        parsed.rows.extend(rows)
        if why:
            parsed.unparsed.append((path.name, why))

    args.out.write_text(render(parsed), encoding="utf-8")
    print(f"{args.out}: {len(parsed.rows)} round(s) scored, {len(parsed.unparsed)} unscored")
    for name, why in parsed.unparsed:
        print(f"  unscored: {name} — {why}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
