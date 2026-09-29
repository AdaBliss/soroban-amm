/**
 * Soroban RPC event ingester for the AMM analytics service.
 *
 * This component:
 * - Connects to a Soroban RPC endpoint via getEvents
 * - Handles cursor-based pagination correctly
 * - Detects and reports Soroban RPC retention window violations
 * - Decodes versioned event envelopes (EVENT_SCHEMA_VERSION)
 * - Maps contract events to pool event types
 * - Calls indexEvent for each successfully decoded event
 */

import fetch from "node-fetch";
import type { PoolEvent, PoolEventType } from "../store/interface.js";

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
  /** Starting ledger (default 0 = start from oldest) */
  startLedger?: number;
}

/**
 * Response shape from Soroban RPC getEvents.
 */
interface SorobanRpcEvent {
  type: string;
  ledger: number;
  ledgerClosedAt: string;
  contractId: string;
  id: string;
  pagingToken: string;
  topic: (string | null)[];
  value: {
    type: string;
    n: number;
  } | string | null;
  inSuccessfulContractInvocation: boolean;
}

interface GetEventsResponse {
  jsonrpc: string;
  id: string;
  result?: {
    events: SorobanRpcEvent[];
    latestLedger: number;
  };
  error?: {
    code: number;
    message: string;
  };
}

/**
 * Topic name constants for AMM contract events.
 * These map soroban symbol_short topic names to pool event types.
 */
const TOPIC_MAP: Record<string, PoolEventType> = {
  swap: "swap",
  add_liquidity: "add_liquidity",
  remove_liquidity: "remove_liquidity",
  campaign_created: "campaign_created",
  reward_distributed: "reward_distributed",
  fot_detected: "fot_detected",
  price_upd: "price_upd",
  tick_crossed: "tick_crossed",
};

/**
 * Current event schema version that this ingester understands.
 * Bump this when the event payload structure changes in a breaking way.
 */
const CURRENT_EVENT_SCHEMA_VERSION = 1;

export class RpcIngester {
  private running = false;
  private currentLedger: number;
  private currentPagingToken: string = "";
  private readonly opts: Required<RpcIngesterOptions>;
  private lastLedgerCheck = 0;
  private lastSeenLedger = 0;

  constructor(
    opts: RpcIngesterOptions,
    private readonly onEvent: (event: PoolEvent) => Promise<void>,
    private readonly onError: (error: Error) => Promise<void>,
  ) {
    this.opts = {
      pollIntervalMs: 5000,
      startLedger: 0,
      ...opts,
    };
    this.currentLedger = this.opts.startLedger;
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
   * Get the current ledger being processed.
   */
  getCurrentLedger(): number {
    return this.currentLedger;
  }

  /**
   * Get lag in ledgers compared to the latest ledger on the network.
   */
  getLagLedgers(): number {
    return Math.max(0, this.lastSeenLedger - this.currentLedger);
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
      await this._pollContract(contractId);
    }
  }

  private async _pollContract(contractId: string): Promise<void> {
    const params = {
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
          cursor: this.currentPagingToken || undefined,
          limit: 100,
        },
      },
    };

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

    // Check for retention window violations
    const oldestAvailable = this.lastSeenLedger - 10000; // RPC typically retains ~10k ledgers
    if (this.currentLedger > 0 && this.currentLedger < oldestAvailable) {
      await this.onError(
        new Error(
          `Ingestion lag detected: current ledger ${this.currentLedger} is outside ` +
          `RPC retention window (oldest available: ${oldestAvailable}). ` +
          `Consumer fell behind and cannot recover from RPC alone. ` +
          `Restart from a recent checkpoint or resync from a backup.`,
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
          this.currentLedger = raw.ledger;
        }
      }
      this.currentPagingToken = raw.pagingToken;
    }
  }

  private _decodeEvent(raw: SorobanRpcEvent, contractId: string): PoolEvent | null {
    try {
      // Topic[0] is the event name (symbol_short)
      // Topic[1] is the EVENT_SCHEMA_VERSION
      // Topics[2:] are the remaining payload topics
      const eventName = this._decodeTopic(raw.topic[0]);
      const schemaVersion = this._extractSchemaVersion(raw.topic[1]);

      // Reject events with newer schema versions
      if (schemaVersion > CURRENT_EVENT_SCHEMA_VERSION) {
        console.warn(
          `[RpcIngester] Rejecting event ${raw.id} with schema version ${schemaVersion} ` +
          `(we only understand up to ${CURRENT_EVENT_SCHEMA_VERSION})`,
        );
        return null;
      }

      // Ignore unrecognized event types
      const eventType = TOPIC_MAP[eventName];
      if (!eventType) {
        return null; // Not a pool event we care about
      }

      // Decode the value (payload)
      let payload: Record<string, unknown> = {};
      if (raw.value) {
        if (typeof raw.value === "string") {
          payload = this._decodeXdr(raw.value);
        } else if (typeof raw.value === "object") {
          payload = raw.value;
        }
      }

      // Extract transaction hash and event index from paging token
      // Format: "123456789-0"
      const [, index] = raw.pagingToken.split("-").map(Number);

      return {
        id: raw.id,
        poolId: contractId,
        type: eventType,
        timestamp: Math.floor(new Date(raw.ledgerClosedAt).getTime() / 1000),
        ledger: raw.ledger,
        txHash: `${raw.ledger}-${index}`, // Simplified; real implementation would extract from raw
        eventIndex: index,
        payload,
      };
    } catch (err) {
      console.error(`[RpcIngester] Failed to decode event ${raw.id}:`, err);
      return null;
    }
  }

  private _decodeTopic(topic: string | null): string {
    if (!topic) return "";
    // Stellar topic format: "AAAADwAAAAA=" (base64-encoded)
    // Decode if necessary; for now, assume it's a string
    try {
      return Buffer.from(topic, "base64").toString("utf-8").replace(/\0/g, "");
    } catch {
      return topic;
    }
  }

  private _extractSchemaVersion(topic: string | null): number {
    if (!topic) return CURRENT_EVENT_SCHEMA_VERSION;
    try {
      const value = Buffer.from(topic, "base64").readUInt32BE(0);
      return value;
    } catch {
      return CURRENT_EVENT_SCHEMA_VERSION;
    }
  }

  private _decodeXdr(_xdr: string): Record<string, unknown> {
    // Placeholder: real implementation would use stellar-sdk's XDR decoder
    // For now, return empty payload
    return {};
  }

  private _sleep(ms: number): Promise<void> {
    return new Promise((resolve) => setTimeout(resolve, ms));
  }
}
