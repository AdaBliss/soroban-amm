import assert from "node:assert/strict";
import crypto from "node:crypto";
import fs from "node:fs";
import path from "node:path";
import { spawnSync } from "node:child_process";
import test from "node:test";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const contracts = fs.readFileSync(path.join(root, "scripts", "abi_contracts.sh"), "utf8")
  .match(/^  ([a-z][a-z0-9_]*)$/gm).map((line) => line.trim());

function fixture() {
  const tempRoot = fs.mkdtempSync(path.join(root, "target", "abi-check-test-"));
  const wasmDir = path.join(tempRoot, "wasm");
  const binDir = path.join(tempRoot, "bin");
  fs.mkdirSync(wasmDir);
  fs.mkdirSync(binDir);
  for (const name of [...contracts, "soroban_amm_sdk", "flash_loan_receiver"]) {
    fs.writeFileSync(path.join(wasmDir, `${name}.wasm`), "fixture");
  }

  const stellar = path.join(binDir, "stellar");
  fs.writeFileSync(stellar, [
    "#!/usr/bin/env bash",
    "set -euo pipefail",
    "wasm=''",
    "while (($#)); do",
    "  if [[ \"$1\" == '--wasm' ]]; then wasm=\"$2\"; shift 2; else shift; fi",
    "done",
    "name=\"$(basename \"$wasm\" .wasm)\"",
    "printf '{\"functions\":[{\"name\":\"%s\"}],\"events\":[]}\\n' \"$name\"",
    "",
  ].join("\n"), { mode: 0o755 });

  const relative = (file) => path.relative(root, file).split(path.sep).join("/");
  return {
    tempRoot,
    relative,
    env: {
      ...process.env,
      ABI_WASM_DIR: relative(wasmDir),
      STELLAR_BIN: relative(stellar),
    },
  };
}

function run(script, args, env) {
  return spawnSync("bash", [script, ...args], { cwd: root, env, encoding: "utf8" });
}

function generate(f, output) {
  return run("scripts/generate_abi.sh", [f.relative(output)], f.env);
}

function hash(file) {
  return crypto.createHash("sha256").update(fs.readFileSync(file)).digest("hex");
}

test("check_abi accepts an up-to-date ABI", (t) => {
  const f = fixture();
  t.after(() => fs.rmSync(f.tempRoot, { recursive: true, force: true }));
  const reference = path.join(f.tempRoot, "reference.json");
  const generated = generate(f, reference);
  assert.equal(generated.status, 0, generated.stderr);
  const checked = run("scripts/check_abi.sh", ["--reference", f.relative(reference)], f.env);
  assert.equal(checked.status, 0, checked.stderr);
  assert.match(checked.stdout, /matches the interface generated/);
});

test("check_abi summarizes a stale ABI and never mutates docs/abi.json", (t) => {
  const f = fixture();
  t.after(() => fs.rmSync(f.tempRoot, { recursive: true, force: true }));
  const reference = path.join(f.tempRoot, "reference.json");
  const generated = generate(f, reference);
  assert.equal(generated.status, 0, generated.stderr);
  const stale = JSON.parse(fs.readFileSync(reference, "utf8"));
  stale.contracts.amm.functions = [{ name: "obsolete_function" }];
  fs.writeFileSync(reference, `${JSON.stringify(stale, null, 2)}\n`);

  const before = hash(path.join(root, "docs", "abi.json"));
  const checked = run("scripts/check_abi.sh", ["--reference", f.relative(reference)], f.env);
  const output = `${checked.stdout}${checked.stderr}`;
  assert.notEqual(checked.status, 0);
  assert.match(output, /amm: functions added \[amm\]; functions removed \[obsolete_function\]/);
  assert.ok(output.split(/\r?\n/).length < 100);
  assert.equal(hash(path.join(root, "docs", "abi.json")), before);
});

