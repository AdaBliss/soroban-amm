/**
 * Abstract store interface for analytics data persistence.
 *
 * The indexer depends on this interface, not on concrete storage implementations.
 * This allows tests to use the fast in-memory store, while production deployments
 * can use a persistent store (SQLite) that survives restarts.
 */

import type { Seconds } from "../time.js";

export interface PoolEvent {
  id: string;
  poolId: string;
  type: PoolEventType;
  /** Seconds since the Unix epoch. */
  timestamp: Seconds;
  ledger: number;
  txHash: string;
  eventIndex: number;
  payload: Record<string, unknown>;
}

export type PoolEventType =
  | "swap"
  | "add_liquidity"
  | "remove_liquidity"
  | "campaign_created"
  | "reward_distributed"
  | "fot_detected"
  | "price_upd"
  | "tick_crossed";

export interface PoolStats {
  poolId: string;
  tokenA: string;
  tokenB: string;
  tvl: number;
  volume24h: number;
  fees24h: number;
  swapCount: number;
  priceDeviationBps: number;
  /** Seconds since the Unix epoch. */
  lastUpdate: Seconds;
}

export interface Position {
  id: string;
  poolId: string;
  owner: string;
  shares: number;
  valueUsd: number;
}

export interface PricePoint {
  poolId: string;
  /** Seconds since the Unix epoch. */
  timestamp: Seconds;
  price: number;
  feeBps: number;
}

export interface HealthAlert {
  poolId: string;
  metric: string;
  threshold: number;
  currentValue: number;
  /** Seconds since the Unix epoch. */
  firedAt: Seconds;
}

export interface AlertConfig {
  poolId: string;
  metric: string;
  thresholdBps: number;
}

/** Ingestion cursor tracking progress through RPC events. */
export interface IngestionCursor {
  /**
   * Contract this cursor tracks progress for. A single ingester polls
   * multiple contracts, each paginating independently, so one cursor per
   * contract is required — otherwise each contract's progress would
   * overwrite the others'.
   */
  contractId: string;
  /**
   * Last ledger sequence number successfully ingested.
   * Used to resume from where we left off.
   */
  ledger: number;
  /**
   * Last transaction hash processed at this ledger.
   * Used for idempotency (events keyed by (ledger, txHash, eventIndex)).
   */
  txHash: string;
  /**
   * Last event index processed in this transaction.
   * Used for idempotency.
   */
  eventIndex: number;
  /** Seconds since the Unix epoch of the last update, for monitoring lag. */
  updatedAt: Seconds;
}

/**
 * Abstract store interface for all analytics data.
 *
 * Implementations must support both in-memory (for tests) and persistent
 * (SQLite, for production) backends transparently.
 */
export interface AnalyticsStore {
  // ─── Event storage ───────────────────────────────────────────────────────
  
  /**
   * Append an event to the store.
   * Must be idempotent: appending the same event twice by (ledger, txHash, eventIndex)
   * does not create a duplicate.
   */
  appendEvent(event: PoolEvent): Promise<void>;

  /**
   * Query events by pool and time range.
   */
  queryEvents(poolId: string, from: Seconds, to: Seconds, limit?: number): Promise<PoolEvent[]>;

  /**
   * Get the most recent events across all pools.
   */
  getRecentEvents(limit: number): Promise<PoolEvent[]>;

  // ─── Pool stats ──────────────────────────────────────────────────────────

  /**
   * Upsert pool statistics.
   * Creates if not exists, updates if already present.
   */
  upsertPoolStats(stats: PoolStats): Promise<void>;

  /**
   * Get pool statistics.
   */
  getPoolStats(poolId?: string): Promise<PoolStats[]>;

  // ─── Positions ───────────────────────────────────────────────────────────

  /**
   * Upsert a liquidity position.
   */
  upsertPosition(position: Position): Promise<void>;

  /**
   * Query positions by owner.
   */
  getPositions(owner?: string): Promise<Position[]>;

  // ─── Price history ──────────────────────────────────────────────────────

  /**
   * Record a price point.
   */
  recordPrice(point: PricePoint): Promise<void>;

  /**
   * Query price history for a pool over a time range.
   */
  getPriceHistory(poolId: string, from: Seconds, to: Seconds): Promise<PricePoint[]>;

  // ─── Alerts ─────────────────────────────────────────────────────────────

  /**
   * Set or update an alert configuration.
   */
  setAlertConfig(config: AlertConfig): Promise<AlertConfig>;

  /**
   * Remove an alert configuration.
   */
  removeAlertConfig(poolId: string, metric: string): Promise<boolean>;

  /**
   * Get alert configurations.
   */
  getAlertConfigs(poolId?: string): Promise<AlertConfig[]>;

  /**
   * Record a fired alert.
   */
  recordAlert(alert: HealthAlert): Promise<void>;

  /**
   * Get fired alerts.
   */
  getFiredAlerts(poolId?: string, limit?: number): Promise<HealthAlert[]>;

  // ─── Ingestion cursor ────────────────────────────────────────────────────

  /**
   * Get the current ingestion cursor for a contract (progress through RPC
   * events). Returns null if that contract has never been ingested.
   */
  getCursor(contractId: string): Promise<IngestionCursor | null>;

  /**
   * Update a contract's ingestion cursor (called after successfully
   * processing its events).
   */
  setCursor(cursor: IngestionCursor): Promise<void>;

  // ─── Lifecycle ───────────────────────────────────────────────────────────

  /**
   * Close the store and release any resources (database connections, etc.).
   * Called during graceful shutdown.
   */
  close(): Promise<void>;
}
