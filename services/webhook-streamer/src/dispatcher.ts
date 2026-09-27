/**
 * Webhook dispatcher — delivers a PoolEvent to all matching subscribers.
 *
 * Delivery reliability (issue #720):
 *   - Every request is bounded by a timeout via AbortController; a timeout is
 *     recorded distinctly from other failures.
 *   - Fan-out uses settled results with bounded concurrency, so one hanging
 *     subscriber cannot delay another or stall the poller loop.
 *   - Retries are iterative (no stack growth) and apply full jitter:
 *     random(0, base * 2 ** attempt), capped at maxDelayMs.
 *   - Only retryable failures are retried: 408, 429, 5xx and network errors.
 *     A 4xx such as 404 is terminal — it will still be a 404 in two seconds.
 *   - Retry-After is honoured when the subscriber sends one.
 *   - Permanently-failed deliveries land in a bounded dead-letter queue.
 *   - A per-subscription circuit breaker stops paying for a dead endpoint.
 *
 * Sends X-Webhook-Secret header when a secret is configured.
 */

import fetch from "node-fetch";
import type {
  AttemptOutcome,
  DeliveryResult,
  MetricsSnapshot,
  PoolEvent,
  WebhookSubscription,
} from "./types.js";
import type { WebhookRegistry } from "./registry.js";
import { DeadLetterQueue, defaultDeadLetterQueue } from "./dead-letter.js";
import { CircuitBreaker, defaultCircuitBreaker } from "./circuit-breaker.js";

export const MAX_RETRIES = 3;
export const BASE_DELAY_MS = 500;
export const MAX_DELAY_MS = 30_000;
export const DEFAULT_TIMEOUT_MS = 10_000;
export const DEFAULT_CONCURRENCY = 10;
/** Cap on a honoured Retry-After, so a hostile header cannot stall delivery. */
export const MAX_RETRY_AFTER_MS = 60_000;

export interface DispatcherOptions {
  /** Per-request timeout in ms. Default 10s. */
  timeoutMs?: number;
  /** Maximum concurrent in-flight deliveries per dispatch. Default 10. */
  concurrency?: number;
  maxRetries?: number;
  baseDelayMs?: number;
  maxDelayMs?: number;
  deadLetterQueue?: DeadLetterQueue;
  circuitBreaker?: CircuitBreaker;
  /** Injectable sleep, for tests. */
  sleep?: (ms: number) => Promise<void>;
  /** Injectable RNG in [0,1), for deterministic jitter in tests. */
  random?: () => number;
}

interface Counters {
  attempted: number;
  succeeded: number;
  failed: number;
  timedOut: number;
  deadLettered: number;
  retries: number;
  shortCircuited: number;
}

/** Internal: an attempt outcome plus any server-directed retry delay. */
type AttemptResult = AttemptOutcome & { retryAfterMs?: number };

/** HTTP statuses worth retrying. Everything else in 4xx is terminal. */
export function isRetryableStatus(status: number): boolean {
  if (status === 408 || status === 429) return true;
  return status >= 500 && status <= 599;
}

/**
 * Parse a Retry-After header, which may be delta-seconds or an HTTP date.
 * Returns undefined when absent or unparseable.
 */
export function parseRetryAfter(
  value: string | null | undefined,
  now: number = Date.now(),
): number | undefined {
  if (!value) return undefined;
  const trimmed = value.trim();
  if (trimmed === "") return undefined;

  // delta-seconds
  if (/^\d+$/.test(trimmed)) {
    return Math.min(Number(trimmed) * 1_000, MAX_RETRY_AFTER_MS);
  }

  // HTTP-date
  const ts = Date.parse(trimmed);
  if (Number.isNaN(ts)) return undefined;
  const delta = ts - now;
  if (delta <= 0) return 0;
  return Math.min(delta, MAX_RETRY_AFTER_MS);
}

/**
 * Full jitter: random(0, min(base * 2 ** attempt, cap)).
 *
 * Un-jittered backoff leaves every failing subscriber retrying in lockstep —
 * a synchronised thundering herd against an endpoint that is trying to
 * recover. Full jitter spreads those retries evenly across the window.
 */
export function fullJitterDelay(
  attempt: number,
  baseDelayMs: number,
  maxDelayMs: number,
  random: () => number = Math.random,
): number {
  const exponential = Math.min(baseDelayMs * 2 ** attempt, maxDelayMs);
  return Math.floor(random() * exponential);
}

function _sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

/**
 * Run `fn` over `items` with at most `limit` in flight at once, returning a
 * settled result per item so one failure never rejects the whole batch.
 */
