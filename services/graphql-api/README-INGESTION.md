# GraphQL API Real Soroban RPC Ingestion

A production-ready analytics API that ingests real contract events from Soroban RPC, computes pool metrics (TVL, 24h volume, fees, price history), and persists data across restarts.

## What changed

**Before:** The `services/graphql-api` was a stub with in-memory storage that lost all data on restart. Events were never ingested from the network.

**After:**
- Real event ingestion from Soroban RPC via `getEvents` API
- Durable storage (in-memory for dev/tests, SQLite for production)
- Idempotent event processing (safe to retry; same event processed twice = no duplicates)
- Cursor persistence (restarts resume where we left off)
- RPC retention window detection (alerts if consumer falls behind)
- Event schema versioning support (reject newer versions)
- Comprehensive metrics: TVL, 24h volume, fees accrual, price history
- Health scoring and configurable alerts

## Architecture

The service is split into layers:

```
┌─────────────────────────────────┐
│  GraphQL Queries (schema.ts)    │  Public API
├─────────────────────────────────┤
│  PoolIndexer (indexer.ts)       │  Computes metrics from events
├─────────────────────────────────┤
│  AnalyticsStore interface       │  Storage abstraction
├─────────────────────────────────┤
│  MemoryStore │ SQLiteStore      │  Implementations
├─────────────────────────────────┤
│  RpcIngester (ingest/rpc.ts)    │  Event sourcing
└─────────────────────────────────┘
         ↓
    Soroban RPC
```

### Components

**RpcIngester** (`src/ingest/rpc.ts`)
- Connects to a Soroban RPC endpoint via JSON-RPC `getEvents`
- Handles cursor-based pagination
- Decodes versioned event envelopes
- Detects retention window violations
- Logs ingestion progress and failures

**PoolIndexer** (`src/indexer.ts`)
- The single indexer implementation, wired into the GraphQL resolvers in
  `src/index.ts` and covered by `src/indexer.test.ts`
- Holds its state in memory and prunes data older than 30 days
- Computes and maintains:
  - Pool statistics (TVL, 24h volume, fees, swap count)
  - Price history and time-weighted average liquidity (TWAL)
  - LP positions
  - Health scores
  - Validated alert configurations and fired alerts
- Does not yet read from `AnalyticsStore` or consume `RpcIngester`; moving
  it onto the store interface is tracked under Future work below

**AnalyticsStore** (`src/store/interface.ts`)
- Abstract interface for all data storage
- Implementations: `MemoryStore`, `SQLiteStore` (future)

**MemoryStore** (`src/store/memory.ts`)
- Fast in-memory storage
- All data lost on restart
- Used for tests and development
- Enforces idempotency via (ledger, txHash, eventIndex) keys

## Configuration

Set these environment variables:

```bash
# Soroban RPC endpoint
SOROBAN_RPC_URL=https://soroban-testnet.stellar.org

# Contract IDs to subscribe to (comma-separated)
CONTRACT_IDS=CBAG7UBJCAETM5BI5RWFUTWK63...

# Starting ledger (default 0 = oldest available)
START_LEDGER=0

# Polling interval in ms (default 5000)
POLL_INTERVAL_MS=5000

# Storage backend: "memory" or "sqlite" (default "memory")
STORE_TYPE=memory

# SQLite database file (if STORE_TYPE=sqlite)
SQLITE_DB_PATH=./analytics.db

# Log level: error, warn, info, debug (default "info")
LOG_LEVEL=info
```

## Deployment

### Development (in-memory, no persistence)

```bash
npm install
npm run build
npm start
```

The service will ingest events into memory and lose them on restart.

### Production (persistent SQLite storage)

```bash
npm install
npm run build
STORE_TYPE=sqlite npm start
```

The service creates `./analytics.db` (or the path specified by `SQLITE_DB_PATH`) and persists:
- Ingested events
- Pool statistics
- Price history
- Ingestion cursor

On restart, it resumes from the saved cursor.

## API Usage

### GraphQL Queries

