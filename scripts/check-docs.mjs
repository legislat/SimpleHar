// Documentation checker for durable docs. Node, no external dependencies.
// Rules (docs/DOCS_README.md sections 5 and 6):
//   1. Front matter with the seven required keys. README.md is exempt: it is the
//      storefront page and GitHub renders front matter there as a table (2026-11-05 ruling).
//   2. Relative Markdown links resolve to existing files.
//   3. No author-specific absolute Windows drive paths in durable docs. Documented
//      product defaults in README/reference and exact quoted decisions are allowed.
//      (fail inside code fences too, except for those documented defaults). The
//      shared machine-path ban set in scripts/machine-path-bans.json is enforced
//      here and by scripts/release-check.ps1 alike.
//   4. No duplicate H1.
//   5. Every durable doc is listed in docs/DOCS_README.md by filename or path.
//   6. No literal DRAFT in a doc whose front matter status is active.
//   7. Repo-relative paths written as inline code resolve to existing files.
//      Local-only paths (handoff/, ModelConf/, archive/, base-archive/, .kilo/)
//      are exempt; a path that names a line range keeps its base file.
// Not scanned: DESIGN.md (historical), handoff/* (local), archive/ and
// base-archive/ (frozen).

import { readFileSync, readdirSync, existsSync } from "node:fs";
import { dirname, join, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(HERE, "..");
const INDEX = "docs/DOCS_README.md";
const REQUIRED_KEYS = [
  "title", "owner", "status", "audience",
  "source_of_truth_for", "last_reviewed", "review_trigger",
];
const FRONT_MATTER_EXEMPT = new Set(["README.md"]);

const failures = [];
const fail = (path, line, message) => failures.push(`${path}:${line}: ${message}`);

// Exact quoted rulings may contain the operator's paths: the two ledgers are
// exempt from the generic drive rule. The shared machine-path bans below apply
// to every durable doc, exactly as scripts/release-check.ps1 applies them, so
// one machine path can never pass this gate and fail the release check.
const PATH_EXEMPT = new Set([
  "docs/architecture/DECISIONS.md",
  "docs/architecture/DOCTRINE.md",
]);
// The canonical machine-path ban set, read from the one shared file so this
// checker and scripts/release-check.ps1 can never disagree. The llama.cpp
// engine-default allowance lives inside the first ban pattern itself.
const MACHINE_PATH_BANS = JSON.parse(
  readFileSync(join(HERE, "machine-path-bans.json"), "utf8")
).bans.map((pattern) => new RegExp(pattern, "i"));
// Documented product defaults: the llama.cpp install default, the strata
// install home, and the model-root example, all named in README/reference.
// Author-specific paths (for example C:\Users\... or an F:\ model path) are
// not product defaults and still fail.
const PRODUCT_DEFAULT_PATH =
  /(?:C:\\Inference\\llama\.cpp|C:\\Strata|D:\\Models)/i;

function listFiles(relDir) {
  const abs = join(ROOT, relDir);
  if (!existsSync(abs)) return [];
  return readdirSync(abs)
    .filter((n) => n.endsWith(".md"))
    .sort()
    .map((n) => `${relDir}/${n}`.split(sep).join("/"));
}

const candidates = [
  "README.md", "AGENTS.md", "CONTRIBUTING.md", "CHANGELOG.md", INDEX,
  ...listFiles("docs/process"),
  ...listFiles("docs/architecture"),
  ...listFiles("docs/evidence"),
  ...listFiles("docs/reference"),
];
const docs = candidates.filter((p) => existsSync(join(ROOT, p)));

// Split a document into lines with a flag for "inside a code fence".
function parseLines(text) {
  let inFence = false;
  return text.split(/\r?\n/).map((line, i) => {
    if (/^(```|~~~)/.test(line)) inFence = !inFence;
    return { line, n: i + 1, inFence };
  });
}

function parseFrontMatter(rel, entries) {
  if (!entries.length || entries[0].line.trim() !== "---") {
    fail(rel, 1, "missing front matter block");
    return null;
  }
  const keys = new Map();
  let status = "";
  for (const e of entries.slice(1, entries.length - 1)) {
    if (e.line.trim() === "---") break;
    const m = e.line.match(/^([A-Za-z_][A-Za-z0-9_-]*)\s*:\s*(.*)$/);
    if (m) {
      keys.set(m[1], m[2].trim());
      if (m[1] === "status") status = m[2].trim().toLowerCase();
    }
  }
  return { keys, status };
}

function checkLinks(rel, entries) {
  const baseDir = dirname(join(ROOT, rel));
  for (const e of entries) {
    if (e.inFence) continue;
    const re = /\]\(([^)\s]+)(?:\s+"[^"]*")?\)/g;
    let m;
    while ((m = re.exec(e.line)) !== null) {
      let target = m[1];
      if (/^[a-z][a-z0-9+.-]*:/i.test(target)) continue; // http:, mailto:, ...
      if (target.startsWith("#")) continue; // same-page anchor
      target = decodeURIComponent(target.split("#")[0]);
      if (target === "") continue;
      if (!existsSync(resolve(baseDir, target))) {
        fail(rel, e.n, `broken relative link: ${m[1]}`);
      }
    }
  }
}

// Rule 7: repo-relative paths in inline code. The set below is the documented
// local-only layer: its paths exist only on the operator machine.
const LOCAL_ONLY_PREFIXES = [
  "handoff/", "ModelConf/", "archive/", "base-archive/", ".kilo/",
  "target/", "node_modules/",
];
const LOCAL_ONLY_FILES = new Set(["HANDOFF.md"]);
const TRACKED_PREFIXES = [
  "docs/", "src/", "tests/", "scripts/", "assets/", "examples/", ".github/",
];
const HAS_EXTENSION = /\.[A-Za-z0-9]{1,6}$/;

function isLocalOnly(target) {
  return LOCAL_ONLY_FILES.has(target) ||
    LOCAL_ONLY_PREFIXES.some((p) => target.startsWith(p));
}

function checkInlinePaths(rel, entries) {
  const baseDir = join(ROOT, rel);
  for (const e of entries) {
    if (e.inFence) continue;
    const re = /`([^`\n]+)`/g;
    let m;
    while ((m = re.exec(e.line)) !== null) {
      let target = m[1].trim();
      if (target === "" || /\s/.test(target)) continue;
      if (/^[a-z][a-z0-9+.-]*:/i.test(target)) continue; // URL or scheme
      if (/^[A-Za-z]:[\\/]/.test(target) || target.includes("\\")) continue; // absolute
      if (target.startsWith("/") || target.startsWith("-")) continue;
      if (/[*?<>{}|$]/.test(target)) continue; // glob, placeholder, pipe
      if (!target.includes("/")) continue; // bare names and identifiers
      // Strip a trailing line reference: file.rs:120-130
      target = target.replace(/:\d+(?:-\d+)?$/, "");
      if (!HAS_EXTENSION.test(target) &&
          !TRACKED_PREFIXES.some((p) => target.startsWith(p))) continue;
      if (isLocalOnly(target)) continue;
      if (target.endsWith("/")) continue; // a directory reference (policy target)
      const roots = [ROOT, join(ROOT, "src"), join(ROOT, "tests"), join(ROOT, "scripts")];
      if (!roots.some((root) => existsSync(resolve(root, target)))) {
        fail(rel, e.n, `unresolved path reference: ${m[1]}`);
      }
    }
  }
}

const indexText = existsSync(join(ROOT, INDEX))
  ? readFileSync(join(ROOT, INDEX), "utf8")
  : "";
if (indexText === "") fail(INDEX, 1, "documentation index is missing; coverage cannot be checked");

for (const rel of docs) {
  const text = readFileSync(join(ROOT, rel), "utf8");
  const entries = parseLines(text);

  // 1. Front matter. README.md is exempt (storefront page); its status is active.
  const exempt = FRONT_MATTER_EXEMPT.has(rel);
  const fm = exempt ? { keys: new Map(), status: "active" } : parseFrontMatter(rel, entries);
  if (exempt || fm) {
    if (!exempt) {
      for (const key of REQUIRED_KEYS) {
        if (!fm.keys.has(key)) fail(rel, 1, `front matter missing key: ${key}`);
      }
    }

    // 6. DRAFT ban on active docs (literal, case-sensitive, outside code fences).
    if (fm.status === "active") {
      for (const e of entries) {
        if (!e.inFence && e.line.includes("DRAFT")) {
          fail(rel, e.n, "literal DRAFT in a doc whose status is active");
        }
      }
    }
  }

  // 2. Relative links.
  checkLinks(rel, entries);

  // 7. Repo-relative paths in inline code.
  checkInlinePaths(rel, entries);

  // 3. Machine paths, code fences included. The shared ban set first: every
  //    durable doc, ruling copies included, exactly like release-check.ps1.
  //    Then the generic drive rule: it rejects author-specific paths on any
  //    drive, exempts the exact ruling copies, and allows the documented
  //    product defaults.
  entries.forEach((e) => {
    for (const ban of MACHINE_PATH_BANS) {
      if (ban.test(e.line)) {
        fail(rel, e.n, `machine path in durable doc (shared ban ${ban.source})`);
        break;
      }
    }
  });
  if (!PATH_EXEMPT.has(rel)) {
    entries.forEach((e) => {
      const m = e.line.match(/(^|[^A-Za-z0-9])([A-Za-z]):[\\/]/);
      if (m && !PRODUCT_DEFAULT_PATH.test(e.line)) {
        fail(rel, e.n, `absolute Windows path in durable doc: ${m[2]}:\\...`);
      }
      const home = e.line.match(/\/(?:home|Users)\/[A-Za-z0-9._-]+\//);
      if (home && !/\/(?:home|Users)\/(?:you|user|username)\//.test(home[0])) {
        fail(rel, e.n, `author home path in durable doc: ${home[0]}`);
      }
    });
  }

  // 4. Single H1 outside code fences.
  const h1s = entries.filter((e) => !e.inFence && /^#\s+\S/.test(e.line));
  for (const e of h1s.slice(1)) {
    fail(rel, e.n, `duplicate H1 (first H1 is line ${h1s[0].n})`);
  }

  // 5. Index coverage: the index lists every durable doc by path or filename.
  //    The index itself cannot list itself.
  if (rel !== INDEX && indexText !== "") {
    const name = rel.split("/").pop();
    if (!indexText.includes(rel) && !indexText.includes(name)) {
      fail(rel, 1, `orphan doc: not listed in ${INDEX} by path or filename`);
    }
  }
}

for (const f of failures) console.log(f);
if (failures.length > 0) {
  console.log(`check-docs: FAIL (${failures.length})`);
  process.exit(1);
}
console.log(`check-docs: PASS (${docs.length} durable docs checked)`);
