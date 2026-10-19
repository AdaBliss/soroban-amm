/**
 * Webhook request signing (issue #1123).
 *
 * Each delivery to a subscription that has a secret carries
 *
 *   X-Webhook-Signature: t=<unix seconds>,v1=<hex>
 *
 * where `v1` is HMAC-SHA256 keyed with the subscription secret over the string
 * `"<t>.<raw request body>"`. The secret itself never goes on the wire.
 * Receivers recompute the HMAC, compare in constant time, and reject a
 * timestamp outside their tolerance window to stop replays. `verifySignature`
 * below is the reference verifier the README documents.
 */

import { createHmac, timingSafeEqual } from "node:crypto";

export const SIGNATURE_HEADER = "X-Webhook-Signature";
export const EVENT_ID_HEADER = "X-Webhook-Id";
/** Replay window receivers should enforce, in seconds. */
export const DEFAULT_TOLERANCE_SECONDS = 300;

/** Computes the hex `v1` signature for `body` signed at `timestamp`. */
export function computeSignature(secret: string, timestamp: number, body: string): string {
  return createHmac("sha256", secret).update(`${timestamp}.${body}`).digest("hex");
}

/** Builds the `X-Webhook-Signature` header value. */
export function signPayload(
  secret: string,
  body: string,
  timestamp: number = Math.floor(Date.now() / 1000),
): string {
  return `t=${timestamp},v1=${computeSignature(secret, timestamp, body)}`;
}

export type SignatureFailure =
  | "malformed_header"
  | "timestamp_out_of_tolerance"
  | "signature_mismatch";

export type VerifyResult = { valid: true } | { valid: false; reason: SignatureFailure };

/**
 * Verifies an `X-Webhook-Signature` header against the raw body.
 *
 * `body` must be the exact bytes received (do not re-serialise parsed JSON).
 */
export function verifySignature(opts: {
  secret: string;
  body: string;
  header: string | undefined;
  toleranceSeconds?: number;
  /** Current time in unix seconds; injectable for tests. */
  now?: number;
}): VerifyResult {
  const tolerance = opts.toleranceSeconds ?? DEFAULT_TOLERANCE_SECONDS;
  const now = opts.now ?? Math.floor(Date.now() / 1000);

  const parts = new Map<string, string>();
  for (const piece of (opts.header ?? "").split(",")) {
    const eq = piece.indexOf("=");
    if (eq > 0) parts.set(piece.slice(0, eq).trim(), piece.slice(eq + 1).trim());
  }
  const t = parts.get("t");
  const v1 = parts.get("v1");
  if (t === undefined || v1 === undefined || !/^\d+$/.test(t) || !/^[0-9a-f]{64}$/.test(v1)) {
    return { valid: false, reason: "malformed_header" };
  }

  const timestamp = Number(t);
  if (Math.abs(now - timestamp) > tolerance) {
    return { valid: false, reason: "timestamp_out_of_tolerance" };
  }

  const expected = Buffer.from(computeSignature(opts.secret, timestamp, opts.body), "hex");
  const received = Buffer.from(v1, "hex");
  if (expected.length !== received.length || !timingSafeEqual(expected, received)) {
    return { valid: false, reason: "signature_mismatch" };
  }
  return { valid: true };
}
