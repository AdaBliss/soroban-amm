import Database from "better-sqlite3";
import type { Seconds } from "../time.js";
import type {
  AlertConfig,
  AnalyticsStore,
  HealthAlert,
  IngestionCursor,
  PoolEvent,
  PoolEventType,
  PoolStats,
  Position,
  PricePoint,
} from "./interface.js";

type SqlRow = Record<string, unknown>;

const MIGRATIONS = [
  `
    CREATE TABLE events (
      id TEXT NOT NULL,
      pool_id TEXT NOT NULL,
      type TEXT NOT NULL,
      timestamp INTEGER NOT NULL,
      ledger INTEGER NOT NULL,
      tx_hash TEXT NOT NULL,
      event_index INTEGER NOT NULL,
      payload TEXT NOT NULL,
      PRIMARY KEY (ledger, tx_hash, event_index)
    );
    CREATE INDEX events_pool_time ON events(pool_id, timestamp);

    CREATE TABLE pool_stats (
      pool_id TEXT PRIMARY KEY,
      token_a TEXT NOT NULL,
      token_b TEXT NOT NULL,
      reserve_a REAL NOT NULL,
      reserve_b REAL NOT NULL,
      tvl REAL,
      volume_24h REAL NOT NULL,
      fees_24h REAL NOT NULL,
      fee_bps INTEGER NOT NULL,
      fee_bps_source TEXT NOT NULL,
      swap_count INTEGER NOT NULL,
      price_deviation_bps INTEGER NOT NULL,
      last_update INTEGER NOT NULL
    );

    CREATE TABLE positions (
      id TEXT PRIMARY KEY,
      pool_id TEXT NOT NULL,
      owner TEXT NOT NULL,
      shares REAL NOT NULL,
      value_usd REAL,
      lower_tick INTEGER,
      upper_tick INTEGER
    );
    CREATE INDEX positions_owner ON positions(owner);

    CREATE TABLE price_history (
      id INTEGER PRIMARY KEY AUTOINCREMENT,
      pool_id TEXT NOT NULL,
      timestamp INTEGER NOT NULL,
      price REAL NOT NULL,
      fee_bps INTEGER NOT NULL,
      fee_bps_source TEXT NOT NULL
    );
    CREATE INDEX prices_pool_time ON price_history(pool_id, timestamp);

    CREATE TABLE alert_configs (
      pool_id TEXT NOT NULL,
      metric TEXT NOT NULL,
      threshold_bps INTEGER,
      threshold_value REAL,
      PRIMARY KEY (pool_id, metric)
    );

    CREATE TABLE fired_alerts (
      id INTEGER PRIMARY KEY AUTOINCREMENT,
      pool_id TEXT NOT NULL,
      metric TEXT NOT NULL,
      threshold REAL NOT NULL,
      current_value REAL NOT NULL,
      fired_at INTEGER NOT NULL
    );
    CREATE INDEX fired_alerts_pool_time ON fired_alerts(pool_id, fired_at);

    CREATE TABLE ingestion_cursors (
      contract_id TEXT PRIMARY KEY,
      ledger INTEGER NOT NULL,
      tx_hash TEXT NOT NULL,
      event_index INTEGER NOT NULL,
      updated_at INTEGER NOT NULL
    );
  `,
] as const;

export class SqliteStore implements AnalyticsStore {
  private readonly db: Database.Database;

  constructor(path = process.env.DATABASE_PATH ?? "./analytics.db") {
    this.db = new Database(path);
    this.db.pragma("journal_mode = WAL");
    this.db.pragma("foreign_keys = ON");
    this.migrate();
  }

  private migrate(): void {
    this.db.exec(`
      CREATE TABLE IF NOT EXISTS schema_migrations (
        version INTEGER PRIMARY KEY,
        applied_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
      )
    `);
    const current = this.db
      .prepare("SELECT COALESCE(MAX(version), 0) AS version FROM schema_migrations")
      .get() as { version: number };
    const apply = this.db.transaction((version: number, sql: string) => {
      this.db.exec(sql);
      this.db.prepare("INSERT INTO schema_migrations(version) VALUES (?)").run(version);
    });
    for (let index = current.version; index < MIGRATIONS.length; index += 1) {
      apply(index + 1, MIGRATIONS[index]);
    }
  }