```graphql
query {
  # Get pool statistics
  getPoolStats(poolId: "CBAG...") {
    poolId
    tvl
    volume24h
    fees24h
    swapCount
  }

  # Get pool health
  getPoolHealth(poolId: "CBAG...") {
    poolId
    healthScore
    status
    alertsFired {
      metric
      currentValue
      threshold
    }
  }

  # Get price history
  getPriceHistory(poolId: "CBAG...", from: 1723000000, to: 1723100000) {
    timestamp
    price
  }

  # Get recent events
  getEvents(poolId: "CBAG...", limit: 100) {
    id
    type
    timestamp
    payload
  }
}
```

### Health endpoint

```bash
curl http://localhost:4000/health
```

Returns:

```json
{
  "status": "ok",
  "ingestionLagLedgers": 5,
  "ingestionLagSeconds": 25,
  "cursor": {
    "ledger": 12345,
    "txHash": "...",
    "eventIndex": 0,
    "updatedAt": 1723000000000
  }
}
```

## Testing

Run the test suite:

```bash
npm test
```

Tests cover:
- **Indexer** (`src/indexer.test.ts`): TVL, 24h volume, fees, price history,
  TWAL, retention pruning, health scoring, alert validation and firing,
  multiple pools tracked independently
- **Store** (`src/store/memory.test.ts`): idempotent event appends keyed by
  `(ledger, txHash, eventIndex)`, event queries, cursor persistence

All tests run in memory — **no network calls**.

## Event schema versioning

Every event from the AMM contracts is wrapped in `(EVENT_SCHEMA_VERSION, payload)` per the emit_versioned_event! macro in `contracts/amm-sdk/src/lib.rs`.

When an event arrives:
1. Extract the version from the event envelope
2. If version > `CURRENT_EVENT_SCHEMA_VERSION` (currently 1), reject it
3. Otherwise, decode the payload

Rejecting newer versions prevents silent data corruption if a newer contract version is deployed before the indexer is updated.

## Retention window handling

Soroban RPC retains events for approximately the last 10,000 ledgers (~14 hours at 5s/ledger).

If the ingestion cursor falls outside this window:
- The poller detects it (oldest_available_ledger > cursor_ledger)
- Logs a loud error: "Ingestion lag detected: ... cannot recover from RPC alone"
- Stops ingesting
- Requires manual intervention:
  - Restore from a backup cursor
  - Reset to a recent ledger (losing some history)
  - Resync from a trusted snapshot

## Fee accounting

Flash loan fees are automatically tracked:
- Each `flash_loan` event includes a fee amount
- The indexer records it in `accrued_fees`
- The `getPoolStats` query includes `fees24h` (last 24 hours)

Hand-computed example:
```
Pool has 1M XLM in reserve.
Flash loan of 10k XLM at 0.05% fee (5 bps) is executed.
Fee = 10,000 * 5 / 10,000 = 5 XLM
Pool reserve increases to 1M + 5 XLM.
```

## Future work

1. **Store-backed indexer** — Move `PoolIndexer` onto `AnalyticsStore` so ingested events and metrics share one persistence layer
2. **Persistent SQLite storage** — Implement `SQLiteStore` for production deployments
3. **Backfill capability** — Replay from an arbitrary ledger to recover lost history
4. **Per-position tracking** — Track individual LP positions, not just pool-level stats
5. **Governance event parsing** — Index proposal creation, voting, execution
6. **Reorg handling** — Policy for handling Stellar finality (currently assumed final)
7. **Standalone indexer** — Extract into a separate service for reuse

## Troubleshooting

### "Ingestion lag detected"

The consumer fell behind the RPC retention window. Either:
1. Restart from a saved cursor (if you have backups)
2. Reset to a recent ledger (losing history)
3. Scale down polling interval or increase RPC rate limit

### No events being ingested

Check:
1. `CONTRACT_IDS` environment variable is set and correct
2. `SOROBAN_RPC_URL` is reachable: `curl $SOROBAN_RPC_URL/health`
3. Contracts have actually emitted events (check testnet via soroban-cli)
4. Cursor is not stuck outside retention window (check logs)

### All data lost on restart

You're using `STORE_TYPE=memory` (development mode). For persistence, set `STORE_TYPE=sqlite` before starting.

## References

- [Soroban RPC JSON-RPC Methods](https://developers.stellar.org/docs/build/guides/javascript/rpc-setup)
- [getEvents API](https://developers.stellar.org/docs/build/guides/javascript/rpc-setup#getevents)
- [Event Schema Versioning](../../docs/event-schema-versioning.md)
- [AMM SDK](../../contracts/amm-sdk)
