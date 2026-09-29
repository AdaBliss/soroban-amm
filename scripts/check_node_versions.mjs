#!/usr/bin/env node
// Fails when a JS package's Node typings or declared runtime drift from the
// Node major CI runs on (#983).
//
// `.nvmrc` is the single source of truth: CI's `setup-node` reads it via
// `node-version-file`, and this script holds every package to it:
//
//   * a package that depends on `@types/node` must use the `.nvmrc` major,
//     in `package.json` and in what `package-lock.json` actually resolved;
//   * every package in the CI `npm-packages` matrix must declare
//     `engines.node` with that major as its minimum;
//   * no workflow may hard-code a different `node-version`.
//
// Types for a newer Node let `tsc` accept APIs the CI runtime does not have,
// and nothing fails until the line runs, so this has to be checked rather
// than left to review. Run with `node scripts/check_node_versions.mjs`.

import { readFileSync, readdirSync, existsSync } from "node:fs";
import { join, relative } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(fileURLToPath(import.meta.url), "..", "..");
const rel = (p) => relative(root, p).replaceAll("\\", "/");

const nvmrc = readFileSync(join(root, ".nvmrc"), "utf8").trim();
const ciMajor = leadingMajor(nvmrc);
if (ciMajor === null) {
    fail([`.nvmrc: expected a Node version such as "22", found "${nvmrc}"`]);
}

/** First integer in a version or range spec: "^22.1.0" -> 22, ">=22" -> 22. */
function leadingMajor(spec) {
    const m = /(\d+)/.exec(String(spec));
    return m ? Number(m[1]) : null;
}

function fail(errors) {
    console.error(`Node version drift from .nvmrc (Node ${nvmrc}):`);
    for (const e of errors) console.error(`  - ${e}`);
    console.error(
        "\nMoving to a new Node major means changing .nvmrc, @types/node and " +
            "engines.node together, in one PR (see CONTRIBUTING.md).",
    );
    process.exit(1);
}

/** Every package.json in the repo outside dependency and build trees. */
function findPackageJsons(dir, out = []) {
    for (const entry of readdirSync(dir, { withFileTypes: true })) {
        if (entry.isDirectory()) {
            if (["node_modules", ".git", "target", "dist", "lib"].includes(entry.name)) continue;
            findPackageJsons(join(dir, entry.name), out);
        } else if (entry.name === "package.json") {
            out.push(join(dir, entry.name));
        }
    }
    return out;
}

/** Workspace directories listed in ci.yml's npm-packages matrix. */
function ciMatrixDirs() {
    const ci = readFileSync(join(root, ".github/workflows/ci.yml"), "utf8");
    return [...ci.matchAll(/^\s*-\s*dir:\s*(\S+)\s*$/gm)].map((m) => m[1]);
}

const errors = [];

// 1. @types/node, declared and resolved.
const depFields = ["dependencies", "devDependencies", "peerDependencies", "optionalDependencies"];
let checked = 0;
for (const file of findPackageJsons(root)) {
    const pkg = JSON.parse(readFileSync(file, "utf8"));
    for (const field of depFields) {
        const spec = pkg[field]?.["@types/node"];
        if (spec === undefined) continue;
        checked++;
        if (leadingMajor(spec) !== ciMajor) {
            errors.push(`${rel(file)}: ${field}["@types/node"] is "${spec}", expected ^${ciMajor}`);
        }
    }
    const lockFile = join(file, "..", "package-lock.json");
    if (existsSync(lockFile)) {
        const lock = JSON.parse(readFileSync(lockFile, "utf8"));
        const resolved = lock.packages?.["node_modules/@types/node"]?.version;
        if (resolved !== undefined && leadingMajor(resolved) !== ciMajor) {
            errors.push(`${rel(lockFile)}: resolves @types/node ${resolved}, expected ${ciMajor}.x`);
        }
    }
}

// 2. engines.node on every package CI builds.
const matrix = ciMatrixDirs();
if (matrix.length === 0) {
    errors.push(".github/workflows/ci.yml: found no `- dir:` entries in the npm-packages matrix");
}
for (const dir of matrix) {
    const file = join(root, dir, "package.json");
    if (!existsSync(file)) {
        errors.push(`${dir}: listed in the CI matrix but has no package.json`);
        continue;
    }
    const engines = JSON.parse(readFileSync(file, "utf8")).engines?.node;
    if (engines === undefined) {
        errors.push(`${dir}/package.json: missing engines.node (expected ">=${ciMajor}")`);
    } else if (leadingMajor(engines) !== ciMajor) {
        errors.push(`${dir}/package.json: engines.node is "${engines}", expected ">=${ciMajor}"`);
    }
}

// 3. No workflow pins a Node version other than .nvmrc.
const workflows = join(root, ".github/workflows");
for (const name of readdirSync(workflows)) {
    if (!/\.ya?ml$/.test(name)) continue;
    const text = readFileSync(join(workflows, name), "utf8");
    for (const m of text.matchAll(/node-version:\s*['"]?([^\s'"#]+)/g)) {
        if (leadingMajor(m[1]) !== ciMajor) {
            errors.push(`.github/workflows/${name}: node-version ${m[1]}; use node-version-file: .nvmrc`);
        }
    }
}

if (errors.length > 0) fail(errors);
console.log(
    `OK: Node ${ciMajor} (.nvmrc); ${checked} @types/node declaration(s) and ` +
        `${matrix.length} CI package(s) agree.`,
);
