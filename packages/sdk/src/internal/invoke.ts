import {
  BASE_FEE,
  Contract,
  Keypair,
  Operation,
  Transaction,
  TransactionBuilder,
  rpc as StellarRpc,
  scValToNative,
  xdr,
} from "@stellar/stellar-sdk";

export interface Signer {
  publicKey(): string | Promise<string>;
  signTransaction(envelopeXdr: string, networkPassphrase: string): string | Promise<string>;
}

export interface InvokeOptions {
  timeoutSeconds?: number;
  pollIntervalMs?: number;
  feeMultiplier?: number;
}

export interface InvokeRequest extends InvokeOptions {
  server: StellarRpc.Server;
  contract: Contract;
  method: string;
  args?: xdr.ScVal[];
  source: string;
  signer: Signer;
  networkPassphrase: string;
  decodeError?: (error: unknown) => Error;
}

export interface InvocationResult<T = unknown> {
  hash: string;
  ledger: number;
  returnValue: T;
}

export class SimulationError extends Error {
  constructor(message: string) {
    super(`Soroban simulation failed: ${message}`);
    this.name = "SimulationError";
  }
}

export class TransactionFailedError extends Error {
  readonly resultXdr?: string;
  readonly diagnosticEvents: readonly unknown[];

  constructor(message: string, resultXdr?: string, diagnosticEvents: readonly unknown[] = []) {
    super(message);
    this.name = "TransactionFailedError";
    this.resultXdr = resultXdr;
    this.diagnosticEvents = diagnosticEvents;
  }
}

export class TransactionTimeoutError extends Error {
  readonly hash: string;

  constructor(hash: string, timeoutSeconds: number) {
    super(`Transaction ${hash} was not confirmed within ${timeoutSeconds} seconds`);
    this.name = "TransactionTimeoutError";
    this.hash = hash;
  }
}

export function keypairSigner(secret: string): Signer {
  const keypair = Keypair.fromSecret(secret);
  return {
    publicKey: () => keypair.publicKey(),
    signTransaction(envelopeXdr, networkPassphrase) {
      const transaction = TransactionBuilder.fromXDR(envelopeXdr, networkPassphrase);
      if (!(transaction instanceof Transaction)) {
        throw new TypeError("Expected a transaction envelope, not a fee-bump transaction");
      }
      transaction.sign(keypair);
      return transaction.toXDR();
    },
  };
}

const delay = (milliseconds: number) =>
  new Promise<void>((resolve) => setTimeout(resolve, milliseconds));

export async function waitForTransaction<T = unknown>(
  server: StellarRpc.Server,
  hash: string,
  options: Pick<InvokeOptions, "timeoutSeconds" | "pollIntervalMs"> = {},
): Promise<InvocationResult<T>> {
  const timeoutSeconds = options.timeoutSeconds ?? 60;
  const pollIntervalMs = options.pollIntervalMs ?? 1_000;
  const deadline = Date.now() + timeoutSeconds * 1_000;

  while (Date.now() < deadline) {
    const result = await server.getTransaction(hash);
    const status = String(result.status);
    if (status === "SUCCESS") {
      const response = result as typeof result & {
        ledger: number;
        returnValue?: xdr.ScVal;
      };
      return {
        hash,
        ledger: response.ledger,
        returnValue: (response.returnValue
          ? scValToNative(response.returnValue)
          : undefined) as T,
      };
    }
    if (status === "FAILED") {
      const response = result as typeof result & {
        resultXdr?: { toXDR(format: "base64"): string };
        diagnosticEventsXdr?: readonly unknown[];
      };
      throw new TransactionFailedError(
        `Transaction ${hash} failed`,
        response.resultXdr?.toXDR("base64"),
        response.diagnosticEventsXdr ?? [],
      );
    }
    await delay(pollIntervalMs);
  }
  throw new TransactionTimeoutError(hash, timeoutSeconds);
}

