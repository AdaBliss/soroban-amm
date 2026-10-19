import { xdr, scValToNative } from "@stellar/stellar-sdk";

import { findVariant } from "./event-schemas.js";
import { MemoryStateStore, type CursorState, type StateStore } from "./storage.js";
import type { PoolEvent, RpcEvent } from "./types.js";

export interface Logger {
  info(msg: string, ...args: unknown[]): void;
  warn(msg: string, ...args: unknown[]): void;
  error(msg: string, ...args: unknown[]): void;
}

export interface RpcPollerOptions {
  rpcUrl: string;
  contractIds: string[];
  /** Defaults to the global `fetch`. */
  fetchFn?: typeof fetch;
  /** Defaults to `console`. */
  logger?: Logger;
  /** Called when an RPC or dispatch error is surfaced. */
  onError?: (err: Error) => void;
  /** Interval between poll ticks, in ms. Defaults to 5000. */
  pollIntervalMs?: number;
  /**
   * Contract kind per contract ID (see `parseContractKinds`). Event fields are
   * only named for contracts listed here; others get the generic shape.
   */
  contractKinds?: Record<string, string>;
  /**
   * Where per-contract cursors are persisted. With a durable store the poller
   * resumes from where it stopped after a restart. Defaults to in-memory.
   */
  store?: StateStore;
}

/** A stretch of ledgers the RPC endpoint no longer retains and the poller never saw. */
export interface LedgerGap {
  contractId: string;
  /** `cursor`: the poller had a position that expired; `start`: START_LEDGER itself was outside the window. */
  reason: "cursor" | "start";
  fromLedger: number;
  toLedger: number;
  /** The oldest ledger the poller resumed from. */
  resumedAt: number;
  detectedAt: string;
}

/** Latest gaps kept for `/health`. */
const MAX_GAPS = 20;

/** `getEvents` reports an out-of-window ledger or cursor as invalid params. */
const INVALID_PARAMS = -32602;

/**
 * Ledger sequence encoded in a Soroban RPC paging token (`<toid>-<event index>`,
 * where the TOID's high 32 bits are the ledger), or `undefined` if the token
 * has another shape.
 */
export function ledgerFromPagingToken(token: string): number | undefined {
  const match = /^(\d{1,19})-\d+$/.exec(token);
  if (!match) return undefined;
  const ledger = Number(BigInt(match[1]) >> 32n);
  return ledger > 0 ? ledger : undefined;
}

/** One fetched page and the cursor state to commit once it has been dispatched. */
interface Page {
  events: PoolEvent[];
  next: CursorState | undefined;
}

/**
 * One page of `getEvents` output. `cursor` and `latestLedger` are optional
 * because a response that ends on an empty page may report neither.
 */
export interface RawGetEventsResult {
  events: RpcEvent[];
  cursor?: string;
  latestLedger?: number;
}

export interface RpcPollerHealth {
  running: boolean;
  pollIntervalMs: number;
  contracts: number;
  ticks: number;
  eventsEmitted: number;
  lastError: string | null;
  /** Ledger ranges the poller could not read because RPC no longer retained them. */
  gaps: LedgerGap[];
}

export interface JsonRpcError {
  code: number;
  message: string;
  data?: unknown;
}

export interface JsonRpcResponse<T> {
  jsonrpc: string;
  id: number | string;
  result?: T;
  error?: JsonRpcError;
}

export class RawRpcError extends Error {
  constructor(
    message: string,
    public readonly code?: number,
    public readonly data?: unknown
  ) {
    super(message);
    this.name = "RawRpcError";
  }
}

/**
 * Polls Stellar RPC's `getEvents` method for Soroban contract events.
 *
 * The poller maintains a per-contract cursor map. On the first poll for a
 * contract it sends a positive `startLedger` (RPC rejects requests without
 * one). After that it sends the cursor returned by the previous response.
 *
 * `poll()` performs a single pass over every contract and returns the decoded
 * events. `start()` drives `poll()` on `pollIntervalMs` and hands each event to
 * the `onEvent` callback passed to the constructor, which is what a long-lived
 * service uses; `poll()` is the entry point for tests and one-shot catch-ups.
 */
