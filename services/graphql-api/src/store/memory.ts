/**
 * In-memory analytics store for testing and development.
 *
 * This implementation holds all data in JavaScript Maps and arrays.
 * It is fast but loses all data on process restart. It's ideal for tests
 * and development; production deployments should use SQLiteStore.
 */

import type {
  AnalyticsStore,
  PoolEvent,
  PoolStats,
  Position,
  PricePoint,
  HealthAlert,
  AlertConfig,
  IngestionCursor,
} from "./interface.js";
import type { Seconds } from "../time.js";

export class MemoryStore implements AnalyticsStore {
  private events: PoolEvent[] = [];
  private stats = new Map<string, PoolStats>();
  private positions = new Map<string, Position>();
  private priceHistory: PricePoint[] = [];
  private alertConfigs = new Map<string, AlertConfig>();
  private firedAlerts: HealthAlert[] = [];
  private cursors = new Map<string, IngestionCursor>();

  // Track (ledger, txHash, eventIndex) for idempotency
  private eventKeys = new Set<string>();

  async appendEvent(event: PoolEvent): Promise<boolean> {
    // Idempotency check: skip if event already exists
    const key = `${event.ledger}:${event.txHash}:${event.eventIndex}`;
    if (this.eventKeys.has(key)) {
      return false;
    }

    this.events.push(event);
    this.eventKeys.add(key);
    return true;
  }

  async appendEventAndSetCursor(
    event: PoolEvent,
    cursor: IngestionCursor,
  ): Promise<boolean> {
    const inserted = await this.appendEvent(event);
    await this.setCursor(cursor);
    return inserted;
  }

  async queryEvents(
    poolId: string,
    from: Seconds,
    to: Seconds,
    limit = 1000,
  ): Promise<PoolEvent[]> {
    return this.events
      .filter(
        (e) =>
          e.poolId === poolId &&
          e.timestamp >= from &&
          e.timestamp <= to,
      )
      .slice(-limit);
  }

  async getRecentEvents(limit: number): Promise<PoolEvent[]> {
    return this.events.slice(-limit).reverse();
  }

  async getEventsSince(from: Seconds): Promise<PoolEvent[]> {
    return this.events
      .filter((event) => event.timestamp >= from)
      .sort((a, b) => a.timestamp - b.timestamp || a.eventIndex - b.eventIndex);
  }

  async upsertPoolStats(stats: PoolStats): Promise<void> {
    this.stats.set(stats.poolId, stats);
  }

  async getPoolStats(poolId?: string): Promise<PoolStats[]> {
    const all = [...this.stats.values()];
    return poolId ? all.filter((s) => s.poolId === poolId) : all;
  }

  async upsertPosition(position: Position): Promise<void> {
    this.positions.set(position.id, position);
  }

  async getPositions(owner?: string): Promise<Position[]> {
    const all = [...this.positions.values()];
    return owner ? all.filter((p) => p.owner === owner) : all;
  }

  async recordPrice(point: PricePoint): Promise<void> {
    this.priceHistory.push(point);
  }

  async getPriceHistory(
    poolId: string,
    from: Seconds,
    to: Seconds,
  ): Promise<PricePoint[]> {
    return this.priceHistory.filter(
      (p) =>
        p.poolId === poolId &&
        p.timestamp >= from &&
        p.timestamp <= to,
    );
  }

  async setAlertConfig(config: AlertConfig): Promise<AlertConfig> {
    const key = `${config.poolId}:${config.metric}`;
    this.alertConfigs.set(key, config);
    return config;
  }

  async removeAlertConfig(poolId: string, metric: string): Promise<boolean> {
    const key = `${poolId}:${metric}`;
    return this.alertConfigs.delete(key);
  }

  async getAlertConfigs(poolId?: string): Promise<AlertConfig[]> {
    const all = [...this.alertConfigs.values()];
    return poolId ? all.filter((c) => c.poolId === poolId) : all;
  }

  async recordAlert(alert: HealthAlert): Promise<void> {
    this.firedAlerts.push(alert);
    // Keep only the most recent 1000 alerts
    if (this.firedAlerts.length > 1000) {
      this.firedAlerts = this.firedAlerts.slice(-1000);
    }
  }

  async getFiredAlerts(poolId?: string, limit = 100): Promise<HealthAlert[]> {
    const all = poolId
      ? this.firedAlerts.filter((a) => a.poolId === poolId)
      : this.firedAlerts;
    return all.slice(-limit).reverse();
  }

  async getCursor(contractId: string): Promise<IngestionCursor | null> {
    return this.cursors.get(contractId) ?? null;
  }

  async setCursor(cursor: IngestionCursor): Promise<void> {
    this.cursors.set(cursor.contractId, cursor);
  }

  async prune(before: Seconds): Promise<void> {
    this.events = this.events.filter((event) => event.timestamp >= before);
    this.eventKeys = new Set(
      this.events.map((event) => `${event.ledger}:${event.txHash}:${event.eventIndex}`),
    );
    this.priceHistory = this.priceHistory.filter((point) => point.timestamp >= before);
    this.firedAlerts = this.firedAlerts.filter((alert) => alert.firedAt >= before);
  }

  async close(): Promise<void> {
    // No resources to release for in-memory store
  }
}
