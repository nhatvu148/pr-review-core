// Build the per-platform npm packages that carry the binaries, and keep the
// wrapper package's version in lockstep with the crate.
//
//   node packaging/npm/build-platform-packages.mjs --bins <dir>   # build for publish
//   node packaging/npm/build-platform-packages.mjs --check        # CI: versions in sync?
//
// `--bins <dir>` expects one subdirectory per Rust target triple, each holding
// the built `kaniscope` (or `kaniscope.exe`) — the layout `actions/download-artifact`
// produces when the build matrix uploads by triple.
//
// The version is read from Cargo.toml and stamped everywhere, never typed. There
// are seven places a release version appears across these packages, and the one
// release step that is a manual edit is the one that gets skipped: this repo has
// already shipped a floating tag pointing at a stale image for exactly that
// reason. `--check` makes a drifted version a red build instead of a bad publish.

import { execFileSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const NPM_DIR = path.dirname(fileURLToPath(import.meta.url));
const ROOT = path.dirname(path.dirname(NPM_DIR));
const OUT_DIR = path.join(NPM_DIR, "platforms");

/** The npm scope the per-platform packages live under.
 *
 * Scoped, because unscoped ones do not survive contact with npm. Publishing five
 * near-identical new names into the global namespace tripped npm's spam
 * detection on the fifth:
 *
 *   403 Forbidden - PUT /kaniscope-win32-x64
 *   Package name triggered spam detection
 *
 * Name-based, not rate-based — an identical retry got the identical error, with
 * the four earlier names already published. A scope sidesteps it because these
 * names no longer compete for the global namespace at all, which is why esbuild
 * and every other project shipping per-platform binaries is scoped too.
 *
 * The WRAPPER stays unscoped: `npm i kaniscope` is the name a user types, and it
 * was never the one npm objected to.
 */
const SCOPE = "@nhatvu148";

/** Rust target triple -> the npm `os`/`cpu` pair that must match it. */
const TARGETS = {
  "aarch64-apple-darwin": { os: "darwin", cpu: "arm64" },
  "x86_64-apple-darwin": { os: "darwin", cpu: "x64" },
  "aarch64-unknown-linux-gnu": { os: "linux", cpu: "arm64" },
  "x86_64-unknown-linux-gnu": { os: "linux", cpu: "x64" },
  "x86_64-pc-windows-msvc": { os: "win32", cpu: "x64" },
};

/** The crate version — the single source of truth for every package here. */
function crateVersion() {
  const toml = fs.readFileSync(path.join(ROOT, "Cargo.toml"), "utf8");
  // The first `version = "..."` after `[package]`, before the next section.
  const pkg = toml.split(/^\[/m).find((s) => s.startsWith("package]"));
  const m = pkg && pkg.match(/^version\s*=\s*"([^"]+)"/m);
  if (!m) throw new Error("could not read version from Cargo.toml");
  return m[1];
}

const version = crateVersion();
const args = process.argv.slice(2);
const check = args.includes("--check");
const binsAt = args.indexOf("--bins");
const bins = binsAt === -1 ? null : args[binsAt + 1];

// --- 1. The wrapper package's own version, and the range it pins each platform
// package at. An exact pin, not a caret: a wrapper must never resolve a binary
// built from a different engine than the types it ships were generated from.
const wrapperPath = path.join(NPM_DIR, "package.json");
const wrapper = JSON.parse(fs.readFileSync(wrapperPath, "utf8"));
const expected = {
  ...wrapper,
  version,
  optionalDependencies: Object.fromEntries(
    Object.values(TARGETS).map(({ os, cpu }) => [`${SCOPE}/kaniscope-${os}-${cpu}`, version])
  ),
};
const serialized = JSON.stringify(expected, null, 2) + "\n";

if (check) {
  const current = fs.readFileSync(wrapperPath, "utf8");
  if (current !== serialized) {
    console.error(
      `packaging/npm/package.json is out of sync with Cargo.toml (${version}).\n` +
        `Run: node packaging/npm/build-platform-packages.mjs`
    );
    process.exit(1);
  }
  console.log(`npm package version is in sync with the crate (${version})`);
  process.exit(0);
}

fs.writeFileSync(wrapperPath, serialized);
console.log(`kaniscope@${version} (wrapper)`);

// --- 2. The platform packages, when binaries were supplied.
if (!bins) {
  console.log("no --bins <dir> given; wrote the wrapper version only");
  process.exit(0);
}

fs.rmSync(OUT_DIR, { recursive: true, force: true });

for (const [triple, { os, cpu }] of Object.entries(TARGETS)) {
  const exe = os === "win32" ? "kaniscope.exe" : "kaniscope";
  const source = path.join(bins, triple, exe);
  if (!fs.existsSync(source)) {
    // Loud, and fatal. A missing platform silently skipped publishes a wrapper
    // whose optionalDependencies name a package that does not exist, and npm
    // treats a missing OPTIONAL dependency as success — so the install is green
    // and the binary is absent, on that platform only.
    console.error(`missing binary for ${triple} at ${source}`);
    process.exit(1);
  }

  // Directory name stays flat — a `@scope/name` path would nest, and the
  // publish loop globs one level.
  const dir = path.join(OUT_DIR, `kaniscope-${os}-${cpu}`);
  fs.mkdirSync(path.join(dir, "bin"), { recursive: true });
  fs.copyFileSync(source, path.join(dir, "bin", exe));
  // npm does not preserve the executable bit from the filesystem for arbitrary
  // files, but it does for files under `bin` of a package with a `bin` field —
  // and this package deliberately has none, because it must not install a
  // command of its own. Set the mode so the tarball carries it.
  if (os !== "win32") fs.chmodSync(path.join(dir, "bin", exe), 0o755);

  fs.writeFileSync(
    path.join(dir, "package.json"),
    JSON.stringify(
      {
        name: `${SCOPE}/kaniscope-${os}-${cpu}`,
        version,
        description: `The kaniscope binary for ${os}-${cpu}. Installed automatically by the \`kaniscope\` package.`,
        license: wrapper.license,
        repository: wrapper.repository,
        // These two fields are the whole mechanism: npm skips an optional
        // dependency whose os/cpu do not match the host, so a machine downloads
        // one binary rather than five.
        os: [os],
        cpu: [cpu],
        files: [`bin/${exe}`],
        preferUnplugged: true,
      },
      null,
      2
    ) + "\n"
  );
  const size = (fs.statSync(source).size / 1024 / 1024).toFixed(1);
  console.log(`${SCOPE}/kaniscope-${os}-${cpu}@${version} (${size} MiB)`);
}

// A published binary that will not start is the worst artifact here, and the one
// platform this machine can actually prove is its own. Cheap, and it has caught
// a stripped-wrong build before.
const self = { darwin: "darwin", linux: "linux", win32: "win32" }[process.platform];
const selfPkg = path.join(OUT_DIR, `kaniscope-${self}-${process.arch}`);
if (fs.existsSync(selfPkg)) {
  const exe = self === "win32" ? "kaniscope.exe" : "kaniscope";
  const reported = execFileSync(path.join(selfPkg, "bin", exe), ["--version"], {
    encoding: "utf8",
  }).trim();
  if (!reported.endsWith(version)) {
    console.error(`host binary reports "${reported}", expected version ${version}`);
    process.exit(1);
  }
  console.log(`verified on this host: ${reported}`);
}
