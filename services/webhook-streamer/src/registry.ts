/**
 * Webhook subscription registry.
 *
 * Subscriptions are held in memory and written through to a `StateStore`, so
 * with a `FileStateStore` (see `DATA_DIR`) they survive a restart. The default
 * store is in-memory.
 */

import { MemoryStateStore, type StateStore } from "./storage.js";
import type { WebhookSubscription } from "./types.js";
import { validateWebhookUrl } from "./url-validation.js";

let _nextId = 1;

export class WebhookRegistry {
  private subscriptions = new Map<string, WebhookSubscription>();

  constructor(private readonly store: StateStore = new MemoryStateStore()) {
    const saved = store.loadSubscriptions();
    for (const sub of saved.subscriptions) {
      this.subscriptions.set(sub.id, sub);
    }
    // Never hand out an id a restored subscription already holds.
    _nextId = Math.max(_nextId, saved.nextId);
  }

  private persist(): void {
    this.store.saveSubscriptions({
      subscriptions: [...this.subscriptions.values()],
      nextId: _nextId,
    });
  }

  /** Register a new webhook. Returns the created subscription. */
  register(
    url: string,
    opts: { contractId?: string; eventType?: string; secret?: string } = {},
  ): WebhookSubscription {
    validateWebhookUrl(url);
    const id = String(_nextId++);
    const sub: WebhookSubscription = {
      id,
      url,
      contractId: opts.contractId,
      eventType: opts.eventType,
      secret: opts.secret,
      createdAt: Date.now(),
    };
    this.subscriptions.set(id, sub);
    try {
      this.persist();
    } catch (err) {
      // Don't report a registration that would be lost on restart.
      this.subscriptions.delete(id);
      throw err;
    }
    return sub;
  }

  /** Remove a webhook by ID. Returns true if it existed. */
  unregister(id: string): boolean {
    const existing = this.subscriptions.get(id);
    if (!existing) return false;
    this.subscriptions.delete(id);
    try {
      this.persist();
    } catch (err) {
      this.subscriptions.set(id, existing);
      throw err;
    }
    return true;
  }

  /** List all registered webhooks, optionally filtered by contractId. */
  list(contractId?: string): WebhookSubscription[] {
    const all = [...this.subscriptions.values()];
    return contractId ? all.filter((s) => !s.contractId || s.contractId === contractId) : all;
  }

  /** Find subscriptions that match a given event. */
  matching(contractId: string, eventType: string): WebhookSubscription[] {
    return [...this.subscriptions.values()].filter((s) => {
      if (s.contractId && s.contractId !== contractId) return false;
      if (s.eventType && s.eventType !== eventType) return false;
      return true;
    });
  }

  get size(): number {
    return this.subscriptions.size;
  }
}

export const defaultRegistry = new WebhookRegistry();
