/**
 * ABI conformance tests for `GovernanceClient` — Issue #1046.
 *
 * The previous version of this client called 13 entrypoints the governance
 * contract does not export, encoded `Vote` and `ProposalKind` in the wrong XDR
 * shape, and passed `unlock_vote`'s arguments in the wrong order. Every one of
 * those failures happened at the network boundary, so the test suite stayed
 * green. These tests move each of them in front of the compiler and the runner.
 *
 * The suite has five layers:
 *
 * 1. **Static sweep.** Reads `contracts/governance/src/lib.rs`, extracts the
 *    `pub fn` names off the real `#[contractimpl]` block, and fails if
 *    `governance.ts` names an entrypoint that is not one of them. It also
 *    checks the SDK's enum and struct field lists against the contract's, so
 *    the TypeScript types cannot silently fall behind the Rust.
 * 2. **Enum encoders.** One assertion per `ProposalKind` variant, per `Vote`
 *    variant and per `ProposalStatus` variant, on the exact XDR produced.
 * 3. **Write builders.** Arity, order and XDR type for every `*Params` method.
 * 4. **Read wrappers.** The method name and argument shape each wrapper
 *    actually puts on the wire, asserted through a mocked RPC server.
 * 5. **Decoding fixtures.** `Proposal` and `GovernanceParams` ScVal shapes built
 *    by hand from the contract's field lists, decoded and compared field by
 *    field.
 */

import { describe, it, expect } from "vitest";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { Address, Networks, nativeToScVal, scValToNative, xdr } from "@stellar/stellar-sdk";

import {
  GovernanceClient,
  GovernanceContractError,
  encodeProposalKind,
  encodeProposalStatus,
  encodeVote,
  decodeProposalKind,
  PROPOSAL_KIND_VARIANTS,
  type ProposalKind,
} from "./governance.js";


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

// ── Contract source: the conformance oracle ───────────────────────────────────

const CONTRACT_RS_URL = new URL("../../../contracts/governance/src/lib.rs", import.meta.url);
const CONTRACT_TS_URL = new URL("./governance.ts", import.meta.url);
/** Normalized to LF so the line-anchored regexes below match on any checkout. */
const readNormalized = (url: URL) => readFileSync(fileURLToPath(url), "utf8").replace(/\r\n/g, "\n");
const CONTRACT_RS = readNormalized(CONTRACT_RS_URL);
const CLIENT_TS = readNormalized(CONTRACT_TS_URL);

/**
 * Slice a top-level block out of the contract source: from the line containing
 * `header` up to the first line that is exactly `}`.
 */
function contractBlock(header: string): string {
  const start = CONTRACT_RS.indexOf(header);
  if (start === -1) throw new Error(`${header} not found in contracts/governance/src/lib.rs`);
  const end = CONTRACT_RS.indexOf("\n}", start);
  if (end === -1) throw new Error(`unterminated block for ${header}`);
  return CONTRACT_RS.slice(start, end);
}

/**
 * `pub fn` names on the real `#[contractimpl] impl Governance` block.
 *
 * Scoped to that block on purpose: the test module further down the file
 * declares a mock contract whose `pub fn`s (`set_pool_count`, `call_count`, …)
 * are not governance entrypoints, and matching those would let a bad method
 * name pass.
 */
const CONTRACT_ENTRYPOINTS: ReadonlySet<string> = new Set(
  [...contractBlock("#[contractimpl]\nimpl Governance").matchAll(/^\s*pub fn ([A-Za-z0-9_]+)/gm)].map(
    (m) => m[1]
  )
);

