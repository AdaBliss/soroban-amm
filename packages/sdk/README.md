# @soroban-amm/sdk

Typed TypeScript clients for the Soroban AMM protocol. The package covers the
core pools and factory plus routing, governance, staking, incentives, vesting,
oracle consumers, aggregation and batch auctions.

## Installation

```bash
npm install @soroban-amm/sdk @stellar/stellar-sdk
```

## Usage

The SDK provides typed clients for each contract. All clients take the same constructor options:
`{ rpcUrl, networkPassphrase, contractId }`.

Property values of type `i128` are represented as `bigint` throughout.

### Signing and submitting writes

Write methods simulate and assemble the Soroban footprint and authorization,
restore archived entries when required, sign, submit, and wait for a terminal
ledger result. Slippage limits and deadlines are always explicit.

```ts
import { AmmPool, keypairSigner } from "@soroban-amm/sdk";

const signer = keypairSigner(process.env.STELLAR_SECRET!);
const trader = await signer.publicKey();
const result = await pool.swap(
  {
    trader,
    tokenIn: info.tokenA,
    amountIn: 1_000_000n,
    minAmountOut: 990_000n,
    deadline: BigInt(Math.floor(Date.now() / 1000) + 300),
  },
  signer,
  { timeoutSeconds: 90, pollIntervalMs: 1_000, feeMultiplier: 1.2 },
);
console.log(result.hash, result.ledger, result.returnValue);
```

Never put a secret key in browser code, logs, source control, or analytics.
Browser wallets should implement the exported `Signer` interface and keep key
material inside the wallet. `keypairSigner` is intended for controlled server
or command-line environments.

### AmmPool

```ts 
import { AmmPool } from "@soroban-amm/sdk";

const pool = new AmmPool({
  rpcUrl: "https://soroban-testnet.stellar.org",
  networkPassphrase: "Test SDF Network ; September 2015",
  contractId: "C...",
});

// Fetch full pool state
const info = await pool.getInfo();
console.log(info.reserveA, info.reserveB, info.feeBps);

// Simulate a swap via the pool's own `simulate_swap` entrypoint
const quote = await pool.simulateSwap(info.tokenA, 1_000_000n);
console.log(`Out: ${quote.amountOut}, price impact: ${quote.priceImpactBps} bps`);

// On-chain quote
const out = await pool.getAmountOut(info.tokenA, 1_000_000n);

// LP share balance
const shares = await pool.sharesOf("G...");
```

### StakingClient

```ts
import { StakingClient } from "@soroban-amm/sdk";

const staking = new StakingClient({
  rpcUrl: "https://soroban-testnet.stellar.org",
  networkPassphrase: "Test SDF Network ; September 2015",
  contractId: "C...",
});

// Read pool information and encode write arguments for a transaction builder.
const poolInfo = await staking.getPoolInfo();
const multiplier = staking.boostMultiplierHuman(await staking.currentBoost("G..."));
const args = staking.stakeLockedParams("G...", 500n, 1200n);
```

### IncentiveCampaignsClient

```ts 
import { IncentiveCampaignsClient } from "@soroban-amm/sdk";

const incentives = new IncentiveCampaignsClient({
  rpcUrl: "https://soroban-testnet.stellar.org",
  networkPassphrase: "Test SDF Network ; September 2015",
  contractId: "C...",
});

const campaigns = await incentives.listCampaignsPaginated(0, 20);
const campaign = await incentives.getCampaign(campaigns[0]);
const claimArgs = incentives.claimRewardsParams("G...", campaign.id);
```

### RouterClient

```ts
import { RouterClient } from "@soroban-amm/sdk";

const router = new RouterClient({
  rpcUrl: "https://soroban-testnet.stellar.org",
  networkPassphrase: "Test SDF Network ; September 2015",
  contractId: "C...",
});

// Quote an out amount for a path
const amountOut = await router.getAmountOutPath(["C...", "C..."], 1_000_000n);

// Swap exact in (with default deadline now+300s)
const swapArgs = router.swapExactInParams({
  trader: "G...",
  path: ["C...", "C..."],
  amountIn: 1_000_000n,
  minAmountOut: 1n,
  deadline: BigInt(Math.floor(Date.now() / 1000) + 300),
});
```

### GovernanceClient

Every method below maps onto a real entrypoint of `contracts/governance`. Methods
whose name is a contract function are marked with the entrypoint they send; the
rest are **client-side compositions** that page or fan out over real
entrypoints, cost more than one RPC call, and say so in their JSDoc.

