/**
 * Validates webhook registration URLs to prevent SSRF: a registered webhook
 * is fetched by this service's own network egress on a recurring, retried
 * schedule, so an unvalidated URL lets any caller of the (unauthenticated)
 * management API turn this service into a proxy into its private network.
 */

export class InvalidWebhookUrlError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "InvalidWebhookUrlError";
  }
}

const ALLOWED_SCHEMES = new Set(["http:", "https:"]);

function ipv4ToInt(parts: number[]): number {
  return ((parts[0] << 24) | (parts[1] << 16) | (parts[2] << 8) | parts[3]) >>> 0;
}

function inIpv4Range(ip: number, base: string, prefixLen: number): boolean {
  const baseParts = base.split(".").map(Number);
  const baseInt = ipv4ToInt(baseParts);
  const mask = prefixLen === 0 ? 0 : (~0 << (32 - prefixLen)) >>> 0;
  return (ip & mask) === (baseInt & mask);
}

function isPrivateOrReservedIpv4(hostname: string): boolean {
  const match = /^(\d{1,3})\.(\d{1,3})\.(\d{1,3})\.(\d{1,3})$/.exec(hostname);
  if (!match) return false;
  const parts = match.slice(1, 5).map(Number);
  if (parts.some((p) => p > 255)) return false;
  const ip = ipv4ToInt(parts);

  return (
    inIpv4Range(ip, "127.0.0.0", 8) || // loopback
    inIpv4Range(ip, "169.254.0.0", 16) || // link-local (cloud metadata)
    inIpv4Range(ip, "10.0.0.0", 8) || // RFC1918
    inIpv4Range(ip, "172.16.0.0", 12) || // RFC1918
    inIpv4Range(ip, "192.168.0.0", 16) || // RFC1918
    hostname === "0.0.0.0"
  );
}

function isPrivateOrReservedIpv6(hostname: string): boolean {
  // Strip brackets/zone-id and normalise for comparison.
  const host = hostname.replace(/^\[|\]$/g, "").toLowerCase();
  if (host === "::1") return true; // loopback
  if (host.startsWith("fe80:") || host.startsWith("fe80::")) return true; // link-local
  // Unique local addresses (fc00::/7), the private-range analogue for IPv6.
  if (/^f[cd][0-9a-f]{2}:/.test(host)) return true;
  return false;
}

/**
 * Throws InvalidWebhookUrlError if `raw` is not a safe webhook target.
 *
 * Set WEBHOOK_ALLOW_PRIVATE_TARGETS=true to bypass the private/loopback/
 * link-local checks for local development against a same-host receiver.
 */
export function validateWebhookUrl(raw: string): void {
  let parsed: URL;
  try {
    parsed = new URL(raw);
  } catch {
    throw new InvalidWebhookUrlError(`url must be a valid absolute URL, got "${raw}"`);
  }

  if (!ALLOWED_SCHEMES.has(parsed.protocol)) {
    throw new InvalidWebhookUrlError(
      `url scheme must be http or https, got "${parsed.protocol}"`,
    );
  }

  const allowPrivate = process.env["WEBHOOK_ALLOW_PRIVATE_TARGETS"] === "true";
  if (allowPrivate) return;

  const hostname = parsed.hostname.toLowerCase();

  if (hostname === "localhost") {
    throw new InvalidWebhookUrlError(`url must not target localhost, got "${raw}"`);
  }
  if (isPrivateOrReservedIpv4(hostname)) {
    throw new InvalidWebhookUrlError(
      `url must not target a loopback, link-local, or private-range address, got "${raw}"`,
    );
  }
  if (isPrivateOrReservedIpv6(hostname)) {
    throw new InvalidWebhookUrlError(
      `url must not target a loopback or link-local address, got "${raw}"`,
    );
  }
}
