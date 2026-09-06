// Generate the wrapper packages' types from the binary's own `--schema`.
//
// The reviewer's wire shape is declared in several places already, and the
// failure that costs the most is the quiet one: a field added to `Finding` in
// Rust and missed in a client ships the feature dark, with every test still
// green, because a missing key in JSON is just `undefined`. Generating both
// clients from the schema and checking the result into git turns that into a
// diff — CI regenerates and fails if the committed types moved.
//
//   node packaging/generate-types.mjs            # regenerate from the built binary
//   node packaging/generate-types.mjs --check    # fail if the committed files differ
//
// Deliberately dependency-free. It handles exactly the JSON Schema that schemars
// emits for these types and refuses anything else, which is a better trade than a
// general-purpose generator: the refusal is a build failure the day the schema
// grows a shape nobody has thought about, rather than plausible-looking types.

import { execFileSync } from "node:child_process";
import { readFileSync, writeFileSync, existsSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";

const ROOT = path.dirname(path.dirname(fileURLToPath(import.meta.url)));
const CHECK = process.argv.includes("--check");

const OUTPUTS = {
  schema: path.join(ROOT, "packaging", "schema.json"),
  ts: path.join(ROOT, "packaging", "npm", "types.d.ts"),
  py: path.join(ROOT, "packaging", "pypi", "python", "kaniscope", "_types.py"),
};

/** The schema, from the built binary — the only source that cannot be stale. */
function loadSchema() {
  const bin = process.env.KANISCOPE_BINARY_PATH || path.join(ROOT, "target", "debug", "kaniscope");
  if (!existsSync(bin)) {
    throw new Error(
      `no binary at ${bin} — build it first:\n  cargo build --features cli --bin kaniscope`
    );
  }
  return JSON.parse(execFileSync(bin, ["--schema"], { encoding: "utf8", maxBuffer: 32 << 20 }));
}

/**
 * Split a schemars property into `{ types, nullable }`.
 *
 * Nullable and optional are DIFFERENT things and this only answers the first.
 * `Option<String>` with no `skip_serializing_if` serializes as an explicit
 * `"commentUrl": null` — the key is present and its value is null — so a client
 * typed `commentUrl?: string` is simply wrong about what arrives on the wire.
 * Optionality (is the key there at all?) comes from the schema's `required`
 * list instead, in [`objects`].
 *
 * Both shapes have to be read, because schemars emits nullability two ways: an
 * `anyOf: [T, {type: "null"}]`, and a bare `type: ["string", "null"]`. Reading
 * only the first dropped null from every field in this schema, which is exactly
 * how a generated client ends up confidently wrong.
 */
function variants(node) {
  const branches = node.anyOf || node.oneOf || [node];
  let nullable = branches.length !== branches.filter((b) => b.type !== "null").length;

  const types = branches
    .filter((b) => b.type !== "null")
    .map((b) => {
      if (!Array.isArray(b.type)) return b;
      if (b.type.includes("null")) nullable = true;
      return { ...b, type: b.type.find((x) => x !== "null") };
    });

  return { types, nullable };
}

/** Wrap a rendered type so it also admits null. */
function nullableOf(inner, lang) {
  return lang === "ts" ? `${inner} | null` : `Optional[${inner}]`;
}

/** Map one non-null schema node to a language type via `prims` + `ref`/`array`. */
function render(node, lang) {
  if (node.$ref) return node.$ref.replace("#/$defs/", "");
  if (node.type === "array") {
    const item = variants(node.items);
    const inner = item.nullable
      ? nullableOf(render(item.types[0], lang), lang)
      : render(item.types[0], lang);
    return lang === "ts" ? `${inner}[]` : `List[${inner}]`;
  }
  const prims =
    lang === "ts"
      ? { string: "string", integer: "number", number: "number", boolean: "boolean" }
      : { string: "str", integer: "int", number: "float", boolean: "bool" };
  const t = Array.isArray(node.type) ? node.type.find((x) => x !== "null") : node.type;
  const mapped = prims[t];
  if (!mapped) {
    throw new Error(`unhandled schema node (no mapping for ${JSON.stringify(node)})`);
  }
  return mapped;
}

/**
 * A one-line summary of a rustdoc comment, for the generated types.
 *
 * The first *sentence*, not the first line: rustdoc wraps prose at the column,
 * so taking a line yields "The post-processed findings that were posted (after
 * self-critique," — a doc comment that stops mid-clause is worse than none.
 */
function summarize(description) {
  const text = (description || "").replace(/\s+/g, " ").trim();
  if (!text) return "";
  // Split on a period that ends a sentence, not one inside `crate::path` or a
  // decimal. Falls back to the whole (collapsed) text when there is no period.
  const end = text.search(/\.(\s|$)/);
  const first = end === -1 ? text : text.slice(0, end + 1);
  return first.length > 200 ? first.slice(0, 197).trimEnd() + "..." : first;
}

/** Every named object in the schema: the root plus each `$defs` entry. */
function objects(schema) {
  return [
    [schema.title, schema],
    ...Object.entries(schema.$defs || {}),
  ].map(([name, node]) => {
    if (!node.properties) throw new Error(`${name} is not an object schema`);
    const required = new Set(node.required || []);
    const fields = Object.entries(node.properties).map(([key, prop]) => {
      const { types, nullable } = variants(prop);
      if (types.length !== 1) {
        throw new Error(`${name}.${key}: expected one non-null variant, got ${types.length}`);
      }
      const ts = render(types[0], "ts");
      const py = render(types[0], "py");
      return {
        key,
        doc: summarize(prop.description),
        // Whether the KEY can be absent. Distinct from whether its VALUE can be
        // null: a `#[serde(default)]` field is optional and not nullable, an
        // `Option<T>` that serializes is nullable and not optional, and a field
        // can be both. Collapsing the two mislabels every one of those cases.
        optional: !required.has(key),
        ts: nullable ? nullableOf(ts, "ts") : ts,
        py: nullable ? nullableOf(py, "py") : py,
      };
    });
    return { name, doc: summarize(node.description), fields };
  });
}

const BANNER = (cmd) =>
  `// GENERATED by packaging/generate-types.mjs — do not edit.\n// Regenerate with: ${cmd}\n`;

function emitTs(defs) {
  let out = BANNER("node packaging/generate-types.mjs") + "\n";
  for (const def of defs) {
    if (def.doc) out += `/** ${def.doc} */\n`;
    out += `export interface ${def.name} {\n`;
    for (const f of def.fields) {
      if (f.doc) out += `  /** ${f.doc} */\n`;
      out += `  ${f.key}${f.optional ? "?" : ""}: ${f.ts};\n`;
    }
    out += "}\n\n";
  }
  return out;
}

function emitPy(defs) {
  let out =
    BANNER("node packaging/generate-types.mjs").replaceAll("//", "#") +
    "\nfrom __future__ import annotations\n\nfrom typing import List, Optional, TypedDict\n\n";
  for (const def of defs) {
    // `total=False` on a second class, not `NotRequired` inline: this has to
    // import on the oldest Python the wheel claims, and `NotRequired` is 3.11+
    // outside typing_extensions, which is a dependency this package will not add.
    const req = def.fields.filter((f) => !f.optional);
    const opt = def.fields.filter((f) => f.optional);
    const base = opt.length ? `_${def.name}Required` : def.name;
    out += `class ${base}(TypedDict):\n`;
    if (def.doc && !opt.length) out += `    """${def.doc}"""\n\n`;
    if (!req.length) out += "    pass\n";
    for (const f of req) out += `    ${f.key}: ${f.py}\n`;
    out += "\n\n";
    if (opt.length) {
      out += `class ${def.name}(${base}, total=False):\n`;
      if (def.doc) out += `    """${def.doc}"""\n\n`;
      for (const f of opt) out += `    ${f.key}: ${f.py}\n`;
      out += "\n\n";
    }
  }
  return out.trimEnd() + "\n";
}

const schema = loadSchema();
const defs = objects(schema);
const files = {
  [OUTPUTS.schema]: JSON.stringify(schema, null, 2) + "\n",
  [OUTPUTS.ts]: emitTs(defs),
  [OUTPUTS.py]: emitPy(defs),
};

let stale = [];
for (const [file, content] of Object.entries(files)) {
  const current = existsSync(file) ? readFileSync(file, "utf8") : null;
  if (current === content) continue;
  if (CHECK) stale.push(path.relative(ROOT, file));
  else writeFileSync(file, content);
}

if (CHECK && stale.length) {
  console.error(
    `Generated types are out of date:\n  ${stale.join("\n  ")}\n\n` +
      `Run: cargo build --features cli --bin kaniscope && node packaging/generate-types.mjs`
  );
  process.exit(1);
}
console.log(
  CHECK ? "generated types are up to date" : `wrote ${Object.keys(files).length} file(s)`
);
