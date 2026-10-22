/**
 * Contract-ABI conformance tests for the SDK's parameter builders and reads —
 * Issue #1047 (supersedes #830).
 *
 * The oracle is `__fixtures__/contract-abi.json`, generated from the contract
 * Rust sources by `scripts/generate-abi-fixture.mjs` (`npm run generate:abi`).
 * It records every exported entrypoint and its ordered `(name, type)` arguments,
 * so these tests fail the moment a builder drifts from the deployed signature or
 * a read wrapper names a function the contract does not export.
 *
 * `docs/abi.json` is deliberately not used: it is produced from built WASM and
 * is behind the contract source (see the note in `governance.ts`), so it cannot
 * be the conformance oracle. The fixture regenerates from source, which the
 * contract cannot change without changing.
 */

import { describe, it, expect } from "vitest";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { Address, Networks, nativeToScVal, scValToNative, xdr } from "@stellar/stellar-sdk";

import { AmmPool } from "./AmmPool.js";
import { ConcentratedLiquidityClient } from "./cl.js";
import { FactoryClient } from "./factory.js";
import { RouterClient } from "./router.js";
import { TokenClient } from "./token.js";


/** The `invokeContract` payload of the first operation of a built transaction. */
function invokeOf(tx: { operations: unknown[] }): { functionName: unknown; args: xdr.ScVal[] } {
  const op = tx.operations[0] as {
    func: { invokeContract: { functionName: unknown; args: xdr.ScVal[] } };
  };
  return op.func.invokeContract;
}

/** The XDR type discriminant name of an ScVal, e.g. `"scvI128"`. */
function scvName(v: xdr.ScVal): string {
  return `scv${v.constructor.name.replace(/^ScVal/, "")}`;
}

// ── Fixture: the conformance oracle ───────────────────────────────────────────

interface AbiEntrypoint {
  name: string;
  inputs: Array<{ name: string; type: string }>;
}

interface AbiFixture {
  contracts: Record<string, { source: string; entrypoints: AbiEntrypoint[] }>;
}

const FIXTURE_URL = new URL("./__fixtures__/contract-abi.json", import.meta.url);
const FIXTURE = JSON.parse(readFileSync(fileURLToPath(FIXTURE_URL), "utf8")) as AbiFixture;

function entrypoint(contract: string, name: string): AbiEntrypoint {
  const found = FIXTURE.contracts[contract]?.entrypoints.find((e) => e.name === name);
  if (!found) throw new Error(`entrypoint ${contract}::${name} is missing from the fixture`);
  return found;
}

/** Expected ScVal type discriminants for a contract parameter type. */
const SCV_FOR_TYPE: Record<string, string[]> = {
  Address: ["scvAddress"],
  i128: ["scvI128"],
  i64: ["scvI64"],
  u128: ["scvU128"],
  u64: ["scvU64"],
  u32: ["scvU32"],
  i32: ["scvI32"],
  bool: ["scvBool"],
  String: ["scvString"],
  Symbol: ["scvSymbol"],
  Bytes: ["scvBytes"],
  "BytesN<32>": ["scvBytes"],
  "Vec<Address>": ["scvVec"],
  "Vec<u64>": ["scvVec"],
  // Option<T> is scvVoid for None and scvVec([value]) for Some.
  "Option<Address>": ["scvVoid", "scvVec"],
  "Option<BytesN<32>>": ["scvVoid", "scvVec"],
};

// ── Test harness ──────────────────────────────────────────────────────────────

const CONTRACT_ID = "CA3D5KRYM6CB7OWQ6TWYRR3Z4T7GNZLKERYNZGGA5SOAOPIFY6YQGAXE";
const ALICE = "GA5WUJ54Z23KILLCUOUNAKTPBVZWKMQVO4O6EQ5GHLAERIMLLHNCSKYH";
const BOB = "GAEQSCIJBEEQSCIJBEEQSCIJBEEQSCIJBEEQSCIJBEEQSCIJBEEQSH7S";
const TOKEN_A = "CAAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQC526";
const TOKEN_B = "CABAEAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAFNSZ";

const config = {
  rpcUrl: "https://rpc.example.invalid",
  networkPassphrase: Networks.TESTNET,
  contractId: CONTRACT_ID,
};

