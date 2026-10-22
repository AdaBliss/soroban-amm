import { asSeconds, systemClock, type Clock, type Seconds } from "./time.js";
import type {
  AlertConfig as StoredAlertConfig,
  AnalyticsStore,
  HealthAlert,
  PoolEvent as StoredPoolEvent,
  PoolEventType,
  PoolStats,
  Position,
  PricePoint,
} from "./store/interface.js";

export const RETENTION_SECONDS = 30 * 24 * 60 * 60;
const ROLLING_WINDOW_SECONDS = 24 * 60 * 60;
const PRICE_DEVIATION_WINDOW_SECONDS = 60 * 60;
const DEFAULT_FEE_BPS = 30;

export type PoolEvent = Omit<StoredPoolEvent, "ledger" | "txHash" | "eventIndex"> &
  Partial<Pick<StoredPoolEvent, "ledger" | "txHash" | "eventIndex">>;
export type { PoolEventType, PoolStats, Position, PricePoint, HealthAlert };

export interface PoolConfig {
  tokenA: string;
  tokenB: string;
  feeBps?: number;
  tokenPrices?: Record<string, number>;
}

export const VALID_ALERT_METRICS = ["price_deviation", "tvl", "volume24h"] as const;
export type AlertMetric = (typeof VALID_ALERT_METRICS)[number];

export interface AlertConfig {
  poolId: string;
  metric: AlertMetric;
  thresholdBps?: number;
  thresholdValue?: number;
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

export interface IndexUpdate {
  inserted: boolean;
  stats?: PoolStats;
  price?: PricePoint;
  positions: Position[];
  alerts: HealthAlert[];
}

interface RollingAmount {
  timestamp: Seconds;
  volume: number;
  fee: number;
}

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

function toStr(value: unknown): string {
  if (value === null || value === undefined) return "";
  if (typeof value === "string") return value;
  if (typeof value === "number" || typeof value === "bigint" || typeof value === "boolean") {
    return value.toString();
  }
  return (value as { toString(): string }).toString();
}

function toNumber(value: unknown): number {
  const parsed = Number(value ?? 0);
  return Number.isFinite(parsed) ? parsed : 0;
}

export class PoolIndexer {
  private stats = new Map<string, PoolStats>();
  private events: PoolEvent[] = [];
  private eventKeys = new Set<string>();
  private positions = new Map<string, Position>();
  private priceHistory: PricePoint[] = [];
  private rolling = new Map<string, RollingAmount[]>();
  private alertConfigs = new Map<string, AlertConfig>();
  private firedAlerts: HealthAlert[] = [];

  constructor(
    private readonly clock: Clock = systemClock,
    private readonly poolConfigs: Record<string, PoolConfig> = {},
  ) {}

  async rebuild(store: AnalyticsStore): Promise<void> {
    this.resetComputedState();
    for (const config of await store.getAlertConfigs()) {
      this.setAlertConfig(config as AlertConfig);
    }
    this.firedAlerts = await store.getFiredAlerts(undefined, 1000);
    const events = await store.getEventsSince(asSeconds(this.clock() - RETENTION_SECONDS));
    for (const event of events) this.indexEvent(event, false);
  }

