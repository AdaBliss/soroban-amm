// Extracts every contract event emit site from the Rust sources.
//
// An emit site is a call to `emit_versioned_event!(env, <topics>, <payload>)`
// or a raw `env.events().publish(<topics>, <payload>)`. For each one this
// records the contract (directory under contracts/), the event name (the first
// topic), the number of topics and the payload arity, so the webhook-streamer
// schema file can be checked against what the contracts actually emit.
//
// Usage as a module: import { extractEmitSites } from "./event_emit_sites.mjs"
// Usage as a CLI:    node scripts/event_emit_sites.mjs   (prints JSON)

import { readdirSync, readFileSync, statSync } from "node:fs";
import { dirname, join, relative } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");
const CONTRACTS_DIR = join(ROOT, "contracts");

/** Directories under contracts/ that hold no deployed contract. */
const SKIPPED_DIRS = new Set(["amm-fuzz", "integration-tests", "pool_interfaces"]);

/** Blanks out comments and string-literal-free regions we must not scan, keeping offsets. */
function stripComments(src) {
  let out = "";
  let i = 0;
  while (i < src.length) {
    const two = src.slice(i, i + 2);
    if (two === "//") {
      while (i < src.length && src[i] !== "\n") out += (src[i++], " ");
    } else if (two === "/*") {
      let depth = 0;
      do {
        if (src.slice(i, i + 2) === "/*") { depth++; out += "  "; i += 2; }
        else if (src.slice(i, i + 2) === "*/") { depth--; out += "  "; i += 2; }
        else { out += src[i] === "\n" ? "\n" : " "; i++; }
      } while (depth > 0 && i < src.length);
    } else if (src[i] === '"') {
      out += src[i++];
      while (i < src.length && src[i] !== '"') {
        if (src[i] === "\\") { out += src[i++]; }
        out += src[i++];
      }
      out += src[i++] ?? "";
    } else {
      out += src[i++];
    }
  }
  return out;
}

/** Index of the bracket matching the opener at `open`, or -1. */
function matching(src, open) {
  const pairs = { "(": ")", "[": "]", "{": "}" };
  const stack = [];
  for (let i = open; i < src.length; i++) {
    const c = src[i];
    if (c === '"') {
      i++;
      while (i < src.length && src[i] !== '"') { if (src[i] === "\\") i++; i++; }
    } else if (pairs[c]) {
      stack.push(pairs[c]);
    } else if (c === ")" || c === "]" || c === "}") {
      if (stack.pop() !== c) return -1;
      if (stack.length === 0) return i;
    }
  }
  return -1;
}

/** Splits `text` on top-level commas. */
function splitTopLevel(text) {
  const parts = [];
  let depth = 0;
  let start = 0;
  for (let i = 0; i < text.length; i++) {
    const c = text[i];
    if (c === '"') { i++; while (i < text.length && text[i] !== '"') { if (text[i] === "\\") i++; i++; } }
    else if ("([{".includes(c)) depth++;
    else if (")]}".includes(c)) depth--;
    else if (c === "<" && /[A-Za-z_]/.test(text[i + 1] ?? "")) { /* generics: ignore */ }
    else if (c === "," && depth === 0) { parts.push(text.slice(start, i)); start = i + 1; }
  }
  parts.push(text.slice(start));
  return parts.map((p) => p.trim());
}

/** Splits `(a, b,)` into its elements; returns null if `expr` is not a tuple. */
function tupleElements(expr) {
  const e = expr.trim();
  if (!e.startsWith("(") || matching(e, 0) !== e.length - 1) return null;
  const inner = e.slice(1, -1);
  const parts = splitTopLevel(inner);
  const hasTrailingComma = parts.length > 0 && parts[parts.length - 1] === "";
  const elements = parts.filter((p, i) => !(i === parts.length - 1 && p === ""));
  // `(x)` is a parenthesised expression, not a tuple; `(x,)` and `()` are.
  if (elements.length === 1 && !hasTrailingComma) return null;
  return elements;
}

function constantTable(src) {
  const table = new Map();
  const re = /const\s+([A-Z0-9_]+)\s*:\s*[^=]+=\s*(?:soroban_sdk::)?(?:symbol_short!\s*\(\s*"([^"]+)"\s*\)|"([^"]+)")/g;
  for (let m; (m = re.exec(src)); ) table.set(m[1], m[2] ?? m[3]);
  return table;
}

function eventName(topic, constants) {
  let m = /symbol_short!\s*\(\s*"([^"]+)"\s*\)/.exec(topic);
  if (m) return m[1];
  m = /Symbol::new\s*\([^,]+,\s*"([^"]+)"\s*\)/.exec(topic);
  if (m) return m[1];
  m = /Symbol::new\s*\([^,]+,\s*([A-Z0-9_]+)\s*\)/.exec(topic);
  if (m && constants.has(m[1])) return constants.get(m[1]);
  m = /^([A-Z0-9_]+)(?:\.clone\(\))?$/.exec(topic.trim());
  if (m && constants.has(m[1])) return constants.get(m[1]);
  return null;
}

