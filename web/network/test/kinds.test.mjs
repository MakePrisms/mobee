import assert from "node:assert/strict";
import { readdirSync, readFileSync, statSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { test } from "node:test";

import * as kinds from "../js/kinds.js";

const NETWORK_ROOT = fileURLToPath(new URL("..", import.meta.url));

/**
 * The marketplace kind numbers that v2 will renumber. They must live in exactly one
 * file (js/kinds.js) so that renumber is a one-file change. Kind 0 (NIP-01 profile) is
 * a standard that will not move and appears everywhere as an index, so it is not gated
 * by digits — it is still routed through the PROFILE constant.
 */
const RENUMBERABLE = [5109, 7000, 6109, 3400, 31990, 30340];

/** Remove block and line comments so the gate scans only operative code/strings. */
function stripComments(src) {
  return src.replace(/\/\*[\s\S]*?\*\//g, "").replace(/\/\/[^\n]*/g, "");
}

test("kinds module exposes every marketplace kind as a named constant", () => {
  assert.equal(kinds.OFFER, 5109);
  assert.equal(kinds.CLAIM, 7000);
  assert.equal(kinds.RESULT, 6109);
  assert.equal(kinds.RECEIPT, 3400);
  assert.equal(kinds.HANDLER, 31990);
  assert.equal(kinds.HEARTBEAT, 30340);
  assert.equal(kinds.PROFILE, 0);
});

/**
 * Recursively list *.js/*.mjs PRODUCTION source files (js/ + scripts/ + config.js).
 * Test files are excluded: they build raw Nostr events, whose kind number is data, not
 * a hard-coded protocol constant — the gate is about the app source, not the fixtures.
 */
function sourceFiles() {
  const out = [];
  const walk = (dir) => {
    for (const name of readdirSync(dir)) {
      if (name === "dist" || name === "node_modules") continue;
      const full = join(dir, name);
      if (statSync(full).isDirectory()) walk(full);
      else if (name.endsWith(".js") || name.endsWith(".mjs")) out.push(full);
    }
  };
  walk(join(NETWORK_ROOT, "js"));
  walk(join(NETWORK_ROOT, "scripts"));
  out.push(join(NETWORK_ROOT, "config.js"));
  return out;
}

test("no marketplace kind literal appears outside js/kinds.js", () => {
  const kindsFile = fileURLToPath(new URL("../js/kinds.js", import.meta.url));
  const offenders = [];

  for (const file of sourceFiles()) {
    if (file === kindsFile) continue; // the one file allowed to hold the numbers
    // Strip comments before scanning: an explanatory comment may reference a historical
    // kind number as prose. The gate is about operative code/strings — anything that a
    // running module actually depends on must come from js/kinds.js.
    const src = stripComments(readFileSync(file, "utf8"));
    for (const n of RENUMBERABLE) {
      // word-boundary match so we catch a bare `5109` but not a substring of a longer id.
      const re = new RegExp(`(?<![\\d.])${n}(?![\\d.])`);
      if (re.test(src)) {
        offenders.push(`${file}: contains kind literal ${n}`);
      }
    }
  }

  assert.deepEqual(
    offenders,
    [],
    `kind literals must be imported from js/kinds.js, not hard-coded:\n${offenders.join("\n")}`,
  );
});
