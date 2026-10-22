/**
 * Event field schema tests (issue #1124).
 *
 * Every payload here is real XDR built with the Stellar SDK from a tuple
 * matching a Rust emit site, e.g. contracts/amm/src/lib.rs `swap`.
 * Run with: node --test dist/event-schemas.test.js
 */

import assert from "node:assert/strict";
import test from "node:test";
import { nativeToScVal, xdr } from "@stellar/stellar-sdk";
import {
  KNOWN_CONTRACT_KINDS,
  findVariant,
  parseContractKinds,
} from "./event-schemas.js";
import schemaFile from "./event-schemas.json" with { type: "json" };
import { decodeEvent, RpcPoller, type Logger } from "./rpc-poller.js";
import type { RpcEvent } from "./types.js";

const ADDR = "GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF";

function symbolTopic(name: string): string {
  return nativeToScVal(name, { type: "symbol" }).toXDR("base64");
}

/** `(EVENT_SCHEMA_VERSION, (a, b, ...))`, the shape `emit_versioned_event!` publishes. */
function versioned(version: number, items: unknown[]): string {
  return xdr.ScVal.scvVec([
    nativeToScVal(version, { type: "u32" }),
    xdr.ScVal.scvVec(items.map(toScVal)),
  ]).toXDR("base64");
}

/** Plain numbers become i64 (decoded as strings); pass an ScVal for a narrower type. */
function toScVal(item: unknown): xdr.ScVal {
  return item instanceof xdr.ScVal ? (item as xdr.ScVal) : nativeToScVal(item);
}

/** A payload published without the version envelope. */
function bare(items: unknown[]): string {
  return xdr.ScVal.scvVec(items.map(toScVal)).toXDR("base64");
}

function event(name: string, value: string): RpcEvent {
  return {
    id: "evt-1",
    type: "contract",
    ledger: 100,
    contractId: "CPOOL",
    topic: [symbolTopic(name), nativeToScVal(ADDR, { type: "address" }).toXDR("base64")],
    value,
  };
}

// ── The cases the issue reports ──────────────────────────────────────────

test("an AMM swap decodes to { token_in, amount_in, token_out, amount_out }", () => {
  // contracts/amm swap: (token_in, amount_in, token_out, amount_out)
  const decoded = decodeEvent(event("swap", versioned(1, [ADDR, 1000, ADDR, 990])), "amm");
  assert.ok(decoded);
  assert.deepEqual(decoded.payload, {
    token_in: ADDR,
    amount_in: "1000",
    token_out: ADDR,
    amount_out: "990",
  });
  assert.equal(decoded.schemaVersion, 1);
  assert.equal(decoded.contractKind, "amm");
});

test("an AMM swap_fot, which also emits `swap`, decodes with its five fields", () => {
  // contracts/amm swap_fot: (token_in, actual_received, token_out, amount_out, referrer)
  const decoded = decodeEvent(event("swap", versioned(1, [ADDR, 1000, ADDR, 990, null])), "amm");
  assert.ok(decoded);
  assert.deepEqual(Object.keys(decoded.payload), [
    "token_in",
    "actual_received",
    "token_out",
    "amount_out",
    "referrer",
  ]);
  assert.equal(decoded.payload["referrer"], null);
});

test("a CL swap decodes to { zero_for_one, amount_in, amount_out, sqrt_price_x96, tick }", () => {
  // contracts/concentrated_liquidity swap: (zero_for_one, amount_in, amount_out, sqrt_price_x96, tick)
  const decoded = decodeEvent(
    event("swap", versioned(1, [true, 500, 480, 79228162514264337593543950336n, nativeToScVal(-12, { type: "i32" })])),
    "concentrated_liquidity",
  );
  assert.ok(decoded);
  assert.deepEqual(decoded.payload, {
    zero_for_one: true,
    amount_in: "500",
    amount_out: "480",
    sqrt_price_x96: "79228162514264337593543950336",
    tick: -12,
  });
});

test("the same swap bytes get different names under different kinds, never a shared table", () => {
  const value = versioned(1, [true, 500, 480, 7, 3]);
  const cl = decodeEvent(event("swap", value), "concentrated_liquidity");
  const amm = decodeEvent(event("swap", value), "amm");
  assert.ok(cl && amm);
  assert.ok("zero_for_one" in cl.payload);
  assert.ok("token_in" in amm.payload);
  assert.notDeepEqual(Object.keys(cl.payload), Object.keys(amm.payload));
});

test("an AMM flash_loan decodes to named fields", () => {
  // contracts/amm flash_loan: (amount_a, amount_b, fee_a, fee_b)
  const decoded = decodeEvent(event("flash_loan", versioned(1, [100, 0, 1, 0])), "amm");
  assert.ok(decoded);
  assert.deepEqual(decoded.payload, { amount_a: "100", amount_b: "0", fee_a: "1", fee_b: "0" });
});