  private insertEvent(event: PoolEvent): boolean {
    const result = this.db.prepare(`
      INSERT OR IGNORE INTO events
        (id, pool_id, type, timestamp, ledger, tx_hash, event_index, payload)
      VALUES
        (@id, @poolId, @type, @timestamp, @ledger, @txHash, @eventIndex, @payload)
    `).run({ ...event, payload: JSON.stringify(event.payload) });
    return result.changes === 1;
  }

  async appendEvent(event: PoolEvent): Promise<boolean> {
    return this.insertEvent(event);
  }

  async appendEventAndSetCursor(
    event: PoolEvent,
    cursor: IngestionCursor,
  ): Promise<boolean> {
    return this.db.transaction(() => {
      const inserted = this.insertEvent(event);
      this.writeCursor(cursor);
      return inserted;
    })();
  }

  async queryEvents(
    poolId: string,
    from: Seconds,
    to: Seconds,
    limit = 1000,
  ): Promise<PoolEvent[]> {
    const rows = this.db.prepare(`
      SELECT * FROM events
      WHERE pool_id = ? AND timestamp BETWEEN ? AND ?
      ORDER BY timestamp ASC, ledger ASC, event_index ASC
      LIMIT ?
    `).all(poolId, from, to, limit) as SqlRow[];
    return rows.map(toEvent);
  }

  async getRecentEvents(limit: number): Promise<PoolEvent[]> {
    const rows = this.db.prepare(`
      SELECT * FROM events
      ORDER BY timestamp DESC, ledger DESC, event_index DESC
      LIMIT ?
    `).all(limit) as SqlRow[];
    return rows.map(toEvent);
  }

  async getEventsSince(from: Seconds): Promise<PoolEvent[]> {
    const rows = this.db.prepare(`
      SELECT * FROM events WHERE timestamp >= ?
      ORDER BY timestamp ASC, ledger ASC, event_index ASC
    `).all(from) as SqlRow[];
    return rows.map(toEvent);
  }

  async upsertPoolStats(stats: PoolStats): Promise<void> {
    this.db.prepare(`
      INSERT INTO pool_stats VALUES
        (@poolId, @tokenA, @tokenB, @reserveA, @reserveB, @tvl, @volume24h,
         @fees24h, @feeBps, @feeBpsSource, @swapCount, @priceDeviationBps, @lastUpdate)
      ON CONFLICT(pool_id) DO UPDATE SET
        token_a=excluded.token_a, token_b=excluded.token_b,
        reserve_a=excluded.reserve_a, reserve_b=excluded.reserve_b, tvl=excluded.tvl,
        volume_24h=excluded.volume_24h, fees_24h=excluded.fees_24h,
        fee_bps=excluded.fee_bps, fee_bps_source=excluded.fee_bps_source,
        swap_count=excluded.swap_count,
        price_deviation_bps=excluded.price_deviation_bps,
        last_update=excluded.last_update
    `).run(stats);
  }

  async getPoolStats(poolId?: string): Promise<PoolStats[]> {
    const rows = (poolId
      ? this.db.prepare("SELECT * FROM pool_stats WHERE pool_id = ?").all(poolId)
      : this.db.prepare("SELECT * FROM pool_stats ORDER BY pool_id").all()) as SqlRow[];
    return rows.map(toPoolStats);
  }

  async upsertPosition(position: Position): Promise<void> {
    this.db.prepare(`
      INSERT INTO positions
        (id, pool_id, owner, shares, value_usd, lower_tick, upper_tick)
      VALUES (@id, @poolId, @owner, @shares, @valueUsd, @lowerTick, @upperTick)
      ON CONFLICT(id) DO UPDATE SET
        pool_id=excluded.pool_id, owner=excluded.owner, shares=excluded.shares,
        value_usd=excluded.value_usd, lower_tick=excluded.lower_tick,
        upper_tick=excluded.upper_tick
    `).run({
      ...position,
      lowerTick: position.lowerTick ?? null,
      upperTick: position.upperTick ?? null,
    });
  }

