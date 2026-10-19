/**
 * Time handling for the whole service.
 *
 * Every timestamp the API stores, filters on or returns is **seconds since the
 * Unix epoch**, the unit the chain and `RpcIngester` already use. The
 * `Seconds` brand makes a raw `Date.now()` (milliseconds) a compile error
 * wherever a timestamp is expected, so the unit can't drift again. Convert
 * once, at the boundary, with the helpers below.
 */

export type Seconds = number & { readonly __unit: "seconds" };

/** Returns the current time. Injectable so tests can control "now". */
export type Clock = () => Seconds;

/** Brands a number that is already in seconds. */
export function asSeconds(value: number): Seconds {
  return value as Seconds;
}

/** Converts a millisecond epoch value (e.g. `Date.now()`) to seconds. */
export function msToSeconds(ms: number): Seconds {
  return Math.floor(ms / 1000) as Seconds;
}

/** Converts an ISO-8601 string (e.g. a ledger `closedAt`) to seconds. */
export function isoToSeconds(iso: string): Seconds {
  return msToSeconds(Date.parse(iso));
}

export const systemClock: Clock = () => msToSeconds(Date.now());
