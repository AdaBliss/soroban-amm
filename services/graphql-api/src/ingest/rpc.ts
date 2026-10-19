/**
 * Soroban RPC event ingester for the AMM analytics service.
 *
 * This component:
 * - Connects to a Soroban RPC endpoint via getEvents
 * - Handles cursor-based pagination correctly (per contract ID)
 * - Detects and reports Soroban RPC retention window violations
 * - Decodes versioned event envelopes (EVENT_SCHEMA_VERSION) using @stellar/stellar-sdk
 * - Maps contract events to pool event types
 * - Calls onEvent / indexer for each successfully decoded event
 */

// Node >=22 (see package.json engines) ships a native global `fetch`, so no
// node-fetch import — tests rely on being able to stub the ambient
// `globalThis.fetch`, which a module-scoped import would bypass entirely.
import { xdr, scValToNative } from "@stellar/stellar-sdk";
import type { PoolEvent, PoolEventType, AnalyticsStore } from "../store/interface.js";
import { isoToSeconds, systemClock } from "../time.js";

/**
 * Configuration for the Soroban RPC ingester.
 */
export interface RpcIngesterOptions {
  /** Soroban RPC base URL, e.g. "https://soroban-testnet.stellar.org" */
  rpcUrl: string;
  /** Contract IDs to subscribe to (pool, factory, governance, etc.) */
  contractIds: string[];
  /** Polling interval in milliseconds (default 5000) */
  pollIntervalMs?: number;
  /** Starting ledger (default 0 = start from latestLedger - retention or oldest available) */
  startLedger?: number;
  /**
   * Swap fee, in basis points, per pool contract ID. A swap event carries no
   * fee field (see contracts/amm-sdk/src/events.rs's SwapEvent), so the fee
   * actually charged can't be recovered from the event stream alone — it
   * must come from how the pool was configured. Pools not listed here fall
   * back to DEFAULT_FEE_BPS, which is very likely wrong for that pool.
   */
  poolFeeBps?: Record<string, number>;
}

/** Fallback swap fee, in basis points, for a pool not in `poolFeeBps`. */
const DEFAULT_FEE_BPS = 30;

/**
 * Response shape from Soroban RPC getEvents. Exported for tests building
 * fixtures against this shape.
 */
export interface SorobanRpcEvent {
  type: string;
  ledger: number;
  ledgerClosedAt: string;
  contractId: string;
  id: string;
  pagingToken: string;
  topic: string[];
  value: string;
  inSuccessfulContractInvocation: boolean;
  /** Not present on every RPC version; falls back to pagingToken/id when absent. */
  txHash?: string;
}

interface GetEventsResponse {
  jsonrpc: string;
  id: string;
  result?: {
    events: SorobanRpcEvent[];
    latestLedger: number;
    oldestLedger?: number;
  };
  error?: {
    code: number;
    message: string;
  };
}

/**
 * Topic name constants for AMM contract events.
 */
const TOPIC_MAP: Record<string, PoolEventType> = {
  swap: "swap",
  add_liquidity: "add_liquidity",
  remove_liquidity: "remove_liquidity",
  rm_liq: "remove_liquidity",
  rm_liq_1s: "remove_liquidity",
  campaign_created: "campaign_created",
  reward_distributed: "reward_distributed",
  fot_detected: "fot_detected",
  price_upd: "price_upd",
};

/**
 * Current event schema version that this ingester understands.
 */
const CURRENT_EVENT_SCHEMA_VERSION = 1;

/** Converts an scValToNative result (string, number, bigint, Address-like
 * object, ...) to a string without risking a plain-object's default
 * `[object Object]` stringification. */
function asStr(val: unknown): string {
  if (val === null || val === undefined) return "";
  if (typeof val === "string") return val;
  if (typeof val === "number" || typeof val === "bigint" || typeof val === "boolean") {
    return val.toString();
  }
  // Address and similar SDK-decoded objects expose a meaningful toString().
  return (val as { toString(): string }).toString();
}

export class RpcIngester {
  private running = false;
  private readonly opts: Required<RpcIngesterOptions>;
  private lastSeenLedger = 0;
  // Per-contract cursors and ledger tracking
  private contractCursors = new Map<string, string>(); // contractId -> pagingToken
  private contractLedgers = new Map<string, number>(); // contractId -> currentLedger
  // contractIds whose persisted cursor we've already tried to restore. Keyed
  // separately from contractLedgers (which is pre-populated for every
  // configured contract in the constructor) so restoration runs exactly once
  // per contract regardless of startLedger.
  private restoredContracts = new Set<string>();