export class RpcPoller {
  private readonly rpcUrl: string;
  private readonly contractIds: string[];
  private readonly fetchFn: typeof fetch;
  private readonly logger: Logger;
  private readonly onError?: (err: Error) => void;
  private readonly onEvent?: (event: PoolEvent) => void | Promise<void>;
  private readonly pollIntervalMs: number;
  private readonly contractKinds: Record<string, string>;
  private readonly cursors: Map<string, string> = new Map();
  /** Ledger of the last event ingested per contract. */
  private readonly ledgers: Map<string, number> = new Map();
  /** Per-contract first ledger to request when there is no cursor (set by retention recovery). */
  private readonly startOverrides: Map<string, number> = new Map();
  private readonly store: StateStore;
  private readonly gaps: LedgerGap[] = [];
  private currentTick: Promise<void> | undefined;
  private readonly startLedger: number;
  private requestId = 0;
  private timer: NodeJS.Timeout | undefined;
  private polling = false;
  private running = false;
  private ticks = 0;
  private eventsEmitted = 0;
  private lastErrorValue: Error | undefined;

  constructor(
    options: RpcPollerOptions,
    onEvent?: (event: PoolEvent) => void | Promise<void>
  ) {
    this.rpcUrl = options.rpcUrl;
    this.contractIds = options.contractIds;
    this.fetchFn = options.fetchFn ?? fetch;
    this.logger = options.logger ?? console;
    this.onError = options.onError;
    this.onEvent = onEvent;
    this.contractKinds = options.contractKinds ?? {};
    this.pollIntervalMs = options.pollIntervalMs ?? 5_000;
    this.store = options.store ?? new MemoryStateStore();
    const saved = this.store.loadCursors();
    for (const contractId of this.contractIds) {
      const state = saved[contractId];
      if (!state) continue;
      this.cursors.set(contractId, state.cursor);
      if (state.ledger !== undefined) this.ledgers.set(contractId, state.ledger);
    }
    const needsStart = this.contractIds.some((id) => !this.cursors.has(id));
    if (needsStart && !process.env["START_LEDGER"]) {
      this.logger.warn(
        "[rpc-poller] START_LEDGER is not set; defaulting to ledger 1. A live RPC " +
          "endpoint's retention window will reject it, and the poller will then " +
          "resume from the oldest retained ledger and report the gap in /health. " +
          "Set START_LEDGER to a recent ledger to start where you intend."
      );
    }
    this.startLedger = Math.max(1, Number(process.env["START_LEDGER"] ?? 1));
  }

  /** The most recent error the poller surfaced, if any. */
  get lastError(): Error | undefined {
    return this.lastErrorValue;
  }

  /** Returns the cursor for a contract, or undefined if none was seen. */
  getCursor(contractId: string): string | undefined {
    return this.cursors.get(contractId);
  }

  /** Last error surfaced by the poller, if any. */
  getLastError(): Error | undefined {
    return this.lastErrorValue;
  }

  /** Point-in-time view of the poller, for `/health`. */
  health(): RpcPollerHealth {
    return {
      running: this.running,
      pollIntervalMs: this.pollIntervalMs,
      contracts: this.contractIds.length,
      ticks: this.ticks,
      eventsEmitted: this.eventsEmitted,
      lastError: this.lastErrorValue?.message ?? null,
      gaps: [...this.gaps],
    };
  }

  /**
   * Ask the RPC endpoint for `getHealth`. Rejects when it answers with an RPC
   * error, so a service can refuse to start against a broken endpoint.
   */
  async checkHealth(): Promise<{ status: string }> {
    return this.call<{ status: string }>("getHealth", {});
  }

  /** Begin polling every `pollIntervalMs`; safe to call more than once. */
  start(): void {
    if (this.running) return;
    this.running = true;
    this.timer = setInterval(() => {
      void this.runTick();
    }, this.pollIntervalMs);
    // Never hold the process open on the poller's own account.
    this.timer.unref();
    void this.runTick();
  }

  private runTick(): Promise<void> {
    // A tick already in progress is the one to wait for; an overlapping call
    // returns immediately and must not replace it.
    if (this.currentTick) return this.currentTick;
    const run = this.tick().finally(() => {
      if (this.currentTick === run) this.currentTick = undefined;
    });
    this.currentTick = run;
    return run;
  }

  /**
   * Stops polling and waits, up to `timeoutMs`, for the tick in progress
   * (including the deliveries it is awaiting) to finish. Returns false if it
   * had to give up. Anything not finished keeps its old cursor, so it is
   * redelivered after a restart rather than lost.
   */
  async stopAndDrain(timeoutMs: number): Promise<boolean> {
    this.stop();
    const tick = this.currentTick;
    if (!tick) return true;
    let timer: NodeJS.Timeout | undefined;
    const timeout = new Promise<false>((resolve) => {
      timer = setTimeout(() => resolve(false), timeoutMs);
    });
    try {
      return await Promise.race([tick.then(() => true as const), timeout]);
    } finally {
      clearTimeout(timer);
    }
  }

