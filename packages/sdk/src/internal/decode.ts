/**
 * Typed decoding of `scValToNative` output shared by every SDK client.
 *
 * `scValToNative` returns `any`, so field reads on its result are `unknown`.
 * Coercing those with `String(...)` accepts every shape, including objects,
 * which silently decode as "[object Object]" (and then surface far away as a
 * `BigInt` SyntaxError or a bogus address). These helpers accept exactly the
 * shapes a contract return value can take for each kind of field and throw a
 * descriptive `TypeError` for anything else.
 */

function describe(value: unknown): string {
  if (value === null) return "null";
  if (Array.isArray(value)) return `array(${value.length})`;
  return typeof value;
}

export class ContractDecodeError extends TypeError {
  constructor(structName: string, field: string, keys: string[]) {
    super(`failed to decode ${structName}.${field}; present keys: ${keys.join(", ") || "<none>"}`);
    this.name = "ContractDecodeError";
  }
}

export function requireValue(
  value: Record<string, unknown>,
  field: string,
  structName: string
): unknown {
  if (value[field] === null || value[field] === undefined) {
    throw new ContractDecodeError(structName, field, Object.keys(value));
  }
  return value[field];
}

export function requireBigInt(
  value: Record<string, unknown>,
  field: string,
  structName: string
): bigint {
  return toBigInt(requireValue(value, field, structName));
}

export function requireText(
  value: Record<string, unknown>,
  field: string,
  structName: string
): string {
  return toText(requireValue(value, field, structName));
}

export function requireBoolean(
  value: Record<string, unknown>,
  field: string,
  structName: string
): boolean {
  const result = requireValue(value, field, structName);
  if (typeof result !== "boolean") {
    throw new TypeError(`expected a boolean ${structName}.${field}, got ${describe(result)}`);
  }
  return result;
}

/** Decode an explicitly optional integer (`Option<T>`). */
export function optionalBigInt(value: unknown): bigint | null {
  if (value === null || value === undefined) return null;
  if (Array.isArray(value)) return value.length === 0 ? null : toBigInt(value[0]);
  return toBigInt(value);
}

/**
 * Decode an integer value (`u32`, `u64`, `i128`, ...). Missing required values
 * are rejected; callers decoding an optional field must use `optionalBigInt`.
 */
export function toBigInt(value: unknown): bigint {
  if (value === null || value === undefined) {
    throw new TypeError(`expected an integer contract value, got ${describe(value)}`);
  }
  if (typeof value === "bigint") return value;
  if (typeof value === "number" || typeof value === "string") return BigInt(value);
  throw new TypeError(`expected an integer contract value, got ${describe(value)}`);
}

/** Decode an address, string or symbol field. */
export function toText(value: unknown): string {
  if (typeof value === "string") return value;
  if (typeof value === "number" || typeof value === "bigint" || typeof value === "boolean") {
    return String(value);
  }
  throw new TypeError(`expected a string contract value, got ${describe(value)}`);
}

/**
 * Decode a `#[contracttype]` enum to its variant name. A unit variant decodes
 * from `scValToNative` as a one-element array (`["Active"]`); a bare symbol
 * decodes as a string. A missing value decodes as `fallback`.
 */
export function toVariant(value: unknown, fallback: string): string {
  if (value === null || value === undefined) return fallback;
  if (typeof value === "string") return value;
  if (Array.isArray(value) && typeof value[0] === "string") return value[0];
  throw new TypeError(`expected a contract enum value, got ${describe(value)}`);
}