async function _mapWithConcurrency<T, R>(
  items: T[],
  limit: number,
  fn: (item: T) => Promise<R>,
): Promise<PromiseSettledResult<R>[]> {
  const results: PromiseSettledResult<R>[] = new Array<PromiseSettledResult<R>>(items.length);
  let cursor = 0;

  const workers = Array.from(
    { length: Math.max(1, Math.min(limit, items.length)) },
    async () => {
      for (;;) {
        const idx = cursor++;
        if (idx >= items.length) return;
        try {
          results[idx] = { status: "fulfilled", value: await fn(items[idx]) };
        } catch (reason) {
          results[idx] = { status: "rejected", reason };
        }
      }
    },
  );

  await Promise.all(workers);
  return results;
}

export class WebhookDispatcher {
  private readonly timeoutMs: number;
  private readonly concurrency: number;
  private readonly maxRetries: number;
  private readonly baseDelayMs: number;
  private readonly maxDelayMs: number;
  private readonly sleep: (ms: number) => Promise<void>;
  private readonly random: () => number;

  readonly deadLetters: DeadLetterQueue;
  readonly circuitBreaker: CircuitBreaker;

  private counters: Counters = {
    attempted: 0,
    succeeded: 0,
    failed: 0,
    timedOut: 0,
    deadLettered: 0,
    retries: 0,
    shortCircuited: 0,
  };

  constructor(
    private readonly registry: WebhookRegistry,
    opts: DispatcherOptions = {},
  ) {
    this.timeoutMs = opts.timeoutMs ?? DEFAULT_TIMEOUT_MS;
    this.concurrency = opts.concurrency ?? DEFAULT_CONCURRENCY;
    this.maxRetries = opts.maxRetries ?? MAX_RETRIES;
    this.baseDelayMs = opts.baseDelayMs ?? BASE_DELAY_MS;
    this.maxDelayMs = opts.maxDelayMs ?? MAX_DELAY_MS;
    this.sleep = opts.sleep ?? _sleep;
    this.random = opts.random ?? Math.random;
    this.deadLetters = opts.deadLetterQueue ?? defaultDeadLetterQueue;
    this.circuitBreaker = opts.circuitBreaker ?? defaultCircuitBreaker;
  }

  /**
   * Fan out `event` to all matching subscriptions.
   *
   * Every subscriber gets a result regardless of how any other behaves, and
   * the fan-out never opens more than `concurrency` sockets at once.
   */
  async dispatch(event: PoolEvent): Promise<DeliveryResult[]> {
    const subs = this.registry.matching(event.contractId, event.eventType);
    const settled = await _mapWithConcurrency(subs, this.concurrency, (sub) =>
      this._deliver(sub, event),
    );

    return settled.map((r, i) => {
      if (r.status === "fulfilled") return r.value;
      // A rejection here is a dispatcher bug rather than a delivery failure,
      // but it must not take down the fan-out.
      const sub = subs[i];
      return {
        subscriptionId: sub.id,
        url: sub.url,
        success: false,
        failureKind: "network",
        error: String(r.reason),
        attemptedAt: Date.now(),
      };
    });
  }

  /**
   * Re-attempt a previously dead-lettered delivery. The entry is removed only
   * once it succeeds, so a failed replay stays available to try again.
   */
  async replay(deadLetterId: string): Promise<DeliveryResult | undefined> {
    const entry = this.deadLetters.get(deadLetterId);
    if (!entry) return undefined;

    const result = await this._deliver(entry.subscription, entry.event);
    if (result.success) {
      this.deadLetters.remove(deadLetterId);
    }
    return result;
  }

  /** Current delivery counters and circuit states. */
  metrics(): MetricsSnapshot {
    return {
      ...this.counters,
      circuits: this.circuitBreaker.snapshots(),
    };
  }

  /** Zero the counters. Exposed for tests. */
  resetMetrics(): void {
    this.counters = {
      attempted: 0,
      succeeded: 0,
      failed: 0,
      timedOut: 0,
      deadLettered: 0,
      retries: 0,
      shortCircuited: 0,
    };
  }