  /** Stop polling. Safe to call when not running. */
  stop(): void {
    this.running = false;
    if (this.timer) {
      clearInterval(this.timer);
      this.timer = undefined;
    }
  }

  /**
   * Poll all configured contracts once and return the decoded events. The
   * cursor moves as soon as a page is read, so use this for one-shot catch-ups
   * and tests; the running service goes through `tick()`, which moves the
   * cursor only after the page has been dispatched.
   */
  async poll(): Promise<PoolEvent[]> {
    return this.sweep();
  }

  /**
   * Reads each contract's next page. With `dispatch`, the page is handed over
   * first and its cursor is committed afterwards: a crash in between replays
   * the page (at-least-once) instead of skipping it.
   */
  private async sweep(dispatch?: (events: PoolEvent[]) => Promise<void>): Promise<PoolEvent[]> {
    const out: PoolEvent[] = [];
    for (const contractId of this.contractIds) {
      // After stop(), skip the contracts not yet reached; their cursors are untouched.
      if (dispatch && !this.running) break;
      let page: Page;
      try {
        page = await this.fetchPage(contractId);
      } catch (err) {
        this.recordError(err, `RPC getEvents failed for contract ${contractId}`);
        continue;
      }
      if (dispatch) await dispatch(page.events);
      try {
        this.commit(contractId, page.next);
      } catch (err) {
        // Not durable: keep the old cursor so the page is read again.
        this.recordError(err, `failed to persist the cursor for contract ${contractId}`);
        continue;
      }
      out.push(...page.events);
    }
    return out;
  }

  private async tick(): Promise<void> {
    // Skip a tick rather than overlap with the previous one, so a slow RPC
    // endpoint cannot queue up an unbounded number of concurrent requests.
    if (this.polling) return;
    this.polling = true;
    try {
      this.ticks += 1;
      await this.sweep(async (events) => {
        for (const event of events) {
          this.eventsEmitted += 1;
          try {
            await this.onEvent?.(event);
          } catch (err) {
            this.recordError(err, "event handler failed");
          }
        }
      });
    } finally {
      this.polling = false;
    }
  }

  private recordError(err: unknown, context: string): void {
    const e = err instanceof Error ? err : new Error(String(err));
    this.lastErrorValue = e;
    this.logger.error(`[rpc-poller] ${context}: ${e.message}`);
    this.onError?.(e);
  }

  /** Writes the cursor through the store, then updates memory (never the other way round). */
  private commit(contractId: string, next: CursorState | undefined): void {
    if (!next) return;
    this.store.saveCursor(contractId, next);
    this.cursors.set(contractId, next.cursor);
    if (next.ledger !== undefined) this.ledgers.set(contractId, next.ledger);
    this.startOverrides.delete(contractId);
  }

  private async fetchPage(contractId: string): Promise<Page> {
    try {
      return await this.requestPage(contractId);
    } catch (err) {
      // After downtime the saved cursor (or START_LEDGER) can fall out of RPC's
      // retention window. Resume from the oldest retained ledger instead of
      // failing the same request forever.
      if (!(await this.recoverFromRetentionGap(contractId, err))) throw err;
      return this.requestPage(contractId);
    }
  }

  private async requestPage(contractId: string): Promise<Page> {
    const cursor = this.cursors.get(contractId);
    const params: Record<string, unknown> = {
      filters: [{ type: "contract", contractIds: [contractId] }],
      limit: 1000,
    };
    if (cursor) {
      params["cursor"] = cursor;
    } else {
      params["startLedger"] = this.startOverrides.get(contractId) ?? this.startLedger;
    }

    const result = await this.call<RawGetEventsResult>("getEvents", params);
    const events = result.events ?? [];
    const decoded: PoolEvent[] = [];
    for (const raw of events) {
      const dec = decodeEvent(raw, this.contractKinds[raw.contractId ?? contractId]);
      if (dec) {
        decoded.push(dec);
      }
    }
    // RPC paginates by event id, so the last event of the page is the cursor
    // for the next one. Reading it from the event keeps a response that
    // omits the top-level cursor from replaying the whole page next tick.
    const last = events.at(-1);
    const nextCursor = result.cursor ?? last?.pagingToken;
    const ledger = last?.ledger ?? this.ledgers.get(contractId);
    const next = nextCursor
      ? { cursor: nextCursor, ...(ledger !== undefined ? { ledger } : {}) }
      : undefined;
    return { events: decoded, next };
  }