  async getPositions(owner?: string): Promise<Position[]> {
    const rows = (owner
      ? this.db.prepare("SELECT * FROM positions WHERE owner = ? ORDER BY id").all(owner)
      : this.db.prepare("SELECT * FROM positions ORDER BY id").all()) as SqlRow[];
    return rows.map(toPosition);
  }

  async recordPrice(point: PricePoint): Promise<void> {
    this.db.prepare(`
      INSERT INTO price_history
        (pool_id, timestamp, price, fee_bps, fee_bps_source)
      VALUES (@poolId, @timestamp, @price, @feeBps, @feeBpsSource)
    `).run({ ...point, feeBpsSource: point.feeBpsSource ?? "default" });
  }

  async getPriceHistory(
    poolId: string,
    from: Seconds,
    to: Seconds,
  ): Promise<PricePoint[]> {
    const rows = this.db.prepare(`
      SELECT * FROM price_history
      WHERE pool_id = ? AND timestamp BETWEEN ? AND ?
      ORDER BY timestamp ASC
    `).all(poolId, from, to) as SqlRow[];
    return rows.map(toPricePoint);
  }

  async setAlertConfig(config: AlertConfig): Promise<AlertConfig> {
    this.db.prepare(`
      INSERT INTO alert_configs
        (pool_id, metric, threshold_bps, threshold_value)
      VALUES (@poolId, @metric, @thresholdBps, @thresholdValue)
      ON CONFLICT(pool_id, metric) DO UPDATE SET
        threshold_bps=excluded.threshold_bps,
        threshold_value=excluded.threshold_value
    `).run({
      ...config,
      thresholdBps: config.thresholdBps ?? null,
      thresholdValue: config.thresholdValue ?? null,
    });
    return config;
  }

  async removeAlertConfig(poolId: string, metric: string): Promise<boolean> {
    return this.db.prepare(
      "DELETE FROM alert_configs WHERE pool_id = ? AND metric = ?",
    ).run(poolId, metric).changes === 1;
  }

  async getAlertConfigs(poolId?: string): Promise<AlertConfig[]> {
    const rows = (poolId
      ? this.db.prepare("SELECT * FROM alert_configs WHERE pool_id = ?").all(poolId)
      : this.db.prepare("SELECT * FROM alert_configs ORDER BY pool_id, metric").all()) as SqlRow[];
    return rows.map(toAlertConfig);
  }

  async recordAlert(alert: HealthAlert): Promise<void> {
    this.db.prepare(`
      INSERT INTO fired_alerts
        (pool_id, metric, threshold, current_value, fired_at)
      VALUES (@poolId, @metric, @threshold, @currentValue, @firedAt)
    `).run(alert);
  }

  async getFiredAlerts(poolId?: string, limit = 100): Promise<HealthAlert[]> {
    const rows = (poolId
      ? this.db.prepare(`
          SELECT * FROM fired_alerts WHERE pool_id = ?
          ORDER BY fired_at DESC, id DESC LIMIT ?
        `).all(poolId, limit)
      : this.db.prepare(`
          SELECT * FROM fired_alerts ORDER BY fired_at DESC, id DESC LIMIT ?
        `).all(limit)) as SqlRow[];
    return rows.map(toHealthAlert);
  }

  async getCursor(contractId: string): Promise<IngestionCursor | null> {
    const row = this.db.prepare(
      "SELECT * FROM ingestion_cursors WHERE contract_id = ?",
    ).get(contractId) as SqlRow | undefined;
    return row ? toCursor(row) : null;
  }

