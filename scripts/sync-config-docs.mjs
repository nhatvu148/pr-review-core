// Sync the README's complete configuration table with the engine's own spec.
//
//   node scripts/sync-config-docs.mjs           # rewrite the section
//   node scripts/sync-config-docs.mjs --check   # CI: fail if it drifted
//
// The table used to be hand-maintained, and it had quietly fallen to 41 of the
// 61 names in use — with no way to tell which 20 were missing, because the
// surface could not be enumerated. `config_spec::SPEC` enumerates it and
// `spec_matches_config_rs` keeps it honest; this puts it in front of a reader.
//
// The curated tables higher up the README stay hand-written on purpose: they
// explain the handful of knobs worth a paragraph. This one guarantees that every
// knob is at least *listed*, which is the property that kept being lost.

import { execFileSync } from "node:child_process";
import { readFileSync, writeFileSync, existsSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";

const ROOT = path.dirname(path.dirname(fileURLToPath(import.meta.url)));
const README = path.join(ROOT, "README.md");
const BEGIN = "<!-- BEGIN GENERATED CONFIG TABLE -->";
const END = "<!-- END GENERATED CONFIG TABLE -->";

const bin = process.env.KANISCOPE_BINARY_PATH || path.join(ROOT, "target", "debug", "kaniscope");
if (!existsSync(bin)) {
  console.error(`no binary at ${bin} — build it:\n  cargo build --features cli --bin kaniscope`);
  process.exit(1);
}
const table = execFileSync(bin, ["--config-docs"], { encoding: "utf8", maxBuffer: 8 << 20 });

const readme = readFileSync(README, "utf8");
const start = readme.indexOf(BEGIN);
const stop = readme.indexOf(END);
if (start === -1 || stop === -1) {
  console.error(`README.md is missing the ${BEGIN} / ${END} markers`);
  process.exit(1);
}

const wanted =
  readme.slice(0, start + BEGIN.length) + "\n" + table + readme.slice(stop);

if (process.argv.includes("--check")) {
  if (wanted !== readme) {
    console.error(
      "README config table is out of date.\n" +
        "Run: cargo build --features cli --bin kaniscope && node scripts/sync-config-docs.mjs"
    );
    process.exit(1);
  }
  console.log("README config table is up to date");
} else {
  writeFileSync(README, wanted);
  console.log(`wrote ${table.trim().split("\n").length - 2} rows into README.md`);
}
