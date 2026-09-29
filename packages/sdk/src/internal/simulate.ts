/**
 * Shared read-only simulation helper for every typed SDK client — Issue #831.
 *
 * Soroban read-only simulation does not need a real, funded source account: the
 * RPC server never checks the source account's existence or sequence number when
 * simulating. Building the envelope around a locally-constructed `Account`
 * therefore avoids both a needless network round-trip and a hard dependency on
 * the dummy account existing on the target network (testnet/futurenet/a fresh
 * local node).
 *
 * Previously each of the five clients inlined its own copy of this helper and
 * fetched a hardcoded source account via `server.getAccount(...)`. That
 * hardcoded string was 55 characters — one short of a valid 56-character
 * Stellar strkey — so it failed strkey decoding and made every read call throw.
 * Keeping the helper in one place prevents that kind of drift from recurring.
 */

import {
  Account,
  Contract,
  TransactionBuilder,
  rpc as StellarRpc,
  xdr,
} from "@stellar/stellar-sdk";

/**
 * Placeholder source account for read-only simulation.
 *
 * A syntactically valid 56-character Ed25519 strkey, derived deterministically
 * from the all-zero seed. It is never signed with, never funded, and never
 * looked up on-chain — it exists only to satisfy envelope construction.
 */
export const SIMULATION_SOURCE_ACCOUNT =
  "GA5WUJ54Z23KILLCUOUNAKTPBVZWKMQVO4O6EQ5GHLAERIMLLHNCSKYH";

/** Base fee used for simulation envelopes. Never charged — nothing is submitted. */
const SIMULATION_FEE = "100";

/** Transaction timeout, in seconds, for simulation envelopes. */
const SIMULATION_TIMEOUT = 30;

/**
 * Translates a failed simulation into an `Error`. Clients pass their own
 * contract-specific decoder (e.g. `AmmPool`'s `Error(Contract, #N)` mapper);
 * the default simply wraps the raw RPC message.
 */
export type SimulationErrorDecoder = (message: string) => Error;

const defaultErrorDecoder: SimulationErrorDecoder = (message) => new Error(message);

/**
 * Invoke `method` on `contract` in read-only simulation and return the raw
 * return value.
 *
 * Performs exactly one network call — `simulateTransaction`. No account fetch.
 *
 * @throws the result of `decodeError` when the simulation reports an error.
 */
export async function simulateRead(
  server: StellarRpc.Server,
  contract: Contract,
  networkPassphrase: string,
  method: string,
  args: xdr.ScVal[] = [],
  decodeError: SimulationErrorDecoder = defaultErrorDecoder
): Promise<xdr.ScVal> {
  // Sequence "0" is arbitrary: simulation ignores it entirely.
  const source = new Account(SIMULATION_SOURCE_ACCOUNT, "0");
  const tx = new TransactionBuilder(source, {
    fee: SIMULATION_FEE,
    networkPassphrase,
  })
    .addOperation(contract.call(method, ...args))
    .setTimeout(SIMULATION_TIMEOUT)
    .build();

  const result = await server.simulateTransaction(tx);
  if (StellarRpc.Api.isSimulationError(result)) {
    throw decodeError(result.error);
  }
  return result.result!.retval;
}
