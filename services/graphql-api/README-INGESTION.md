# GraphQL API Soroban RPC Ingestion

The service ingests contract events from Soroban RPC, stores them durably, and
computes pool analytics. SQLite is the default backend. The in-memory backend is
available only as an explicit development/test option.

## Architecture

```text
Soroban RPC -> RpcIngester -> AnalyticsStore -> PoolIndexer -> GraphQL
                                   |                |
                                   +-- cursor       +-- derived analytics
```

- `RpcIngester` polls `getEvents` with an independent cursor per contract.
- `SqliteStore` runs in WAL mode and applies numbered schema migrations.
- Event insertion and cursor advancement occur in one SQLite transaction.
- Events are idempotent by `(ledger, txHash, eventIndex)`.
- `PoolIndexer` is a computation layer. It rebuilds its in-memory aggregates
  from retained store events on startup, so query and ingestion state cannot
  diverge after a restart.
- GraphQL HTTP and `graphql-ws` subscriptions share the same schema and runtime.

The store persists events, per-contract cursors, computed pool statistics,
positions, price history, alert configuration, and fired alerts. Retained event,
price, and fired-alert rows older than 30 days are pruned in SQL.

## Configuration

```bash
SOROBAN_RPC_URL=https://soroban-testnet.stellar.org
CONTRACT_IDS=CBAG7UBJCAETM5BI5RWFUTWK63...
START_LEDGER=123456
POLL_INTERVAL_MS=5000

# Default: sqlite. Set memory only for ephemeral development/tests.
STORE_TYPE=sqlite
DATABASE_PATH=./analytics.db

# Canonical token order and the actual configured pool fee.
POOL_CONFIG_JSON='{"CPOOL":{"tokenA":"CXLM","tokenB":"CUSDC","feeBps":30}}'

# USD price source used for TVL. TVL remains null unless both assets are priced.
TOKEN_PRICES='{"CXLM":0.12,"CUSDC":1}'

# Explicit local-only sample data. Ignored when NODE_ENV=production.
SEED_DEMO_DATA=false
```

`START_LEDGER=0` (or omitting it) resolves the oldest currently available RPC
ledger on first poll. A persisted cursor takes precedence after restart.

Pool token order is required to orient prices as token B per token A. The
configured fee is attached to each price point and pool statistic. When a pool
fee is absent, the service exposes `feeBpsSource: "default"`; it never presents
the fallback as an on-chain value.

## Analytics Semantics

- `reserveA` and `reserveB` are tracked independently from liquidity and swap
  flows. Different assets are never added together as raw units.
- `tvl` is USD-denominated when `TOKEN_PRICES` contains both reserve assets;
  otherwise it is `null`.
- `volume24h` and `fees24h` are true trailing 24-hour values, denominated in the
  pool's quote token (`tokenB`).
- Swap prices are always token B per token A, including B-to-A swaps.
- AMM LP positions are derived from provider/share liquidity events. CL
  positions are keyed by owner and tick range.
- `valueUsd` is `null` until the pool has a usable TVL price source.

The ingester recognizes AMM swap/add/remove events and CL `mint_pos`,
`burn_pos`, `swap_out`, and `coll_fees` events. Governance and staking events
are retained and queryable even though they do not change pool reserves.

## API Endpoints

- GraphQL HTTP: `http://localhost:4000/graphql`
- GraphQL WebSocket (`graphql-ws`): `ws://localhost:4000/graphql`
- Health: `http://localhost:4000/health`

Each stored event is published after indexing on both `EVENT:ALL` and
`EVENT:<poolId>`. The `poolEvent(poolId: ID)` subscription uses those topics.

Example:

```graphql
query Pool($id: ID!) {
  poolStats(poolId: $id) {
    poolId
    tokenA
    tokenB
    reserveA
    reserveB
    tvl
    volume24h
    fees24h
    feeBps
    feeBpsSource
  }
}
```

```graphql
subscription Events($id: ID) {
  poolEvent(poolId: $id) {
    id
    poolId
    type
    timestamp
    payload
  }
}
```

## Development

```bash
npm ci
npm run build
npm test
npm run lint
```

Tests cover SQLite close/reopen persistence, atomic event/cursor writes,
idempotency, retention, cursor isolation, canonical price direction, rolling
windows, nullable TVL, positions, and a real WebSocket client subscription.

## Recovery and Shutdown

If a cursor falls behind the RPC retention window, the ingester reports a
retention error rather than silently jumping forward. Restore a database backup
or deliberately reset the cursor/start ledger before resuming.

`SIGINT` and `SIGTERM` stop polling, drain HTTP and WebSocket traffic, and close
the SQLite connection. WAL sidecar files are managed by SQLite and should be
included in a consistent backup procedure.
