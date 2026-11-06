// Release-facts checker. Node ESM, built-in modules only, no network.
// One source of truth: the version in Cargo.toml. Every other release fact
// must equal it. The documentation-review findings F-02 and F-05 showed that
// shape/link checks pass while the content is wrong, so this checker pins
// semantic release facts:
//   1. CHANGELOG.md: the latest dated release heading equals v<version>.
//   2. README.md: "Current release: **v<version>**." is present.
//   3. README.md: the quick start names the asset SimpleHarn-<version>-win-x64.zip.
//   4. README.md: the "The release zip holds" paragraph names exactly the
//      five canonical package entries as backticked items (any order).
//   5. docs/reference/COMPATIBILITY.md: "Current release: **v<version>**." is
//      present and the stale release claims are gone.
//   6. tests/webui/fixtures/status.json: the top-level "version" equals it.
//   7. scripts/make-public-asset.ps1: the zip name is built as
//      SimpleHarn-<version-without-v>-win-x64.zip and the five canonical
//      entries appear.
// Usage: node scripts/check-doc-facts.mjs [--root <dir>]
// Exit 0 with "check-doc-facts: PASS (n checks)" or exit 1 naming every FAIL.

import { readFileSync, existsSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));

function parseRoot(argv) {
  const i = argv.indexOf("--root");
  if (i >= 0) {
    const value = argv[i + 1];
    if (!value || value.startsWith("--")) {
      console.error("check-doc-facts: --root needs a directory argument");
      process.exit(2);
    }
    return resolve(value);
  }
  return resolve(HERE, "..");
}

const ROOT = parseRoot(process.argv.slice(2));

const CANONICAL_PACKAGE = [
  "simpleharn.exe",
  "SimpleHarn.vbs",
  "README.md",
  "LICENSE",
  "docs/assets/control-page.png",
];
// Phrases review F-03 proved false for the current release.
const STALE_COMPAT_PHRASES = [
  "ship in no released binary",
  "v1.3.0, the current release",
  "main (unreleased)",
];

const failures = [];
let checks = 0;

function fail(file, expected, actual) {
  failures.push(`FAIL ${file}: expected ${expected}, actual ${actual}`);
}

function check(condition, file, expected, actual) {
  checks += 1;
  if (!condition) fail(file, expected, actual);
}

function read(rel) {
  const abs = join(ROOT, rel);
  if (!existsSync(abs)) return null;
  return readFileSync(abs, "utf8");
}

function present(rel) {
  checks += 1;
  if (read(rel) === null) {
    fail(rel, "file to exist", "file not found under root " + ROOT);
    return null;
  }
  return read(rel);
}

const normalize = (s) => s.replace(/\s+/g, " ").trim();
const normalizePaths = (s) => normalize(s).replace(/\\/g, "/");

// 0. The version: Cargo.toml is the single truth for every fact below.
const cargo = present("Cargo.toml");
let version = null;
if (cargo !== null) {
  const m = cargo.match(/^\s*version\s*=\s*"([^"]+)"/m);
  version = m ? m[1] : null;
  check(m !== null, "Cargo.toml", 'a package version line: version = "X.Y.Z"', "no match");
}

