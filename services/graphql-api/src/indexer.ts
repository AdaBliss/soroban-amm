import { asSeconds, systemClock, type Clock, type Seconds } from "./time.js";

/**
 * In-memory indexer for pool analytics. Production deployments should
 * back this with Horizon/Soroban RPC event ingestion.
 *
 * All timestamps are seconds since the Unix epoch (see `time.ts`).
 *
 * Historical data is retained for 30 days (RETENTION_SECONDS), measured
 * against the indexer's clock (wall-clock time in production). Older price
 * history and events are pruned on each `indexEvent` call.
 */

export const RETENTION_SECONDS = 30 * 24 * 60 * 60; // 30 days
const PRICE_DEVIATION_WINDOW_SECONDS = 60 * 60; // 1 hour

// Kept a superset of store/interface.ts's PoolEventType/PoolEvent (rather
// than importing those directly) so this module's own narrower PoolEvent —
// which only requires id/poolId/type/timestamp/payload, not the store's
// ledger/txHash/eventIndex — keeps working as the minimal shape indexEvent
// actually needs. A RpcIngester-sourced PoolEvent (the store's richer type)
// is still assignable here; TypeScript's structural typing allows an object
// with extra fields to satisfy a type that requires fewer of them.
export type PoolEventType =
  | "swap"
  | "add_liquidity"
  | "remove_liquidity"
  | "campaign_created"
  | "reward_distributed"
  | "fot_detected"
  | "price_upd"
  | "tick_crossed";

export interface PoolEvent {
  id: string;
  poolId: string;
  type: PoolEventType;
  /** Seconds since the Unix epoch. */
  timestamp: Seconds;
  payload: Record<string, unknown>;
}

export interface PoolStats {
  poolId: string;
  tokenA: string;
  tokenB: string;
  tvl: number;
  volume24h: number;
  fees24h: number;
  swapCount: number;
  priceDeviationBps: number;
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

export interface PoolHealth {
  poolId: string;
  healthScore: number;
  tvlScore: number;
  volumeScore: number;
  feeEfficiencyScore: number;
  priceDeviationBps: number;
  status: "healthy" | "warning" | "critical";
  alertsFired: HealthAlert[];
}

export const VALID_ALERT_METRICS = ["price_deviation", "tvl", "volume24h"] as const;
export type AlertMetric = (typeof VALID_ALERT_METRICS)[number];

export class InvalidMetricError extends Error {
  constructor(metric: string) {
    super(`Invalid alert metric "${metric}". Must be one of: ${VALID_ALERT_METRICS.join(", ")}`);
    this.name = "InvalidMetricError";
  }
}

export class InvalidThresholdError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "InvalidThresholdError";
  }
}

function isValidMetric(metric: string): metric is AlertMetric {
  return (VALID_ALERT_METRICS as readonly string[]).includes(metric);
}

/** Stringifies a payload field without risking a plain object's default
 * `[object Object]` stringification — payload values are `unknown` since
 * they come straight off the wire. */
function toStr(val: unknown): string {
  if (val === null || val === undefined) return "";
  if (typeof val === "string") return val;
  if (typeof val === "number" || typeof val === "bigint" || typeof val === "boolean") {
    return val.toString();
  }
  return (val as { toString(): string }).toString();
}

export interface AlertConfig {
  poolId: string;
  metric: AlertMetric;
  /** Basis-points threshold. Required (and only meaningful) for "price_deviation". */
  thresholdBps?: number;
  /** Raw-value threshold in the metric's native units. Required for "tvl" and "volume24h". */
  thresholdValue?: number;
}

export class PoolIndexer {
  private stats = new Map<string, PoolStats>();
  private events: PoolEvent[] = [];
  private positions = new Map<string, Position>();
  private priceHistory: PricePoint[] = [];
  private alertConfigs = new Map<string, AlertConfig>();
  private firedAlerts: HealthAlert[] = [];

  constructor(private readonly clock: Clock = systemClock) {}

  // ── Event ingestion ─────────────────────────────────────────────────────────

