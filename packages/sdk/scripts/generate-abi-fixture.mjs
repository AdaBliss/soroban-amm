#!/usr/bin/env node
/**
 * Generate `packages/sdk/src/__fixtures__/contract-abi.json` — the conformance
 * oracle for `abi.test.ts`.
 *
 * The fixture records, for every contract the SDK wraps, each exported
 * entrypoint and the ordered `(name, type)` list of its arguments. It is the
 * expected shape a builder must produce and the set of names a read wrapper may
 * send.
 *
 * Why the Rust source and not `docs/abi.json`: `docs/abi.json` is produced by
 * `scripts/generate_abi.sh` from built WASM and is currently behind the contract
 * source (see the note in `governance.ts`), so it cannot be trusted as the
 * oracle. The Rust source is the thing the contracts are built from: it cannot
 * go stale without the contract changing, and it needs no WASM toolchain, so the
 * fixture regenerates with plain Node.
 *
 * Usage:
 *   node scripts/generate-abi-fixture.mjs           # write the fixture
 *   node scripts/generate-abi-fixture.mjs --check    # fail if the file is stale
 */

import { readFileSync, writeFileSync, mkdirSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";

const HERE = dirname(fileURLToPath(import.meta.url));
const SDK_ROOT = resolve(HERE, "..");
const REPO_ROOT = resolve(SDK_ROOT, "..", "..");
const FIXTURE_PATH = resolve(SDK_ROOT, "src/__fixtures__/contract-abi.json");

/**
 * Contract key -> the file that declares it plus the exact header of the real
 * `#[contractimpl]` block. Scoping to that block keeps test-module mocks (which
 * also use `#[contractimpl]`) out of the fixture.
 */
const CONTRACTS = {
  amm: { source: "contracts/amm/src/lib.rs", impl: "#[contractimpl]\nimpl AmmPool" },
  token: { source: "contracts/token/src/lib.rs", impl: "#[contractimpl]\nimpl LpToken" },
  factory: { source: "contracts/factory/src/lib.rs", impl: "#[contractimpl]\nimpl Factory" },
  router: { source: "contracts/router/src/lib.rs", impl: "#[contractimpl]\nimpl Router" },
  concentrated_liquidity: {
    source: "contracts/concentrated_liquidity/src/lib.rs",
    impl: "#[contractimpl]\nimpl ConcentratedLiquidity",
  },
  governance: { source: "contracts/governance/src/lib.rs", impl: "#[contractimpl]\nimpl Governance" },
  staking: { source: "contracts/staking/src/lib.rs", impl: "#[contractimpl]\nimpl Staking" },
  incentive_campaigns: { source: "contracts/incentive_campaigns/src/lib.rs", impl: "#[contractimpl]\nimpl IncentiveCampaigns" },
  pol_vesting: { source: "contracts/pol_vesting/src/lib.rs", impl: "#[contractimpl]\nimpl PolVestingContract" },
  oracle_aggregator: { source: "contracts/oracle_aggregator/src/lib.rs", impl: "#[contractimpl]\nimpl OracleAggregator" },
  twap_consumer: { source: "contracts/twap_consumer/src/lib.rs", impl: "#[contractimpl]\nimpl TwapConsumer" },
  twal_consumer: { source: "contracts/twal_consumer/src/lib.rs", impl: "#[contractimpl]\nimpl TwalConsumer" },
  dex_aggregator: { source: "contracts/dex_aggregator/src/lib.rs", impl: "#[contractimpl]\nimpl DexAggregator" },
  batch_auction: { source: "contracts/batch_auction/src/lib.rs", impl: "#[contractimpl]\nimpl BatchAuction" },
};

/** Slice a top-level block: from the line holding `header` to the first line that is exactly `}`. */
function contractBlock(source, header) {
  const start = source.indexOf(header);
  if (start === -1) {
    throw new Error(`entrypoint block not found in source: ${JSON.stringify(header)}`);
  }
  const end = source.indexOf("\n}", start);
  if (end === -1) throw new Error(`unterminated block for ${JSON.stringify(header)}`);
  return source.slice(start, end);
}

/** Split on commas that are not nested inside `()`, `<>` or `[]`. */
function splitTopLevel(text) {
  const parts = [];
  let depth = 0;
  let current = "";
  for (const ch of text) {
    if (ch === "(" || ch === "<" || ch === "[") depth += 1;
    else if (ch === ")" || ch === ">" || ch === "]") depth -= 1;
    if (ch === "," && depth === 0) {
      parts.push(current);
      current = "";
    } else {
      current += ch;
    }
  }
  if (current.trim() !== "") parts.push(current);
  return parts;
}

/** Extract every `pub fn` on a contract impl block as `{ name, inputs }`. */
function extractEntrypoints(block) {
  const entrypoints = [];
  const re = /\bpub fn\s+([A-Za-z0-9_]+)\s*\(/g;
  let match;
  while ((match = re.exec(block)) !== null) {
    const open = match.index + match[0].length - 1;
    let depth = 0;
    let close = -1;
    for (let i = open; i < block.length; i += 1) {
      const ch = block[i];
      if (ch === "(") depth += 1;
      else if (ch === ")") {
        depth -= 1;
        if (depth === 0) {
          close = i;
          break;
        }
      }
    }
    if (close === -1) throw new Error(`unterminated signature for ${match[1]}`);

    const params = splitTopLevel(block.slice(open + 1, close));
    const inputs = params
      .map((p) => p.trim())
      .filter((p) => p.length > 0 && !/^env\s*:/.test(p))
      .map((p) => {
        const colon = p.indexOf(":");
        if (colon === -1) throw new Error(`cannot parse parameter '${p}' of ${match[1]}`);
        return {
          name: p.slice(0, colon).trim(),
          type: p.slice(colon + 1).trim().replace(/\s+/g, " "),
        };
      });

    entrypoints.push({ name: match[1], inputs });
    re.lastIndex = close + 1;
  }
  return entrypoints;
}

/** Extract fields from every `#[contracttype]` struct in a contract source. */
function extractContractStructs(source) {
  const structs = {};
  const annotation = /#\[contracttype\]/g;
  while (annotation.exec(source) !== null) {
    const nextAnnotation = source.indexOf("#[contracttype]", annotation.lastIndex);
    const structMatch = /\bpub struct\s+([A-Za-z0-9_]+)\s*\{/.exec(
      source.slice(annotation.lastIndex, nextAnnotation === -1 ? undefined : nextAnnotation)
    );
    if (!structMatch) continue;

    const name = structMatch[1];
    const open = annotation.lastIndex + structMatch.index + structMatch[0].length - 1;
    let depth = 0;
    let close = -1;
    for (let index = open; index < source.length; index += 1) {
      if (source[index] === "{") depth += 1;
      else if (source[index] === "}") {
        depth -= 1;
        if (depth === 0) {
          close = index;
          break;
        }
      }
    }
    if (close === -1) throw new Error(`unterminated contracttype struct ${name}`);

    const fields = splitTopLevel(source.slice(open + 1, close).replace(/\/\/.*$/gm, ""))
      .map((part) => {
        const field = /\bpub\s+([A-Za-z0-9_]+)\s*:\s*([\s\S]*)/.exec(part);
        if (!field) return null;
        return {
          name: field[1],
          type: field[2].replace(/\/\/.*$/gm, "").trim().replace(/\s+/g, " "),
        };
      })
      .filter(Boolean);
    structs[name] = fields;
    annotation.lastIndex = close + 1;
  }
  return Object.fromEntries(Object.entries(structs).sort(([a], [b]) => a.localeCompare(b)));
}

function buildFixture() {
  const contracts = {};
  for (const [name, cfg] of Object.entries(CONTRACTS)) {
    const raw = readFileSync(resolve(REPO_ROOT, cfg.source), "utf8").replace(/\r\n/g, "\n");
    const entrypoints = cfg.impl ? extractEntrypoints(contractBlock(raw, cfg.impl)) : [];
    entrypoints.sort((a, b) => a.name.localeCompare(b.name));
    contracts[name] = { source: cfg.source, entrypoints, structs: extractContractStructs(raw) };
  }
  return {
    $comment:
      "Generated by packages/sdk/scripts/generate-abi-fixture.mjs from the contract Rust sources. Do not edit by hand.",
    contracts,
  };
}

const serialized = `${JSON.stringify(buildFixture(), null, 2)}\n`;
const check = process.argv.includes("--check");

if (check) {
  let existing = "";
  try {
    existing = readFileSync(FIXTURE_PATH, "utf8");
  } catch {
    console.error(`ERROR: ${FIXTURE_PATH} is missing. Run 'npm run generate:abi'.`);
    process.exit(1);
  }
  if (existing !== serialized) {
    console.error(
      `ERROR: ${FIXTURE_PATH} is out of date with the contract sources.\n` +
        "Run 'npm run generate:abi' and commit the result."
    );
    process.exit(1);
  }
  console.log("contract-abi.json is up to date with the contract sources.");
} else {
  mkdirSync(dirname(FIXTURE_PATH), { recursive: true });
  writeFileSync(FIXTURE_PATH, serialized);
  console.log(`Wrote ${FIXTURE_PATH}`);
}
