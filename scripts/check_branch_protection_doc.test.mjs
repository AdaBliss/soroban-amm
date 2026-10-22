// Tests for the branch-protection doc checker.
// Run with: node --test scripts/check_branch_protection_doc.test.mjs

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import {
  BEGIN,
  CI_PATH,
  DOC_PATH,
  diffLists,
  docListedContexts,
  docPayloadContexts,
  END,
  requiredContexts,
  rewriteDoc,
} from "./check_branch_protection_doc.mjs";

const CI = `name: CI
on:
    push:
jobs:
    build:
        runs-on: ubuntu-latest
        steps:
            - uses: rustsec/audit-check@v2
    lint:
        name: Lint
        runs-on: ubuntu-latest
    pkgs:
        name: pkgs (\${{ matrix.dir }})
        strategy:
            fail-fast: false
            matrix:
                include:
                    - dir: a
                      test_cmd: npm test
                    - dir: b
                      test_cmd: npm run t
        runs-on: ubuntu-latest
    unnamed:
        strategy:
            matrix:
                include:
                    - dir: c
                      test_cmd: x
        runs-on: ubuntu-latest
`;

test("a job reports its explicit name, or its id without one", () => {
  const got = requiredContexts(CI);
  assert.ok(got.includes("build"));
  assert.ok(got.includes("Lint"));
  assert.ok(!got.includes("lint"));
});

test("the rustsec step adds the separate Security audit check right after its job", () => {
  const got = requiredContexts(CI);
  assert.equal(got[got.indexOf("build") + 1], "Security audit");
});

test("a matrix job reports one context per entry, using its explicit name", () => {
  const got = requiredContexts(CI);
  assert.ok(got.includes("pkgs (a)"));
  assert.ok(got.includes("pkgs (b)"));
});

test("without an explicit name, GitHub's default includes every matrix value", () => {
  // This is the trap: the test command ends up in the required context.
  assert.ok(requiredContexts(CI).includes("unnamed (c, x)"));
});

test("diffLists reports missing, phantom and misordered checks", () => {
  assert.deepEqual(diffLists("l", ["a", "b"], ["a", "b"]), []);
  assert.match(diffLists("l", ["a"], ["a", "b"])[0], /missing `b`/);
  assert.match(diffLists("l", ["a", "ghost"], ["a"])[0], /`ghost`, which ci.yml never reports/);
  assert.match(diffLists("l", ["b", "a"], ["a", "b"])[0], /different order/);
});

test("rewriteDoc regenerates both the bullet list and the gh api payload", () => {
  const doc = `x\n${BEGIN}\n- \`old\`\n${END}\n\`\`\`\n  "contexts": [\n      "old"\n    ]\n\`\`\`\n`;
  const next = rewriteDoc(doc, ["one", "two (a, b)"]);
  assert.deepEqual(docListedContexts(next), ["one", "two (a, b)"]);
  assert.deepEqual(docPayloadContexts(next), ["one", "two (a, b)"]);
  assert.equal(rewriteDoc(next, ["one", "two (a, b)"]), next, "rewriting is idempotent");
});

test("the committed doc lists exactly the checks the committed ci.yml reports", () => {
  const expected = requiredContexts(readFileSync(CI_PATH, "utf8"));
  const doc = readFileSync(DOC_PATH, "utf8");
  assert.deepEqual(docListedContexts(doc), expected);
  assert.deepEqual(docPayloadContexts(doc), expected);
});

test("no committed required check includes a test command", () => {
  const doc = readFileSync(DOC_PATH, "utf8");
  for (const c of docListedContexts(doc)) assert.ok(!/npm (test|run)|--passWithNoTests|--coverage/.test(c), `${c} leaks a test command`);
});