  indexEvent(event: PoolEvent): void {
    this.events.push(event);
    this.pruneOldData();

    const stats = this.stats.get(event.poolId) ?? {
      poolId: event.poolId,
      tokenA: toStr(event.payload["tokenA"] ?? ""),
      tokenB: toStr(event.payload["tokenB"] ?? ""),
      tvl: 0,
      volume24h: 0,
      fees24h: 0,
      swapCount: 0,
      priceDeviationBps: 0,
    };

    if (event.type === "swap") {
      stats.swapCount += 1;
      stats.volume24h += Number(event.payload["amountIn"] ?? 0);
      stats.fees24h += Number(event.payload["fee"] ?? 0);
      const price = Number(event.payload["price"] ?? 0);
      if (price > 0) {
        this.recordPrice({ poolId: event.poolId, timestamp: event.timestamp, price, feeBps: 30 });
        stats.priceDeviationBps = this.computePriceDeviation(event.poolId, price);
      }
    }
    if (event.type === "add_liquidity") {
      stats.tvl += Number(event.payload["amountA"] ?? 0) + Number(event.payload["amountB"] ?? 0);
    }
    if (event.type === "remove_liquidity") {
      stats.tvl -= Number(event.payload["amountA"] ?? 0) + Number(event.payload["amountB"] ?? 0);
      stats.tvl = Math.max(0, stats.tvl);
    }

    this.stats.set(event.poolId, stats);
    this.checkAlerts(event.poolId, stats);
  }

  // ── Queries ─────────────────────────────────────────────────────────────────

  getPoolStats(poolId?: string): PoolStats[] {
    const all = [...this.stats.values()];
    return poolId ? all.filter((s) => s.poolId === poolId) : all;
  }

  getEvents(poolId?: string, limit = 100): PoolEvent[] {
    const filtered = poolId
      ? this.events.filter((e) => e.poolId === poolId)
      : this.events;
    return filtered.slice(-limit).reverse();
  }

  getPositions(owner?: string): Position[] {
    const all = [...this.positions.values()];
    return owner ? all.filter((p) => p.owner === owner) : all;
  }

  upsertPosition(position: Position): void {
    this.positions.set(position.id, position);
  }

  recordPrice(point: PricePoint): void {
    this.priceHistory.push(point);
  }

  getPriceHistory(poolId: string, from?: Seconds, to?: Seconds): PricePoint[] {
    return this.priceHistory.filter((p) => {
      if (p.poolId !== poolId) return false;
      if (from !== undefined && p.timestamp < from) return false;
      if (to !== undefined && p.timestamp > to) return false;
      return true;
    });
  }

  /**
   * Time-weighted average liquidity (price) over the trailing `windowSeconds`.
   * Each price point is weighted by the duration it was in effect: the time
   * until the next point, or until now for the most recent point.
   */
  getTwal(poolId: string, windowSeconds: number): number | null {
    const now = this.clock();
    const from = asSeconds(now - windowSeconds);
    const points = this.getPriceHistory(poolId, from, now).sort(
      (a, b) => a.timestamp - b.timestamp,
    );
    if (points.length === 0) return null;

    let weightedSum = 0;
    let totalWeight = 0;
    for (let i = 0; i < points.length; i++) {
      const point = points[i];
      const nextTimestamp = i + 1 < points.length ? points[i + 1].timestamp : now;
      const weight = Math.max(0, nextTimestamp - point.timestamp);
      weightedSum += point.price * weight;
      totalWeight += weight;
    }

    if (totalWeight === 0) return points[points.length - 1].price;
    return weightedSum / totalWeight;
  }

  // ── Health scoring ──────────────────────────────────────────────────────────

  getPoolHealth(poolId: string): PoolHealth | null {
    const stats = this.stats.get(poolId);
    if (!stats) return null;

    // TVL score: 0-100. Pools with TVL > 1M score near 100.
    const tvlScore = Math.min(100, (stats.tvl / 1_000_000) * 100);

    // Volume score: volume/TVL ratio. Healthy is 5–20% daily turnover.
    const volumeRatio = stats.tvl > 0 ? stats.volume24h / stats.tvl : 0;
    const volumeScore = Math.min(100, volumeRatio * 500); // 20% ratio = 100 pts

    // Fee efficiency: fees relative to TVL. Healthy is 0.01–0.1% daily.
    const feeRatio = stats.tvl > 0 ? stats.fees24h / stats.tvl : 0;
    const feeEfficiencyScore = Math.min(100, feeRatio * 100_000); // 0.1% = 100 pts

    // Price deviation penalty: high deviation reduces score.
    const deviationPenalty = Math.min(100, stats.priceDeviationBps / 10);

    const healthScore = Math.max(
      0,
      (tvlScore * 0.4 + volumeScore * 0.35 + feeEfficiencyScore * 0.25) -
        deviationPenalty * 0.5,
    );

    const status: "healthy" | "warning" | "critical" =
      healthScore >= 70
        ? "healthy"
        : healthScore >= 40
          ? "warning"
          : "critical";

    const alertsFired = this.firedAlerts.filter((a) => a.poolId === poolId);

    return {
      poolId,
      healthScore: Math.round(healthScore * 10) / 10,
      tvlScore: Math.round(tvlScore * 10) / 10,
      volumeScore: Math.round(volumeScore * 10) / 10,
      feeEfficiencyScore: Math.round(feeEfficiencyScore * 10) / 10,
      priceDeviationBps: stats.priceDeviationBps,
      status,
      alertsFired,
    };
  }

