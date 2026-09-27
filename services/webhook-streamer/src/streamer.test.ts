/**
 * Unit tests for the webhook-streamer service (issue #306).
 * Run with: node --test dist/streamer.test.js
 */

import assert from "node:assert/strict";
import { afterEach, beforeEach, describe, it } from "node:test";
import { createServer, type Server } from "node:http";
import type { AddressInfo } from "node:net";
import { WebhookRegistry } from "./registry.js";
import {
    WebhookDispatcher,
    fullJitterDelay,
    isRetryableStatus,
    parseRetryAfter,
} from "./dispatcher.js";
import { DeadLetterQueue } from "./dead-letter.js";
import { CircuitBreaker } from "./circuit-breaker.js";
import {
    InvalidWebhookUrlError,
    validateWebhookUrl,
} from "./url-validation.js";
import type { PoolEvent } from "./types.js";

describe("WebhookRegistry", () => {
    it("registers and lists webhooks", () => {
        const reg = new WebhookRegistry();
        const sub = reg.register("https://example.com/hook");
        assert.equal(reg.size, 1);
        assert.equal(reg.list().length, 1);
        assert.equal(sub.url, "https://example.com/hook");
    });

    it("unregisters a webhook", () => {
        const reg = new WebhookRegistry();
        const sub = reg.register("https://example.com/hook");
        assert.equal(reg.unregister(sub.id), true);
        assert.equal(reg.size, 0);
        assert.equal(reg.unregister(sub.id), false);
    });

    it("filters matching subscriptions by contractId", () => {
        const reg = new WebhookRegistry();
        reg.register("https://a.com", { contractId: "CONTRACT_A" });
        reg.register("https://b.com", { contractId: "CONTRACT_B" });
        reg.register("https://all.com"); // no filter

        const matches = reg.matching("CONTRACT_A", "swap");
        assert.equal(matches.length, 2); // CONTRACT_A + catch-all
        assert.ok(matches.some((s) => s.url === "https://a.com"));
        assert.ok(matches.some((s) => s.url === "https://all.com"));
    });

    it("filters matching subscriptions by eventType", () => {
        const reg = new WebhookRegistry();
        reg.register("https://swap.com", { eventType: "swap" });
        reg.register("https://mint.com", { eventType: "mint_pos" });
        reg.register("https://all.com");

        const matches = reg.matching("ANY_CONTRACT", "swap");
        assert.equal(matches.length, 2); // swap + catch-all
        assert.ok(matches.some((s) => s.url === "https://swap.com"));
        assert.ok(matches.some((s) => s.url === "https://all.com"));
    });

    it("returns empty array when no subscriptions match", () => {
        const reg = new WebhookRegistry();
        reg.register("https://other.com", { contractId: "OTHER" });
        const matches = reg.matching("MY_CONTRACT", "swap");
        assert.equal(matches.length, 0);
    });

    it("rejects an SSRF-prone URL and does not register it", () => {
        const reg = new WebhookRegistry();
        assert.throws(
            () => reg.register("http://169.254.169.254/latest/meta-data/"),
            InvalidWebhookUrlError,
        );
        assert.equal(reg.size, 0);
    });
});