test("generator ignores non-contract WASM files in the release directory", (t) => {
  const f = fixture();
  t.after(() => fs.rmSync(f.tempRoot, { recursive: true, force: true }));
  const output = path.join(f.tempRoot, "generated.json");
  const generated = generate(f, output);
  assert.equal(generated.status, 0, generated.stderr);
  const keys = Object.keys(JSON.parse(fs.readFileSync(output, "utf8")).contracts);
  assert.deepEqual(keys, contracts);
  assert.ok(!keys.includes("soroban_amm_sdk"));
  assert.ok(!keys.includes("flash_loan_receiver"));
});

test("generator gives a clear error when Stellar CLI is missing", (t) => {
  const f = fixture();
  t.after(() => fs.rmSync(f.tempRoot, { recursive: true, force: true }));
  const generated = run("scripts/generate_abi.sh", [f.relative(path.join(f.tempRoot, "unused.json"))], {
    ...f.env,
    STELLAR_BIN: "abi-check-test-stellar-not-installed",
  });
  assert.notEqual(generated.status, 0);
  assert.match(generated.stderr, /Stellar CLI .* was not found.*\.stellar-version/);
});

function walkRustFiles(directory) {
  return fs.readdirSync(directory, { withFileTypes: true }).flatMap((entry) => {
    const item = path.join(directory, entry.name);
    if (entry.isDirectory()) return walkRustFiles(item);
    return entry.isFile() && entry.name.endsWith(".rs") ? [item] : [];
  });
}

function contractFunctionNames(source) {
  const testCode = /^\s*#\[cfg\([^\]]*\btest\b[^\]]*\)\][^\n]*$/m.exec(source);
  const production = testCode ? source.slice(0, testCode.index) : source;
  const result = new Set();
  const contractImpl = /#\[contractimpl(?:\([^\]]*\))?\]\s*impl\b/g;

  for (const marker of production.matchAll(contractImpl)) {
    const open = production.indexOf("{", marker.index + marker[0].length);
    if (open < 0) continue;
    let depth = 1;
    let cursor = open + 1;
    while (cursor < production.length && depth > 0) {
      if (production[cursor] === "{") depth++;
      if (production[cursor] === "}") depth--;
      cursor++;
    }
    const body = production.slice(open + 1, cursor - 1);
    for (const method of body.matchAll(/\bpub\s+(?:async\s+)?fn\s+([A-Za-z_][A-Za-z0-9_]*)\s*\(/g)) {
      result.add(method[1]);
    }
  }
  return result;
}

test("docs ABI covers public functions in every deployable contract impl", () => {
  const abi = JSON.parse(fs.readFileSync(path.join(root, "docs", "abi.json"), "utf8"));
  assert.deepEqual(Object.keys(abi.contracts), contracts);

  const contractDirs = fs.readdirSync(path.join(root, "contracts"), { withFileTypes: true })
    .filter((entry) => entry.isDirectory());
  for (const abiName of contracts) {
    const crate = contractDirs.find((entry) => {
      const manifest = fs.readFileSync(path.join(root, "contracts", entry.name, "Cargo.toml"), "utf8");
      const packageName = /^name\s*=\s*"([^"]+)"/m.exec(manifest)?.[1];
      return packageName?.replaceAll("-", "_") === abiName;
    });
    assert.ok(crate, `No contract crate found for ABI key ${abiName}`);

    const sourceNames = new Set();
    for (const file of walkRustFiles(path.join(root, "contracts", crate.name, "src"))) {
      if (/(?:^|_)(?:test|tests|prop_tests|bench)(?:_|\.)/i.test(path.basename(file))) continue;
      for (const name of contractFunctionNames(fs.readFileSync(file, "utf8"))) sourceNames.add(name);
    }
    const entries = Array.isArray(abi.contracts[abiName])
      ? abi.contracts[abiName]
      : abi.contracts[abiName].functions ?? [];
    const abiNames = new Set(entries.map((entry) => entry.function_v0?.name ?? entry.name).filter(Boolean));
    const missing = [...sourceNames].filter((name) => !abiNames.has(name));
    assert.deepEqual(missing, [], `${abiName} ABI is missing source functions: ${missing.join(", ")}`);
  }
});
