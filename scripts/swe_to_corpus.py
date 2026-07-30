#!/usr/bin/env python3
"""Build a bench_local corpus from a public SWE-bench-style bug-fix dataset.

A fix commit is a ground-truth annotation: reverse the gold fix patch to get a
"bug-introducing" diff, and the patched (new-side) lines become the known issues.
No hand-annotation, no repo checkout — just the `patch` field each instance ships.

Reads rows from the Hugging Face datasets-server (no auth) and writes a JSON array
in the format examples/bench_local.rs expects.

Usage:
  # Python (SWE-bench Lite)
  python3 scripts/swe_to_corpus.py --dataset princeton-nlp/SWE-bench_Lite \
      --split test --limit 50 --out corpus-swe-py.json

  # Multilingual (Go / Rust / JS / TS / Java / C++ / Ruby / PHP)
  python3 scripts/swe_to_corpus.py --dataset SWE-bench/SWE-bench_Multilingual \
      --split test --limit 100 --out corpus-swe-multi.json

Language is derived from the repo (see REPO_LANG); unknown repos fall back to
extension-based inference from the patched file paths.
"""
import argparse
import json
import re
import sys
import time
import urllib.request

ROWS_URL = "https://datasets-server.huggingface.co/rows"

# Known repos in SWE-bench_Multilingual / Lite -> language label.
REPO_LANG = {
    "caddyserver/caddy": "go",
    "gin-gonic/gin": "go",
    "astral-sh/ruff": "rust",
    "burntsushi/ripgrep": "rust",
    "axios/axios": "javascript",
    "babel/babel": "javascript",
    "facebook/docusaurus": "typescript",
    "apache/lucene": "java",
    "apache/druid": "java",
    "fmtlib/fmt": "cpp",
    "briannesbitt/carbon": "php",
    "fluent/fluentd": "ruby",
    "faker-ruby/faker": "ruby",
    "fastlane/fastlane": "ruby",
}

EXT_LANG = {
    ".py": "python", ".rs": "rust", ".go": "go", ".ts": "typescript",
    ".tsx": "typescript", ".js": "javascript", ".jsx": "javascript",
    ".java": "java", ".rb": "ruby", ".php": "php", ".c": "c",
    ".cc": "cpp", ".cpp": "cpp", ".h": "cpp", ".hpp": "cpp",
}

# Our reviewer's tree-sitter languages — instances outside these are still scored,
# but this lets --only filter to what structural context/complexity can parse.
CORE_LANGS = {"rust", "typescript", "javascript", "python", "go"}


def fetch_rows(dataset, config, split, limit):
    """Page the datasets-server 100 rows at a time up to `limit`."""
    out = []
    offset = 0
    while len(out) < limit:
        n = min(100, limit - len(out))
        url = f"{ROWS_URL}?dataset={dataset}&config={config}&split={split}&offset={offset}&length={n}"
        req = urllib.request.Request(url, headers={"User-Agent": "bench-corpus-builder"})
        with urllib.request.urlopen(req, timeout=60) as r:
            data = json.load(r)
        rows = data.get("rows", [])
        if not rows:
            break
        out.extend(x["row"] for x in rows)
        offset += len(rows)
        if len(rows) < n:
            break
        time.sleep(0.3)  # be polite to the public endpoint
    return out


HUNK_RE = re.compile(r"^@@ -\d+(?:,\d+)? \+(\d+)(?:,\d+)? @@")
NEWFILE_RE = re.compile(r"^\+\+\+ b/(.+)$")