/** Variant names of a unit/payload `#[contracttype]` enum declared in the contract. */
function contractEnumVariants(name: string): string[] {
  return [...contractBlock(`pub enum ${name} {`).matchAll(/^\s{4}([A-Za-z0-9_]+)\s*[,({]/gm)].map(
    (m) => m[1]
  );
}

/** Field names of a `#[contracttype]` struct declared in the contract. */
function contractStructFields(name: string): string[] {
  return [...contractBlock(`pub struct ${name} {`).matchAll(/^\s{4}pub ([A-Za-z0-9_]+):/gm)].map(
    (m) => m[1]
  );
}

/** Every contract method name the client hands to `simulate`. */
const CLIENT_METHOD_NAMES: string[] = [
  ...new Set([...CLIENT_TS.matchAll(/simulate\(\s*"([A-Za-z0-9_]+)"/g)].map((m) => m[1])),
];

/** Every contract method name a `*Params` builder documents as its target. */
const BUILDER_TARGETS: ReadonlySet<string> = new Set(
  [...CLIENT_TS.matchAll(/Parameters for `([a-z0-9_]+)\(/g)].map((m) => m[1])
);

// ── Test harness ──────────────────────────────────────────────────────────────

const CONTRACT_ID = "CA3D5KRYM6CB7OWQ6TWYRR3Z4T7GNZLKERYNZGGA5SOAOPIFY6YQGAXE";
const ALICE = "GA5WUJ54Z23KILLCUOUNAKTPBVZWKMQVO4O6EQ5GHLAERIMLLHNCSKYH";
const BOB = "GAEQSCIJBEEQSCIJBEEQSCIJBEEQSCIJBEEQSCIJBEEQSCIJBEEQSH7S";
const CAROL = "GADQOBYHA4DQOBYHA4DQOBYHA4DQOBYHA4DQOBYHA4DQOBYHA4DQOZPI";

const config = {
  rpcUrl: "https://rpc.example.invalid",
  networkPassphrase: Networks.TESTNET,
  contractId: CONTRACT_ID,
};

/** One recorded `invokeContract` operation. */
interface Call {
  method: string;
  args: xdr.ScVal[];
}

/**
 * A `GovernanceClient` whose RPC server is replaced by a responder, plus the
 * list of calls it made. This asserts what actually goes on the wire, not what
 * the method is called.
 */
function mockClient(responder: (call: Call) => xdr.ScVal | Error) {
  const client = new GovernanceClient(config);
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

  return { client, calls };
}

/** The XDR type discriminant name of an ScVal, e.g. `"scvI128"`. */
function typeOf(v: xdr.ScVal): string {
  return scvName(v);
}

/** Assert the ordered list of XDR type discriminants of an argument array. */
function expectTypes(args: xdr.ScVal[], types: string[]) {
  expect(args).toHaveLength(types.length);
  expect(args.map(typeOf)).toEqual(types);
}

const u32v = (n: number) => nativeToScVal(n, { type: "u32" });
const u64v = (n: bigint) => nativeToScVal(n, { type: "u64" });
const i128v = (n: bigint) => nativeToScVal(n, { type: "i128" });
const boolv = (b: boolean) => xdr.ScVal.scvBool(b);
const symv = (s: string) => nativeToScVal(s, { type: "symbol" });
const addrv = (a: string) => nativeToScVal(Address.fromString(a));
const somev = (v: xdr.ScVal) => xdr.ScVal.scvVec([v]);
const mapv = (entries: Array<[string, xdr.ScVal]>) =>
  xdr.ScVal.scvMap(entries.map(([k, v]) => new xdr.ScMapEntry({ key: symv(k), val: v })));

/** The elements of an `scvVec`, asserted present. */
function vecOf(v: xdr.ScVal): xdr.ScVal[] {
  const items = (v as unknown as { vec?: xdr.ScVal[] }).vec;
  if (!items) throw new Error(`expected an scvVec, got ${scvName(v)}`);
  return items;
}

/** The entries of an `scvMap`, asserted present. */
function mapOf(v: xdr.ScVal): xdr.ScMapEntry[] {
  const entries = (v as unknown as { map?: xdr.ScMapEntry[] }).map;
  if (!entries) throw new Error(`expected an scvMap, got ${scvName(v)}`);
  return entries;
}

// ── 1. Static conformance sweep ────────────────────────────────────────────────

describe("GovernanceClient entrypoint conformance", () => {
  it("finds the real contract entrypoints", () => {
    // Guards the extractor itself: a regex that silently matched nothing would
    // make the sweep below vacuously pass.
    expect(CONTRACT_ENTRYPOINTS.size).toBeGreaterThan(20);
    expect(CONTRACT_ENTRYPOINTS.has("get_proposal")).toBe(true);
    expect(CONTRACT_ENTRYPOINTS.has("unlock_vote")).toBe(true);
  });

  it("sends only method names that are a pub fn in the governance contract", () => {
    expect(CLIENT_METHOD_NAMES.length).toBeGreaterThan(0);
    const unknown = CLIENT_METHOD_NAMES.filter((m) => !CONTRACT_ENTRYPOINTS.has(m));
    expect(unknown).toEqual([]);
  });

  it("names no entrypoint from the 13 the previous client invented", () => {
    // The exact list this issue was filed for.
    const removed = [
      "try_get_proposal",
      "list_proposals",
      "list_proposals_desc",
      "list_proposals_by_status",
      "get_active_proposal_ids",
      "count_proposals_by_status",
      "list_proposals_by_status_from",
      "get_proposals_by_proposer",
      "get_voter_count",
      "list_voters",
      "get_delegators",
      "has_voted",
      "get_vote_record",
    ];
    for (const name of removed) {
      expect(CONTRACT_ENTRYPOINTS.has(name)).toBe(false);
      expect(CLIENT_METHOD_NAMES).not.toContain(name);
    }
  });

  it("documents a real entrypoint for every *Params builder", () => {
    expect(BUILDER_TARGETS.size).toBeGreaterThanOrEqual(8);
    const unknown = [...BUILDER_TARGETS].filter((m) => !CONTRACT_ENTRYPOINTS.has(m));
    expect(unknown).toEqual([]);
  });

  it("keeps PROPOSAL_KIND_VARIANTS in sync with the contract enum", () => {
    expect([...PROPOSAL_KIND_VARIANTS]).toEqual(contractEnumVariants("ProposalKind"));
  });

  it("keeps the Vote choices in sync with the contract enum", () => {
    expect(contractEnumVariants("Vote")).toEqual(["For", "Against", "Abstain"]);
  });

  it("covers every ProposalStatus variant the contract declares", () => {
    // The old union was missing InDiscussion and Vetoed, so a vetoed proposal
    // could not be represented at all.
    const declared = contractEnumVariants("ProposalStatus");
    expect(declared).toHaveLength(9);
    expect(declared).toContain("InDiscussion");
    expect(declared).toContain("Vetoed");
  });

  it("reads every field of the contract's Proposal struct", () => {
    for (const field of contractStructFields("Proposal")) {
      expect(CLIENT_TS).toContain(`native.${field}`);
    }
  });

  it("reads every field of the contract's GovernanceParams struct", () => {
    for (const field of contractStructFields("GovernanceParams")) {
      expect(CLIENT_TS).toContain(`native.${field}`);
    }
  });

  it("reads every field of the contract's VetoAudit struct", () => {
    for (const field of contractStructFields("VetoAudit")) {
      expect(CLIENT_TS).toContain(`native.${field}`);
    }
  });
});

// ── 2. Enum encoders ──────────────────────────────────────────────────────────

describe("encodeVote", () => {
  it("encodes For as scvVec([scvSymbol(\"For\")]), not a bare symbol", () => {
    const v = encodeVote("For");
    expect(typeOf(v)).toBe("scvVec");
    expect(typeOf(vecOf(v)[0])).toBe("scvSymbol");
    expect(scValToNative(v)).toEqual(["For"]);
  });

  it("encodes Against the same way", () => {
    expect(typeOf(encodeVote("Against"))).toBe("scvVec");
    expect(scValToNative(encodeVote("Against"))).toEqual(["Against"]);
  });

  it("encodes Abstain the same way", () => {
    expect(typeOf(encodeVote("Abstain"))).toBe("scvVec");
    expect(scValToNative(encodeVote("Abstain"))).toEqual(["Abstain"]);
  });

  it("never produces a scvString, which the contract rejects", () => {
    // The regression: nativeToScVal("For") is a scvString.
    expect(typeOf(nativeToScVal("For"))).toBe("scvString");
    for (const choice of ["For", "Against", "Abstain"] as const) {
      expect(typeOf(encodeVote(choice))).not.toBe("scvString");
    }
  });
});

describe("encodeProposalStatus", () => {
  it("encodes every declared variant as scvVec([scvSymbol(variant)])", () => {
    const declared = contractEnumVariants("ProposalStatus");
    for (const status of declared) {
      const v = encodeProposalStatus(status as Parameters<typeof encodeProposalStatus>[0]);
      expect(typeOf(v)).toBe("scvVec");
      expect(typeOf(vecOf(v)[0])).toBe("scvSymbol");
      expect(scValToNative(v)).toEqual([status]);
    }
  });
});

describe("encodeProposalKind", () => {
  it("produces scvVec([scvSymbol(variant), ...payload]) for every variant", () => {
    for (const variant of PROPOSAL_KIND_VARIANTS) {
      const kind = KINDS[variant];
      const v = encodeProposalKind(kind);
      expect(typeOf(v), variant).toBe("scvVec");
      expect(typeOf(vecOf(v)[0]), variant).toBe("scvSymbol");
      expect(scValToNative(vecOf(v)[0]), variant).toBe(variant);
    }
  });
});

// One encoder assertion per ProposalKind variant.
describe("ProposalKind encoding — one assertion per contract variant", () => {
  it("UpdateFee(i128) is scvVec([sym, scvI128])", () => {
    const v = encodeProposalKind({ kind: "UpdateFee", newFeeBps: 30n });
    expectTypes(vecOf(v).slice(1), ["scvI128"]);
    expect(scValToNative(v)).toEqual(["UpdateFee", 30n]);
  });

  it("UpdateFeeTier(i128) is scvVec([sym, scvI128])", () => {
    const v = encodeProposalKind({ kind: "UpdateFeeTier", feeTier: 2 });
    expect(typeOf(vecOf(v)[1])).toBe("scvI128");
    expect(scValToNative(v)).toEqual(["UpdateFeeTier", 2n]);
  });

  it("UpdateProtocolFee(struct) nests a symbol-keyed scvMap", () => {
    const v = encodeProposalKind({
      kind: "UpdateProtocolFee",
      params: { newBps: 5n, newRecipient: BOB },
    });
    expect(typeOf(vecOf(v)[1])).toBe("scvMap");
    expect(scValToNative(v)).toEqual([
      "UpdateProtocolFee",
      { new_bps: 5n, new_recipient: BOB },
    ]);
  });

  it("UpdateFlashLoanFee(i128) is scvVec([sym, scvI128])", () => {
    expect(scValToNative(encodeProposalKind({ kind: "UpdateFlashLoanFee", newFeeBps: 7n }))).toEqual([
      "UpdateFlashLoanFee",
      7n,
    ]);
  });

  it("TransferAdmin(Address) is scvVec([sym, scvAddress])", () => {
    const v = encodeProposalKind({ kind: "TransferAdmin", newAdmin: BOB });
    expect(typeOf(vecOf(v)[1])).toBe("scvAddress");
    expect(scValToNative(v)).toEqual(["TransferAdmin", BOB]);
  });

  it("PausePool is a unit variant: scvVec([sym])", () => {
    const v = encodeProposalKind({ kind: "PausePool" });
    expect(vecOf(v)).toHaveLength(1);
    expect(scValToNative(v)).toEqual(["PausePool"]);
  });

  it("UnpausePool is a unit variant: scvVec([sym])", () => {
    const v = encodeProposalKind({ kind: "UnpausePool" });
    expect(vecOf(v)).toHaveLength(1);
    expect(scValToNative(v)).toEqual(["UnpausePool"]);
  });

  it("EmergencyWithdraw(Address) is scvVec([sym, scvAddress])", () => {
    const v = encodeProposalKind({ kind: "EmergencyWithdraw", to: BOB });
    expect(typeOf(vecOf(v)[1])).toBe("scvAddress");
    expect(scValToNative(v)).toEqual(["EmergencyWithdraw", BOB]);
  });

  it("UpdateFactoryTreasury(struct) keeps every field", () => {
    const v = encodeProposalKind({
      kind: "UpdateFactoryTreasury",
      params: { factory: BOB, treasury: CAROL, globalProtocolFeeBps: 9n },
    });
    expect(scValToNative(v)).toEqual([
      "UpdateFactoryTreasury",
      { factory: BOB, treasury: CAROL, global_protocol_fee_bps: 9n },
    ]);
  });

  it("UpdateFactoryGlobalFee(struct) encodes offset/limit as u32", () => {
    const v = encodeProposalKind({
      kind: "UpdateFactoryGlobalFee",
      params: { factory: BOB, offset: 1, limit: 5 },
    });
    const map = mapOf(vecOf(v)[1]);
    const types = map.map((e) => typeOf(e.val));
    expect(types).toEqual(["scvAddress", "scvU32", "scvU32"]);
    expect(scValToNative(v)).toEqual([
      "UpdateFactoryGlobalFee",
      { factory: BOB, offset: 1, limit: 5 },
    ]);
  });

  it("CreatePolVesting(struct) encodes all eight fields", async () => {
    const { client } = mockClient(() =>
      PROPOSAL_SCV({
        kind: encodeProposalKind({
          kind: "CreatePolVesting",
          params: {
            polVesting: ALICE,
            beneficiary: BOB,
            lpToken: CAROL,
            pool: ALICE,
            total: 1_000n,
            startLedger: 1,
            cliffLedger: 2,
            endLedger: 3,
          },
        }),
      })
    );
    const p = await client.getProposal(7);
    const kind = p.kind as Extract<ProposalKind, { kind: "CreatePolVesting" }>;
    // The contract's struct has exactly these eight fields; a dropped or
    // renamed one shows up as an undefined field here.
    expect(contractStructFields("CreatePolVestingParams")).toHaveLength(8);
    expect(kind.params).toEqual({
      polVesting: ALICE,
      beneficiary: BOB,
      lpToken: CAROL,
      pool: ALICE,
      total: 1_000n,
      startLedger: 1,
      cliffLedger: 2,
      endLedger: 3,
    });
  });

  it("PauseClPool(Address) is scvVec([sym, scvAddress])", () => {
    const v = encodeProposalKind({ kind: "PauseClPool", clPool: BOB });
    expect(typeOf(vecOf(v)[1])).toBe("scvAddress");
    expect(scValToNative(v)).toEqual(["PauseClPool", BOB]);
  });

  it("UnpauseClPool(Address) is scvVec([sym, scvAddress])", () => {
    const v = encodeProposalKind({ kind: "UnpauseClPool", clPool: BOB });
    expect(typeOf(vecOf(v)[1])).toBe("scvAddress");
    expect(scValToNative(v)).toEqual(["UnpauseClPool", BOB]);
  });

  it("UpdateClOracle(struct) encodes Option::Some as a one-element vec", () => {
    const v = encodeProposalKind({
      kind: "UpdateClOracle",
      params: { clPool: BOB, oracle: CAROL },
    });
    const map = mapOf(vecOf(v)[1]);
    const oracleVal = map[1].val;
    expect(typeOf(oracleVal)).toBe("scvVec");
    expect(vecOf(oracleVal)).toHaveLength(1);
    expect(scValToNative(v)).toEqual([
      "UpdateClOracle",
      { cl_pool: BOB, oracle: [CAROL] },
    ]);
  });

  it("UpdateClOracle(struct) encodes Option::None as scvVoid", () => {
    const v = encodeProposalKind({ kind: "UpdateClOracle", params: { clPool: BOB, oracle: null } });
    const map = mapOf(vecOf(v)[1]);
    expect(typeOf(map[1].val)).toBe("scvVoid");
    expect(scValToNative(v)).toEqual(["UpdateClOracle", { cl_pool: BOB, oracle: null }]);
  });

  it("UpdateClMaxOracleDeviation(struct) keeps its payload", () => {
    const v = encodeProposalKind({
      kind: "UpdateClMaxOracleDeviation",
      params: { clPool: BOB, maxDeviationBps: 3n },
    });
    expect(scValToNative(v)).toEqual([
      "UpdateClMaxOracleDeviation",
      { cl_pool: BOB, max_deviation_bps: 3n },
    ]);
  });

  it("UpdateClProtocolFee(struct) keeps its payload", () => {
    const v = encodeProposalKind({
      kind: "UpdateClProtocolFee",
      params: { clPool: BOB, recipient: CAROL, bps: 4n },
    });
    expect(scValToNative(v)).toEqual([
      "UpdateClProtocolFee",
      { cl_pool: BOB, recipient: CAROL, bps: 4n },
    ]);
  });

  it("TransferClPoolAdmin(struct) keeps its payload", () => {
    const v = encodeProposalKind({
      kind: "TransferClPoolAdmin",
      params: { clPool: BOB, newAdmin: CAROL },
    });
    expect(scValToNative(v)).toEqual([
      "TransferClPoolAdmin",
      { cl_pool: BOB, new_admin: CAROL },
    ]);
  });

  it("SetClPositionNft(struct) encodes Option::None as scvVoid", () => {
    const v = encodeProposalKind({ kind: "SetClPositionNft", params: { clPool: BOB, nft: null } });
    const map = mapOf(vecOf(v)[1]);
    expect(typeOf(map[1].val)).toBe("scvVoid");
    expect(scValToNative(v)).toEqual(["SetClPositionNft", { cl_pool: BOB, nft: null }]);
  });

  it("round-trips every variant back through decodeProposalKind", () => {
    for (const variant of PROPOSAL_KIND_VARIANTS) {
      const kind = KINDS[variant];
      const decoded = decodeProposalKind(scValToNative(encodeProposalKind(kind)));
      expect(decoded, variant).toEqual(kind);
    }
  });

  it("never encodes a ProposalKind as a scvMap keyed by the variant", () => {
    // The regression: { UpdateFee: n } is a scvMap, which the contract rejects.
    expect(typeOf(nativeToScVal({ UpdateFee: 30n }))).toBe("scvMap");
    for (const variant of PROPOSAL_KIND_VARIANTS) {
      expect(typeOf(encodeProposalKind(KINDS[variant])), variant).not.toBe("scvMap");
    }
  });
});

// ── 3. Write builders ─────────────────────────────────────────────────────────

describe("proposeParams", () => {
  const gov = new GovernanceClient(config);

  it("is (proposer: Address, kind: ProposalKind) — two arguments", () => {
    expectTypes(gov.proposeParams(ALICE, { kind: "UpdateFee", newFeeBps: 30n }), [
      "scvAddress",
      "scvVec",
    ]);
  });

  it("accepts all 18 ProposalKind variants", () => {
    for (const variant of PROPOSAL_KIND_VARIANTS) {
      expectTypes(gov.proposeParams(ALICE, KINDS[variant]), ["scvAddress", "scvVec"]);
    }
  });

  it("proposeUpdateFeeParams is a thin wrapper over it", () => {
    expect(gov.proposeUpdateFeeParams(ALICE, 30n)).toEqual(
      gov.proposeParams(ALICE, { kind: "UpdateFee", newFeeBps: 30n })
    );
  });
});

describe("voteParams", () => {
  const gov = new GovernanceClient(config);

  it("is (voter, proposal_id: u32, choice: Vote) — address, u32, enum", () => {
    expectTypes(gov.voteParams(ALICE, 7, "For"), ["scvAddress", "scvU32", "scvVec"]);
  });

  it("encodes the choice as a contract enum, not a string", () => {
    const args = gov.voteParams(ALICE, 7, "For");
    expect(typeOf(args[2])).toBe("scvVec");
    expect(scValToNative(args[2])).toEqual(["For"]);
    expect(scValToNative(args[0])).toBe(ALICE);
    expect(scValToNative(args[1])).toBe(7);
  });
});

describe("executeParams", () => {
  it("is (proposal_id: u32) — one argument", () => {
    expectTypes(new GovernanceClient(config).executeParams(7), ["scvU32"]);
  });
});

describe("cancelParams", () => {
  it("is (proposal_id: u32, proposer: Address) for cancel_proposal", () => {
    const args = new GovernanceClient(config).cancelParams(7, ALICE);
    expectTypes(args, ["scvU32", "scvAddress"]);
    expect(scValToNative(args[0])).toBe(7);
    expect(scValToNative(args[1])).toBe(ALICE);
  });
});

describe("unlockVoteParams", () => {
  // REGRESSION. contracts/governance/src/lib.rs:1090
  //   pub fn unlock_vote(env: Env, voter: Address, proposal_id: u32)
  // The client previously took (proposal_id, voter) and emitted the arguments
  // in that order, so the contract received a u32 where it wanted an Address.
  const gov = new GovernanceClient(config);
  const args = gov.unlockVoteParams(ALICE, 7);

  it("produces [scvAddress, scvU32] — address first, id second", () => {
    expectTypes(args, ["scvAddress", "scvU32"]);
  });

  it("puts the voter in position 0 and the proposal id in position 1", () => {
    expect(scValToNative(args[0])).toBe(ALICE);
    expect(scValToNative(args[1])).toBe(7);
  });

  it("does not produce the reversed order", () => {
    expect(args.map(typeOf)).not.toEqual(["scvU32", "scvAddress"]);
  });
});

describe("vetoParams", () => {
  it("is (proposal_id: u32) — one argument", () => {
    expectTypes(new GovernanceClient(config).vetoParams(7), ["scvU32"]);
  });
});

describe("delegateParams", () => {
  it("is (from: Address, to: Address)", () => {
    const args = new GovernanceClient(config).delegateParams(ALICE, BOB);
    expectTypes(args, ["scvAddress", "scvAddress"]);
    expect(scValToNative(args[0])).toBe(ALICE);
    expect(scValToNative(args[1])).toBe(BOB);
  });
});

describe("undelegateParams", () => {
  it("is (from: Address) — one argument", () => {
    const args = new GovernanceClient(config).undelegateParams(ALICE);
    expectTypes(args, ["scvAddress"]);
    expect(scValToNative(args[0])).toBe(ALICE);
  });
});

// ── 4. Read wrappers: method name and argument shape on the wire ──────────────

describe("read wrappers target real entrypoints", () => {
  it("getParams sends get_params with no arguments", async () => {
    const { client, calls } = mockClient(() => mapv([]));
    await client.getParams();
    expect(calls).toHaveLength(1);
    expect(calls[0].method).toBe("get_params");
    expect(calls[0].args).toHaveLength(0);
  });

  it("getProposalCount sends get_proposal_count", async () => {
    const { client, calls } = mockClient(() => u32v(3));
    expect(await client.getProposalCount()).toBe(3);
    expect(calls[0].method).toBe("get_proposal_count");
    expect(calls[0].args).toHaveLength(0);
  });

  it("proposalCount aliases get_proposal_count", async () => {
    const { client, calls } = mockClient(() => u32v(4));
    expect(await client.proposalCount()).toBe(4);
    expect(calls[0].method).toBe("get_proposal_count");
  });

  it("getProposal sends get_proposal with a single u32", async () => {
    const { client, calls } = mockClient(() => PROPOSAL_SCV());
    await client.getProposal(7);
    expect(calls[0].method).toBe("get_proposal");
    expectTypes(calls[0].args, ["scvU32"]);
    expect(scValToNative(calls[0].args[0])).toBe(7);
  });

  it("proposalStatus sends proposal_status with a single u32", async () => {
    const { client, calls } = mockClient(() => xdr.ScVal.scvVec([symv("Queued")]));
    expect(await client.proposalStatus(7)).toBe("Queued");
    expect(calls[0].method).toBe("proposal_status");
    expectTypes(calls[0].args, ["scvU32"]);
  });

  it("listProposals sends get_proposals_paginated with (u32, u32)", async () => {
    const { client, calls } = mockClient(() => xdr.ScVal.scvVec([PROPOSAL_SCV()]));
    await client.listProposals(0, 50);
    expect(calls[0].method).toBe("get_proposals_paginated");
    expectTypes(calls[0].args, ["scvU32", "scvU32"]);
    expect(scValToNative(calls[0].args[0])).toBe(0);
    expect(scValToNative(calls[0].args[1])).toBe(50);
  });

  it("getVoteInfo sends get_vote_info with (u32, Address)", async () => {
    const { client, calls } = mockClient(() => xdr.ScVal.scvVec([symv("VotedFor")]));
    expect(await client.getVoteInfo(7, ALICE)).toBe("VotedFor");
    expect(calls[0].method).toBe("get_vote_info");
    expectTypes(calls[0].args, ["scvU32", "scvAddress"]);
    expect(scValToNative(calls[0].args[0])).toBe(7);
    expect(scValToNative(calls[0].args[1])).toBe(ALICE);
  });

  it("getVoteRecord is an alias for get_vote_info", async () => {
    const { client, calls } = mockClient(() => xdr.ScVal.scvVec([symv("VotedAgainst")]));
    expect(await client.getVoteRecord(7, ALICE)).toBe("VotedAgainst");
    expect(calls[0].method).toBe("get_vote_info");
  });

  it("getDelegate sends get_delegate with one Address", async () => {
    const { client, calls } = mockClient(() => somev(addrv(BOB)));
    expect(await client.getDelegate(ALICE)).toBe(BOB);
    expect(calls[0].method).toBe("get_delegate");
    expectTypes(calls[0].args, ["scvAddress"]);
  });

  it("getEffectiveQuorum sends get_effective_quorum with a u32", async () => {
    const { client, calls } = mockClient(() => i128v(1_500n));
    expect(await client.getEffectiveQuorum(7)).toBe(1_500n);
    expect(calls[0].method).toBe("get_effective_quorum");
    expectTypes(calls[0].args, ["scvU32"]);
  });

  it("getVetoAudit sends get_veto_audit with a u32", async () => {
    const { client, calls } = mockClient(() =>
      mapv([
        ["proposal_id", u32v(7)],
        ["vetoed_by", addrv(BOB)],
        ["vetoed_at", u64v(1_000n)],
        ["discussion_end", u64v(2_000n)],
      ])
    );
    const audit = await client.getVetoAudit(7);
    expect(audit).toEqual({ proposalId: 7, vetoedBy: BOB, vetoedAt: 1_000n, discussionEnd: 2_000n });
    expect(calls[0].method).toBe("get_veto_audit");
    expectTypes(calls[0].args, ["scvU32"]);
  });

  it("getSnapshotBalance sends get_snapshot_balance with (u32, Address)", async () => {
    const { client, calls } = mockClient(() => i128v(4_200n));
    expect(await client.getSnapshotBalance(7, ALICE)).toBe(4_200n);
    expect(calls[0].method).toBe("get_snapshot_balance");
    expectTypes(calls[0].args, ["scvU32", "scvAddress"]);
  });
});

// ── 5. Decoding fixtures, built by hand from the contract's field lists ────────

/** A `Proposal` ScVal matching the contract's `struct Proposal` field for field. */
function PROPOSAL_SCV(overrides: Record<string, xdr.ScVal> = {}): xdr.ScVal {
  const fields: Array<[string, xdr.ScVal]> = [
    ["id", u32v(7)],
    ["proposer", addrv(ALICE)],
    ["kind", encodeProposalKind({ kind: "UpdateFee", newFeeBps: 30n })],
    ["snapshot_total_supply", i128v(1_000_000n)],
    ["snapshot_ledger", u32v(555)],
    ["vote_start", u64v(1_000n)],
    ["vote_end", u64v(2_000n)],
    ["execute_after", u64v(2_600n)],
    ["expires_at", u64v(3_200n)],
    ["votes_for", i128v(400_000n)],
    ["votes_against", i128v(100_000n)],
    ["votes_abstain", i128v(50_000n)],
    ["executed", boolv(false)],
    ["cancelled", boolv(false)],
    ["vetoed", boolv(false)],
    ["vetoed_by", xdr.ScVal.scvVoid()],
    ["vetoed_at", xdr.ScVal.scvVoid()],
    ["discussion_end", xdr.ScVal.scvVoid()],
  ];
  for (const [k, v] of Object.entries(overrides)) {
    const i = fields.findIndex(([name]) => name === k);
    fields[i] = [k, v];
  }
  return mapv(fields);
}

describe("Proposal decoding", () => {
  it("round-trips every field of a contract-shaped Proposal", async () => {
    const { client } = mockClient(() => PROPOSAL_SCV());
    const p = await client.getProposal(7);

    expect(p.id).toBe(7);
    expect(p.proposer).toBe(ALICE);
    expect(p.kind).toEqual({ kind: "UpdateFee", newFeeBps: 30n });
    expect(p.snapshotTotalSupply).toBe(1_000_000n);
    expect(p.snapshotLedger).toBe(555);
    expect(p.voteStart).toBe(1_000n);
    expect(p.voteEnd).toBe(2_000n);
    expect(p.executeAfter).toBe(2_600n);
    expect(p.expiresAt).toBe(3_200n);
    expect(p.votesFor).toBe(400_000n);
    expect(p.votesAgainst).toBe(100_000n);
    expect(p.votesAbstain).toBe(50_000n);
    expect(p.executed).toBe(false);
    expect(p.cancelled).toBe(false);
    expect(p.vetoed).toBe(false);
    expect(p.vetoedBy).toBeNull();
    expect(p.vetoedAt).toBeNull();
    expect(p.discussionEnd).toBeNull();
  });

  it("has no status field: the contract's Proposal struct has none", async () => {
    const { client } = mockClient(() => PROPOSAL_SCV());
    const p = await client.getProposal(7);
    expect(Object.keys(p)).not.toContain("status");
    expect(contractStructFields("Proposal")).not.toContain("status");
  });

  it("decodes a vetoed proposal's Some(...) fields", async () => {
    const { client } = mockClient(() =>
      PROPOSAL_SCV({
        vetoed: boolv(true),
        vetoed_by: somev(addrv(CAROL)),
        vetoed_at: somev(u64v(4_000n)),
        discussion_end: somev(u64v(5_000n)),
      })
    );
    const p = await client.getProposal(7);
    expect(p.vetoed).toBe(true);
    expect(p.vetoedBy).toBe(CAROL);
    expect(p.vetoedAt).toBe(4_000n);
    expect(p.discussionEnd).toBe(5_000n);
  });

  it("decodes every ProposalKind variant stored on a proposal", async () => {
    for (const variant of PROPOSAL_KIND_VARIANTS) {
      const { client } = mockClient(() =>
        PROPOSAL_SCV({ kind: encodeProposalKind(KINDS[variant]) })
      );
      const p = await client.getProposal(7);
      expect(p.kind, variant).toEqual(KINDS[variant]);
    }
  });

  it("getProposalWithStatus takes status from proposal_status, not from the struct", async () => {
    const { client, calls } = mockClient((call) =>
      call.method === "proposal_status" ? xdr.ScVal.scvVec([symv("Vetoed")]) : PROPOSAL_SCV()
    );
    const p = await client.getProposalWithStatus(7);
    expect(p.status).toBe("Vetoed");
    expect(calls.map((c) => c.method).sort()).toEqual(["get_proposal", "proposal_status"]);
  });

  it("proposalStatus reports the veto-window status, which the old union could not hold", async () => {
    const { client } = mockClient(() => xdr.ScVal.scvVec([symv("InDiscussion")]));
    expect(await client.proposalStatus(7)).toBe("InDiscussion");
  });
});

describe("GovernanceParams decoding", () => {
  function paramsScv(vetoMultisig: xdr.ScVal, decay: bigint) {
    return mapv([
      ["voting_period_secs", u64v(86_400n)],
      ["timelock_secs", u64v(3_600n)],
      ["quorum_bps", i128v(2_000n)],
      ["min_proposer_stake_bps", i128v(100n)],
      ["veto_multisig", vetoMultisig],
      ["quorum_decay_rate_bps_per_day", i128v(decay)],
    ]);
  }

  it("decodes all six fields, including the two the old type was missing", async () => {
    const { client } = mockClient(() => paramsScv(somev(addrv(CAROL)), 25n));
    expect(await client.getParams()).toEqual({
      votingPeriodSecs: 86_400n,
      timelockSecs: 3_600n,
      quorumBps: 2_000n,
      minProposerStakeBps: 100n,
      vetoMultisig: CAROL,
      quorumDecayRateBpsPerDay: 25n,
    });
  });

  it("decodes veto_multisig as null when the multisig is unset", async () => {
    const { client } = mockClient(() => paramsScv(xdr.ScVal.scvVoid(), 0n));
    const p = await client.getParams();
    expect(p.vetoMultisig).toBeNull();
    expect(p.quorumDecayRateBpsPerDay).toBe(0n);
  });
});

describe("getVoteInfo decoding", () => {
  it("decodes VotedAbstain, which the old VoteRecord union was missing", async () => {
    const { client } = mockClient(() => xdr.ScVal.scvVec([symv("VotedAbstain")]));
    expect(await client.getVoteInfo(7, ALICE)).toBe("VotedAbstain");
  });

  it("decodes DidNotVote for a voter who has not voted", async () => {
    const { client } = mockClient(() => xdr.ScVal.scvVec([symv("DidNotVote")]));
    expect(await client.getVoteInfo(7, ALICE)).toBe("DidNotVote");
  });
});

// ── 6. Client-side compositions ───────────────────────────────────────────────

describe("client-side compositions", () => {
  /**
   * A responder over a fixed set of `count` proposals, honouring the
   * `(offset, limit)` window the way `get_proposals_paginated` does.
   */
  function pagedProposals(
    count: number,
    one: (id: number) => xdr.ScVal,
    extra?: (call: Call) => xdr.ScVal
  ) {
    return (call: Call): xdr.ScVal => {
      if (call.method === "get_proposal_count") return u32v(count);
      if (call.method === "get_proposals_paginated") {
        const offset = Number(scValToNative(call.args[0]));
        const limit = Number(scValToNative(call.args[1]));
        return xdr.ScVal.scvVec(
          Array.from({ length: Math.max(0, Math.min(limit, count - offset)) }, (_, i) =>
            one(offset + i)
          )
        );
      }
      return extra!(call);
    };
  }

  it("hasVoted composes get_vote_info", async () => {
    const { client, calls } = mockClient(() => xdr.ScVal.scvVec([symv("VotedFor")]));
    expect(await client.hasVoted(7, ALICE)).toBe(true);
    expect(calls[0].method).toBe("get_vote_info");
  });

  it("hasVoted is false for DidNotVote", async () => {
    const { client } = mockClient(() => xdr.ScVal.scvVec([symv("DidNotVote")]));
    expect(await client.hasVoted(7, ALICE)).toBe(false);
  });

  it("listProposalsByStatus pages get_proposals_paginated and filters by proposal_status", async () => {
    const statuses: Record<number, string> = { 0: "Active", 1: "Executed", 2: "Active" };
    const { client, calls } = mockClient(
      pagedProposals(
        3,
        (id) => PROPOSAL_SCV({ id: u32v(id) }),
        (call) => xdr.ScVal.scvVec([symv(statuses[Number(scValToNative(call.args[0]))])])
      )
    );

    expect(await client.listProposalsByStatus("Active", 0, 10)).toEqual([0, 2]);

    // Composed only from real entrypoints.
    const methods = new Set(calls.map((c) => c.method));
    expect([...methods].sort()).toEqual([
      "get_proposal_count",
      "get_proposals_paginated",
      "proposal_status",
    ]);
    for (const m of methods) expect(CONTRACT_ENTRYPOINTS.has(m)).toBe(true);
  });

  it("listProposalsByStatus paginates its result", async () => {
    const { client } = mockClient(
      pagedProposals(3, (id) => PROPOSAL_SCV({ id: u32v(id) }), () =>
        xdr.ScVal.scvVec([symv("Active")])
      )
    );
    expect(await client.listProposalsByStatus("Active", 1, 1)).toEqual([1]);
  });

  it("listProposalsDesc reverses the ascending page order", async () => {
    const { client } = mockClient(pagedProposals(3, (id) => PROPOSAL_SCV({ id: u32v(id) })));
    const ids = (await client.listProposalsDesc(0, 10)).map((p) => p.id);
    expect(ids).toEqual([2, 1, 0]);
  });

  it("countProposalsByStatus counts matching proposals", async () => {
    const statuses: Record<number, string> = { 0: "Active", 1: "Active", 2: "Defeated" };
    const { client } = mockClient(
      pagedProposals(
        3,
        (id) => PROPOSAL_SCV({ id: u32v(id) }),
        (call) => xdr.ScVal.scvVec([symv(statuses[Number(scValToNative(call.args[0]))])])
      )
    );
    expect(await client.countProposalsByStatus("Active")).toBe(2);
  });

  it("getProposalsByProposer filters on the stored proposer field", async () => {
    const { client, calls } = mockClient(
      pagedProposals(2, (id) => PROPOSAL_SCV({ id: u32v(id), proposer: addrv(id === 0 ? ALICE : BOB) }))
    );
    expect(await client.getProposalsByProposer(BOB, 0, 10)).toEqual([1]);
    // Filtering by proposer needs no status lookup at all.
    expect(calls.every((c) => c.method !== "proposal_status")).toBe(true);
  });

  it("tryGetProposal resolves to null on ProposalNotFound (#9)", async () => {
    const { client, calls } = mockClient(() => new Error("HostError: Error(Contract, #9)"));
    expect(await client.tryGetProposal(999)).toBeNull();
    expect(calls[0].method).toBe("get_proposal");
  });

  it("tryGetProposal rethrows every other failure", async () => {
    const { client } = mockClient(() => new Error("HostError: Error(Contract, #35)"));
    await expect(client.tryGetProposal(7)).rejects.toThrow(/NotInitialized/);
  });
});

describe("GovernanceContractError", () => {
  it("decodes a contract discriminant into its variant name", async () => {
    const { client } = mockClient(() => new Error("HostError: Error(Contract, #19)"));
    await expect(client.getProposal(7)).rejects.toBeInstanceOf(GovernanceContractError);
    await expect(client.getProposal(7)).rejects.toThrow(/QuorumNotMet/);
  });

  it("falls back to a plain Error when there is no discriminant", async () => {
    const { client } = mockClient(() => new Error("connection refused"));
    await expect(client.getProposal(7)).rejects.toThrow(/connection refused/);
  });
});

// ── One ProposalKind sample per contract variant, keyed by variant name ───────

const KINDS: Record<(typeof PROPOSAL_KIND_VARIANTS)[number], ProposalKind> = {
  UpdateFee: { kind: "UpdateFee", newFeeBps: 30n },
  UpdateFeeTier: { kind: "UpdateFeeTier", feeTier: 2 },
  UpdateProtocolFee: { kind: "UpdateProtocolFee", params: { newBps: 5n, newRecipient: BOB } },
  UpdateFlashLoanFee: { kind: "UpdateFlashLoanFee", newFeeBps: 7n },
  TransferAdmin: { kind: "TransferAdmin", newAdmin: BOB },
  PausePool: { kind: "PausePool" },
  UnpausePool: { kind: "UnpausePool" },
  EmergencyWithdraw: { kind: "EmergencyWithdraw", to: BOB },
  UpdateFactoryTreasury: {
    kind: "UpdateFactoryTreasury",
    params: { factory: BOB, treasury: CAROL, globalProtocolFeeBps: 9n },
  },
  UpdateFactoryGlobalFee: { kind: "UpdateFactoryGlobalFee", params: { factory: BOB, offset: 1, limit: 5 } },
  CreatePolVesting: {
    kind: "CreatePolVesting",
    params: {
      polVesting: ALICE,
      beneficiary: BOB,
      lpToken: CAROL,
      pool: ALICE,
      total: 1_000n,
      startLedger: 1,
      cliffLedger: 2,
      endLedger: 3,
    },
  },
  PauseClPool: { kind: "PauseClPool", clPool: BOB },
  UnpauseClPool: { kind: "UnpauseClPool", clPool: BOB },
  UpdateClOracle: { kind: "UpdateClOracle", params: { clPool: BOB, oracle: CAROL } },
  UpdateClMaxOracleDeviation: {
    kind: "UpdateClMaxOracleDeviation",
    params: { clPool: BOB, maxDeviationBps: 3n },
  },
  UpdateClProtocolFee: { kind: "UpdateClProtocolFee", params: { clPool: BOB, recipient: CAROL, bps: 4n } },
  TransferClPoolAdmin: { kind: "TransferClPoolAdmin", params: { clPool: BOB, newAdmin: CAROL } },
  SetClPositionNft: { kind: "SetClPositionNft", params: { clPool: BOB, nft: null } },
};