const cl = new ConcentratedLiquidityClient(config);
const router = new RouterClient(config);
const token = new TokenClient(config);
const factory = new FactoryClient(config);

/** The XDR type discriminant name of an ScVal, e.g. "scvI128". */
function typeOf(v: xdr.ScVal): string {
  return scvName(v);
}

/** Assert the ordered list of XDR type discriminants of an argument array. */
function expectTypes(args: xdr.ScVal[], types: string[]) {
  expect(args).toHaveLength(types.length);
  expect(args.map(typeOf)).toEqual(types);
}

const addrv = (a: string) => nativeToScVal(Address.fromString(a));
const i128v = (n: bigint) => nativeToScVal(n, { type: "i128" });
const u64v = (n: bigint) => nativeToScVal(n, { type: "u64" });

/** One recorded `invokeContract` operation. */
interface Call {
  method: string;
  args: xdr.ScVal[];
}

/**
 * Replace a client's RPC server with a responder and record the calls it makes.
 * Asserts what actually goes on the wire, not what the method is called.
 */
function mockServer<T>(client: T, responder: (call: Call) => xdr.ScVal | Error): Call[] {
  const server = (client as unknown as { server: Record<string, unknown> }).server;
  const calls: Call[] = [];
  server.simulateTransaction = async (tx: { operations: unknown[] }) => {
    const invoke = invokeOf(tx);
    const call: Call = { method: String(invoke.functionName), args: invoke.args };
    calls.push(call);
    const out = responder(call);
    if (out instanceof Error) return { error: out.message };
    return { result: { retval: out } };
  };
  return calls;
}

/** An `get_info` return value as the contract emits it. */
function poolInfoScVal(overrides: Record<string, xdr.ScVal> = {}): xdr.ScVal {
  const fields: Record<string, xdr.ScVal> = {
    token_a: addrv(TOKEN_A),
    token_b: addrv(TOKEN_B),
    reserve_a: i128v(1_000_000n),
    reserve_b: i128v(4_000_000n),
    total_shares: i128v(2_000_000n),
    fee_bps: i128v(30n),
    flash_loan_fee_bps: i128v(5n),
    admin: addrv(ALICE),
    fee_recipient: addrv(BOB),
    protocol_fee_bps: i128v(5n),
    lp_rebate_bps: i128v(0n),
    ...overrides,
  };
  return xdr.ScVal.scvMap(
    Object.entries(fields).map(
      ([key, val]) => new xdr.ScMapEntry({ key: xdr.ScVal.scvSymbol(key), val })
    )
  );
}

// ── 1. Fixture-driven builder conformance ─────────────────────────────────────