  indexEvent(event: PoolEvent, emitAlerts = true): IndexUpdate {
    const key = this.eventKey(event);
    if (this.eventKeys.has(key)) return { inserted: false, positions: [], alerts: [] };
    this.eventKeys.add(key);
    this.events.push(event);
    this.pruneOldData();
    if (!this.isAnalyticsEvent(event.type)) return { inserted: true, positions: [], alerts: [] };

    const config = this.poolConfigs[event.poolId];
    const tokenIn = toStr(event.payload["token_in"] ?? event.payload["tokenA"]);
    const tokenOut = toStr(event.payload["token_out"] ?? event.payload["tokenB"]);
    const existing = this.stats.get(event.poolId);
    const tokenA = config?.tokenA ?? existing?.tokenA ?? tokenIn;
    const tokenB = config?.tokenB ?? existing?.tokenB ?? tokenOut;
    const feeBps = toNumber(event.payload["feeBps"] ?? config?.feeBps ?? DEFAULT_FEE_BPS);
    const feeBpsSource = (event.payload["feeBpsSource"] ?? (config?.feeBps === undefined ? "default" : "configured")) as "configured" | "default";
    const stats: PoolStats = existing ?? {
      poolId: event.poolId, tokenA, tokenB, reserveA: 0, reserveB: 0, tvl: null,
      volume24h: 0, fees24h: 0, feeBps, feeBpsSource, swapCount: 0,
      priceDeviationBps: 0, lastUpdate: event.timestamp,
    };
    stats.feeBps = feeBps;
    stats.feeBpsSource = feeBpsSource;
    stats.lastUpdate = event.timestamp;

    let price: PricePoint | undefined;
    const changedPositions: Position[] = [];
    if (event.type === "swap" || event.type === "swap_out") {
      const amountIn = toNumber(event.payload["amountIn"]);
      const amountOut = toNumber(event.payload["amountOut"]);
      const aToB = tokenIn === stats.tokenA && tokenOut === stats.tokenB;
      const bToA = tokenIn === stats.tokenB && tokenOut === stats.tokenA;
      if (aToB) {
        stats.reserveA += amountIn;
        stats.reserveB = Math.max(0, stats.reserveB - amountOut);
      } else if (bToA) {
        stats.reserveB += amountIn;
        stats.reserveA = Math.max(0, stats.reserveA - amountOut);
      }
      const orientedPrice = aToB && amountIn > 0 ? amountOut / amountIn
        : bToA && amountOut > 0 ? amountIn / amountOut : 0;
      const quoteVolume = aToB ? amountOut : bToA ? amountIn : 0;
      const feeInInput = toNumber(event.payload["fee"]);
      const feeInQuote = bToA ? feeInInput : aToB && orientedPrice > 0 ? feeInInput * orientedPrice : 0;
      this.rollingEntry(event.poolId, { timestamp: event.timestamp, volume: quoteVolume, fee: feeInQuote });
      stats.swapCount += 1;
      if (orientedPrice > 0) {
        price = { poolId: event.poolId, timestamp: event.timestamp, price: orientedPrice, feeBps, feeBpsSource };
        this.recordPrice(price);
        stats.priceDeviationBps = this.computePriceDeviation(event.poolId, orientedPrice);
      }
    }

    if (event.type === "add_liquidity" || event.type === "mint_pos") {
      stats.reserveA += toNumber(event.payload["amountA"] ?? event.payload["amount_a"]);
      stats.reserveB += toNumber(event.payload["amountB"] ?? event.payload["amount_b"]);
      const position = this.updatePosition(event, 1);
      if (position) changedPositions.push(position);
    }
    if (event.type === "remove_liquidity" || event.type === "burn_pos") {
      stats.reserveA = Math.max(0, stats.reserveA - toNumber(event.payload["amountA"] ?? event.payload["amount_a"]));
      stats.reserveB = Math.max(0, stats.reserveB - toNumber(event.payload["amountB"] ?? event.payload["amount_b"]));
      const position = this.updatePosition(event, -1);
      if (position) changedPositions.push(position);
    }

    this.applyRollingWindow(event.poolId, stats);
    stats.tvl = this.computeTvl(stats, config?.tokenPrices);
    this.stats.set(event.poolId, stats);
    this.refreshPositionValues(event.poolId, stats.tvl);
    const alerts = emitAlerts ? this.checkAlerts(event.poolId, stats) : [];
    return { inserted: true, stats: { ...stats }, price, positions: changedPositions, alerts };
  }

  getPoolStats(poolId?: string): PoolStats[] {
    const all = [...this.stats.values()].map((stats) => {
      this.applyRollingWindow(stats.poolId, stats);
      return { ...stats };
    });
    return poolId ? all.filter((stats) => stats.poolId === poolId) : all;
  }

  getEvents(poolId?: string, limit = 100): PoolEvent[] {
    const filtered = poolId ? this.events.filter((event) => event.poolId === poolId) : this.events;
    return filtered.slice(-limit).reverse();
  }

  getPositions(owner?: string): Position[] {
    const all = [...this.positions.values()];
    return owner ? all.filter((position) => position.owner === owner) : all;
  }

  upsertPosition(position: Position): void { this.positions.set(position.id, position); }
  recordPrice(point: PricePoint): void {
    this.priceHistory.push({ ...point, feeBpsSource: point.feeBpsSource ?? "default" });
  }

  getPriceHistory(poolId: string, from?: Seconds, to?: Seconds): PricePoint[] {
    return this.priceHistory.filter((point) => point.poolId === poolId &&
      (from === undefined || point.timestamp >= from) &&
      (to === undefined || point.timestamp <= to));
  }