  /**
   * Deliver one event to one subscriber, retrying iteratively.
   *
   * Written as a loop rather than recursion so the stack does not grow with
   * each attempt.
   */
  private async _deliver(
    sub: WebhookSubscription,
    event: PoolEvent,
  ): Promise<DeliveryResult> {
    // Short-circuit a known-dead endpoint before spending a socket on it.
    if (!this.circuitBreaker.canAttempt(sub.id)) {
      this.counters.shortCircuited += 1;
      return {
        subscriptionId: sub.id,
        url: sub.url,
        success: false,
        failureKind: "circuit_open",
        error: "circuit open",
        attemptedAt: Date.now(),
        attempts: [],
      };
    }

    const headers: Record<string, string> = {
      "Content-Type": "application/json",
    };
    if (sub.secret) {
      headers["X-Webhook-Secret"] = sub.secret;
    }
    const body = JSON.stringify(event);

    const attempts: AttemptOutcome[] = [];
    let delayMs = 0;

    for (let attempt = 0; ; attempt++) {
      if (delayMs > 0) {
        await this.sleep(delayMs);
      }
      if (attempt > 0) {
        this.counters.retries += 1;
      }

      const attemptedAt = Date.now();
      this.counters.attempted += 1;

      const outcome = await this._attempt(
        sub,
        headers,
        body,
        attempt,
        delayMs,
        attemptedAt,
      );
      const { retryAfterMs, ...record } = outcome;
      attempts.push(record);

      if (outcome.success) {
        this.circuitBreaker.recordSuccess(sub.id);
        this.counters.succeeded += 1;
        const ok: DeliveryResult = {
          subscriptionId: sub.id,
          url: sub.url,
          success: true,
          attemptedAt: outcome.attemptedAt,
          attempts,
        };
        if (outcome.statusCode !== undefined) ok.statusCode = outcome.statusCode;
        return ok;
      }

      if (outcome.failureKind === "timeout") {
        this.counters.timedOut += 1;
      }

      // Network and timeout failures are retryable; HTTP failures only when
      // the status says so. A 404 will still be a 404 in two seconds.
      const retryable =
        outcome.failureKind !== "http" ||
        (outcome.statusCode !== undefined &&
          isRetryableStatus(outcome.statusCode));

      if (!retryable || attempt >= this.maxRetries) {
        return this._fail(sub, event, attempts, record);
      }

      delayMs =
        retryAfterMs ??
        fullJitterDelay(attempt, this.baseDelayMs, this.maxDelayMs, this.random);
    }
  }

  /** One timeout-bounded HTTP attempt. Never throws. */
  private async _attempt(
    sub: WebhookSubscription,
    headers: Record<string, string>,
    body: string,
    attempt: number,
    delayMs: number,
    attemptedAt: number,
  ): Promise<AttemptResult> {
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), this.timeoutMs);

    try {
      const res = await fetch(sub.url, {
        method: "POST",
        headers,
        body,
        signal: controller.signal,
      });

      if (res.ok) {
        return {
          attempt,
          success: true,
          statusCode: res.status,
          delayMs,
          attemptedAt,
        };
      }

      const retryAfterMs = parseRetryAfter(res.headers.get("retry-after"));
      return {
        attempt,
        success: false,
        statusCode: res.status,
        failureKind: "http",
        error: `HTTP ${res.status}`,
        delayMs,
        attemptedAt,
        ...(retryAfterMs !== undefined ? { retryAfterMs } : {}),
      };
    } catch (err) {
      // AbortError is how the timeout surfaces; record it distinctly from a
      // genuine network error so operators can tell slow from broken.
      const aborted =
        controller.signal.aborted ||
        (err as { name?: string })?.name === "AbortError";
      return {
        attempt,
        success: false,
        failureKind: aborted ? "timeout" : "network",
        error: aborted ? `timeout after ${this.timeoutMs}ms` : String(err),
        delayMs,
        attemptedAt,
      };
    } finally {
      clearTimeout(timer);
    }
  }

  /** Terminal failure: record it, trip the circuit, dead-letter the event. */
  private _fail(
    sub: WebhookSubscription,
    event: PoolEvent,
    attempts: AttemptOutcome[],
    last: AttemptOutcome,
  ): DeliveryResult {
    this.circuitBreaker.recordFailure(sub.id);
    this.counters.failed += 1;

    const reason = last.error ?? "delivery failed";
    const entry = this.deadLetters.add(event, sub, attempts, reason);
    this.counters.deadLettered += 1;

    console.warn(
      `[dead-letter] event=${event.id} type=${event.eventType} ` +
        `contract=${event.contractId} subscription=${sub.id} url=${sub.url} ` +
        `attempts=${attempts.length} reason="${reason}" ` +
        `deadLetterId=${entry.id} circuit=${this.circuitBreaker.stateOf(sub.id)}`,
    );

    const result: DeliveryResult = {
      subscriptionId: sub.id,
      url: sub.url,
      success: false,
      error: reason,
      attemptedAt: last.attemptedAt,
      attempts,
      deadLettered: true,
      deadLetterId: entry.id,
    };
    if (last.statusCode !== undefined) result.statusCode = last.statusCode;
    if (last.failureKind !== undefined) result.failureKind = last.failureKind;
    if (last.failureKind === "timeout") result.timedOut = true;
    return result;
  }
}