  constructor(
    opts: RpcIngesterOptions,
    private readonly onEvent: (event: PoolEvent) => Promise<void>,
    private readonly onError: (error: Error) => Promise<void>,
    private readonly store?: AnalyticsStore,
  ) {
    this.opts = {
      pollIntervalMs: 5000,
      startLedger: 0,
      poolFeeBps: {},
      ...opts,
    };
    for (const contractId of this.opts.contractIds) {
      this.contractLedgers.set(contractId, this.opts.startLedger);
    }
  }

  /**
   * Start polling for events.
   */
  start(): void {
    if (this.running) return;
    this.running = true;
    void this._loop();
  }

  /**
   * Stop polling.
   */
  stop(): void {
    this.running = false;
  }

  /**
   * Get the current ledger being processed for a contract (or first contract).
   */
  getCurrentLedger(contractId?: string): number {
    const target = contractId ?? this.opts.contractIds[0];
    return target ? (this.contractLedgers.get(target) ?? this.opts.startLedger) : this.opts.startLedger;
  }

  /**
   * Get lag in ledgers compared to the latest ledger on the network.
   */
  getLagLedgers(): number {
    const maxLedger = Math.max(...Array.from(this.contractLedgers.values()), this.opts.startLedger);
    return Math.max(0, this.lastSeenLedger - maxLedger);
  }

  /**
   * Get lag in seconds (rough estimate: ~5 seconds per ledger).
   */
  getLagSeconds(): number {
    return this.getLagLedgers() * 5;
  }

  private async _loop(): Promise<void> {
    while (this.running) {
      try {
        await this._poll();
      } catch (err) {
        await this.onError(err instanceof Error ? err : new Error(String(err)));
      }
      await this._sleep(this.opts.pollIntervalMs);
    }
  }

  private async _poll(): Promise<void> {
    for (const contractId of this.opts.contractIds) {
      // Isolate one contract's failure from the rest: otherwise a single
      // misbehaving contract (a bad getEvents response, a transient RPC
      // error) would stop every other configured contract from being
      // polled for the rest of this cycle.
      try {
        await this._pollContract(contractId);
      } catch (err) {
        await this.onError(err instanceof Error ? err : new Error(String(err)));
      }
    }
  }

  /**
   * On a contract's very first poll after this ingester starts, resume from
   * its persisted cursor instead of `startLedger`. There's no persisted
   * getEvents pagination token to restore (IngestionCursor doesn't carry
   * one — it's an idempotency marker, not an RPC cursor), so resumption
   * works by setting `contractLedgers` to the last successfully ingested
   * ledger and letting the normal startLedger path in `_pollContract` take
   * it from there.
   */
  private async _restoreCursorOnce(contractId: string): Promise<void> {
    if (!this.store || this.restoredContracts.has(contractId)) {
      return;
    }
    this.restoredContracts.add(contractId);
    const persisted = await this.store.getCursor(contractId);
    if (persisted) {
      this.contractLedgers.set(contractId, persisted.ledger);
    }
  }

  private async _pollContract(contractId: string): Promise<void> {
    await this._restoreCursorOnce(contractId);
    let currentLedger = this.contractLedgers.get(contractId) ?? this.opts.startLedger;
    const cursor = this.contractCursors.get(contractId);

    const params: Record<string, unknown> = {
      jsonrpc: "2.0",
      id: `getEvents-${Date.now()}`,
      method: "getEvents",
      params: {
        filters: [
          {
            type: "contract",
            contractIds: [contractId],
          },
        ],
        pagination: {
          cursor: cursor || undefined,
          limit: 100,
        },
      },
    };

    // If there is no cursor and startLedger is specified / resolved, send startLedger
    if (!cursor) {
      if (currentLedger > 0) {
        (params.params as Record<string, unknown>).startLedger = currentLedger;
      } else {
        // First request without cursor and startLedger=0: we need to query latest ledger or get oldest available
        // Or if startLedger is 0, we can probe / fetch latest ledger first or send a preliminary call or fetch latestLedger.
        // Actually, requirement: "With START_LEDGER unset, the first getEvents request contains a positive startLedger."
        // Let's fetch latestLedger if startLedger === 0.
        const latestInfo = await this._getLatestLedgerInfo();
        if (latestInfo.latestLedger > 0) {
          const retention = 10000;
          const resolvedStart = Math.max(1, latestInfo.oldestLedger ?? (latestInfo.latestLedger - retention));
          currentLedger = resolvedStart;
          this.contractLedgers.set(contractId, currentLedger);
          (params.params as Record<string, unknown>).startLedger = currentLedger;
        }
      }
    }

    const res = await fetch(this.opts.rpcUrl, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(params),
    });