// ── Never a wrong name ───────────────────────────────────────────────────

test("without a contract kind the payload keeps the generic shape", () => {
  const decoded = decodeEvent(event("swap", versioned(1, [ADDR, 1000, ADDR, 990])));
  assert.ok(decoded);
  assert.deepEqual(decoded.payload, { value: [ADDR, "1000", ADDR, "990"] });
  assert.equal(decoded.contractKind, undefined);
});

test("an unknown contract kind keeps the generic shape", () => {
  const decoded = decodeEvent(event("swap", versioned(1, [ADDR, 1000, ADDR, 990])), "not_a_kind");
  assert.ok(decoded);
  assert.deepEqual(decoded.payload, { value: [ADDR, "1000", ADDR, "990"] });
});

test("an arity the kind never emits keeps the generic shape", () => {
  const decoded = decodeEvent(event("swap", versioned(1, [ADDR, 1000, ADDR])), "amm");
  assert.ok(decoded);
  assert.deepEqual(decoded.payload, { value: [ADDR, "1000", ADDR] });
});

test("a schema version the table does not describe keeps the generic shape", () => {
  const decoded = decodeEvent(event("swap", versioned(2, [ADDR, 1000, ADDR, 990])), "amm");
  assert.ok(decoded);
  assert.equal(decoded.schemaVersion, 2);
  assert.deepEqual(decoded.payload, { value: [ADDR, "1000", ADDR, "990"] });
});

test("an event name the kind never emits keeps the generic shape", () => {
  const decoded = decodeEvent(event("rng_ord", versioned(1, [1, 2, 3, true])), "amm");
  assert.ok(decoded);
  assert.deepEqual(decoded.payload, { value: ["1", "2", "3", true] });
});

test("random tuples never receive field names unless kind, event, version and arity all match", () => {
  // Small deterministic PRNG so a failure is reproducible.
  let seed = 0x1234abcd;
  const rand = (n: number) => {
    seed = (Math.imul(seed, 1664525) + 1013904223) >>> 0;
    return seed % n;
  };
  const kinds = [undefined, "bogus", ...KNOWN_CONTRACT_KINDS];
  const names = ["swap", "staked", "admin_changed", "upgraded", "nope", "pause", "price_upd"];
  const samples = [1, "text", true, null, 2n ** 70n, ADDR];

  for (let i = 0; i < 400; i++) {
    const kind = kinds[rand(kinds.length)];
    const name = names[rand(names.length)];
    const version = 1 + rand(2);
    const items = Array.from({ length: rand(8) }, () => samples[rand(samples.length)]);

    const decoded = decodeEvent(event(name, versioned(version, items)), kind);
    assert.ok(decoded);
    const exact = findVariant(kind, name, { version, arity: items.length });
    if (exact) {
      assert.deepEqual(Object.keys(decoded.payload), exact.fields, `${kind}.${name}/${items.length}`);
    } else {
      assert.deepEqual(
        Object.keys(decoded.payload),
        ["value"],
        `${kind}.${name}/${items.length} should stay generic`,
      );
    }
  }
});

// ── Other payload shapes ─────────────────────────────────────────────────

test("an unversioned event (no envelope) is named and reports schemaVersion 0", () => {
  // contracts/token burn is a SEP-41 event, published without the version envelope.
  const decoded = decodeEvent(event("burn", bare([500])), "token");
  assert.ok(decoded);
  assert.equal(decoded.schemaVersion, 0);
  assert.deepEqual(decoded.payload, { amount: "500" });
});

test("an event with an empty payload decodes to an empty object", () => {
  // contracts/reserve_manager pause: ()
  const decoded = decodeEvent(event("pause", versioned(1, [])), "reserve_manager");
  assert.ok(decoded);
  assert.deepEqual(decoded.payload, {});
});

test("a bare-value payload is named from its single field", () => {
  // contracts/twap_consumer snap_del: (1, ledger_ts) with ledger_ts not wrapped in a tuple
  const value = xdr.ScVal.scvVec([
    nativeToScVal(1, { type: "u32" }),
    nativeToScVal(1700000000, { type: "u64" }),
  ]).toXDR("base64");
  const decoded = decodeEvent(event("snap_del", value), "twap_consumer");
  assert.ok(decoded);
  assert.deepEqual(decoded.payload, { ledger_ts: "1700000000" });
});

