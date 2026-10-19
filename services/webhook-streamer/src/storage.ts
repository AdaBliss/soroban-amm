/**
 * Durable state for the webhook-streamer (issue #1125).
 *
 * Subscriptions, dead letters and the poller's per-contract cursors used to
 * live in memory only, so a restart dropped every integrator's registration
 * and every failed delivery waiting for replay, and the poller forgot where it
 * had stopped. A `StateStore` is the small seam those three components write
 * through:
 *
 *   - `MemoryStateStore` keeps today's behaviour (and is what tests use).
 *   - `FileStateStore` keeps everything in one JSON file under `DATA_DIR`,
 *     replaced atomically on every write (temp file + fsync + rename), so a
 *     crash leaves either the old state or the new one, never half of it.
 *
 * The interface is synchronous on purpose: the state is small, and it keeps
 * `register()`/`add()` returning only once the change is durable.
 */

import { closeSync, fsyncSync, mkdirSync, openSync, readFileSync, renameSync, writeSync, existsSync, unlinkSync } from "node:fs";
import { dirname, join } from "node:path";
import type { DeadLetter, WebhookSubscription } from "./types.js";

/** Where the poller resumed for one contract. */
export interface CursorState {
  /** Opaque `getEvents` cursor (the last event's paging token). */
  cursor: string;
  /** Ledger of the last event ingested, when known. */
  ledger?: number;
}

export interface SubscriptionState {
  subscriptions: WebhookSubscription[];
  /** Next subscription id to hand out. */
  nextId: number;
}

export interface DeadLetterState {
  entries: DeadLetter[];
  /** Next dead-letter id to hand out. */
  nextId: number;
}

export interface StateStore {
  loadSubscriptions(): SubscriptionState;
  saveSubscriptions(state: SubscriptionState): void;
  loadDeadLetters(): DeadLetterState;
  saveDeadLetters(state: DeadLetterState): void;
  loadCursors(): Record<string, CursorState>;
  saveCursor(contractId: string, state: CursorState): void;
  /** Flushes anything buffered. Both implementations write through, so this is a no-op today. */
  flush(): void;
}

/** In-memory store: nothing survives the process. */
export class MemoryStateStore implements StateStore {
  private subscriptions: SubscriptionState = { subscriptions: [], nextId: 1 };
  private deadLetters: DeadLetterState = { entries: [], nextId: 1 };
  private cursors: Record<string, CursorState> = {};

  loadSubscriptions(): SubscriptionState {
    return structuredClone(this.subscriptions);
  }
  saveSubscriptions(state: SubscriptionState): void {
    this.subscriptions = structuredClone(state);
  }
  loadDeadLetters(): DeadLetterState {
    return structuredClone(this.deadLetters);
  }
  saveDeadLetters(state: DeadLetterState): void {
    this.deadLetters = structuredClone(state);
  }
  loadCursors(): Record<string, CursorState> {
    return structuredClone(this.cursors);
  }
  saveCursor(contractId: string, state: CursorState): void {
    this.cursors[contractId] = { ...state };
  }
  flush(): void {}
}

export class StateFileError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "StateFileError";
  }
}

interface StateFile {
  version: 1;
  subscriptions: SubscriptionState;
  deadLetters: DeadLetterState;
  cursors: Record<string, CursorState>;
}

export const STATE_FILE_NAME = "state.json";

function emptyState(): StateFile {
  return {
    version: 1,
    subscriptions: { subscriptions: [], nextId: 1 },
    deadLetters: { entries: [], nextId: 1 },
    cursors: {},
  };
}

/**
 * Single-file JSON store. The file holds subscription secrets (they are needed
 * to sign deliveries and replays), so it is created readable by the owner only.
 */
export class FileStateStore implements StateStore {
  readonly path: string;
  private state: StateFile;

  constructor(dataDir: string) {
    this.path = join(dataDir, STATE_FILE_NAME);
    mkdirSync(dataDir, { recursive: true, mode: 0o700 });
    this.state = this.read();
  }

  private read(): StateFile {
    if (!existsSync(this.path)) return emptyState();
    let parsed: unknown;
    try {
      parsed = JSON.parse(readFileSync(this.path, "utf8"));
    } catch (err) {
      throw new StateFileError(
        `Cannot read ${this.path}: ${err instanceof Error ? err.message : String(err)}. ` +
          "Refusing to start with empty state. Fix or move the file aside to start fresh.",
      );
    }
    const file = parsed as Partial<StateFile> | null;
    if (!file || typeof file !== "object" || file.version !== 1) {
      throw new StateFileError(
        `${this.path} has an unsupported format (expected version 1). ` +
          "Refusing to start; move the file aside to start fresh.",
      );
    }
    const empty = emptyState();
    return {
      version: 1,
      subscriptions: file.subscriptions ?? empty.subscriptions,
      deadLetters: file.deadLetters ?? empty.deadLetters,
      cursors: file.cursors ?? empty.cursors,
    };
  }

  /** Writes the whole state atomically: temp file, fsync, rename. */
  private write(): void {
    const tmp = `${this.path}.${process.pid}.tmp`;
    const fd = openSync(tmp, "w", 0o600);
    try {
      writeSync(fd, JSON.stringify(this.state));
      fsyncSync(fd);
    } finally {
      closeSync(fd);
    }
    try {
      renameSync(tmp, this.path);
    } catch (err) {
      try { unlinkSync(tmp); } catch { /* best effort */ }
      throw err;
    }
    // Persist the rename itself.
    try {
      const dirFd = openSync(dirname(this.path), "r");
      try { fsyncSync(dirFd); } finally { closeSync(dirFd); }
    } catch {
      // Directory fsync is not supported everywhere; the file fsync above is the important one.
    }
  }

  loadSubscriptions(): SubscriptionState {
    return structuredClone(this.state.subscriptions);
  }
  saveSubscriptions(state: SubscriptionState): void {
    this.state.subscriptions = structuredClone(state);
    this.write();
  }
  loadDeadLetters(): DeadLetterState {
    return structuredClone(this.state.deadLetters);
  }
  saveDeadLetters(state: DeadLetterState): void {
    this.state.deadLetters = structuredClone(state);
    this.write();
  }
  loadCursors(): Record<string, CursorState> {
    return structuredClone(this.state.cursors);
  }
  saveCursor(contractId: string, state: CursorState): void {
    this.state.cursors[contractId] = { ...state };
    this.write();
  }
  flush(): void {
    // Every save writes through, so there is nothing buffered.
  }
}