  /**
   * If `err` says the requested ledger or cursor is outside the retained
   * window, reset the contract to the oldest retained ledger, record the gap
   * and return true.
   */
  private async recoverFromRetentionGap(contractId: string, err: unknown): Promise<boolean> {
    if (
      !(err instanceof RawRpcError) ||
      err.code !== INVALID_PARAMS ||
      !/ledger range/i.test(err.message)
    ) {
      return false;
    }
    const range = await this.retainedRange(err);
    if (!range) return false;

    const cursor = this.cursors.get(contractId);
    const lastLedger =
      this.ledgers.get(contractId) ?? (cursor ? ledgerFromPagingToken(cursor) : undefined);
    const wanted = lastLedger ?? this.startOverrides.get(contractId) ?? this.startLedger;
    // Only a request that fell behind the window can be recovered by moving
    // forward. One ahead of the newest ledger just has to wait.
    if (wanted >= range.oldest) return false;

    const fromLedger = lastLedger !== undefined ? lastLedger + 1 : wanted;
    const gap: LedgerGap = {
      contractId,
      reason: cursor ? "cursor" : "start",
      fromLedger,
      toLedger: range.oldest - 1,
      resumedAt: range.oldest,
      detectedAt: new Date().toISOString(),
    };
    this.gaps.push(gap);
    if (this.gaps.length > MAX_GAPS) this.gaps.shift();
    this.logger.warn(
      `[rpc-poller] gap contract=${contractId} reason=${gap.reason} ledgers=${gap.fromLedger}-${gap.toLedger} ` +
        `resumedAt=${gap.resumedAt}: RPC no longer retains these ledgers, so their events were not read`,
    );

    this.cursors.delete(contractId);
    this.startOverrides.set(contractId, range.oldest);
    return true;
  }

  /** Oldest and newest retained ledger, from the error text or else `getHealth`. */
  private async retainedRange(err: RawRpcError): Promise<{ oldest: number; latest: number } | undefined> {
    const match = /ledger range:?\s*(\d+)\s*-\s*(\d+)/i.exec(err.message);
    if (match) {
      return { oldest: Number(match[1]), latest: Number(match[2]) };
    }
    try {
      const health = await this.call<{ oldestLedger?: number; latestLedger?: number }>("getHealth", {});
      if (typeof health.oldestLedger === "number" && typeof health.latestLedger === "number") {
        return { oldest: health.oldestLedger, latest: health.latestLedger };
      }
    } catch {
      // Fall through: the original error is reported.
    }
    return undefined;
  }

  private async call<T>(
    method: string,
    params: Record<string, unknown>
  ): Promise<T> {
    const id = ++this.requestId;
    const resp = await this.fetchFn(this.rpcUrl, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ jsonrpc: "2.0", id, method, params }),
    });
    if (!resp.ok) {
      throw new RawRpcError(
        `HTTP ${resp.status} from RPC ${method}`,
        resp.status
      );
    }
    const json = (await resp.json()) as JsonRpcResponse<T>;
    if (json.error) {
      throw new RawRpcError(
        `RPC error ${json.error.code}: ${json.error.message}`,
        json.error.code,
        json.error.data
      );
    }
    if (json.result === undefined) {
      throw new RawRpcError(`RPC response for ${method} had no result`);
    }
    return json.result;
  }
}

/**
 * Maps the raw (sometimes abbreviated) on-chain topic to the friendly name
 * documented for webhook subscribers, so an `eventType` filter can target it.
 * Topics not listed here pass through `decodeTopicName` unchanged.
 */
const TOPIC_NAME_ALIASES: Record<string, string> = {
  rm_liq: "remove_liquidity",
  rm_liq_1s: "remove_liquidity_one_sided",
};

/**
 * Decode a single RPC event into a PoolEvent.
 *
 * Topics and values are base64 XDR ScVals. The first topic is the
 * event name; the value is an event envelope. Versioned events carry
 * `(EVENT_SCHEMA_VERSION, payload)` as their data. Events that carry no topic
 * at all are skipped rather than forwarded with an empty type.
 *
 * `contractKind` (a directory name under `contracts/`, e.g. `amm`) selects the
 * field schema. Without it the payload keeps the generic shape, because the
 * same event name means different things in different contracts.
 */