test("an abbreviated topic is looked up by its on-chain name and reported by its friendly one", () => {
  const decoded = decodeEvent(event("rm_liq", versioned(1, [ADDR, 10, 4, 6])), "amm");
  assert.ok(decoded);
  assert.equal(decoded.eventType, "remove_liquidity");
  assert.deepEqual(Object.keys(decoded.payload), ["provider", "shares_burned", "amount_a", "amount_b"]);
});

// ── Every deployed contract is covered ───────────────────────────────────

for (const kind of KNOWN_CONTRACT_KINDS) {
  test(`${kind}: its events decode to the schema's field names`, () => {
    const events = (schemaFile as { kinds: Record<string, { events: Record<string, { version: number; fields: string[]; shape: string; versioned?: boolean }[]> }> }).kinds[kind].events;
    const names = Object.keys(events);
    assert.ok(names.length > 0, `${kind} has no events`);
    for (const name of names) {
      for (const variant of events[name]) {
        const items = variant.fields.map((_, i) => 100 + i);
        let value: string;
        if (variant.versioned === false) {
          value = bare(items);
        } else if (variant.shape === "scalar") {
          value = xdr.ScVal.scvVec([
            nativeToScVal(variant.version, { type: "u32" }),
            nativeToScVal(items[0]),
          ]).toXDR("base64");
        } else {
          value = versioned(variant.version, items);
        }
        const decoded = decodeEvent(event(name, value), kind);
        assert.ok(decoded);
        assert.deepEqual(Object.keys(decoded.payload), variant.fields, `${kind}.${name}`);
      }
    }
  });
}

// ── Configuration ────────────────────────────────────────────────────────

test("parseContractKinds reads <contractId>:<kind> pairs", () => {
  assert.deepEqual(parseContractKinds(" CAAA:amm , CBBB:concentrated_liquidity ,"), {
    CAAA: "amm",
    CBBB: "concentrated_liquidity",
  });
  assert.deepEqual(parseContractKinds(undefined), {});
  assert.deepEqual(parseContractKinds(""), {});
});

test("parseContractKinds rejects an unknown kind and lists the valid ones", () => {
  assert.throws(() => parseContractKinds("CAAA:ammm"), /unknown kind "ammm".*amm/);
});

test("parseContractKinds rejects a malformed pair", () => {
  for (const bad of ["CAAA", "CAAA:", ":amm"]) {
    assert.throws(() => parseContractKinds(bad), /must look like <contractId>:<kind>/, bad);
  }
});

test("the schema file is well formed", () => {
  const kinds = (schemaFile as { kinds: Record<string, { events: Record<string, { version: number; topics: number; fields: string[]; shape: string }[]> }> }).kinds;
  for (const [kind, { events }] of Object.entries(kinds)) {
    for (const [name, variants] of Object.entries(events)) {
      const arities = new Set<string>();
      for (const v of variants) {
        const id = `${kind}.${name}`;
        assert.ok(Number.isInteger(v.version) && v.version >= 1, `${id} version`);
        assert.ok(Number.isInteger(v.topics) && v.topics >= 1, `${id} topics`);
        assert.equal(new Set(v.fields).size, v.fields.length, `${id} has duplicate field names`);
        assert.ok(v.fields.every((f) => /^[a-z][a-z0-9_]*$/.test(f)), `${id} field names`);
        const key = `${v.version}/${v.fields.length}/${v.shape}`;
        assert.ok(!arities.has(key), `${id} lists the same shape twice`);
        arities.add(key);
      }
    }
  }
});

// ── Through the poller ───────────────────────────────────────────────────

test("the poller names fields using the kind configured for each contract", async () => {
  const silent: Logger = { info: () => {}, warn: () => {}, error: () => {} };
  const rpcEvent = (contractId: string): RpcEvent => ({ ...event("swap", versioned(1, [true, 1, 2, 3, 4])), contractId });
  const poller = new RpcPoller({
    rpcUrl: "https://rpc.example",
    contractIds: ["CCL", "CUNKNOWN"],
    contractKinds: { CCL: "concentrated_liquidity" },
    logger: silent,
    fetchFn: (async (_url: unknown, init?: { body?: string }) => {
      const body = JSON.parse(init?.body ?? "{}") as { params: { filters: { contractIds: string[] }[] } };
      const id = body.params.filters[0].contractIds[0];
      return {
        ok: true,
        status: 200,
        json: async () => ({ jsonrpc: "2.0", id: 1, result: { events: [rpcEvent(id)] } }),
      };
    }) as unknown as typeof fetch,
  });

  const [cl, unknown] = await poller.poll();
  assert.ok(cl && unknown);
  assert.equal(cl.contractKind, "concentrated_liquidity");
  assert.ok("sqrt_price_x96" in cl.payload);
  assert.equal(unknown.contractKind, undefined);
  assert.deepEqual(Object.keys(unknown.payload), ["value"]);
});
