export const typeDefs = `#graphql
  # Every timestamp in this API (event timestamp, price point timestamp,
  # alert firedAt, and the priceHistory from/to arguments) is seconds since
  # the Unix epoch, not milliseconds.

  type PoolStats {
    poolId: ID!
    tokenA: String!
    tokenB: String!
    tvl: Float!
    volume24h: Float!
    fees24h: Float!
    swapCount: Int!
    priceDeviationBps: Int!
  }

  type PoolEvent {
    id: ID!
    poolId: ID!
    type: String!
    "Seconds since the Unix epoch (chain close time of the ledger)."
    timestamp: Float!
    payload: String!
  }

  type Position {
    id: ID!
    poolId: ID!
    owner: String!
    shares: Float!
    valueUsd: Float!
  }

  type PricePoint {
    poolId: ID!
    "Seconds since the Unix epoch."
    timestamp: Float!
    price: Float!
    feeBps: Int!
  }

  type PoolHealth {
    poolId: ID!
    healthScore: Float!
    tvlScore: Float!
    volumeScore: Float!
    feeEfficiencyScore: Float!
    priceDeviationBps: Int!
    status: String!
    alertsFired: [HealthAlert!]!
  }

  type HealthAlert {
    poolId: ID!
    metric: String!
    threshold: Float!
    currentValue: Float!
    "Seconds since the Unix epoch."
    firedAt: Float!
  }

  """
  Configured alert threshold for a pool metric. metric must be one of
  "price_deviation", "tvl", "volume24h". "price_deviation" uses thresholdBps
  (basis points); "tvl" and "volume24h" use thresholdValue (a raw value in
  the metric's native units, not basis points).
  """
  type AlertConfig {
    poolId: ID!
    metric: String!
    thresholdBps: Int
    thresholdValue: Float
  }

  type Query {
    poolStats(poolId: ID): [PoolStats!]!
    poolEvents(poolId: ID, limit: Int = 100): [PoolEvent!]!
    positions(owner: String): [Position!]!
    "from and to are inclusive bounds in seconds since the Unix epoch."
    priceHistory(poolId: ID!, from: Float, to: Float): [PricePoint!]!
    twal(poolId: ID!, windowSeconds: Int!): Float
    poolHealth(poolId: ID!): PoolHealth
    alertConfigs(poolId: ID): [AlertConfig!]!
  }

  type Mutation {
    """
    metric must be one of "price_deviation", "tvl", "volume24h". Pass
    thresholdBps for "price_deviation"; pass thresholdValue for "tvl" and
    "volume24h". Both fields must be >= 0.
    """
    setAlertConfig(poolId: ID!, metric: String!, thresholdBps: Int, thresholdValue: Float): AlertConfig!
    removeAlertConfig(poolId: ID!, metric: String!): Boolean!
  }

  type Subscription {
    poolEvent(poolId: ID): PoolEvent!
  }
`;