export function decodeEvent(raw: RpcEvent, contractKind?: string): PoolEvent | undefined {
  const topics = raw.topic ?? [];
  if (topics.length === 0) {
    return undefined;
  }
  const rawName = decodeRawTopicName(topics[0]);
  const eventType = TOPIC_NAME_ALIASES[rawName] ?? rawName;
  const { schemaVersion, payload } = decodeValue(raw.value, { kind: contractKind, event: rawName });
  const event: PoolEvent = {
    id: raw.id,
    contractId: raw.contractId ?? "",
    eventType,
    schemaVersion,
    payload,
    ledger: raw.ledger,
    timestamp: raw.ledgerClosedAt ?? "",
    txHash: raw.txHash,
  };
  if (contractKind !== undefined) {
    event.contractKind = contractKind;
  }
  return event;
}

/** The on-chain topic name, before `TOPIC_NAME_ALIASES` is applied. */
function decodeRawTopicName(topic: string): string {
  try {
    // `scValToNative` is untyped, so narrow it here rather than leaking the
    // library's `any` through the rest of the decoding helpers.
    const native = scValToNative(xdr.ScVal.fromXDR(topic, "base64")) as unknown;
    return typeof native === "string" ? native : String(native);
  } catch {
    return topic;
  }
}

export function decodeTopicName(topic: string): string {
  const name = decodeRawTopicName(topic);
  return TOPIC_NAME_ALIASES[name] ?? name;
}

/** Identifies the schema to decode a payload with: contract kind + on-chain event name. */
export interface DecodeContext {
  kind?: string;
  /** The on-chain topic name (before aliasing), e.g. `rm_liq`. */
  event: string;
}

export function decodeValue(
  value?: string,
  ctx?: DecodeContext
): {
  schemaVersion: number;
  payload: Record<string, unknown>;
} {
  if (!value) {
    return { schemaVersion: 0, payload: {} };
  }
  try {
    const native = scValToNative(xdr.ScVal.fromXDR(value, "base64")) as unknown;
    return unwrapVersionedEnvelope(native, ctx);
  } catch {
    return { schemaVersion: 0, payload: { raw: value } };
  }
}

/** Maps a decoded payload body onto the variant's field names. */
function nameFields(
  body: unknown,
  fields: string[],
  scalar: boolean
): Record<string, unknown> {
  const out: Record<string, unknown> = {};
  if (scalar) {
    out[fields[0]] = normalise(body);
    return out;
  }
  const items = body as unknown[];
  fields.forEach((field, i) => {
    out[field] = normalise(items[i]);
  });
  return out;
}

export function unwrapVersionedEnvelope(
  native: unknown,
  ctx?: DecodeContext
): {
  schemaVersion: number;
  payload: Record<string, unknown>;
} {
  const envelope = Array.isArray(native) && native.length === 2;
  const [version, body] = envelope ? (native as [unknown, unknown]) : [undefined, undefined];
  const schemaVersion = typeof version === "number" ? version : Number(version);

  if (ctx?.kind !== undefined) {
    if (envelope && Number.isInteger(schemaVersion)) {
      const scalar = !Array.isArray(body);
      const arity = scalar ? 1 : (body as unknown[]).length;
      const variant = findVariant(ctx.kind, ctx.event, { version: schemaVersion, arity, scalar });
      if (variant) {
        return { schemaVersion, payload: nameFields(body, variant.fields, scalar) };
      }
    }
    // Some contracts publish a payload with no (version, payload) envelope.
    if (Array.isArray(native)) {
      const variant = findVariant(ctx.kind, ctx.event, { version: undefined, arity: native.length });
      if (variant) {
        return { schemaVersion: 0, payload: nameFields(native, variant.fields, false) };
      }
    }
  }

  if (envelope) {
    return {
      schemaVersion: Number.isFinite(schemaVersion) ? schemaVersion : 0,
      payload: toPlainObject(body),
    };
  }
  return { schemaVersion: 0, payload: toPlainObject(native) };
}

export function toPlainObject(value: unknown): Record<string, unknown> {
  if (value === null || typeof value !== "object") {
    return { value: normalise(value) };
  }
  if (Array.isArray(value)) {
    return { value: value.map(normalise) };
  }
  const out: Record<string, unknown> = {};
  for (const [k, v] of Object.entries(value as Record<string, unknown>)) {
    out[k] = normalise(v);
  }
  return out;
}

export function normalise(value: unknown): unknown {
  if (typeof value === "bigint") {
    return value.toString();
  }
  if (Array.isArray(value)) {
    return value.map(normalise);
  }
  if (value && typeof value === "object") {
    const out: Record<string, unknown> = {};
    for (const [k, v] of Object.entries(value as Record<string, unknown>)) {
      out[k] = normalise(v);
    }
    return out;
  }
  return value;
}