    if (!res.ok) {
      throw new Error(
        `RPC error: HTTP ${res.status} from ${this.opts.rpcUrl}`,
      );
    }

    const body = (await res.json()) as GetEventsResponse;

    if (body.error) {
      throw new Error(
        `RPC error ${body.error.code}: ${body.error.message}`,
      );
    }

    const result = body.result;
    if (!result) {
      throw new Error("RPC getEvents returned empty result");
    }

    this.lastSeenLedger = result.latestLedger;
    const oldestAvailable = result.oldestLedger ?? (result.latestLedger - 10000);

    if (currentLedger > 0 && currentLedger < oldestAvailable) {
      await this.onError(
        new Error(
          `Ingestion lag detected: current ledger ${currentLedger} is outside ` +
          `RPC retention window (oldest available: ${oldestAvailable}). ` +
          `Consumer fell behind and cannot recover from RPC alone.`,
        ),
      );
      return;
    }

    // Process events
    for (const raw of result.events) {
      if (raw.inSuccessfulContractInvocation) {
        const event = this._decodeEvent(raw, contractId);
        if (event) {
          await this.onEvent(event);
          if (this.store) {
            await this.store.setCursor({
              contractId,
              ledger: raw.ledger,
              txHash: event.txHash,
              eventIndex: event.eventIndex,
              updatedAt: systemClock(),
            });
          }
        }
        currentLedger = raw.ledger;
        this.contractLedgers.set(contractId, currentLedger);
      }
      if (raw.pagingToken) {
        this.contractCursors.set(contractId, raw.pagingToken);
      }
    }
  }

  private async _getLatestLedgerInfo(): Promise<{ latestLedger: number; oldestLedger?: number }> {
    // Deliberately no catch-and-default here: a hardcoded fallback ledger
    // would make the caller start polling from a bogus, network-independent
    // ledger with no visible failure — silently missing every real event, or
    // repeatedly tripping the retention-window check. Any failure here must
    // propagate to the caller's error handling instead.
    const res = await fetch(this.opts.rpcUrl, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        jsonrpc: "2.0",
        id: `latestLedger-${Date.now()}`,
        method: "getLatestLedger",
        params: {},
      }),
    });
    if (!res.ok) {
      throw new Error(
        `RPC error: HTTP ${res.status} from ${this.opts.rpcUrl} (getLatestLedger)`,
      );
    }
    const body = (await res.json()) as {
      result?: { sequence?: number; latestLedger?: number; oldestLedger?: number };
      error?: { code: number; message: string };
    };
    if (body.error) {
      throw new Error(`RPC error ${body.error.code}: ${body.error.message} (getLatestLedger)`);
    }
    const latestLedger = body.result?.sequence ?? body.result?.latestLedger;
    if (!latestLedger) {
      throw new Error("RPC getLatestLedger returned no sequence");
    }
    return { latestLedger, oldestLedger: body.result?.oldestLedger };
  }

  private _decodeEvent(raw: SorobanRpcEvent, contractId: string): PoolEvent | null {
    try {
      // Topic[0] is event name, Topic[1] is schema version (or part of versioned envelope)
      // Let's decode topics using xdr.ScVal.fromXDR(..., "base64") and scValToNative
      const topics = (raw.topic || []).map((t): unknown => {
        try {
          const scVal = xdr.ScVal.fromXDR(t, "base64");
          return scValToNative(scVal);
        } catch {
          return t;
        }
      });

      const eventName = asStr(topics[0] ?? "");
      const eventType = TOPIC_MAP[eventName];
      if (!eventType) {
        return null;
      }

      // Decode value (payload) as XDR ScVal -> native tuple [schemaVersion, payloadData]
      // emit_versioned_event! puts version in data tuple: publish(topics, (EVENT_SCHEMA_VERSION, payload))
      let schemaVersion = CURRENT_EVENT_SCHEMA_VERSION;
      let rawPayload: unknown = {};

      if (raw.value) {
        try {
          const valScVal = xdr.ScVal.fromXDR(raw.value, "base64");
          const nativeVal: unknown = scValToNative(valScVal);
          if (Array.isArray(nativeVal) && nativeVal.length >= 2) {
            schemaVersion = Number(nativeVal[0]);
            rawPayload = nativeVal[1];
          } else {
            rawPayload = nativeVal;
          }
        } catch {
          // fallback if not a tuple
        }
      }

      // Reject events with newer schema versions than supported
      if (schemaVersion > CURRENT_EVENT_SCHEMA_VERSION) {
        console.warn(
          `[RpcIngester] Rejecting event ${raw.id} with schema version ${schemaVersion} ` +
          `(we only understand up to ${CURRENT_EVENT_SCHEMA_VERSION})`,
        );
        return null;
      }

      // Maps rawPayload onto PoolEvent fields per contracts/amm-sdk/src/events.rs:
      // - swap: topics `("swap", trader)`, data `(1, (token_in, amount_in, token_out, amount_out, referrer))`
      // - add_liquidity: topics `("add_liquidity", provider)`, data `(1, (amount_a, amount_b, shares_minted))`
      // - remove_liquidity (`rm_liq`/`rm_liq_1s`): topics `("rm_liq",)`, data `(1, (provider, shares_burned, amount_a, amount_b))`
      const payload = this._mapPayload(eventName, topics, rawPayload, contractId);

      const [txHashPart, indexStr] = (raw.pagingToken || "").split("-");
      const eventIndex = Number(indexStr ?? "0");
      // Use real transaction hash from RPC response if available, else txHashPart or raw.id
      const txHash = raw.txHash ?? txHashPart ?? raw.id;

      return {
        id: raw.id,
        poolId: contractId,
        type: eventType,
        timestamp: raw.ledgerClosedAt ? isoToSeconds(raw.ledgerClosedAt) : systemClock(),
        ledger: raw.ledger,
        txHash,
        eventIndex: Number.isFinite(eventIndex) ? eventIndex : 0,
        payload,
      };
    } catch (err) {
      console.error(`[RpcIngester] Failed to decode event ${raw.id}:`, err);
      return null;
    }
  }

  private _mapPayload(
    eventName: string,
    topics: unknown[],
    rawPayload: unknown,
    contractId: string,
  ): Record<string, unknown> {
    const payload: Record<string, unknown> = {};

    if (eventName === "swap") {
      // topics[1] is trader (Address)
      if (topics.length > 1) {
        payload["trader"] = asStr(topics[1]);
      }
      // rawPayload is tuple: (token_in, amount_in, token_out, amount_out, referrer)
      if (Array.isArray(rawPayload)) {
        payload["tokenA"] = asStr(rawPayload[0]); // token_in / tokenA representation in indexer
        payload["token_in"] = asStr(rawPayload[0]);
        payload["amountIn"] = Number(rawPayload[1] ?? 0);
        payload["tokenB"] = asStr(rawPayload[2]); // token_out
        payload["token_out"] = asStr(rawPayload[2]);
        payload["amountOut"] = Number(rawPayload[3] ?? 0);
        payload["referrer"] = rawPayload[4] !== null && rawPayload[4] !== undefined ? asStr(rawPayload[4]) : null;
        payload["price"] = Number(rawPayload[3] ?? 0) > 0 && Number(rawPayload[1] ?? 0) > 0 ? Number(rawPayload[3]) / Number(rawPayload[1]) : 0;
        const feeBps = this.opts.poolFeeBps[contractId] ?? DEFAULT_FEE_BPS;
        payload["fee"] = Math.round((Number(rawPayload[1] ?? 0) * feeBps) / 10_000);
      }
    } else if (eventName === "add_liquidity") {
      // topics[1] is provider
      if (topics.length > 1) {
        payload["provider"] = asStr(topics[1]);
      }
      // rawPayload: (amount_a, amount_b, shares)
      if (Array.isArray(rawPayload)) {
        payload["amountA"] = Number(rawPayload[0] ?? 0);
        payload["amountB"] = Number(rawPayload[1] ?? 0);
        payload["shares"] = Number(rawPayload[2] ?? 0);
      }
    } else if (eventName === "remove_liquidity" || eventName === "rm_liq" || eventName === "rm_liq_1s") {
      // rawPayload: (provider, shares, amount_a, amount_b) or similar
      if (Array.isArray(rawPayload)) {
        payload["provider"] = asStr(rawPayload[0]);
        payload["shares"] = Number(rawPayload[1] ?? 0);
        payload["amountA"] = Number(rawPayload[2] ?? 0);
        payload["amountB"] = Number(rawPayload[3] ?? 0);
      }
    } else if (typeof rawPayload === "object" && rawPayload !== null) {
      Object.assign(payload, rawPayload);
    }

    return payload;
  }

  private _sleep(ms: number): Promise<void> {
    return new Promise((resolve) => setTimeout(resolve, ms));
  }
}
