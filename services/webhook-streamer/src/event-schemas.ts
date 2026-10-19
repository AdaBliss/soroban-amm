/**
 * Named payload fields for contract events (issue #1124).
 *
 * The table lives in `event-schemas.json`, keyed by contract kind (the
 * directory under `contracts/`), on-chain event name, schema version and
 * payload arity. The same event name carries different payloads in different
 * contracts, and one contract can emit a name in several shapes, so a lookup
 * needs all of those. `scripts/check_event_schemas.mjs` fails CI when a
 * contract emit site and this file disagree, which is what keeps it honest.
 *
 * A payload that does not match exactly is never given names: the caller falls
 * back to the generic `{ value: [...] }` shape.
 */

import schemaFile from "./event-schemas.json" with { type: "json" };

export interface EventVariant {
  /** `EVENT_SCHEMA_VERSION` this layout was emitted under. */
  version: number;
  /** Number of on-chain topics, the event name included. */
  topics: number;
  /** `scalar` payloads are a bare value, not a one-field tuple. */
  shape: "tuple" | "scalar";
  fields: string[];
  /** `false` for events published without the `(version, payload)` envelope. */
  versioned?: boolean;
}

interface SchemaFile {
  kinds: Record<string, { events: Record<string, EventVariant[]> }>;
}

const SCHEMAS = schemaFile as SchemaFile;

/** Contract kinds that have a schema, i.e. the valid `CONTRACT_KINDS` values. */
export const KNOWN_CONTRACT_KINDS: readonly string[] = Object.keys(SCHEMAS.kinds);

export function isKnownContractKind(kind: string): boolean {
  return Object.hasOwn(SCHEMAS.kinds, kind);
}

function variantsFor(kind: string | undefined, event: string): EventVariant[] {
  if (kind === undefined || !isKnownContractKind(kind)) return [];
  const events = SCHEMAS.kinds[kind].events;
  return Object.hasOwn(events, event) ? events[event] : [];
}

/**
 * Finds the variant describing a payload with `arity` fields at `version`,
 * or `undefined` when the kind, event, version or arity is not an exact match.
 */
export function findVariant(
  kind: string | undefined,
  event: string,
  opts: { version: number | undefined; arity: number; scalar?: boolean },
): EventVariant | undefined {
  const versioned = opts.version !== undefined;
  return variantsFor(kind, event).find(
    (v) =>
      (v.versioned !== false) === versioned &&
      (!versioned || v.version === opts.version) &&
      v.fields.length === opts.arity &&
      (v.shape === "scalar") === Boolean(opts.scalar),
  );
}

/**
 * Parses `CONTRACT_KINDS`: comma-separated `<contractId>:<kind>` pairs.
 * Throws on a malformed pair or an unknown kind so a typo fails at startup
 * instead of silently leaving a contract's events unnamed.
 */
export function parseContractKinds(value: string | undefined): Record<string, string> {
  const out: Record<string, string> = {};
  for (const pair of (value ?? "").split(",").map((s) => s.trim()).filter(Boolean)) {
    const colon = pair.lastIndexOf(":");
    const contractId = pair.slice(0, colon).trim();
    const kind = pair.slice(colon + 1).trim();
    if (colon <= 0 || !contractId || !kind) {
      throw new Error(`CONTRACT_KINDS entry "${pair}" must look like <contractId>:<kind>`);
    }
    if (!isKnownContractKind(kind)) {
      throw new Error(
        `CONTRACT_KINDS entry "${pair}" has unknown kind "${kind}". Known kinds: ${KNOWN_CONTRACT_KINDS.join(", ")}`,
      );
    }
    out[contractId] = kind;
  }
  return out;
}