describe("parameter builders match the generated contract ABI fixture", () => {
  const cases: Array<{
    label: string;
    contract: string;
    entrypoint: string;
    args: xdr.ScVal[];
  }> = [
    {
      label: "cl.mintPositionParams",
      contract: "concentrated_liquidity",
      entrypoint: "mint_position",
      args: cl.mintPositionParams(ALICE, -100, 100, 5_000n, 6_000n, 4_500n, 5_400n, 1_700_000_000n),
    },
    {
      label: "cl.modifyPositionParams",
      contract: "concentrated_liquidity",
      entrypoint: "modify_position",
      args: cl.modifyPositionParams(ALICE, -100, 100, 1_000n, 900n, 900n, 1_700_000_000n),
    },
    {
      label: "cl.burnPositionParams",
      contract: "concentrated_liquidity",
      entrypoint: "burn_position",
      args: cl.burnPositionParams(ALICE, -60, 60, 2_500n),
    },
    {
      label: "cl.collectFeesParams",
      contract: "concentrated_liquidity",
      entrypoint: "collect_fees",
      args: cl.collectFeesParams(ALICE, -60, 60),
    },
    {
      label: "cl.swapParams",
      contract: "concentrated_liquidity",
      entrypoint: "swap",
      args: cl.swapParams(ALICE, true, 1_000n, 79_228_162_514_264_337_593_543_950_336n, 950n, 1_700_000_000n),
    },
    {
      label: "token.transferParams",
      contract: "token",
      entrypoint: "transfer",
      args: token.transferParams(ALICE, BOB, 1_000n),
    },
    {
      label: "token.transferFromParams",
      contract: "token",
      entrypoint: "transfer_from",
      args: token.transferFromParams(ALICE, BOB, ALICE, 1_000n),
    },
    {
      label: "token.approveParams",
      contract: "token",
      entrypoint: "approve",
      args: token.approveParams(ALICE, BOB, 1_000n, 500_000),
    },
    {
      label: "token.mintParams",
      contract: "token",
      entrypoint: "mint",
      args: token.mintParams(ALICE, 1_000n),
    },
    {
      label: "token.burnParams",
      contract: "token",
      entrypoint: "burn",
      args: token.burnParams(ALICE, 1_000n),
    },
    {
      label: "router.swapExactInParams",
      contract: "router",
      entrypoint: "swap_exact_in",
      args: router.swapExactInParams({
        trader: ALICE,
        path: [TOKEN_A, TOKEN_B],
        amountIn: 1_000n,
        minAmountOut: 900n,
        deadline: 1_700_000_000n,
      }),
    },
    {
      label: "router.swapExactOutParams",
      contract: "router",
      entrypoint: "swap_exact_out",
      args: router.swapExactOutParams({
        trader: BOB,
        path: [TOKEN_A, TOKEN_B],
        amountOut: 500n,
        maxIn: 600n,
        deadline: 1_700_000_000n,
      }),
    },
    {
      label: "factory.createPoolParams (no governance)",
      contract: "factory",
      entrypoint: "create_pool",
      args: factory.createPoolParams(ALICE, TOKEN_A, TOKEN_B, 2n),
    },
    {
      label: "factory.createPoolWithFeeBpsParams (no governance)",
      contract: "factory",
      entrypoint: "create_pool_with_fee_bps",
      args: factory.createPoolWithFeeBpsParams(ALICE, TOKEN_A, TOKEN_B, 30n),
    },
  ];

  for (const { label, contract, entrypoint: name, args } of cases) {
    it(`${label} matches ${contract}::${name} arity and types`, () => {
      const ep = entrypoint(contract, name);
      expect(args).toHaveLength(ep.inputs.length);
      for (const [i, input] of ep.inputs.entries()) {
        const expected = SCV_FOR_TYPE[input.type];
        if (!expected) {
          throw new Error(`fixture type '${input.type}' (${input.name}) has no ScVal mapping`);
        }
        expect(expected).toContain(typeOf(args[i]));
      }
    });
  }
});

// ── 2. Read wrappers name real entrypoints ────────────────────────────────────