describe("validateWebhookUrl (SSRF protection)", () => {
    const original = process.env["WEBHOOK_ALLOW_PRIVATE_TARGETS"];
    function resetEnv() {
        if (original === undefined)
            delete process.env["WEBHOOK_ALLOW_PRIVATE_TARGETS"];
        else process.env["WEBHOOK_ALLOW_PRIVATE_TARGETS"] = original;
    }

    it("accepts a normal public https URL", () => {
        assert.doesNotThrow(() =>
            validateWebhookUrl("https://example.com/hook"),
        );
    });

    it("rejects the cloud metadata link-local address", () => {
        assert.throws(
            () =>
                validateWebhookUrl("http://169.254.169.254/latest/meta-data/"),
            InvalidWebhookUrlError,
        );
    });

    it("rejects loopback addresses", () => {
        assert.throws(
            () => validateWebhookUrl("http://127.0.0.1:9999/x"),
            InvalidWebhookUrlError,
        );
        assert.throws(
            () => validateWebhookUrl("http://localhost:9999/x"),
            InvalidWebhookUrlError,
        );
        assert.throws(
            () => validateWebhookUrl("http://[::1]:9999/x"),
            InvalidWebhookUrlError,
        );
    });

    it("rejects RFC1918 private ranges", () => {
        assert.throws(
            () => validateWebhookUrl("http://10.0.0.5/x"),
            InvalidWebhookUrlError,
        );
        assert.throws(
            () => validateWebhookUrl("http://172.16.0.5/x"),
            InvalidWebhookUrlError,
        );
        assert.throws(
            () => validateWebhookUrl("http://192.168.1.5/x"),
            InvalidWebhookUrlError,
        );
    });

    it("rejects the bare 0.0.0.0 IP literal", () => {
        assert.throws(
            () => validateWebhookUrl("http://0.0.0.0/x"),
            InvalidWebhookUrlError,
        );
    });

    it("rejects non-http(s) schemes", () => {
        assert.throws(
            () => validateWebhookUrl("javascript:alert(1)"),
            InvalidWebhookUrlError,
        );
        assert.throws(
            () => validateWebhookUrl("file:///etc/passwd"),
            InvalidWebhookUrlError,
        );
    });

    it("rejects unparseable URLs", () => {
        assert.throws(
            () => validateWebhookUrl("not a url"),
            InvalidWebhookUrlError,
        );
    });

    it("allows a private-range URL when WEBHOOK_ALLOW_PRIVATE_TARGETS=true", () => {
        process.env["WEBHOOK_ALLOW_PRIVATE_TARGETS"] = "true";
        try {
            assert.doesNotThrow(() =>
                validateWebhookUrl("http://127.0.0.1:9999/x"),
            );
        } finally {
            resetEnv();
        }
    });
});

describe("PoolEvent shape", () => {
    it("conforms to expected interface", () => {
        const event: PoolEvent = {
            id: "evt-1",
            contractId: "CABC123",
            eventType: "swap",
            ledger: 1234,
            timestamp: "2026-06-01T00:00:00Z",
            payload: { amountIn: 1000, amountOut: 990 },
        };
        assert.equal(event.eventType, "swap");
        assert.equal(event.payload["amountIn"], 1000);
    });
});

// ── Delivery reliability (issue #720) ───────────────────────────────────────
//
// Every test below drives a real local HTTP server. No external network.

const EVENT: PoolEvent = {
    id: "evt-1",
    contractId: "CABC123",
    eventType: "swap",
    ledger: 1234,
    timestamp: "2026-06-01T00:00:00Z",
    payload: { amountIn: 1000, amountOut: 990 },
};

interface TestServer {
    url: string;
    /** Number of requests received. */
    hits: () => number;
    /** Bodies of received requests. */
    bodies: () => string[];
    /** Headers of received requests. */
    headers: () => Array<Record<string, string | string[] | undefined>>;
    close: () => Promise<void>;
}

type Handler = (hit: number) => {
    status?: number;
    headers?: Record<string, string>;
    body?: string;
    /** Accept the connection and never respond. */
    hang?: boolean;
    /** Delay in ms before responding. */
    delayMs?: number;
};

/** Start a local HTTP server driven by `handler`. */
async function startServer(handler: Handler): Promise<TestServer> {
    let hits = 0;
    const bodies: string[] = [];
    const headers: Array<Record<string, string | string[] | undefined>> = [];
    const openTimers: NodeJS.Timeout[] = [];

    const server: Server = createServer((req, res) => {
        const hit = ++hits;
        headers.push({ ...req.headers });
        let body = "";
        req.on("data", (c) => (body += c));
        req.on("end", () => {
            bodies.push(body);
            const spec = handler(hit);
            if (spec.hang) return; // never respond
            const send = () => {
                res.writeHead(spec.status ?? 200, spec.headers ?? {});
                res.end(spec.body ?? "ok");
            };
            if (spec.delayMs) {
                openTimers.push(setTimeout(send, spec.delayMs));
            } else {
                send();
            }
        });
    });

    await new Promise<void>((resolve) =>
        server.listen(0, "127.0.0.1", resolve),
    );
    const { port } = server.address() as AddressInfo;

    return {
        url: `http://127.0.0.1:${port}/hook`,
        hits: () => hits,
        bodies: () => [...bodies],
        headers: () => [...headers],
        close: () =>
            new Promise<void>((resolve) => {
                openTimers.forEach(clearTimeout);
                server.closeAllConnections?.();
                server.close(() => resolve());
            }),
    };
}