async function signAndSubmit(
  server: StellarRpc.Server,
  transaction: Transaction,
  signer: Signer,
  networkPassphrase: string,
  options: InvokeOptions,
): Promise<InvocationResult> {
  const signedXdr = await signer.signTransaction(transaction.toXDR(), networkPassphrase);
  const signed = TransactionBuilder.fromXDR(signedXdr, networkPassphrase);
  if (!(signed instanceof Transaction)) {
    throw new TypeError("Signer returned a fee-bump transaction");
  }

  const timeoutSeconds = options.timeoutSeconds ?? 60;
  const deadline = Date.now() + timeoutSeconds * 1_000;
  let backoff = options.pollIntervalMs ?? 1_000;
  while (Date.now() < deadline) {
    const submitted = await server.sendTransaction(signed);
    if (String(submitted.status) !== "TRY_AGAIN_LATER") {
      if (String(submitted.status) === "ERROR") {
        throw new TransactionFailedError(
          `Transaction submission failed: ${String(submitted.errorResult)}`,
        );
      }
      return waitForTransaction(server, submitted.hash, options);
    }
    await delay(backoff);
    backoff = Math.min(backoff * 2, 5_000);
  }
  throw new TransactionTimeoutError(Buffer.from(signed.hash()).toString("hex"), timeoutSeconds);
}

function transactionFee(multiplier: number, resourcePadding = 0): string {
  return String(Math.ceil(Number(BASE_FEE) * Math.max(1, multiplier) + resourcePadding));
}

export async function invoke<T = unknown>(request: InvokeRequest): Promise<InvocationResult<T>> {
  const {
    server,
    contract,
    method,
    args = [],
    source,
    signer,
    networkPassphrase,
    decodeError,
  } = request;
  if ((await signer.publicKey()) !== source) {
    throw new Error("Signer public key does not match the transaction source");
  }

  const timeoutSeconds = request.timeoutSeconds ?? 60;
  const feeMultiplier = request.feeMultiplier ?? 1.15;
  const buildInvocation = async (resourcePadding = 0) => {
    const account = await server.getAccount(source);
    return new TransactionBuilder(account, {
      fee: transactionFee(feeMultiplier, resourcePadding),
      networkPassphrase,
    })
      .addOperation(contract.call(method, ...args))
      .setTimeout(timeoutSeconds)
      .build();
  };

  let transaction = await buildInvocation();
  let simulation = await server.simulateTransaction(transaction);
  if (StellarRpc.Api.isSimulationError(simulation)) {
    throw (decodeError?.(simulation.error) ?? new SimulationError(simulation.error));
  }

  if (StellarRpc.Api.isSimulationRestore(simulation)) {
    const account = await server.getAccount(source);
    const restoreFee = Math.ceil(
      (Number(BASE_FEE) + Number(simulation.restorePreamble.minResourceFee)) * feeMultiplier,
    );
    const restore = new TransactionBuilder(account, {
      fee: String(restoreFee),
      networkPassphrase,
    })
      .addOperation(Operation.restoreFootprint({}))
      .setSorobanData(simulation.restorePreamble.transactionData.build())
      .setTimeout(timeoutSeconds)
      .build();
    await signAndSubmit(server, restore, signer, networkPassphrase, request);
    transaction = await buildInvocation();
    simulation = await server.simulateTransaction(transaction);
    if (StellarRpc.Api.isSimulationError(simulation)) {
      throw (decodeError?.(simulation.error) ?? new SimulationError(simulation.error));
    }
  }

  if (!StellarRpc.Api.isSimulationSuccess(simulation)) {
    throw new SimulationError("RPC returned an unrecognized simulation response");
  }

  const resourcePadding = Math.ceil(
    Number(simulation.minResourceFee) * Math.max(0, feeMultiplier - 1),
  );
  transaction = await buildInvocation(resourcePadding);

  let prepared: Transaction;
  try {
    prepared = await server.prepareTransaction(transaction);
  } catch (error) {
    throw (decodeError?.(error) ?? new SimulationError(String(error)));
  }
  return (await signAndSubmit(
    server,
    prepared,
    signer,
    networkPassphrase,
    request,
  )) as InvocationResult<T>;
}