describe("read wrappers send only names the contracts export", () => {
  const clients: Array<{ contract: string; file: string }> = [
    { contract: "amm", file: "AmmPool.ts" },
    { contract: "token", file: "token.ts" },
    { contract: "factory", file: "factory.ts" },
    { contract: "router", file: "router.ts" },
    { contract: "concentrated_liquidity", file: "cl.ts" },
  ];

  for (const { contract, file } of clients) {
    const source = readFileSync(new URL(`./${file}`, import.meta.url), "utf8");
    const methodNames = [
      ...new Set([...source.matchAll(/simulate\(\s*"([A-Za-z0-9_]+)"/g)].map((m) => m[1])),
    ];
    const entrypoints = new Set(FIXTURE.contracts[contract].entrypoints.map((e) => e.name));

    it(`${file} sends only ${contract} entrypoints`, () => {
      expect(methodNames.length).toBeGreaterThan(0);
      expect(methodNames.filter((m) => !entrypoints.has(m))).toEqual([]);
    });

    it(`${file} documents a real ${contract} entrypoint for every builder`, () => {
      const targets = [
        ...new Set([...source.matchAll(/Parameters for `([a-z0-9_]+)`/g)].map((m) => m[1])),
      ];
      expect(targets.filter((m) => !entrypoints.has(m))).toEqual([]);
    });
  }

  it("no client sends the entrypoints the issue reported as nonexistent", () => {
    const invented = ["get_name", "get_flash_loan_fee_bps", "pool_count"];
    for (const name of invented) {
      for (const contract of ["amm", "factory"]) {
        expect(entrypointNames(contract)).not.toContain(name);
      }
    }
    const amm = readFileSync(new URL("./AmmPool.ts", import.meta.url), "utf8");
    const factorySrc = readFileSync(new URL("./factory.ts", import.meta.url), "utf8");
    for (const name of ["get_name", "get_flash_loan_fee_bps", "pool_count"]) {
      expect(amm).not.toContain(`simulate("${name}"`);
      expect(factorySrc).not.toContain(`simulate("${name}"`);
    }
  });

  function entrypointNames(contract: string): string[] {
    return FIXTURE.contracts[contract].entrypoints.map((e) => e.name);
  }
});

// ── 3. Corrected builders ─────────────────────────────────────────────────────

describe("cl.mintPositionParams", () => {
  // contracts/concentrated_liquidity/src/lib.rs:706
  // (provider, lower_tick, upper_tick, amount_a_desired, amount_b_desired,
  //  min_a, min_b, deadline: u64)
  const args = cl.mintPositionParams(ALICE, -100, 100, 5_000n, 6_000n, 4_500n, 5_400n, 1_700_000_000n);

  it("matches the contract's 8-argument arity, order and types", () => {
    expectTypes(args, [
      "scvAddress",
      "scvI32",
      "scvI32",
      "scvI128",
      "scvI128",
      "scvI128",
      "scvI128",
      "scvU64",
    ]);
  });

  it("encodes the deadline as the trailing u64", () => {
    expect(typeOf(args[7])).toBe("scvU64");
    expect(scValToNative(args[7])).toBe(1_700_000_000n);
  });

  it("orders its arguments exactly as the contract does", () => {
    const ep = entrypoint("concentrated_liquidity", "mint_position");
    expect(ep.inputs.map((i) => i.name)).toEqual([
      "provider",
      "lower_tick",
      "upper_tick",
      "amount_a_desired",
      "amount_b_desired",
      "min_a",
      "min_b",
      "deadline",
    ]);
  });
});

describe("token.approveParams", () => {
  // contracts/token/src/lib.rs:334
  // (from, spender, amount: i128, live_until_ledger: u32)
  const args = token.approveParams(ALICE, BOB, 1_000n, 500_000);

  it("matches the contract's 4-argument arity, order and types", () => {
    expectTypes(args, ["scvAddress", "scvAddress", "scvI128", "scvU32"]);
  });

  it("encodes live_until_ledger as a trailing u32", () => {
    expect(typeOf(args[3])).toBe("scvU32");
    expect(scValToNative(args[3])).toBe(500_000);
  });

  it("matches the fixture's approve inputs by name", () => {
    const ep = entrypoint("token", "approve");
    expect(ep.inputs.map((i) => i.name)).toEqual(["from", "spender", "amount", "live_until_ledger"]);
  });
});

describe("factory.createPoolParams", () => {
  // contracts/factory/src/lib.rs:253
  // (caller, token_a, token_b, fee_tier: i128, governance_wasm_hash: Option<BytesN<32>>)
  const args = factory.createPoolParams(ALICE, TOKEN_A, TOKEN_B, 2n);

  it("leads with caller and encodes fee_tier, not fee_bps", () => {
    expectTypes(args, ["scvAddress", "scvAddress", "scvAddress", "scvI128", "scvVoid"]);
    expect(scValToNative(args[0])).toBe(ALICE);
    expect(scValToNative(args[3])).toBe(2n);
  });

  it("encodes None as scvVoid and Some as scvVec([scvBytes])", () => {
    const hashHex = "aa".repeat(32);
    const withGovernance = factory.createPoolParams(ALICE, TOKEN_A, TOKEN_B, 2n, hashHex);
    expect(typeOf(withGovernance[4])).toBe("scvVec");
    const decoded = scValToNative(withGovernance[4]) as unknown[];
    expect(decoded).toHaveLength(1);
    expect(Buffer.from(decoded[0] as Uint8Array)).toEqual(Buffer.from(hashHex, "hex"));
  });
});

describe("factory.createPoolWithFeeBpsParams", () => {
  // contracts/factory/src/lib.rs:277
  // (caller, token_a, token_b, fee_bps: i128, governance_wasm_hash: Option<BytesN<32>>)
  const args = factory.createPoolWithFeeBpsParams(ALICE, TOKEN_A, TOKEN_B, 30n);

  it("matches create_pool_with_fee_bps arity, order and types", () => {
    expectTypes(args, ["scvAddress", "scvAddress", "scvAddress", "scvI128", "scvVoid"]);
    expect(scValToNative(args[3])).toBe(30n);
  });

  it("is the bps variant of create_pool", () => {
    const ep = entrypoint("factory", "create_pool_with_fee_bps");
    expect(ep.inputs.map((i) => i.name)).toEqual([
      "caller",
      "token_a",
      "token_b",
      "fee_bps",
      "governance_wasm_hash",
    ]);
  });
});

// ── 4. Corrected reads ────────────────────────────────────────────────────────

describe("AmmPool.getFlashLoanFeeBps", () => {
  it("reads flash_loan_fee_bps from get_info, not a private helper", async () => {
    const client = new AmmPool(config);
    const calls = mockServer(client, () => poolInfoScVal({ flash_loan_fee_bps: i128v(9n) }));
    expect(await client.getFlashLoanFeeBps()).toBe(9n);
    expect(calls).toHaveLength(1);
    expect(calls[0].method).toBe("get_info");
  });
});

describe("FactoryClient.poolCount", () => {
  it("calls the exported get_pool_count entrypoint", async () => {
    const client = new FactoryClient(config);
    const calls = mockServer(client, () => u64v(7n));
    expect(await client.poolCount()).toBe(7n);
    expect(calls).toHaveLength(1);
    expect(calls[0].method).toBe("get_pool_count");
  });
});

describe("cl.observe", () => {
  it("sends a single scvU64 and decodes a single i64", async () => {
    const client = new ConcentratedLiquidityClient(config);
    const calls = mockServer(client, () => nativeToScVal(123n, { type: "i64" }));
    expect(await client.observe(3_600n)).toBe(123n);
    expect(calls).toHaveLength(1);
    expect(calls[0].method).toBe("observe");
    expectTypes(calls[0].args, ["scvU64"]);
    expect(scValToNative(calls[0].args[0])).toBe(3_600n);
  });

  it("observeBatch loops one single-point call per sample", async () => {
    const client = new ConcentratedLiquidityClient(config);
    const calls = mockServer(client, (call) => {
      const secondsAgo = scValToNative(call.args[0]) as bigint;
      return nativeToScVal(secondsAgo * 2n, { type: "i64" });
    });
    expect(await client.observeBatch([100n, 200n])).toEqual([200n, 400n]);
    expect(calls.map((c) => c.method)).toEqual(["observe", "observe"]);
    expect(calls.every((c) => typeOf(c.args[0]) === "scvU64")).toBe(true);
  });
});

// ── 5. Already-correct builders, kept under the fixture sweep ──────────────────

describe("cl.swapParams", () => {
  const args = cl.swapParams(ALICE, true, 1_000n, 79_228_162_514_264_337_593_543_950_336n, 950n, 1_700_000_000n);

  it("passes direction as a bool, never as a token address", () => {
    expect(typeOf(args[1])).toBe("scvBool");
    expect(scValToNative(cl.swapParams(ALICE, false, 1n, 0n, 0n, 0n)[1])).toBe(false);
  });

  it("encodes sqrt_price_limit_x96 as u128, not i128", () => {
    expect(typeOf(args[3])).toBe("scvU128");
  });
});

describe("cl.burnPositionParams / cl.collectFeesParams", () => {
  it("burn_position takes the liquidity amount as the 4th argument", () => {
    const args = cl.burnPositionParams(ALICE, -60, 60, 2_500n);
    expectTypes(args, ["scvAddress", "scvI32", "scvI32", "scvI128"]);
    expect(scValToNative(args[3])).toBe(2_500n);
  });

  it("collect_fees takes only the provider and tick range", () => {
    expectTypes(cl.collectFeesParams(ALICE, -60, 60), ["scvAddress", "scvI32", "scvI32"]);
  });
});

describe("router.getAmountOutPath argument order", () => {
  it("sends the path first and the amount second", async () => {
    const client = new RouterClient(config);
    const calls = mockServer(client, () => i128v(7n));
    await client.getAmountOutPath([TOKEN_A, TOKEN_B], 1_000n);
    expectTypes(calls[0].args, ["scvVec", "scvI128"]);
    expect(scValToNative(calls[0].args[0])).toEqual([TOKEN_A, TOKEN_B]);
    expect(scValToNative(calls[0].args[1])).toBe(1_000n);
  });
});
