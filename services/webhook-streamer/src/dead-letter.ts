/**
 * Bounded dead-letter queue for permanently-failed webhook deliveries
 * (issue #720).
 *
 * Before this existed a delivery that exhausted its retries was reported as a
 * failure count and then discarded — there was no record of what was lost and
 * no way to replay it. Entries here retain the event, the subscription and
 * every attempt's outcome so an operator can diagnose and re-deliver.
 *
 * The queue is bounded: at capacity the oldest entry is evicted so a
 * permanently-broken subscriber cannot exhaust memory.
 */

import { MemoryStateStore, type StateStore } from "./storage.js";
import type {
  AttemptOutcome,
  DeadLetter,
  PoolEvent,
  WebhookSubscription,
} from "./types.js";

export const DEFAULT_MAX_ENTRIES = 1_000;

let _nextId = 1;

/** Reset the dead-letter ID counter. Exposed for deterministic tests. */
export function _resetDeadLetterIds(): void {
  _nextId = 1;
}

export class DeadLetterQueue {
  private entries: DeadLetter[] = [];
  readonly maxEntries: number;

  /**
   * With a persistent `store`, entries survive a restart and stay replayable.
   * The default store is in-memory.
   */
  constructor(
    maxEntries: number = DEFAULT_MAX_ENTRIES,
    private readonly store: StateStore = new MemoryStateStore(),
  ) {
    if (!Number.isFinite(maxEntries) || maxEntries < 1) {
      throw new RangeError("maxEntries must be a positive integer");
    }
    this.maxEntries = Math.floor(maxEntries);

    const saved = store.loadDeadLetters();
    this.entries = saved.entries.slice(-this.maxEntries);
    _nextId = Math.max(_nextId, saved.nextId);
  }

  /**
   * A failed write must not turn a delivery failure into a crash: the entry
   * stays in memory and the problem is logged.
   */
  private persist(): void {
    try {
      this.store.saveDeadLetters({ entries: this.entries, nextId: _nextId });
    } catch (err) {
      console.error("[dead-letter] failed to persist the queue:", err);
    }
  }

  /**
   * Record a permanently-failed delivery.
   *
   * Evicts the oldest entry when already at capacity, so the queue never grows
   * beyond `maxEntries`.
   */
  add(
    event: PoolEvent,
    subscription: WebhookSubscription,
    attempts: AttemptOutcome[],
    reason: string,
    failedAt: number = Date.now(),
  ): DeadLetter {
    const entry: DeadLetter = {
      id: String(_nextId++),
      event,
      subscription,
      attempts,
      failedAt,
      reason,
    };

    this.entries.push(entry);
    // Bounded: drop oldest-first once over capacity.
    while (this.entries.length > this.maxEntries) {
      this.entries.shift();
    }
    this.persist();
    return entry;
  }

  /** All retained entries, oldest first. */
  list(): DeadLetter[] {
    return [...this.entries];
  }

  /** Look up a single entry by ID. */
  get(id: string): DeadLetter | undefined {
    return this.entries.find((e) => e.id === id);
  }

  /** Remove an entry by ID. Returns true if it existed. */
  remove(id: string): boolean {
    const idx = this.entries.findIndex((e) => e.id === id);
    if (idx === -1) return false;
    this.entries.splice(idx, 1);
    this.persist();
    return true;
  }

  /** Drop every entry. */
  clear(): void {
    this.entries = [];
    this.persist();
  }

  get size(): number {
    return this.entries.length;
  }
}

export const defaultDeadLetterQueue = new DeadLetterQueue();
