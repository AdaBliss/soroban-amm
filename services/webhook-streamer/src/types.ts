// ── Shared types for the webhook-streamer service (issue #306) ──────────────

/**
 * Raw Soroban contract event as returned by Stellar RPC's `getEvents` method.
 *
 * `topic` and `value` are base64-encoded XDR `ScVal`s, not plain strings or
 * JSON. Use `xdr.ScVal.fromXDR(..., "base64")` + `scValToNative` to decode.
 */
export interface RpcEvent {
  id: string;
  type: string;
  ledger: number;
  /** ISO-8601 close time of `ledger`, as reported by RPC. */
  ledgerClosedAt?: string;
  contractId?: string;
  /** Hash of the transaction that emitted the event. */
  txHash?: string;
  /**
   * Per-event pagination cursor. Soroban RPC paginates events by this token;
   * the poller falls back to it when a response carries no top-level cursor,
   * which is what keeps a poll from replaying or skipping events.
   */
  pagingToken?: string;
  /** Base64 XDR ScVals: the first is the event name, the rest are indexed data. */
  topic?: string[];
  /** Base64 XDR ScVal holding the event body (see `types.ts`'s PoolEvent). */
  value?: string;
}

/** Normalised pool event forwarded to webhooks. */
export interface PoolEvent {
  id: string;
  contractId: string;
  eventType: string;
  ledger: number;
  timestamp: string;
  /**
   * Schema version carried by versioned events (see
   * `contracts/amm-sdk/src/lib.rs`). `undefined` for legacy/unversioned events.
   */
  schemaVersion?: number;
  /**
   * Contract kind the event came from (e.g. `amm`, `concentrated_liquidity`),
   * when configured via `CONTRACT_KINDS`. Payload field names are only
   * applied when the kind is known.
   */
  contractKind?: string;
  payload: Record<string, unknown>;
  /**
   * Hash of the transaction that emitted the event, when the RPC endpoint
   * reports one. Subscribers use it to link an event back to its transaction.
   */
  txHash?: string;
}

/** A registered webhook subscription. */
export interface WebhookSubscription {
  id: string;
  url: string;
  /** Filter by contract ID; undefined = all contracts. */
  contractId?: string;
  /** Filter by event type (e.g. "swap", "mint_pos"); undefined = all types. */
  eventType?: string;
  /**
   * Shared secret used to sign deliveries (`X-Webhook-Signature`). Stored but
   * never serialised: go through `toPublic()` before returning a subscription
   * from the API or writing it to a log.
   */
  secret?: string;
  createdAt: number;
}

/** A subscription as exposed to API callers: the secret is replaced by a flag. */
export type PublicWebhookSubscription = Omit<WebhookSubscription, "secret"> & {
  hasSecret: boolean;
};

/** Strips the secret from a subscription. */
export function toPublic(sub: WebhookSubscription): PublicWebhookSubscription {
  const { secret, ...rest } = sub;
  return { ...rest, hasSecret: Boolean(secret) };
}

/**
 * Why a delivery attempt failed.
 *
 * `timeout` is deliberately distinct from `network` so operators can tell a
 * slow subscriber from a broken one (issue #720).
 */
export type FailureKind = "timeout" | "network" | "http" | "circuit_open";

/** Outcome of one individual HTTP attempt within a delivery. */
export interface AttemptOutcome {
  /** 0-based attempt index. */
  attempt: number;
  success: boolean;
  statusCode?: number;
  failureKind?: FailureKind;
  error?: string;
  /** Delay waited *before* this attempt was made, in ms. */
  delayMs: number;
  attemptedAt: number;
}

/** Result of a single webhook delivery attempt. */
export interface DeliveryResult {
  subscriptionId: string;
  url: string;
  success: boolean;
  statusCode?: number;
  error?: string;
  attemptedAt: number;
  /** Set when the delivery failed; distinguishes a timeout from other failures. */
  failureKind?: FailureKind;
  /** True when the request was abandoned at the configured timeout. */
  timedOut?: boolean;
  /** Every attempt made, in order. Length is 1 when no retry occurred. */
  attempts?: AttemptOutcome[];
  /** True when the delivery was abandoned and written to the dead-letter queue. */
  deadLettered?: boolean;
  /** Identifier of the dead-letter entry, when one was created. */
  deadLetterId?: string;
}

/** A permanently-failed delivery, retained for inspection and replay. */
export interface DeadLetter {
  id: string;
  event: PoolEvent;
  subscription: WebhookSubscription;
  attempts: AttemptOutcome[];
  /** When the delivery was finally abandoned. */
  failedAt: number;
  /** Human-readable summary of the terminal failure. */
  reason: string;
}

/** A dead letter as exposed to API callers (its subscription's secret removed). */
export type PublicDeadLetter = Omit<DeadLetter, "subscription"> & {
  subscription: PublicWebhookSubscription;
};

/** Strips the subscription secret from a dead letter. */
export function toPublicDeadLetter(entry: DeadLetter): PublicDeadLetter {
  return { ...entry, subscription: toPublic(entry.subscription) };
}

/** Circuit-breaker states, following the standard closed → open → half-open cycle. */
export type CircuitState = "closed" | "open" | "half_open";

/** Point-in-time view of one subscription's circuit. */
export interface CircuitSnapshot {
  subscriptionId: string;
  state: CircuitState;
  consecutiveFailures: number;
  /** Epoch ms at which an open circuit may next be probed. */
  nextProbeAt?: number;
}

/** Aggregate delivery counters exposed via GET /metrics. */
export interface MetricsSnapshot {
  attempted: number;
  succeeded: number;
  failed: number;
  timedOut: number;
  deadLettered: number;
  retries: number;
  shortCircuited: number;
  circuits: CircuitSnapshot[];
}