/** Dispatcher wired for fast, deterministic tests. */
function makeDispatcher(
    registry: WebhookRegistry,
    overrides: Partial<{
        timeoutMs: number;
        maxRetries: number;
        baseDelayMs: number;
        concurrency: number;
        deadLetterQueue: DeadLetterQueue;
        circuitBreaker: CircuitBreaker;
        random: () => number;
    }> = {},
) {
    return new WebhookDispatcher(registry, {
        timeoutMs: 200,
        maxRetries: 2,
        baseDelayMs: 1,
        maxDelayMs: 10,
        deadLetterQueue: new DeadLetterQueue(100),
        circuitBreaker: new CircuitBreaker({ failureThreshold: 100 }),
        // Keep retry sleeps effectively instant unless a test overrides them.
        sleep: () => Promise.resolve(),
        ...overrides,
    });
}

describe("local webhook delivery integration", () => {
    const originalAllowPrivate = process.env["WEBHOOK_ALLOW_PRIVATE_TARGETS"];

    beforeEach(() => {
        process.env["WEBHOOK_ALLOW_PRIVATE_TARGETS"] = "true";
    });

    afterEach(() => {
        if (originalAllowPrivate === undefined) {
            delete process.env["WEBHOOK_ALLOW_PRIVATE_TARGETS"];
        } else {
            process.env["WEBHOOK_ALLOW_PRIVATE_TARGETS"] = originalAllowPrivate;
        }
    });

    describe("delivery timeout", () => {
        it("abandons a subscriber that never responds, and dispatch resolves", async () => {
            // Fails against main: node-fetch has no default timeout, so this hangs.
            const srv = await startServer(() => ({ hang: true }));
            try {
                const reg = new WebhookRegistry();
                reg.register(srv.url);
                const d = makeDispatcher(reg, { maxRetries: 0 });

                const results = await d.dispatch(EVENT);
                assert.equal(results.length, 1);
                assert.equal(results[0].success, false);
                assert.equal(results[0].timedOut, true);
                assert.equal(results[0].failureKind, "timeout");
            } finally {
                await srv.close();
            }
        });

        it("records a timeout distinctly from a network error", async () => {
            const srv = await startServer(() => ({ hang: true }));
            try {
                const reg = new WebhookRegistry();
                reg.register(srv.url);
                const d = makeDispatcher(reg, { maxRetries: 0 });
                await d.dispatch(EVENT);
                const m = d.metrics();
                assert.equal(m.timedOut, 1);
                assert.equal(m.failed, 1);
            } finally {
                await srv.close();
            }
        });

        it("succeeds when the subscriber responds inside the timeout", async () => {
            const srv = await startServer(() => ({ status: 200, delayMs: 10 }));
            try {
                const reg = new WebhookRegistry();
                reg.register(srv.url);
                const d = makeDispatcher(reg);
                const [r] = await d.dispatch(EVENT);
                assert.equal(r.success, true);
                assert.equal(r.statusCode, 200);
            } finally {
                await srv.close();
            }
        });
    });

    describe("subscriber isolation", () => {
        it("one hanging subscriber does not delay a healthy one", async () => {
            const hanging = await startServer(() => ({ hang: true }));
            const healthy = await startServer(() => ({ status: 200 }));
            try {
                const reg = new WebhookRegistry();
                reg.register(hanging.url);
                reg.register(healthy.url);
                const d = makeDispatcher(reg, {
                    maxRetries: 0,
                    timeoutMs: 1_000,
                });

                const started = Date.now();
                let healthyDoneAt = 0;

                // Observe when the healthy endpoint is actually hit, independent of
                // when the overall dispatch settles.
                const dispatched = d.dispatch(EVENT);
                while (healthy.hits() === 0) {
                    await new Promise((r) => setTimeout(r, 5));
                }
                healthyDoneAt = Date.now() - started;

                const results = await dispatched;
                assert.equal(results.length, 2);
                assert.ok(
                    healthyDoneAt < 500,
                    `healthy subscriber waited ${healthyDoneAt}ms behind the hanging one`,
                );
                assert.equal(results.filter((r) => r.success).length, 1);
            } finally {
                await hanging.close();
                await healthy.close();
            }
        });

        it("returns a result for every subscriber even when some fail", async () => {
            const ok = await startServer(() => ({ status: 200 }));
            const bad = await startServer(() => ({ status: 500 }));
            try {
                const reg = new WebhookRegistry();
                reg.register(ok.url);
                reg.register(bad.url);
                const d = makeDispatcher(reg, { maxRetries: 0 });
                const results = await d.dispatch(EVENT);
                assert.equal(results.length, 2);
                assert.equal(results.filter((r) => r.success).length, 1);
                assert.equal(results.filter((r) => !r.success).length, 1);
            } finally {
                await ok.close();
                await bad.close();
            }
        });

        it("bounds concurrency so it never exceeds the configured limit", async () => {
            let inFlight = 0;
            let peak = 0;
            const srv = await startServer(() => {
                inFlight++;
                peak = Math.max(peak, inFlight);
                // Released on response; approximate by decrementing after the delay.
                setTimeout(() => inFlight--, 30);
                return { status: 200, delayMs: 30 };
            });
            try {
                const reg = new WebhookRegistry();
                for (let i = 0; i < 10; i++) reg.register(srv.url);
                const d = makeDispatcher(reg, { concurrency: 3 });
                await d.dispatch(EVENT);
                assert.ok(
                    peak <= 3,
                    `peak in-flight was ${peak}, expected <= 3`,
                );
                assert.equal(srv.hits(), 10);
            } finally {
                await srv.close();
            }
        });
    });

    describe("retry policy", () => {
        it("does not retry a 404", async () => {
            const srv = await startServer(() => ({ status: 404 }));
            try {
                const reg = new WebhookRegistry();
                reg.register(srv.url);
                const d = makeDispatcher(reg, { maxRetries: 3 });
                const [r] = await d.dispatch(EVENT);
                assert.equal(r.success, false);
                assert.equal(srv.hits(), 1);
                assert.equal(r.attempts!.length, 1);
            } finally {
                await srv.close();
            }
        });

        it("retries a 503", async () => {
            const srv = await startServer(() => ({ status: 503 }));
            try {
                const reg = new WebhookRegistry();
                reg.register(srv.url);
                const d = makeDispatcher(reg, { maxRetries: 2 });
                const [r] = await d.dispatch(EVENT);
                assert.equal(r.success, false);
                assert.equal(srv.hits(), 3); // initial + 2 retries
                assert.equal(r.attempts!.length, 3);
            } finally {
                await srv.close();
            }
        });

        it("retries a 500 and succeeds when the endpoint recovers", async () => {
            const srv = await startServer((hit) => ({
                status: hit === 1 ? 500 : 200,
            }));
            try {
                const reg = new WebhookRegistry();
                reg.register(srv.url);
                const d = makeDispatcher(reg);
                const [r] = await d.dispatch(EVENT);
                assert.equal(r.success, true);
                assert.equal(srv.hits(), 2);
            } finally {
                await srv.close();
            }
        });

        it("retries 408 and 429 but not 400 or 403", () => {
            assert.equal(isRetryableStatus(408), true);
            assert.equal(isRetryableStatus(429), true);
            assert.equal(isRetryableStatus(500), true);
            assert.equal(isRetryableStatus(503), true);
            assert.equal(isRetryableStatus(400), false);
            assert.equal(isRetryableStatus(403), false);
            assert.equal(isRetryableStatus(404), false);
            assert.equal(isRetryableStatus(200), false);
        });

        it("retries a network error (connection refused)", async () => {
            // Bind then immediately close, so the port refuses connections.
            const srv = await startServer(() => ({ status: 200 }));
            const url = srv.url;
            await srv.close();

            const reg = new WebhookRegistry();
            reg.register(url);
            const d = makeDispatcher(reg, { maxRetries: 2 });
            const [r] = await d.dispatch(EVENT);
            assert.equal(r.success, false);
            assert.equal(r.failureKind, "network");
            assert.equal(r.attempts!.length, 3);
        });

        it("is iterative, not recursive — deep retry chains do not grow the stack", async () => {
            const srv = await startServer(() => ({ status: 503 }));
            try {
                const reg = new WebhookRegistry();
                reg.register(srv.url);
                // A recursive implementation would nest 500 frames here.
                const d = makeDispatcher(reg, { maxRetries: 500 });
                const [r] = await d.dispatch(EVENT);
                assert.equal(r.success, false);
                assert.equal(r.attempts!.length, 501);
            } finally {
                await srv.close();
            }
        });

        it("counts retries in metrics", async () => {
            const srv = await startServer(() => ({ status: 503 }));
            try {
                const reg = new WebhookRegistry();
                reg.register(srv.url);
                const d = makeDispatcher(reg, { maxRetries: 2 });
                await d.dispatch(EVENT);
                assert.equal(d.metrics().retries, 2);
            } finally {
                await srv.close();
            }
        });
    });

    describe("backoff jitter", () => {
        it("produces different delays across repeated runs", () => {
            const delays = new Set<number>();
            for (let i = 0; i < 50; i++) {
                delays.add(fullJitterDelay(3, 500, 30_000));
            }
            assert.ok(
                delays.size > 1,
                `expected jittered delays to vary, got ${[...delays].join(", ")}`,
            );
        });

        it("stays within [0, base * 2 ** attempt]", () => {
            for (let attempt = 0; attempt < 6; attempt++) {
                const ceiling = 500 * 2 ** attempt;
                for (let i = 0; i < 25; i++) {
                    const d = fullJitterDelay(attempt, 500, 30_000);
                    assert.ok(
                        d >= 0 && d < ceiling,
                        `delay ${d} outside [0, ${ceiling})`,
                    );
                }
            }
        });

        it("caps the delay at maxDelayMs", () => {
            for (let i = 0; i < 50; i++) {
                assert.ok(fullJitterDelay(20, 500, 1_000) < 1_000);
            }
        });

        it("returns 0 when the RNG returns 0", () => {
            assert.equal(
                fullJitterDelay(5, 500, 30_000, () => 0),
                0,
            );
        });
    });

    describe("Retry-After", () => {
        it("parses delta-seconds", () => {
            assert.equal(parseRetryAfter("2"), 2_000);
            assert.equal(parseRetryAfter("0"), 0);
        });

        it("parses an HTTP date", () => {
            const now = Date.parse("2026-06-01T00:00:00Z");
            const at = new Date(now + 5_000).toUTCString();
            const parsed = parseRetryAfter(at, now)!;
            assert.ok(parsed >= 4_000 && parsed <= 5_000, `got ${parsed}`);
        });

        it("returns undefined when absent or unparseable", () => {
            assert.equal(parseRetryAfter(null), undefined);
            assert.equal(parseRetryAfter(""), undefined);
            assert.equal(parseRetryAfter("not-a-date"), undefined);
        });

        it("clamps an absurd Retry-After to the cap", () => {
            assert.equal(parseRetryAfter("999999"), 60_000);
        });

        it("is honoured over computed jitter when the subscriber sends one", async () => {
            const srv = await startServer((hit) => ({
                status: hit === 1 ? 503 : 200,
                headers:
                    hit === 1
                        ? { "retry-after": "1" }
                        : undefined,
            }));
            try {
                const reg = new WebhookRegistry();
                reg.register(srv.url);
                const slept: number[] = [];
                const d = new WebhookDispatcher(reg, {
                    timeoutMs: 200,
                    maxRetries: 2,
                    baseDelayMs: 1,
                    maxDelayMs: 5,
                    deadLetterQueue: new DeadLetterQueue(10),
                    circuitBreaker: new CircuitBreaker({
                        failureThreshold: 100,
                    }),
                    sleep: (ms) => {
                        slept.push(ms);
                        return Promise.resolve();
                    },
                });
                const [r] = await d.dispatch(EVENT);
                assert.equal(r.success, true);
                // 1000ms from the header, not the <=5ms jitter ceiling.
                assert.deepEqual(slept, [1_000]);
            } finally {
                await srv.close();
            }
        });
    });

    describe("dead-letter queue", () => {
        it("captures a permanently-failing delivery", async () => {
            const srv = await startServer(() => ({ status: 500 }));
            try {
                const reg = new WebhookRegistry();
                reg.register(srv.url);
                const dlq = new DeadLetterQueue(10);
                const d = makeDispatcher(reg, {
                    maxRetries: 1,
                    deadLetterQueue: dlq,
                });
                const [r] = await d.dispatch(EVENT);

                assert.equal(r.deadLettered, true);
                assert.equal(dlq.size, 1);
                const entry = dlq.list()[0];
                assert.equal(entry.event.id, EVENT.id);
                assert.equal(entry.subscription.url, srv.url);
                assert.equal(entry.attempts.length, 2);
                assert.ok(entry.failedAt > 0);
                assert.match(entry.reason, /HTTP 500/);
            } finally {
                await srv.close();
            }
        });

        it("does not dead-letter a successful delivery", async () => {
            const srv = await startServer(() => ({ status: 200 }));
            try {
                const reg = new WebhookRegistry();
                reg.register(srv.url);
                const dlq = new DeadLetterQueue(10);
                const d = makeDispatcher(reg, { deadLetterQueue: dlq });
                await d.dispatch(EVENT);
                assert.equal(dlq.size, 0);
            } finally {
                await srv.close();
            }
        });

        it("is bounded and evicts oldest-first at capacity", () => {
            const dlq = new DeadLetterQueue(3);
            const sub = { id: "s1", url: "http://x", createdAt: 0 };
            const ids: string[] = [];
            for (let i = 0; i < 5; i++) {
                ids.push(
                    dlq.add({ ...EVENT, id: `e${i}` }, sub, [], "boom").id,
                );
            }
            assert.equal(dlq.size, 3);
            const kept = dlq.list().map((e) => e.event.id);
            assert.deepEqual(kept, ["e2", "e3", "e4"]);
            // The two oldest are gone.
            assert.equal(dlq.get(ids[0]), undefined);
            assert.equal(dlq.get(ids[1]), undefined);
        });

        it("rejects a non-positive capacity", () => {
            assert.throws(() => new DeadLetterQueue(0), RangeError);
        });

        it("removes an entry by id", () => {
            const dlq = new DeadLetterQueue(5);
            const e = dlq.add(
                EVENT,
                { id: "s1", url: "http://x", createdAt: 0 },
                [],
                "boom",
            );
            assert.equal(dlq.remove(e.id), true);
            assert.equal(dlq.remove(e.id), false);
            assert.equal(dlq.size, 0);
        });

        it("replays successfully once the endpoint recovers, and clears the entry", async () => {
            // Fail hard enough to dead-letter, then recover.
            let recovered = false;
            const srv = await startServer(() => ({
                status: recovered ? 200 : 500,
            }));
            try {
                const reg = new WebhookRegistry();
                reg.register(srv.url);
                const dlq = new DeadLetterQueue(10);
                const d = makeDispatcher(reg, {
                    maxRetries: 0,
                    deadLetterQueue: dlq,
                });

                const [failed] = await d.dispatch(EVENT);
                assert.equal(failed.deadLettered, true);
                assert.equal(dlq.size, 1);

                recovered = true;
                const replayed = await d.replay(failed.deadLetterId!);
                assert.equal(replayed!.success, true);
                assert.equal(dlq.size, 0);
            } finally {
                await srv.close();
            }
        });

        it("keeps a failed replay in the queue rather than dropping the event", async () => {
            const srv = await startServer(() => ({ status: 500 }));
            try {
                const reg = new WebhookRegistry();
                reg.register(srv.url);
                const dlq = new DeadLetterQueue(10);
                const d = makeDispatcher(reg, {
                    maxRetries: 0,
                    deadLetterQueue: dlq,
                });
                const [failed] = await d.dispatch(EVENT);
                const originalId = failed.deadLetterId!;

                const replayed = await d.replay(originalId);
                assert.equal(replayed!.success, false);
                // The original entry survives, and the failed replay is itself recorded,
                // so no failed delivery is ever silently discarded.
                assert.ok(
                    dlq.get(originalId),
                    "original entry should survive a failed replay",
                );
                assert.equal(dlq.size, 2);
            } finally {
                await srv.close();
            }
        });

        it("returns undefined when replaying an unknown id", async () => {
            const d = makeDispatcher(new WebhookRegistry());
            assert.equal(await d.replay("nope"), undefined);
        });
    });

    describe("circuit breaker", () => {
        it("opens after the configured consecutive failures", () => {
            const cb = new CircuitBreaker({
                failureThreshold: 3,
                cooldownMs: 1_000,
            });
            cb.recordFailure("s1");
            cb.recordFailure("s1");
            assert.equal(cb.stateOf("s1"), "closed");
            cb.recordFailure("s1");
            assert.equal(cb.stateOf("s1"), "open");
        });

        it("stops attempting delivery while open", async () => {
            const srv = await startServer(() => ({ status: 500 }));
            try {
                const reg = new WebhookRegistry();
                const sub = reg.register(srv.url);
                const cb = new CircuitBreaker({
                    failureThreshold: 1,
                    cooldownMs: 60_000,
                });
                const d = makeDispatcher(reg, {
                    maxRetries: 0,
                    circuitBreaker: cb,
                });

                await d.dispatch(EVENT);
                const hitsAfterFirst = srv.hits();
                assert.equal(cb.stateOf(sub.id), "open");

                const [second] = await d.dispatch(EVENT);
                assert.equal(second.failureKind, "circuit_open");
                assert.equal(
                    srv.hits(),
                    hitsAfterFirst,
                    "no request should be sent while open",
                );
                assert.equal(d.metrics().shortCircuited, 1);
            } finally {
                await srv.close();
            }
        });

        it("half-opens once the cooldown elapses", () => {
            let now = 1_000;
            const cb = new CircuitBreaker({
                failureThreshold: 1,
                cooldownMs: 500,
                now: () => now,
            });
            cb.recordFailure("s1");
            assert.equal(cb.canAttempt("s1"), false);

            now += 499;
            assert.equal(cb.canAttempt("s1"), false);

            now += 1; // exactly at the cooldown boundary
            assert.equal(cb.canAttempt("s1"), true);
            assert.equal(cb.stateOf("s1"), "half_open");
        });

        it("closes again after a successful probe", () => {
            let now = 0;
            const cb = new CircuitBreaker({
                failureThreshold: 1,
                cooldownMs: 10,
                now: () => now,
            });
            cb.recordFailure("s1");
            now += 10;
            cb.canAttempt("s1");
            cb.recordSuccess("s1");
            assert.equal(cb.stateOf("s1"), "closed");
            assert.equal(cb.snapshot("s1").consecutiveFailures, 0);
        });

        it("re-opens immediately when the probe fails", () => {
            let now = 0;
            const cb = new CircuitBreaker({
                failureThreshold: 1,
                cooldownMs: 10,
                now: () => now,
            });
            cb.recordFailure("s1");
            now += 10;
            cb.canAttempt("s1");
            assert.equal(cb.stateOf("s1"), "half_open");
            cb.recordFailure("s1");
            assert.equal(cb.stateOf("s1"), "open");
            assert.equal(cb.canAttempt("s1"), false);
        });

        it("a success resets the consecutive-failure count", () => {
            const cb = new CircuitBreaker({ failureThreshold: 3 });
            cb.recordFailure("s1");
            cb.recordFailure("s1");
            cb.recordSuccess("s1");
            cb.recordFailure("s1");
            assert.equal(cb.stateOf("s1"), "closed");
        });

        it("tracks circuits independently per subscription", () => {
            const cb = new CircuitBreaker({ failureThreshold: 1 });
            cb.recordFailure("s1");
            assert.equal(cb.stateOf("s1"), "open");
            assert.equal(cb.stateOf("s2"), "closed");
        });

        it("exposes a nextProbeAt on an open circuit", () => {
            const cb = new CircuitBreaker({
                failureThreshold: 1,
                cooldownMs: 1_000,
                now: () => 5_000,
            });
            cb.recordFailure("s1");
            assert.equal(cb.snapshot("s1").nextProbeAt, 6_000);
        });
    });

    describe("metrics", () => {
        it("reports accurate counts across a mixed success/failure run", async () => {
            const ok = await startServer(() => ({ status: 200 }));
            const bad = await startServer(() => ({ status: 500 }));
            const hang = await startServer(() => ({ hang: true }));
            try {
                const reg = new WebhookRegistry();
                reg.register(ok.url);
                reg.register(bad.url);
                reg.register(hang.url);
                const d = makeDispatcher(reg, {
                    maxRetries: 1,
                    timeoutMs: 150,
                });

                await d.dispatch(EVENT);
                const m = d.metrics();

                // ok: 1 attempt. bad: 2 attempts. hang: 2 attempts (both time out).
                assert.equal(m.attempted, 5);
                assert.equal(m.succeeded, 1);
                assert.equal(m.failed, 2);
                assert.equal(m.timedOut, 2);
                assert.equal(m.deadLettered, 2);
                assert.equal(m.retries, 2);
                assert.equal(m.circuits.length, 3);
            } finally {
                await ok.close();
                await bad.close();
                await hang.close();
            }
        });

        it("starts at zero and resets", async () => {
            const d = makeDispatcher(new WebhookRegistry());
            const m = d.metrics();
            assert.equal(m.attempted, 0);
            assert.equal(m.succeeded, 0);
            assert.equal(m.deadLettered, 0);
            d.resetMetrics();
            assert.equal(d.metrics().attempted, 0);
        });
    });

    describe("request shape", () => {
        it("sends the event as JSON with the secret header when configured", async () => {
            const srv = await startServer(() => ({ status: 200 }));
            try {
                const reg = new WebhookRegistry();
                reg.register(srv.url, { secret: "s3cr3t" });
                const d = makeDispatcher(reg);
                await d.dispatch(EVENT);

                assert.equal(srv.headers()[0]["x-webhook-secret"], "s3cr3t");
                assert.equal(
                    srv.headers()[0]["content-type"],
                    "application/json",
                );
                assert.deepEqual(JSON.parse(srv.bodies()[0]), EVENT);
            } finally {
                await srv.close();
            }
        });

        it("omits the secret header when none is configured", async () => {
            const srv = await startServer(() => ({ status: 200 }));
            try {
                const reg = new WebhookRegistry();
                reg.register(srv.url);
                const d = makeDispatcher(reg);
                await d.dispatch(EVENT);
                assert.equal(srv.headers()[0]["x-webhook-secret"], undefined);
            } finally {
                await srv.close();
            }
        });

        it("delivers only to subscriptions matching the event", async () => {
            const match = await startServer(() => ({ status: 200 }));
            const other = await startServer(() => ({ status: 200 }));
            try {
                const reg = new WebhookRegistry();
                reg.register(match.url, { eventType: "swap" });
                reg.register(other.url, { eventType: "mint_pos" });
                const d = makeDispatcher(reg);
                const results = await d.dispatch(EVENT);
                assert.equal(results.length, 1);
                assert.equal(match.hits(), 1);
                assert.equal(other.hits(), 0);
            } finally {
                await match.close();
                await other.close();
            }
        });

        it("resolves to an empty array when nothing matches", async () => {
            const reg = new WebhookRegistry();
            reg.register("http://127.0.0.1:1/never", { eventType: "other" });
            const d = makeDispatcher(reg);
            assert.deepEqual(await d.dispatch(EVENT), []);
        });
    });
});