  getTwal(poolId: string, windowSeconds: number): number | null {
    const now = this.clock();
    const points = this.getPriceHistory(poolId, asSeconds(now - windowSeconds), now).sort((a, b) => a.timestamp - b.timestamp);
    if (points.length === 0) return null;
    let weightedSum = 0;
    let totalWeight = 0;
    for (let index = 0; index < points.length; index += 1) {
      const next = index + 1 < points.length ? points[index + 1].timestamp : now;
      const weight = Math.max(0, next - points[index].timestamp);
      weightedSum += points[index].price * weight;
      totalWeight += weight;
    }
    return totalWeight === 0 ? points[points.length - 1].price : weightedSum / totalWeight;
  }

  getPoolHealth(poolId: string): PoolHealth | null {
    const stats = this.getPoolStats(poolId)[0];
    if (!stats) return null;
    const tvl = stats.tvl ?? 0;
    const tvlScore = Math.min(100, (tvl / 1_000_000) * 100);
    const volumeScore = Math.min(100, (tvl > 0 ? stats.volume24h / tvl : 0) * 500);
    const feeEfficiencyScore = Math.min(100, (tvl > 0 ? stats.fees24h / tvl : 0) * 100_000);
    const deviationPenalty = Math.min(100, stats.priceDeviationBps / 10);
    const healthScore = Math.max(0, tvlScore * 0.4 + volumeScore * 0.35 + feeEfficiencyScore * 0.25 - deviationPenalty * 0.5);
    return {
      poolId, healthScore: Math.round(healthScore * 10) / 10,
      tvlScore: Math.round(tvlScore * 10) / 10,
      volumeScore: Math.round(volumeScore * 10) / 10,
      feeEfficiencyScore: Math.round(feeEfficiencyScore * 10) / 10,
      priceDeviationBps: stats.priceDeviationBps,
      status: healthScore >= 70 ? "healthy" : healthScore >= 40 ? "warning" : "critical",
      alertsFired: this.firedAlerts.filter((alert) => alert.poolId === poolId),
    };
  }

  setAlertConfig(config: AlertConfig): AlertConfig {
    if (!isValidMetric(config.metric)) throw new InvalidMetricError(config.metric);
    const threshold = config.metric === "price_deviation" ? config.thresholdBps : config.thresholdValue;
    if (threshold === undefined || !Number.isFinite(threshold)) {
      throw new InvalidThresholdError(config.metric === "price_deviation"
        ? "thresholdBps is required for metric \"price_deviation\""
        : `thresholdValue is required for metric "${config.metric}"`);
    }
    if (threshold < 0) throw new InvalidThresholdError("Threshold must be >= 0");
    this.alertConfigs.set(`${config.poolId}:${config.metric}`, config);
    return config;
  }

  removeAlertConfig(poolId: string, metric: string): boolean {
    return this.alertConfigs.delete(`${poolId}:${metric}`);
  }

  getAlertConfigs(poolId?: string): AlertConfig[] {
    const all = [...this.alertConfigs.values()];
    return poolId ? all.filter((config) => config.poolId === poolId) : all;
  }

  private eventKey(event: PoolEvent): string {
    return event.ledger !== undefined && event.txHash !== undefined && event.eventIndex !== undefined
      ? `${event.ledger}:${event.txHash}:${event.eventIndex}` : event.id;
  }

  private isAnalyticsEvent(type: PoolEventType): boolean {
    return ["swap", "swap_out", "add_liquidity", "remove_liquidity", "mint_pos", "burn_pos"].includes(type);
  }

  private rollingEntry(poolId: string, entry: RollingAmount): void {
    const entries = this.rolling.get(poolId) ?? [];
    entries.push(entry);
    this.rolling.set(poolId, entries);
  }

  private applyRollingWindow(poolId: string, stats: PoolStats): void {
    const cutoff = this.clock() - ROLLING_WINDOW_SECONDS;
    const entries = (this.rolling.get(poolId) ?? []).filter((entry) => entry.timestamp >= cutoff);
    this.rolling.set(poolId, entries);
    stats.volume24h = entries.reduce((sum, entry) => sum + entry.volume, 0);
    stats.fees24h = entries.reduce((sum, entry) => sum + entry.fee, 0);
  }