  private writeCursor(cursor: IngestionCursor): void {
    this.db.prepare(`
      INSERT INTO ingestion_cursors
        (contract_id, ledger, tx_hash, event_index, updated_at)
      VALUES (@contractId, @ledger, @txHash, @eventIndex, @updatedAt)
      ON CONFLICT(contract_id) DO UPDATE SET
        ledger=excluded.ledger, tx_hash=excluded.tx_hash,
        event_index=excluded.event_index, updated_at=excluded.updated_at
    `).run(cursor);
  }

  async setCursor(cursor: IngestionCursor): Promise<void> {
    this.writeCursor(cursor);
  }

  async prune(before: Seconds): Promise<void> {
    this.db.transaction(() => {
      this.db.prepare("DELETE FROM events WHERE timestamp < ?").run(before);
      this.db.prepare("DELETE FROM price_history WHERE timestamp < ?").run(before);
      this.db.prepare("DELETE FROM fired_alerts WHERE fired_at < ?").run(before);
    })();
  }

  async close(): Promise<void> {
    this.db.close();
  }
}

function number(row: SqlRow, name: string): number {
  return Number(row[name]);
}

function nullableNumber(row: SqlRow, name: string): number | null {
  return row[name] === null || row[name] === undefined ? null : Number(row[name]);
}

function toEvent(row: SqlRow): PoolEvent {
  return {
    id: String(row["id"]),
    poolId: String(row["pool_id"]),
    type: String(row["type"]) as PoolEventType,
    timestamp: number(row, "timestamp") as Seconds,
    ledger: number(row, "ledger"),
    txHash: String(row["tx_hash"]),
    eventIndex: number(row, "event_index"),
    payload: JSON.parse(String(row["payload"])) as Record<string, unknown>,
  };
}

function toPoolStats(row: SqlRow): PoolStats {
  return {
    poolId: String(row["pool_id"]), tokenA: String(row["token_a"]), tokenB: String(row["token_b"]),
    reserveA: number(row, "reserve_a"), reserveB: number(row, "reserve_b"),
    tvl: nullableNumber(row, "tvl"), volume24h: number(row, "volume_24h"),
    fees24h: number(row, "fees_24h"), feeBps: number(row, "fee_bps"),
    feeBpsSource: String(row["fee_bps_source"]) as "configured" | "default",
    swapCount: number(row, "swap_count"),
    priceDeviationBps: number(row, "price_deviation_bps"),
    lastUpdate: number(row, "last_update") as Seconds,
  };
}

function toPosition(row: SqlRow): Position {
  return {
    id: String(row["id"]), poolId: String(row["pool_id"]), owner: String(row["owner"]),
    shares: number(row, "shares"), valueUsd: nullableNumber(row, "value_usd"),
    lowerTick: nullableNumber(row, "lower_tick") ?? undefined,
    upperTick: nullableNumber(row, "upper_tick") ?? undefined,
  };
}

function toPricePoint(row: SqlRow): PricePoint {
  return {
    poolId: String(row["pool_id"]), timestamp: number(row, "timestamp") as Seconds,
    price: number(row, "price"), feeBps: number(row, "fee_bps"),
    feeBpsSource: String(row["fee_bps_source"]) as "configured" | "default",
  };
}

function toAlertConfig(row: SqlRow): AlertConfig {
  return {
    poolId: String(row["pool_id"]), metric: String(row["metric"]),
    thresholdBps: nullableNumber(row, "threshold_bps") ?? undefined,
    thresholdValue: nullableNumber(row, "threshold_value") ?? undefined,
  };
}

function toHealthAlert(row: SqlRow): HealthAlert {
  return {
    poolId: String(row["pool_id"]), metric: String(row["metric"]),
    threshold: number(row, "threshold"), currentValue: number(row, "current_value"),
    firedAt: number(row, "fired_at") as Seconds,
  };
}

function toCursor(row: SqlRow): IngestionCursor {
  return {
    contractId: String(row["contract_id"]), ledger: number(row, "ledger"),
    txHash: String(row["tx_hash"]), eventIndex: number(row, "event_index"),
    updatedAt: number(row, "updated_at") as Seconds,
  };
}
