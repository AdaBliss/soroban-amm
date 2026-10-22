#!/usr/bin/env node
// Keeps .github/BRANCH_PROTECTION.md in step with the status checks that
// .github/workflows/ci.yml really reports (#1154).
//
// GitHub reports one status context per job, and one per matrix entry for a
// matrix job. A job's context is its explicit `name:` or, without one, its id —
// and for a matrix job the default name also lists *every* matrix value, test
// command included. This script derives the contexts from ci.yml, so a required
// check can no longer be a name GitHub never reports, and holds the doc to them:
//
//   * the bullet list between the `required-checks` markers, and
//   * the `required_status_checks.contexts` array in the `gh api` payload.
//
// The `rustsec/audit-check` step publishes its own check run, "Security audit",
// which is reported under that name rather than under the job.
//
//   node scripts/check_branch_protection_doc.mjs                 # verify the doc
//   node scripts/check_branch_protection_doc.mjs --write         # regenerate it
//   node scripts/check_branch_protection_doc.mjs --verify-remote # also check the
//                                                                  live rule (gh, admin scope)

import { execFileSync } from "node:child_process";
import { readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(fileURLToPath(import.meta.url), "..", "..");
export const CI_PATH = join(root, ".github", "workflows", "ci.yml");
export const DOC_PATH = join(root, ".github", "BRANCH_PROTECTION.md");

export const BEGIN = "<!-- begin: required-checks (generated from ci.yml) ** DO NOT EDIT MANUALLY ** -->";
export const END = "<!-- end: required-checks -->";
const AUDIT_CONTEXT = "Security audit";

const indentOf = (line) => line.length - line.trimStart().length;
const unquote = (v) => v.trim().replace(/^(["'])(.*)\1$/, "$2");

/** Split the `jobs:` mapping of a workflow into `{ id, lines }` blocks. */
export function parseJobs(yaml) {
    const lines = yaml.split(/\r?\n/);
    const start = lines.findIndex((l) => /^jobs:\s*$/.test(l));
    if (start === -1) throw new Error("ci.yml has no top-level `jobs:` mapping");

    const jobs = [];
    let jobIndent = null;
    for (let i = start + 1; i < lines.length; i++) {
        const line = lines[i];
        if (!line.trim() || line.trim().startsWith("#")) continue;
        if (indentOf(line) === 0) break; // next top-level key
        jobIndent ??= indentOf(line);
        const id = indentOf(line) === jobIndent ? /^\s*([\w-]+):\s*$/.exec(line) : null;
        if (id) jobs.push({ id: id[1], lines: [] });
        else if (jobs.length) jobs[jobs.length - 1].lines.push(line);
    }
    return jobs;
}

/** The contexts one job reports, in the order GitHub lists matrix entries. */
export function jobContexts(job) {
    const body = job.lines.filter((l) => l.trim() && !l.trim().startsWith("#"));
    const childIndent = Math.min(...body.map(indentOf));
    const nameLine = body.find((l) => indentOf(l) === childIndent && /^\s*name:/.test(l));
    const name = nameLine ? unquote(nameLine.slice(nameLine.indexOf(":") + 1)) : null;

    const contexts = [];
    const matrixAt = body.findIndex((l) => /^\s*matrix:\s*$/.test(l));
    if (matrixAt === -1) {
        contexts.push(name ?? job.id);
    } else {
        // Each `- key: value` list item of `include:` starts one matrix entry.
        const entries = [];
        for (const line of body.slice(matrixAt + 1)) {
            if (indentOf(line) <= indentOf(body[matrixAt]) && !/^\s*-/.test(line)) break;
            const item = /^\s*-\s+([\w-]+):\s*(.*)$/.exec(line);
            const pair = /^\s*([\w-]+):\s*(.*)$/.exec(line);
            if (item) entries.push({ [item[1]]: unquote(item[2]) });
            else if (pair && entries.length && !/^\s*(include|fail-fast|exclude)\s*:/.test(line)) {
                entries[entries.length - 1][pair[1]] = unquote(pair[2]);
            }
        }
        for (const entry of entries) {
            contexts.push(
                name
                    ? name.replace(/\$\{\{\s*matrix\.([\w-]+)\s*\}\}/g, (_, key) => entry[key] ?? "")
                    : `${job.id} (${Object.values(entry).join(", ")})`,
            );
        }
    }
    return contexts;
}

/** Every required context for a workflow, in workflow order. */
export function requiredContexts(yaml) {
    const out = [];
    for (const job of parseJobs(yaml)) {
        out.push(...jobContexts(job));
        if (job.lines.some((l) => /uses:\s*rustsec\/audit-check/.test(l))) out.push(AUDIT_CONTEXT);
    }
    return out;
}

export function renderList(contexts) {
    return contexts.map((c) => `- \`${c}\``).join("\n");
}

/** Contexts as the doc currently lists them (bullet list between the markers). */
export function docListedContexts(doc) {
    const a = doc.indexOf(BEGIN);
    const b = doc.indexOf(END);
    if (a === -1 || b === -1 || b < a) throw new Error(`BRANCH_PROTECTION.md is missing the ${BEGIN} ... ${END} markers`);
    return [...doc.slice(a + BEGIN.length, b).matchAll(/^- `(.+)`\s*$/gm)].map((m) => m[1]);
}

const PAYLOAD_RE = /("contexts":\s*\[)([\s\S]*?)(\n\s*\])/;

/** Contexts as the doc's `gh api` payload lists them. */
export function docPayloadContexts(doc) {
    const m = PAYLOAD_RE.exec(doc);
    if (!m) throw new Error("BRANCH_PROTECTION.md has no `contexts` array in its gh api payload");
    return [...m[2].matchAll(/"((?:[^"\\]|\\.)*)"/g)].map((x) => JSON.parse(`"${x[1]}"`));
}

/** The doc with both lists regenerated from `contexts`. */
export function rewriteDoc(doc, contexts) {
    const a = doc.indexOf(BEGIN) + BEGIN.length;
    const b = doc.indexOf(END);
    const withList = `${doc.slice(0, a)}\n${renderList(contexts)}\n${doc.slice(b)}`;
    return withList.replace(PAYLOAD_RE, (_, open, __, close) => {
        const indent = /\n(\s*)\]/.exec(close)[1] + "  ";
        return `${open}\n${contexts.map((c) => `${indent}${JSON.stringify(c)}`).join(",\n")}${close}`;
    });
}

const same = (a, b) => a.length === b.length && a.every((v, i) => v === b[i]);

/** Human-readable differences between what the doc lists and what CI reports. */
export function diffLists(label, listed, expected) {
    const problems = [];
    for (const c of expected) if (!listed.includes(c)) problems.push(`${label}: missing \`${c}\` (reported by ci.yml)`);
    for (const c of listed) if (!expected.includes(c)) problems.push(`${label}: lists \`${c}\`, which ci.yml never reports`);
    if (!problems.length && !same(listed, expected)) problems.push(`${label}: same checks, different order than ci.yml`);
    return problems;
}

function remoteContexts() {
    const slug = execFileSync("gh", ["repo", "view", "--json", "nameWithOwner", "--jq", ".nameWithOwner"], { encoding: "utf8" }).trim();
    const raw = execFileSync("gh", ["api", `repos/${slug}/branches/main/protection/required_status_checks`, "--jq", ".contexts[]"], { encoding: "utf8" });
    return raw.split("\n").map((s) => s.trim()).filter(Boolean);
}

function main(argv) {
    const write = argv.includes("--write");
    const verifyRemote = argv.includes("--verify-remote");
    const expected = requiredContexts(readFileSync(CI_PATH, "utf8"));
    if (!expected.length) throw new Error("found no status checks in ci.yml");

    let doc = readFileSync(DOC_PATH, "utf8");
    if (write) {
        const next = rewriteDoc(doc, expected);
        if (next !== doc) writeFileSync(DOC_PATH, next);
        console.log(next === doc ? "BRANCH_PROTECTION.md is already up to date." : "Updated BRANCH_PROTECTION.md.");
        doc = next;
    }

    const problems = [
        ...diffLists("required-checks list", docListedContexts(doc), expected),
        ...diffLists("gh api payload", docPayloadContexts(doc), expected),
    ];

    if (verifyRemote) {
        const live = remoteContexts();
        for (const c of live) if (!expected.includes(c)) problems.push(`live rule requires \`${c}\`, which ci.yml never reports (merges would be blocked forever)`);
        for (const c of expected) if (!live.includes(c)) problems.push(`live rule does not require \`${c}\``);
    }

    if (problems.length) {
        console.error(`Branch protection check failed (${problems.length} problem(s)):\n`);
        for (const p of problems) console.error(`  - ${p}`);
        console.error("\nRun: node scripts/check_branch_protection_doc.mjs --write");
        process.exit(1);
    }
    console.log(`BRANCH_PROTECTION.md lists the ${expected.length} status checks ci.yml reports${verifyRemote ? ", and the live rule matches" : ""}.`);
}

if (process.argv[1] && fileURLToPath(import.meta.url) === process.argv[1]) main(process.argv.slice(2));
