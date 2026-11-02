// Task file checker for handoff/tasks/*.md. Node, no external dependencies.
// Schema: docs/process/TASKS.md. An absent or empty task directory is valid:
// the queue can legitimately hold no task files.
//
// Required per task file:
//   - an H1 beginning "# TASK-";
//   - fields: Class, Priority, Readiness, Owner (table rows) and Goal, Scope,
//     Non-goals, Acceptance criteria, Dependencies, Evidence, Next step
//     (table rows or section headings);
//   - Class in S/R/C/O, Priority in P0-P3, Readiness in
//     ready/blocked/needs-design/needs-operator.

import { readFileSync, readdirSync, existsSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(HERE, "..");
const TASK_DIR = join(ROOT, "handoff", "tasks");

const TABLE_FIELDS = ["Class", "Priority", "Readiness", "Owner"];
const TEXT_FIELDS = ["Goal", "Scope", "Non-goals", "Acceptance criteria",
  "Dependencies", "Evidence", "Next step"];
const CLASS_VALUES = ["S", "R", "C", "O"];
const PRIORITY_VALUES = ["P0", "P1", "P2", "P3"];
const READINESS_VALUES = ["ready", "blocked", "needs-design", "needs-operator"];

const failures = [];
const fail = (name, line, message) => failures.push(`${name}:${line}: ${message}`);

if (!existsSync(TASK_DIR)) {
  console.log("check-tasks: handoff/tasks/ does not exist; no task files to check");
  process.exit(0);
}
const names = readdirSync(TASK_DIR).filter((n) => n.endsWith(".md")).sort();
if (names.length === 0) {
  console.log("check-tasks: handoff/tasks/ is empty; no task files to check");
  process.exit(0);
}

function tableValue(lines, field) {
  for (let i = 0; i < lines.length; i++) {
    const m = lines[i].match(/^\|\s*([^|]+?)\s*\|\s*([^|]*?)\s*\|/);
    if (m && m[1].toLowerCase() === field.toLowerCase()) {
      return { value: m[2], n: i + 1 };
    }
  }
  return null;
}

function hasHeading(lines, field) {
  const re = new RegExp(`^#{2,6}\\s+${field.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}(\\s|$)`, "i");
  return lines.some((l) => re.test(l));
}

function checkEnum(rel, entry, field, value, allowed) {
  if (value.includes("/")) {
    fail(rel, entry.n, `${field} is an unfilled template placeholder: "${value}"`);
    return;
  }
  if (!allowed.some((a) => a.toLowerCase() === value.toLowerCase())) {
    fail(rel, entry.n, `${field} has invalid value "${value}" (allowed: ${allowed.join(", ")})`);
  }
}

for (const name of names) {
  const rel = `handoff/tasks/${name}`;
  const lines = readFileSync(join(TASK_DIR, name), "utf8").split(/\r?\n/);

  const h1Index = lines.findIndex((l) => /^#\s+\S/.test(l));
  if (h1Index < 0) {
    fail(rel, 1, "no H1 found");
  } else if (!/^#\s+TASK-/.test(lines[h1Index])) {
    fail(rel, h1Index + 1, `H1 must begin "# TASK-": ${lines[h1Index]}`);
  }

  for (const field of TABLE_FIELDS) {
    const entry = tableValue(lines, field);
    if (!entry) {
      fail(rel, 1, `missing field: ${field}`);
      continue;
    }
    if (entry.value === "") {
      fail(rel, entry.n, `empty field: ${field} (write "none" if empty)`);
      continue;
    }
    if (field === "Class") checkEnum(rel, entry, field, entry.value, CLASS_VALUES);
    if (field === "Priority") checkEnum(rel, entry, field, entry.value, PRIORITY_VALUES);
    if (field === "Readiness") checkEnum(rel, entry, field, entry.value, READINESS_VALUES);
  }

  for (const field of TEXT_FIELDS) {
    if (!tableValue(lines, field) && !hasHeading(lines, field)) {
      fail(rel, 1, `missing field: ${field}`);
    }
  }
}

for (const f of failures) console.log(f);
if (failures.length > 0) {
  console.log(`check-tasks: FAIL (${failures.length})`);
  process.exit(1);
}
console.log(`check-tasks: PASS (${names.length} task files checked)`);