  // ── Alert configuration ─────────────────────────────────────────────────────

  setAlertConfig(config: AlertConfig): AlertConfig {
    if (!isValidMetric(config.metric)) {
      throw new InvalidMetricError(config.metric);
    }
    const threshold = config.metric === "price_deviation" ? config.thresholdBps : config.thresholdValue;
    if (threshold === undefined || !Number.isFinite(threshold)) {
      throw new InvalidThresholdError(
        config.metric === "price_deviation"
          ? "thresholdBps is required for metric \"price_deviation\""
          : `thresholdValue is required for metric "${config.metric}"`,
      );
    }
    if (threshold < 0) {
      throw new InvalidThresholdError("Threshold must be >= 0");
    }

    const key = `${config.poolId}:${config.metric}`;
    this.alertConfigs.set(key, config);
    return config;
  }

  removeAlertConfig(poolId: string, metric: string): boolean {
    const key = `${poolId}:${metric}`;
    return this.alertConfigs.delete(key);
  }

  getAlertConfigs(poolId?: string): AlertConfig[] {
    const all = [...this.alertConfigs.values()];
    return poolId ? all.filter((c) => c.poolId === poolId) : all;
  }

  // ── Internals ────────────────────────────────────────────────────────────────

  private computePriceDeviation(poolId: string, currentPrice: number): number {
    const history = this.getPriceHistory(
      poolId,
      asSeconds(this.clock() - PRICE_DEVIATION_WINDOW_SECONDS),
    );
    if (history.length < 2) return 0;
    const twap =
      history.reduce((sum, p) => sum + p.price, 0) / history.length;
    if (twap === 0) return 0;
    return Math.round(Math.abs(currentPrice - twap) / twap * 10_000);
  }

  private checkAlerts(poolId: string, stats: PoolStats): void {
    const configs = this.getAlertConfigs(poolId);
    for (const cfg of configs) {
      // Metrics are validated (allow-listed) at write time in setAlertConfig,
      // so every branch here is reachable and there is no silent fallthrough.
      let currentValue: number;
      let threshold: number;
      if (cfg.metric === "price_deviation") {
        currentValue = stats.priceDeviationBps;
        threshold = cfg.thresholdBps!;
      } else if (cfg.metric === "tvl") {
        currentValue = stats.tvl;
        threshold = cfg.thresholdValue!;
      } else {
        currentValue = stats.volume24h;
        threshold = cfg.thresholdValue!;
      }

      if (currentValue > threshold) {
        const alert: HealthAlert = {
          poolId,
          metric: cfg.metric,
          threshold,
          currentValue,
          firedAt: this.clock(),
        };
        this.firedAlerts.push(alert);
        // Retain only the last 100 fired alerts.
        if (this.firedAlerts.length > 100) {
          this.firedAlerts.shift();
        }
      }
    }
  }

  private pruneOldData(): void {
    const cutoff = this.clock() - RETENTION_SECONDS;
    this.events = this.events.filter((e) => e.timestamp > cutoff);
    this.priceHistory = this.priceHistory.filter((p) => p.timestamp > cutoff);
    this.firedAlerts = this.firedAlerts.filter((a) => a.firedAt > cutoff);
  }
}

export const defaultIndexer = new PoolIndexer();

// Seed demo data for local development
const now = systemClock();
defaultIndexer.indexEvent({
  id: "evt-1",
  poolId: "pool-demo",
  type: "swap",
  timestamp: asSeconds(now - 60),
  payload: { amountIn: 1000, fee: 3, tokenA: "XLM", tokenB: "USDC", price: 0.12 },
});
defaultIndexer.indexEvent({
  id: "evt-2",
  poolId: "pool-demo",
  type: "add_liquidity",
  timestamp: asSeconds(now - 120),
  payload: { amountA: 500_000, amountB: 60_000, tokenA: "XLM", tokenB: "USDC" },
});
defaultIndexer.upsertPosition({
  id: "pos-1",
  poolId: "pool-demo",
  owner: "G...DEMO",
  shares: 5000,
  valueUsd: 12_500,
});
defaultIndexer.recordPrice({
  poolId: "pool-demo",
  timestamp: now,
  price: 0.12,
  feeBps: 30,
});