def reverse_patch(patch):
    """Reverse a unified diff: swap ---/+++ and +/- so the gold *fix* becomes the
    *bug-introducing* change. `git apply -R` semantics, done textually so we need no
    working tree. Returns (reversed_diff, [(file, new_side_line), ...])."""
    out_lines = []
    issues = []  # (file, line) on the reversed new side = the introduced-bug lines
    cur_file = None
    new_line = 0
    for line in patch.splitlines():
        if line.startswith("diff --git"):
            out_lines.append(line)
        elif line.startswith("--- a/"):
            # old '---a' becomes new '+++'
            out_lines.append("+++ b/" + line[len("--- a/"):])
        elif line.startswith("+++ b/"):
            cur_file = line[len("+++ b/"):]
            out_lines.append("--- a/" + cur_file)
        elif line.startswith("@@"):
            # After reversal, the fix's '-' (original/buggy) lines become '+' adds.
            # Recompute the new-side start from the ORIGINAL old-side (-) range.
            m = re.match(r"^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@(.*)$", line)
            if m:
                old_start, new_start, tail = m.group(1), m.group(2), m.group(3)
                # reversed: new side is the original old side
                out_lines.append(f"@@ -{new_start} +{old_start} @@{tail}")
                new_line = int(old_start)
            else:
                out_lines.append(line)
        elif line.startswith("+"):
            # a fix-added line -> becomes a removed line in the reversed diff
            out_lines.append("-" + line[1:])
        elif line.startswith("-"):
            # a fix-removed (original/buggy) line -> becomes an added line; this is
            # the bug being (re)introduced, so record it as a ground-truth issue.
            out_lines.append("+" + line[1:])
            if cur_file:
                issues.append((cur_file, new_line))
            new_line += 1
        elif line.startswith(" "):
            out_lines.append(line)
            new_line += 1
        # index/mode/other headers are dropped; not needed for review
    return "\n".join(out_lines) + "\n", issues


def infer_lang(repo, files):
    if repo in REPO_LANG:
        return REPO_LANG[repo]
    for f in files:
        for ext, lang in EXT_LANG.items():
            if f.endswith(ext):
                return lang
    return "unknown"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--dataset", required=True)
    ap.add_argument("--config", default="default")
    ap.add_argument("--split", default="test")
    ap.add_argument("--limit", type=int, default=50)
    ap.add_argument("--out", required=True)
    ap.add_argument("--only-core", action="store_true",
                    help="keep only rust/ts/js/python/go (what our tree-sitter parses)")
    # Outlier bounds. A reverse-patched refactor can carry hundreds of changed
    # lines, every one of which becomes a "known issue" — but no reviewer is
    # expected to flag them all, so such a case can only ever score near-zero
    # recall while dominating the aggregate and the token bill. Measured on
    # SWE-bench_Multilingual: median 2 issues/case, mean 25.8, max 1617, and one
    # 205 KB diff (larger than the reviewer's own MAX_DIFF_CHARS).
    ap.add_argument("--max-issues", type=int, default=0,
                    help="drop cases with more than N known issues (0 = no limit)")
    ap.add_argument("--max-diff-chars", type=int, default=0,
                    help="drop cases whose diff exceeds N characters (0 = no limit)")
    args = ap.parse_args()

    rows = fetch_rows(args.dataset, args.config, args.split, args.limit)
    print(f"fetched {len(rows)} instance(s) from {args.dataset}", file=sys.stderr)

    corpus = []
    skipped = 0
    for r in rows:
        patch = r.get("patch") or ""
        if not patch.strip():
            skipped += 1
            continue
        diff, issues = reverse_patch(patch)
        if not issues:
            skipped += 1  # pure-addition fix has no removed (buggy) line to flag
            continue
        # Dedupe issue lines per file, keep the earliest per hunk for clarity.
        seen = set()
        uniq = []
        for f, ln in issues:
            key = (f, ln)
            if key not in seen:
                seen.add(key)
                uniq.append({"file": f, "line": ln})
        files = sorted({i["file"] for i in uniq})
        lang = infer_lang(r.get("repo", ""), files)
        if args.only_core and lang not in CORE_LANGS:
            skipped += 1
            continue
        if args.max_issues and len(uniq) > args.max_issues:
            skipped += 1  # a mass refactor, not a scoreable bug
            continue
        if args.max_diff_chars and len(diff) > args.max_diff_chars:
            skipped += 1
            continue
        corpus.append({
            "id": r.get("instance_id", ""),
            "lang": lang,
            "repo": r.get("repo", ""),
            "diff": diff,
            "issues": uniq,
        })

    with open(args.out, "w") as f:
        json.dump(corpus, f, indent=2)
    langs = {}
    for c in corpus:
        langs[c["lang"]] = langs.get(c["lang"], 0) + 1
    print(f"wrote {len(corpus)} case(s) to {args.out} "
          f"({sum(len(c['issues']) for c in corpus)} issues) · skipped {skipped}",
          file=sys.stderr)
    print(f"languages: {langs}", file=sys.stderr)


if __name__ == "__main__":
    main()
