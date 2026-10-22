# Protocol TWAP oracle adapters

`oracle_adapters.wasm` implements the `quote(token_a, token_b) -> (price,
last_updated_timestamp)` source interface consumed by `oracle_aggregator`.
One instance is initialized in `Amm` mode and another in
`ConcentratedLiquidity` mode.

Both modes resolve pools through the configured factory, use the configured
TWAP window, reject missing or stale snapshots, and return price scaled by
`1_000_000` as token B per token A. Reverse requests use the same pool and
invert the result. The CL mode examines its bounded fee-tier list and selects
the pool with the greatest active liquidity.

Before permissionless snapshotting, the TWAP consumer keeper configures its
factory and minimum interval:

```bash
stellar contract invoke --id "$TWAP_CONSUMER_CONTRACT_ID" --source "$ADMIN" \
  -- configure_public_snapshots --factory "$FACTORY_CONTRACT_ID" \
  --min_interval_seconds 300
```

Then any account may call `save_snapshot_public` or
`save_cl_snapshot_public` for a pool registered by that factory. The original
keeper-authorized methods remain available for non-factory pools.

Register both deployed adapters as ordinary aggregator sources:

```bash
stellar contract invoke --id "$ORACLE_AGGREGATOR_CONTRACT_ID" --source "$ADMIN" \
  -- register_source --admin "$ADMIN_ADDRESS" \
  --source_contract "$AMM_TWAP_ADAPTER_CONTRACT_ID" --source_type AmmTwap --weight 10000

stellar contract invoke --id "$ORACLE_AGGREGATOR_CONTRACT_ID" --source "$ADMIN" \
  -- register_source --admin "$ADMIN_ADDRESS" \
  --source_contract "$CL_TWAP_ADAPTER_CONTRACT_ID" --source_type ClTwap --weight 10000
```

An adapter validates availability and freshness at quote time. It does not
turn one pool into an independent oracle: production deployments should still
register multiple economically independent sources.
