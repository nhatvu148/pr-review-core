#!/usr/bin/env python3
"""Turn the hand-written review-feedback entries into a scoreboard.

`docs/feedback/*.md` records, per reviewed PR, how many findings were confirmed,
how many were false positives, and what was missed. That is precision and recall
data — but it currently only exists as prose, so "is the reviewer getting better?"
can only be answered by re-reading eight files and trusting the reader's memory.

This reads the verdict lines and writes `docs/SCOREBOARD.md`: one row per PR-round,
a running precision figure, and the worst severity any false positive was filed at
(the number that matters most, since a false BLOCKING stops a merge and a false
MEDIUM moves the recommendation). The "where the false positives landed" rollup
is counted per finding from each entry's findings table, not from the verdict
line's single severity tag — see `fp_rollup`.

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
    # Per file, the filed severity of every FALSE_POSITIVE row in its findings
    # tables ("" where the severity cell names none). Read by the rollup only.
    table_fps: dict[str, list[str]] = field(default_factory=dict)


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


def parse_entry(path: Path, text: str | None = None) -> tuple[list[Row], str | None]:
    if text is None:
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


def split_cells(line: str) -> list[str]:
    r"""Cells of one table row. An escaped pipe (`\|`, e.g. inside a quoted regex)
    stays in its cell.

    >>> split_cells(r"| HIGH | `a \| b` | x |")
    ['HIGH', '`a \\| b`', 'x']
    """
    return [c.strip() for c in re.split(r"(?<!\\)\|", line.strip())[1:-1]]


def is_separator(line: str) -> bool:
    """`|---|:--:|` — the row that makes the row above it a header."""
    s = line.strip()
    return s.startswith("|") and "-" in s and set(s) <= set("|-: ")


def filed_severity(cell: str) -> str:
    """The severity a finding was filed at, from its table cell.

    The FIRST severity named, not the worst: a miscalibrated finding is written
    `LOW → **MEDIUM**` (filed, then warranted), and the rollup is about what the
    reviewer said.

    >>> filed_severity("LOW → **MEDIUM**")
    'LOW'
    >>> filed_severity("**BLOCKING**")
    'BLOCKING'
    >>> filed_severity("—")
    ''
    """
    up = strip_md(cell).upper()
    hits = [(up.find(s), s) for s in SEVERITIES if s in up]
    return min(hits)[1] if hits else ""


def table_fp_severities(lines: list[str]) -> list[str]:
    """Filed severity of every FALSE_POSITIVE row in the entry's findings tables.

    Only tables whose header names both a `severity` and a `verdict` column
    count, which skips the misses and regression tables that share the page.
    A header is the row directly above a separator, as Markdown defines it, so a
    second table's rows are never read against the first table's columns.

    >>> table_fp_severities([
    ...     "| severity | verdict | finding |",
    ...     "|---|---|---|",
    ...     "| HIGH | **FALSE_POSITIVE** | a |",
    ...     "| LOW | CONFIRMED | b |",
    ...     "| defect | verdict |",
    ...     "|---|---|",
    ...     "| MEDIUM | FALSE_POSITIVE |",
    ... ])
    ['HIGH']
    """
    out: list[str] = []
    header: list[str] | None = None
    for i, raw in enumerate(lines):
        if not raw.startswith("|"):
            header = None
            continue
        if is_separator(raw):
            continue
        cells = split_cells(raw)
        if i + 1 < len(lines) and is_separator(lines[i + 1]):
            low = [c.lower() for c in cells]
            header = low if "severity" in low and "verdict" in low else None
            continue
        if header is None:
            continue
        row = dict(zip(header, cells))
        verdict = re.sub(r"[\s*`]+", "_", row.get("verdict", "").upper())
        if "FALSE_POSITIVE" in verdict:
            out.append(filed_severity(row.get("severity", "")))
    return out


def fp_rollup(parsed: Parsed) -> tuple[dict[str, int], list[str]]:
    """False positives by the severity each one was filed at.

    Itemised from the findings tables, one count per row, wherever a file's
    tables hold exactly as many FALSE_POSITIVE rows as its verdict lines claim.
    Where they disagree — a false positive described only in prose, or one note
    whose table covers a second PR scored in its own entry — the file falls back
    to crediting each verdict line's whole count to its one severity tag, and is
    returned in the second value so the output can say so.

    The fallback is the old behaviour everywhere, and it is wrong for a line
    whose false positives were filed at different severities: tagged, they all
    land on the tag; untagged, they all land on "unlabelled". Itemising is what
    lets an entry state its counts plainly instead of working around that.
    """
    counts = {s: 0 for s in (*SEVERITIES, "")}
    fallback: list[str] = []
    by_file: dict[str, list[Row]] = {}
    for r in parsed.rows:
        by_file.setdefault(r.source, []).append(r)
    for name, rows in by_file.items():
        claimed = sum(r.false_positive for r in rows)
        itemised = parsed.table_fps.get(name, [])
        if len(itemised) == claimed:
            for sev in itemised:
                counts[sev] += 1
            continue
        for r in rows:
            counts[r.worst_fp_severity] += r.false_positive
        if claimed or itemised:
            fallback.append(f"{name} — verdict lines claim {claimed}, tables list {len(itemised)}")
    return counts, fallback


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

    by_sev, fallback = fp_rollup(parsed)
    for sev in SEVERITIES:
        if by_sev[sev]:
            out.append(f"- **{sev}**: {by_sev[sev]}")
    if by_sev[""]:
        out.append(f"- unlabelled: {by_sev['']}")
    if not any(by_sev.values()):
        out.append("- none recorded")
    if fallback:
        out += [
            "",
            "Counted per finding from each entry's findings table. These entries' tables",
            "do not match their verdict lines, so their verdict-line counts are used",
            "instead, credited to the line's severity tag:",
            "",
        ]
        out += [f"- `{f}`" for f in fallback]

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
        text = path.read_text(encoding="utf-8")
        rows, why = parse_entry(path, text)
        parsed.rows.extend(rows)
        parsed.table_fps[path.name] = table_fp_severities(text.splitlines())
        if why:
            parsed.unparsed.append((path.name, why))

    args.out.write_text(render(parsed), encoding="utf-8")
    print(f"{args.out}: {len(parsed.rows)} round(s) scored, {len(parsed.unparsed)} unscored")
    for name, why in parsed.unparsed:
        print(f"  unscored: {name} — {why}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
