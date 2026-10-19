/**
 * Security helpers for the management API (issue #1123): bearer-token
 * authentication, a bounded request-body reader and request validation.
 *
 * Kept free of side effects so each piece can be tested without starting the
 * service.
 */

import { createHash, timingSafeEqual } from "node:crypto";
import type { IncomingMessage } from "node:http";

// ── Authentication ──────────────────────────────────────────────────────────

export interface AuthConfig {
  /** Bearer token required on every route except minimal `/health`. */
  token?: string;
  /** Explicit opt-out for local development. */
  insecure: boolean;
}

export class AuthConfigError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "AuthConfigError";
  }
}

/**
 * Reads `MANAGEMENT_API_TOKEN` / `MANAGEMENT_API_INSECURE`. Throws when the
 * management routes would otherwise start with no authentication, so a
 * forgotten variable fails closed instead of exposing the API.
 */
export function resolveAuthConfig(env: NodeJS.ProcessEnv): AuthConfig {
  const token = env["MANAGEMENT_API_TOKEN"]?.trim() || undefined;
  const insecure = env["MANAGEMENT_API_INSECURE"] === "true";
  if (!token && !insecure) {
    throw new AuthConfigError(
      "MANAGEMENT_API_TOKEN is not set. Set it to a long random string, or set " +
        "MANAGEMENT_API_INSECURE=true to run the management API without " +
        "authentication (local development only).",
    );
  }
  return { token, insecure };
}

/** Compares two strings in constant time, whatever their lengths. */
export function safeEqual(a: string, b: string): boolean {
  // Hashing first gives equal-length buffers, so the compare never leaks the
  // token length through an early return.
  const ha = createHash("sha256").update(a).digest();
  const hb = createHash("sha256").update(b).digest();
  return timingSafeEqual(ha, hb);
}

/** True when the request carries `Authorization: Bearer <configured token>`. */
export function isAuthorized(req: IncomingMessage, config: AuthConfig): boolean {
  if (config.insecure && !config.token) return true;
  if (!config.token) return false;
  const header = req.headers["authorization"];
  if (typeof header !== "string") return false;
  const match = /^Bearer +(\S+)$/i.exec(header);
  if (!match) return false;
  return safeEqual(match[1], config.token);
}

// ── Bounded body reading ────────────────────────────────────────────────────

export const DEFAULT_MAX_BODY_BYTES = 64 * 1024;

export class BodyTooLargeError extends Error {
  constructor(readonly limit: number) {
    super(`request body exceeds ${limit} bytes`);
    this.name = "BodyTooLargeError";
  }
}

/**
 * Reads the request body, rejecting as soon as it exceeds `limit` bytes. The
 * declared Content-Length is checked first so an honest oversized request is
 * refused before any of its body is read; the running total covers chunked or
 * lying clients.
 */
export function readBody(req: IncomingMessage, limit: number): Promise<string> {
  return new Promise((resolve, reject) => {
    const declared = Number(req.headers["content-length"]);
    if (Number.isFinite(declared) && declared > limit) {
      reject(new BodyTooLargeError(limit));
      return;
    }

    const chunks: Buffer[] = [];
    let received = 0;
    const cleanup = () => {
      req.off("data", onData);
      req.off("end", onEnd);
      req.off("error", onError);
    };
    const onData = (chunk: Buffer) => {
      received += chunk.length;
      if (received > limit) {
        cleanup();
        chunks.length = 0;
        reject(new BodyTooLargeError(limit));
        return;
      }
      chunks.push(chunk);
    };
    const onEnd = () => {
      cleanup();
      resolve(Buffer.concat(chunks).toString("utf8"));
    };
    const onError = (err: Error) => {
      cleanup();
      reject(err);
    };
    req.on("data", onData);
    req.on("end", onEnd);
    req.on("error", onError);
  });
}

// ── Request validation ──────────────────────────────────────────────────────

export const MAX_URL_LENGTH = 2048;
export const MAX_CONTRACT_ID_LENGTH = 128;
export const MAX_EVENT_TYPE_LENGTH = 64;
export const MAX_SECRET_LENGTH = 256;

export interface WebhookRegistration {
  url: string;
  contractId?: string;
  eventType?: string;
  secret?: string;
}

export class ValidationError extends Error {
  constructor(
    readonly field: string,
    message: string,
  ) {
    super(message);
    this.name = "ValidationError";
  }
}

export class InvalidJsonError extends Error {
  constructor() {
    super("invalid JSON");
    this.name = "InvalidJsonError";
  }
}

function optionalString(
  body: Record<string, unknown>,
  field: string,
  maxLength: number,
): string | undefined {
  const value = body[field];
  if (value === undefined) return undefined;
  if (typeof value !== "string") {
    throw new ValidationError(field, `${field} must be a string`);
  }
  if (value.length > maxLength) {
    throw new ValidationError(field, `${field} must be at most ${maxLength} characters`);
  }
  return value;
}

/**
 * Parses and validates a `POST /webhooks` body. Throws `InvalidJsonError` for
 * unparseable input and `ValidationError` (naming the field) for bad fields,
 * so the two can be answered with distinct messages.
 */
export function parseWebhookRegistration(raw: string): WebhookRegistration {
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch {
    throw new InvalidJsonError();
  }
  if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) {
    throw new ValidationError("body", "body must be a JSON object");
  }
  const body = parsed as Record<string, unknown>;

  const url = body["url"];
  if (url === undefined) throw new ValidationError("url", "url is required");
  if (typeof url !== "string") throw new ValidationError("url", "url must be a string");
  if (url.trim() === "") throw new ValidationError("url", "url must not be empty");
  if (url.length > MAX_URL_LENGTH) {
    throw new ValidationError("url", `url must be at most ${MAX_URL_LENGTH} characters`);
  }

  const secret = optionalString(body, "secret", MAX_SECRET_LENGTH);
  if (secret === "") throw new ValidationError("secret", "secret must not be empty");

  return {
    url,
    contractId: optionalString(body, "contractId", MAX_CONTRACT_ID_LENGTH),
    eventType: optionalString(body, "eventType", MAX_EVENT_TYPE_LENGTH),
    secret,
  };
}