```ts
import { GovernanceClient } from "@soroban-amm/sdk";

const gov = new GovernanceClient({
  rpcUrl: "https://soroban-testnet.stellar.org",
  networkPassphrase: "Test SDF Network ; September 2015",
  contractId: "C...",
});

// ── Reads ─────────────────────────────────────────────────────────────────────

const params = await gov.getParams();            // get_params
console.log(params.quorumBps, params.vetoMultisig, params.quorumDecayRateBpsPerDay);

const count = await gov.getProposalCount();      // get_proposal_count

// The contract's Proposal struct has no `status` field: status is derived from
// the stored fields plus the current ledger clock and returned separately.
const proposal = await gov.getProposal(7);      // get_proposal  -> ProposalData
const status = await gov.proposalStatus(7);     // proposal_status

// Both at once (two RPC calls).
const full = await gov.getProposalWithStatus(7);
console.log(full.kind, full.status);

const quorum = await gov.getEffectiveQuorum(7); // get_effective_quorum
const balance = await gov.getSnapshotBalance(7, voter); // get_snapshot_balance
const audit = await gov.getVetoAudit(7);        // get_veto_audit -> VetoAudit | null
const record = await gov.getVoteInfo(7, voter); // get_vote_info -> VoteRecord
const to = await gov.getDelegate(voter);        // get_delegate -> string | null

// Client-side composition: pages get_proposals_paginated.
const page = await gov.listProposals(0, 50);   // get_proposals_paginated
const newest = await gov.listProposalsDesc(0, 10);
const active = await gov.getActiveProposalIds(); // one proposal_status per proposal

// ── Writes ────────────────────────────────────────────────────────────────────
// The *Params builders return `xdr.ScVal[]` ready to hand to a transaction.

gov.proposeParams(proposer, { kind: "UpdateFee", newFeeBps: 30n });
gov.proposeParams(proposer, { kind: "PausePool" });
gov.proposeParams(proposer, { kind: "TransferAdmin", newAdmin: newAdmin });
gov.proposeParams(proposer, {
  kind: "UpdateClProtocolFee",
  params: { clPool, recipient, bps: 10n },
});

gov.voteParams(voter, 7, "For");        // vote(voter, proposal_id, choice)
gov.executeParams(7);                   // execute(proposal_id)
gov.cancelParams(7, proposer);          // cancel_proposal(proposal_id, proposer)
gov.unlockVoteParams(voter, 7);         // unlock_vote(voter, proposal_id)
gov.vetoParams(7);                      // veto(proposal_id)
gov.delegateParams(from, to);           // delegate(from, to)
gov.undelegateParams(from);             // undelegate(from)
```

#### Enum encodings

The governance contract's enums are `#[contracttype]` enums, so a unit variant
is a `scvVec` whose head is a `scvSymbol` — not a bare symbol or string. The SDK
encodes and decodes them for you:

| Contract type | Encoding | Helper |
|---|---|---|
| `Vote` | `scvVec([scvSymbol("For")])` | `encodeVote` |
| `ProposalStatus` | `scvVec([scvSymbol("Queued")])` | `encodeProposalStatus` |
| `ProposalKind` | `scvVec([scvSymbol("UpdateFee"), scvI128(30n)])` | `encodeProposalKind` |
| `VoteRecord` | `scvVec([scvSymbol("VotedFor")])` | `encodeVoteRecord` |

`ProposalKind` covers all 18 variants of the contract enum; the exported
`PROPOSAL_KIND_VARIANTS` list is asserted against the Rust `enum ProposalKind` in
`governance.test.ts`, so it cannot fall behind the contract.

Errors returned by the contract decode into a typed `GovernanceContractError`
carrying the `GovernanceError` discriminant and its symbolic name:

```ts
import { GovernanceContractError } from "@soroban-amm/sdk";

try {
  await gov.getProposal(999);
} catch (err) {
  if (err instanceof GovernanceContractError && err.variant === "ProposalNotFound") {
    // ...
  }
}
```

`tryGetProposal` returns `null` for an unknown id instead of throwing; every
other failure still propagates.

## Contract ABI conformance

`npm run generate:abi` regenerates `src/__fixtures__/contract-abi.json` from the
contract Rust sources; `npm run check:abi` fails when the checked-in fixture is
stale. `abi.test.ts` is driven by that fixture: every `*Params` builder must
match its entrypoint's arity, order and XDR types, and every method name a client
sends must exist in the fixture.

The fixture is generated from the Rust source rather than `docs/abi.json`, which
is produced from built WASM and currently lags the source (see `governance.ts`).

Write builders follow the deployed signatures: `mintPositionParams` takes the
trailing `deadline`, `approveParams` takes `liveUntilLedger`, and
`createPoolParams` takes `caller` first plus a `feeTier` — use
`createPoolWithFeeBpsParams` for a custom fee in basis points. Returning reads
name real entrypoints: `getFlashLoanFeeBps` reads `get_info().flash_loan_fee_bps`,
`poolCount` calls `get_pool_count`, and `observe(secondsAgo)` mirrors the
single-argument contract function (`observeBatch` samples several points
client-side).

## Exported types

| Type | Description |
~|---|---|
| `PoolInfo` | Full pool state from `get_info` |
| `SwapSimulation` | Result of `simulateSwap` (decoded from the contract's `simulate_swap`) |
| `SwapParams` | Parameters for a swap transaction |
| `AddLiquidityParams` | Parameters for adding liquidity |
| `RemoveLiquidityParams` | Parameters for removing liquidity |
| `LiquidityResult` | Amounts returned from liquidity ops |
| `FlashLoanParams` | Flash loan parameters |
| `NetworkConfig` | RPC + contract configuration |
| `AmmErrors` | Well-known AMM error strings |
| `PoolInfo` (Staking) | Staking pool state - contract type |
| `StakerInfo` | Staker information |
| `LockedPosition` | Locked staking position |
| `Campaign` | Incentive campaign data |
| `DistributionRecord` | Reward distribution record |
| `GovernanceParams` | Governance configuration from `get_params` |
| `ProposalData` | Stored proposal — contract `Proposal` struct, no status |
| `Proposal` | `ProposalData` plus the derived `status` |
| `ProposalKind` | Discriminated union over all 18 `ProposalKind` variants |
| `ProposalStatus` | `Active` … `Vetoed` (9 variants) |
| `VoteChoice` / `VoteRecord` | Contract `Vote` / `VoteRecord` variants |
| `VetoAudit` | Veto audit record from `get_veto_audit` |
| `GovernanceContractError` | Typed contract error with `code` and `variant` |
