"use strict";

// Where the native `kaniscope` binary is, for this host.
//
// The binary is not in this package. It ships in one small per-platform package
// each declaring `os`/`cpu`, listed here as `optionalDependencies` — so npm
// installs exactly the one matching the host and skips the rest, and a Windows
// developer never downloads a Linux binary. This is esbuild's arrangement, and it
// is the reason this package works without a postinstall script that downloads
// from the network (blocked in a lot of CI, and a supply-chain surface besides).

const path = require("node:path");

/** npm's `process.platform`-`process.arch` pair, as it appears in the package name. */
function platformKey() {
  return `${process.platform}-${process.arch}`;
}

const SUPPORTED = new Set([
  "darwin-arm64",
  "darwin-x64",
  "linux-arm64",
  "linux-x64",
  "win32-x64",
]);

/**
 * Absolute path to the `kaniscope` executable.
 *
 * Throws with the actual reason rather than letting a spawn fail later with
 * ENOENT: by far the most common cause is an install with
 * `--no-optional`/`--omit=optional`, which silently skips every platform package
 * and leaves this one intact but inert. That produces a package that is present
 * and unusable, and "ENOENT: kaniscope" points at nothing.
 */
function binaryPath() {
  // An explicit override wins, always, and is therefore checked FIRST — before
  // the supported-platform test below. The unsupported-platform error tells you
  // to build from source and set this variable, so reading it only after that
  // check makes the advertised escape hatch unreachable on precisely the
  // platforms it exists for. It is also how you point at a locally built binary
  // or an air-gapped vendored copy without editing inside node_modules.
  const override = process.env.KANISCOPE_BINARY_PATH;
  if (override) return override;

  const key = platformKey();
  if (!SUPPORTED.has(key)) {
    throw new Error(
      `kaniscope has no prebuilt binary for ${key}. Supported: ${[...SUPPORTED].join(", ")}. ` +
        `Build from source with \`cargo install pr-review-core --features cli\`, and set ` +
        `KANISCOPE_BINARY_PATH to the result.`
    );
  }

  // Scoped, matching what the generator publishes — see the note there on npm's
  // spam detection. The wrapper itself stays unscoped.
  const pkg = `@nhatvu148/kaniscope-${key}`;
  const exe = process.platform === "win32" ? "kaniscope.exe" : "kaniscope";

  // Resolve the package's manifest, not the binary: `bin/` is not an export, and
  // `require.resolve` on a bare non-JS path fails under some resolvers.
  //
  // Two search roots, in this order. Normally the first is the only one that
  // matters — a real install puts this package and its platform package side by
  // side in one `node_modules`, and pnpm's layout resolves the same way. The
  // second covers the case where this package is a SYMLINK: `npm link`, a
  // `file:` dependency, or a monorepo workspace. Resolution then starts from the
  // link target — outside the tree that holds the binary — and the first attempt
  // fails on a perfectly good install. Version skew is not a risk in the
  // fallback, because the wrapper pins its platform packages to one exact
  // version rather than a range.
  for (const from of [null, process.cwd()]) {
    try {
      const manifest = from
        ? require.resolve(`${pkg}/package.json`, { paths: [from] })
        : require.resolve(`${pkg}/package.json`);
      return path.join(path.dirname(manifest), "bin", exe);
    } catch {
      // Try the next root; the throw below reports the failure once.
    }
  }

  throw new Error(
    `kaniscope could not find its native binary (package ${pkg} is not installed). ` +
      `If you installed with --no-optional or --omit=optional, reinstall without it: ` +
      `the binary ships in an optional per-platform package.`
  );
}

module.exports = { binaryPath, platformKey };