  private computeTvl(stats: PoolStats, prices?: Record<string, number>): number | null {
    const priceA = prices?.[stats.tokenA];
    const priceB = prices?.[stats.tokenB];
    return priceA === undefined || priceB === undefined ? null : stats.reserveA * priceA + stats.reserveB * priceB;
  }

  private updatePosition(event: PoolEvent, direction: 1 | -1): Position | null {
    const owner = toStr(event.payload["provider"] ?? event.payload["owner"]);
    if (!owner) return null;
    const lowerTick = event.payload["lowerTick"] ?? event.payload["tick_lower"];
    const upperTick = event.payload["upperTick"] ?? event.payload["tick_upper"];
    const suffix = lowerTick === undefined ? "lp" : `${toNumber(lowerTick)}:${toNumber(upperTick)}`;
    const id = `${event.poolId}:${owner}:${suffix}`;
    const current = this.positions.get(id);
    const shares = Math.max(0, (current?.shares ?? 0) + direction * toNumber(event.payload["shares"] ?? event.payload["liquidity"]));
    const position: Position = {
      id, poolId: event.poolId, owner, shares, valueUsd: current?.valueUsd ?? null,
      lowerTick: lowerTick === undefined ? undefined : toNumber(lowerTick),
      upperTick: upperTick === undefined ? undefined : toNumber(upperTick),
    };
    this.positions.set(id, position);
    return position;
  }

  private refreshPositionValues(poolId: string, tvl: number | null): void {
    const positions = [...this.positions.values()].filter((position) => position.poolId === poolId);
    const totalShares = positions.reduce((sum, position) => sum + position.shares, 0);
    for (const position of positions) {
      position.valueUsd = tvl === null || totalShares === 0 ? null : tvl * position.shares / totalShares;
    }
  }

  private computePriceDeviation(poolId: string, currentPrice: number): number {
    const history = this.getPriceHistory(poolId, asSeconds(this.clock() - PRICE_DEVIATION_WINDOW_SECONDS));
    if (history.length < 2) return 0;
    const average = history.reduce((sum, point) => sum + point.price, 0) / history.length;
    return average === 0 ? 0 : Math.round(Math.abs(currentPrice - average) / average * 10_000);
  }

  private checkAlerts(poolId: string, stats: PoolStats): HealthAlert[] {
    const fired: HealthAlert[] = [];
    for (const config of this.getAlertConfigs(poolId)) {
      const currentValue = config.metric === "price_deviation" ? stats.priceDeviationBps
        : config.metric === "tvl" ? stats.tvl : stats.volume24h;
      const threshold = config.metric === "price_deviation" ? config.thresholdBps! : config.thresholdValue!;
      if (currentValue !== null && currentValue > threshold) {
        const alert = { poolId, metric: config.metric, threshold, currentValue, firedAt: this.clock() };
        fired.push(alert);
        this.firedAlerts.push(alert);
      }
    }
    this.firedAlerts = this.firedAlerts.slice(-1000);
    return fired;
  }

  private pruneOldData(): void {
    const cutoff = this.clock() - RETENTION_SECONDS;
    this.events = this.events.filter((event) => event.timestamp >= cutoff);
    this.priceHistory = this.priceHistory.filter((point) => point.timestamp >= cutoff);
    this.firedAlerts = this.firedAlerts.filter((alert) => alert.firedAt >= cutoff);
  }

  private resetComputedState(): void {
    this.stats.clear();
    this.events = [];
    this.eventKeys.clear();
    this.positions.clear();
    this.priceHistory = [];
    this.rolling.clear();
    this.alertConfigs.clear();
    this.firedAlerts = [];
  }
}

export const defaultIndexer = new PoolIndexer();

export function seedDemoData(indexer: PoolIndexer): void {
  if (process.env.SEED_DEMO_DATA !== "true" || process.env.NODE_ENV === "production") return;
  const now = systemClock();
  indexer.indexEvent({
    id: "evt-1", poolId: "pool-demo", type: "swap", timestamp: asSeconds(now - 60),
    payload: { amountIn: 1000, amountOut: 120, fee: 3, token_in: "XLM", token_out: "USDC" },
  });
  indexer.indexEvent({
    id: "evt-2", poolId: "pool-demo", type: "add_liquidity", timestamp: asSeconds(now - 120),
    payload: { amountA: 500_000, amountB: 60_000, provider: "G...DEMO", shares: 5000, tokenA: "XLM", tokenB: "USDC" },
  });
}

export function storedAlertConfig(config: AlertConfig): StoredAlertConfig { return config; }