if (version !== null) {
  const tag = "v" + version;

  // 1. CHANGELOG.md: latest dated release heading equals the version.
  const changelog = present("CHANGELOG.md");
  if (changelog !== null) {
    const headings = [...changelog.matchAll(/^## (v\d+\.\d+\.\d+)/gm)].map((h) => h[1]);
    check(
      headings.length > 0,
      "CHANGELOG.md",
      `a latest dated release heading "## ${tag}"`,
      "no dated release heading"
    );
    if (headings.length > 0) {
      check(
        headings[0] === tag,
        "CHANGELOG.md",
        `latest release heading "## ${tag}"`,
        `"## ${headings[0]}"`
      );
    }
  }

  // 2..4 README.md release line, quick-start asset, package sentence.
  const readme = present("README.md");
  if (readme !== null) {
    const releaseLine = `Current release: **${tag}**.`;
    check(
      readme.includes(releaseLine),
      "README.md",
      `the sentence "${releaseLine}"`,
      readme.includes("Current release:")
        ? "a different Current release line"
        : "no Current release line"
    );
    const asset = `SimpleHarn-${version}-win-x64.zip`;
    check(
      readme.includes(asset),
      "README.md",
      `the quick-start asset name "${asset}"`,
      "the asset name is absent or names a different version"
    );
    const anchor = "The release zip holds";
    const flat = normalize(readme);
    const at = flat.indexOf(anchor);
    if (at < 0) {
      checks += 1;
      fail("README.md", `the sentence starting "${anchor}"`, "sentence not found");
    } else {
      // Match inside the whitespace-normalized paragraph that holds the
      // anchor, so a line-wrapped sentence still parses.
      const paragraphs = readme.split(/\r?\n\s*\r?\n/);
      const paragraph = paragraphs.find((p) => normalize(p).includes(anchor)) ?? "";
      const sentence = normalize(paragraph);
      const named = [...sentence.matchAll(/`([^`]+)`/g)]
        .map((m) => m[1].replace(/\\/g, "/"))
        .sort();
      const canonical = [...CANONICAL_PACKAGE].sort();
      check(
        JSON.stringify(named) === JSON.stringify(canonical),
        "README.md",
        `the package paragraph to name exactly ${canonical.length} entries: ${canonical.join(", ")}`,
        named.length === 0 ? "no backticked entry in the paragraph" : `named: ${named.join(", ")}`
      );
    }
  }

  // 5. COMPATIBILITY.md: current release claim, no stale phrases.
  const compat = present("docs/reference/COMPATIBILITY.md");
  if (compat !== null) {
    const releaseLine = `Current release: **${tag}**.`;
    check(
      compat.includes(releaseLine),
      "docs/reference/COMPATIBILITY.md",
      `the sentence "${releaseLine}"`,
      "sentence absent"
    );
    for (const phrase of STALE_COMPAT_PHRASES) {
      check(
        !compat.includes(phrase),
        "docs/reference/COMPATIBILITY.md",
        `no stale phrase "${phrase}"`,
        "phrase still present"
      );
    }
  }

  // 6. Web UI fixture: top-level "version" equals the crate version.
  const fixture = present("tests/webui/fixtures/status.json");
  if (fixture !== null) {
    let fixtureVersion = null;
    let parsed = null;
    try {
      parsed = JSON.parse(fixture);
    } catch {
      checks += 1;
      fail("tests/webui/fixtures/status.json", "valid JSON", "parse error");
    }
    if (parsed !== null) {
      fixtureVersion = parsed.version ?? null;
      check(
        fixtureVersion === version,
        "tests/webui/fixtures/status.json",
        `top-level "version" equal to "${version}"`,
        String(fixtureVersion)
      );
    }
  }

  // 7. make-public-asset.ps1: zip name pattern and package manifest.
  const maker = present("scripts/make-public-asset.ps1");
  if (maker !== null) {
    const direct = /SimpleHarn-"\s*\+\s*\$Version\b/;
    const built = /SimpleHarn-"\s*\+\s*\$[A-Za-z]\w*/;
    const vstrip = /\$Version\s+-replace\s+'\^v'/;
    check(
      maker.includes('"-win-x64.zip"') && built.test(maker) && vstrip.test(maker) && !direct.test(maker),
      "scripts/make-public-asset.ps1",
      'a zip name built as "SimpleHarn-" + (version without v) + "-win-x64.zip"',
      direct.test(maker)
        ? "the zip name still embeds the tag variable directly (keeps the v)"
        : "zip name pattern not found"
    );
    const flat = normalizePaths(maker);
    for (const entry of CANONICAL_PACKAGE) {
      check(
        flat.includes(entry),
        "scripts/make-public-asset.ps1",
        `the canonical package entry "${entry}"`,
        "entry not found in the script"
      );
    }
  }
}

if (failures.length === 0) {
  console.log(`check-doc-facts: PASS (${checks} checks)`);
  process.exit(0);
}
for (const line of failures) console.log(line);
console.log(`check-doc-facts: FAIL (${failures.length} of ${checks} checks failed)`);
process.exit(1);
