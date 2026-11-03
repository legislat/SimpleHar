// Instruction checker. Node, no external dependencies.
// Validates the agent entry point and the anti-duplication rules:
//   - AGENTS.md carries a numbered Precedence section, a Conflict rules
//     section, and an Autonomy table naming the three levels;
//   - relative Markdown links in AGENTS.md resolve;
//   - README.md keeps the retired strata_config and "no auth" claims out;
//   - the cardless gate command block lives only in docs/process/VERIFICATION.md.
// Durable scan scope mirrors scripts/check-docs.mjs; audit snapshots, DESIGN.md,
// handoff/, and archives are exempt (frozen or historical, not current rules).

import { readFileSync, readdirSync, existsSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(HERE, "..");

const GATE_MARKER_A = "cargo check --all-targets";
const GATE_MARKER_B = "node tests/webui/webui_dom_test.mjs";
const GATE_MARKERS = [
  "cargo check --all-targets",
  "cargo test",
  "cargo clippy --all-targets -- -D warnings",
  "cargo fmt --check",
  "node tests/webui/webui_dom_test.mjs",
];
const VERIFICATION = "docs/process/VERIFICATION.md";

const failures = [];
const fail = (path, line, message) => failures.push(`${path}:${line}: ${message}`);

const read = (rel) => readFileSync(join(ROOT, rel), "utf8");

function listFiles(relDir) {
  const abs = join(ROOT, relDir);
  if (!existsSync(abs)) return [];
  return readdirSync(abs).filter((n) => n.endsWith(".md")).sort()
    .map((n) => `${relDir}/${n}`);
}

// --- AGENTS.md ---
const agentsPath = "AGENTS.md";
if (!existsSync(join(ROOT, agentsPath))) {
  fail(agentsPath, 1, "missing file");
} else {
  const text = read(agentsPath);
  const lines = text.split(/\r?\n/);

  if (!/^#{1,6}\s*\d+\.\s*Precedence\b/m.test(text)) {
    fail(agentsPath, 1, "no numbered Precedence section heading");
  }
  const conflictLine = lines.findIndex((l) => /^#{0,6}\s*\**\s*Conflict rules\b/.test(l));
  if (conflictLine < 0) fail(agentsPath, 1, "no Conflict rules section");

  const autonomyLine = lines.findIndex((l) => /^#{1,6}\s*\d*\.*\s*Autonomy\b/.test(l));
  if (autonomyLine < 0) {
    fail(agentsPath, 1, "no Autonomy section heading");
  } else {
    for (const level of ["Allowed", "Requires approval", "Forbidden"]) {
      if (!lines.some((l) => l.includes("|") && l.includes(level))) {
        fail(agentsPath, autonomyLine + 1, `Autonomy table lacks level: ${level}`);
      }
    }
  }

  lines.forEach((line, i) => {
    const re = /\]\(([^)\s]+)(?:\s+"[^"]*")?\)/g;
    let m;
    while ((m = re.exec(line)) !== null) {
      let target = m[1];
      if (/^[a-z][a-z0-9+.-]*:/i.test(target) || target.startsWith("#")) continue;
      target = decodeURIComponent(target.split("#")[0]);
      if (target === "") continue;
      const abs = resolve(ROOT, target);
      if (!existsSync(abs)) fail(agentsPath, i + 1, `broken relative link: ${m[1]}`);
    }
  });
}

// --- README.md retired claims ---
const readmePath = "README.md";
if (!existsSync(join(ROOT, readmePath))) {
  fail(readmePath, 1, "missing file");
} else {
  const lines = read(readmePath).split(/\r?\n/);
  lines.forEach((line, i) => {
    if (/strata_config/i.test(line) && /required\s+for\s+strata/i.test(line)) {
      fail(readmePath, i + 1, "retired claim: strata_config is not required for strata");
    }
    // "no authentication" as a conditional fact stays allowed; a flat "no auth"
    // claim is the retired one.
    const m = line.match(/no\s+auth(?!entication)/i);
    if (m) fail(readmePath, i + 1, `retired claim: "${m[0]}"`);
  });
}

// --- Gate block duplication ---
if (!existsSync(join(ROOT, VERIFICATION))) {
  fail(VERIFICATION, 1, "missing file");
} else {
  const text = read(VERIFICATION);
  if (!text.includes(GATE_MARKER_A)) fail(VERIFICATION, 1, `gate lacks "${GATE_MARKER_A}"`);
  if (!text.includes(GATE_MARKER_B)) fail(VERIFICATION, 1, `gate lacks "${GATE_MARKER_B}"`);
}

const scanSet = [
  "README.md", "AGENTS.md", "CONTRIBUTING.md", "CHANGELOG.md", "docs/DOCS_README.md",
  ...listFiles("docs/process"),
  ...listFiles("docs/architecture"),
  ...listFiles("docs/reference"),
].filter((p) => existsSync(join(ROOT, p)) && p !== VERIFICATION);

for (const rel of scanSet) {
  const text = read(rel);
  const hits = GATE_MARKERS.filter((m) => text.includes(m)).length;
  if (hits >= 3) {
    fail(rel, 1, `duplicates ${hits} cardless gate commands; only ` +
      `${VERIFICATION} may hold the command list (link instead)`);
  }
}

for (const f of failures) console.log(f);
if (failures.length > 0) {
  console.log(`check-instructions: FAIL (${failures.length})`);
  process.exit(1);
}
console.log("check-instructions: PASS");