/** Removes `#[cfg(...test...)] mod ... { }` blocks (and `mod x;` declarations). */
function removeTestModules(src) {
  let out = src;
  for (;;) {
    const m = /#\[cfg\([^\]]*\btest\b[^\]]*\)\]\s*(?:pub\s+)?mod\s+\w+\s*(\{|;)/.exec(out);
    if (!m) return out;
    const open = m.index + m[0].length - 1;
    const end = m[1] === ";" ? open : matching(out, open);
    const stop = end === -1 ? out.length : end + 1;
    out = out.slice(0, m.index) + out.slice(stop).replace(/^/, "");
  }
}

function lineOf(src, index) {
  let line = 1;
  for (let i = 0; i < index; i++) if (src[i] === "\n") line++;
  return line;
}

export function extractFromSource(source, file, contract) {
  const src = removeTestModules(stripComments(source));
  const constants = constantTable(src);
  const sites = [];
  const re = /(emit_versioned_event!|\.publish)\s*\(/g;
  for (let m; (m = re.exec(src)); ) {
    const open = m.index + m[0].length - 1;
    const close = matching(src, open);
    if (close === -1) continue;
    const args = splitTopLevel(src.slice(open + 1, close));
    const versioned = m[1] === "emit_versioned_event!";
    const [topicsExpr, payloadExpr] = versioned ? [args[1], args[2]] : [args[0], args[1]];
    if (topicsExpr === undefined || payloadExpr === undefined) continue;
    // The macro definition itself (and its doc examples) use `$` metavariables.
    if (topicsExpr.includes("$") || payloadExpr.includes("$")) continue;

    const topics = tupleElements(topicsExpr) ?? [topicsExpr];
    const name = eventName(topics[0], constants);

    let payloadArity;
    let shape;
    let args_ = [];
    if (versioned) {
      const elements = tupleElements(payloadExpr);
      shape = elements ? "tuple" : "scalar";
      payloadArity = elements ? elements.length : 1;
      args_ = elements ?? [payloadExpr];
    } else {
      // A raw publish carries the version itself: (VERSION, payload).
      const envelope = tupleElements(payloadExpr);
      if (envelope && envelope.length === 2 && /VERSION/.test(envelope[0])) {
        const elements = tupleElements(envelope[1]);
        shape = elements ? "tuple" : "scalar";
        payloadArity = elements ? elements.length : 1;
        args_ = elements ?? [envelope[1]];
      } else {
        shape = "unversioned";
        const elements = tupleElements(payloadExpr);
        payloadArity = elements ? elements.length : 1;
        args_ = elements ?? [payloadExpr];
      }
    }
    sites.push({
      contract,
      event: name,
      topics: topics.length,
      arity: payloadArity,
      shape,
      versioned: shape !== "unversioned",
      args: args_.map((a) => a.replace(/\s+/g, " ")),
      source: `${file}:${lineOf(src, m.index)}`,
    });
  }
  return sites;
}

function walk(dir) {
  const files = [];
  for (const entry of readdirSync(dir)) {
    const path = join(dir, entry);
    if (statSync(path).isDirectory()) {
      if (entry === "tests" || entry === "target") continue;
      files.push(...walk(path));
    } else if (entry.endsWith(".rs") && !/(^|[._-])tests?([._-]|\.rs$)/.test(entry)) {
      files.push(path);
    }
  }
  return files;
}

export function extractEmitSites(contractsDir = CONTRACTS_DIR) {
  const sites = [];
  for (const contract of readdirSync(contractsDir).sort()) {
    const dir = join(contractsDir, contract, "src");
    if (SKIPPED_DIRS.has(contract) || !safeIsDir(dir)) continue;
    for (const file of walk(dir).sort()) {
      sites.push(...extractFromSource(readFileSync(file, "utf8"), relative(ROOT, file), contract));
    }
  }
  return sites;
}

function safeIsDir(path) {
  try { return statSync(path).isDirectory(); } catch { return false; }
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  console.log(JSON.stringify(extractEmitSites(), null, 2));
}
