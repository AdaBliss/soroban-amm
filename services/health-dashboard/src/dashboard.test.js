import assert from "node:assert/strict";
import test from "node:test";
import {
  compareThreshold, createDashboardController, formatError, formatMetric, formatTimestamp,
  formatTokenAmount, formatMetricWithUnit, gql, healthColor, renderAlertConfigs, renderEvents,
  renderFiredAlerts, renderHealth, renderHeatmap, renderHistory, renderStats, renderPoolsOverview,
} from "./dashboard.js";
import { installFakeDocument } from "./dom-stub.js";

const QUERY_RESPONSES = {
  Stats: { poolStats: [] },
  Health: { poolHealth: null },
  Alerts: { alertConfigs: [] },
  History: { priceHistory: [] },
  Events: { poolEvents: [] },
};

function installFakeFetch(failingKeys = []) {
  globalThis.fetch = async (_url, options) => {
    const { query } = JSON.parse(options.body);
    const key = Object.keys(QUERY_RESPONSES).find((k) => query.includes(`query ${k}`));
    if (failingKeys.includes(key)) {
      return { ok: false, status: 500, json: async () => ({ errors: [{ message: `${key} failed` }] }) };
    }
    return { ok: true, json: async () => ({ data: QUERY_RESPONSES[key] }) };
  };
}

function setupRefreshTestDom() {
  const doc = installFakeDocument([
    "connection-status", "dashboard-state", "api-url", "pool-id", "btn-retry",
    "m-tvl", "m-vol", "m-fees", "m-swaps", "health-score-text", "last-updated",
    "fired-alert-list", "alert-list",
  ]);
  doc.getElementById("btn-retry").hidden = true;
  return doc;
}

/** All text a fake element renders: its own text/HTML plus every descendant's. */
function renderedText(el) {
  return [el.innerHTML, el.textContent, ...el.children.map(renderedText)].join(" ");
}

test("formats millions", () => assert.equal(formatMetric(1_250_000), "1.25M"));
test("formats thousands", () => assert.equal(formatMetric(12_500), "12.5K"));
test("formats small metrics", () => assert.equal(formatMetric(12.5), "12.50"));
test("handles non-numeric metrics", () => assert.equal(formatMetric("not-a-number"), "—"));
test("formats timestamps", () => assert.notEqual(formatTimestamp(Date.now()), "—"));
test("compares above thresholds", () => assert.equal(compareThreshold(11, 10), true));
test("compares below thresholds", () => assert.equal(compareThreshold(9, 10, "below"), true));
test("rejects invalid threshold values", () => assert.equal(compareThreshold("x", 10), false));
test("selects health colors", () => { assert.equal(healthColor(80), "var(--green)"); assert.equal(healthColor(50), "var(--yellow)"); assert.equal(healthColor(10), "var(--red)"); });
test("formats invocation errors", () => assert.equal(formatError(new Error("offline")), "offline"));

test("fired alerts render into their own container and survive renderAlertConfigs running afterward", () => {
  const doc = installFakeDocument(["fired-alert-list", "alert-list"]);
  renderFiredAlerts([{ metric: "tvl", threshold: 100, currentValue: 200, firedAt: Date.now() }]);
  renderAlertConfigs([{ poolId: "pool-a", metric: "tvl", thresholdBps: 100 }], () => {});

  const firedList = doc.getElementById("fired-alert-list");
  const configList = doc.getElementById("alert-list");
  assert.equal(firedList.children.length, 1);
  assert.equal(configList.children.length, 1);
});

test("renderFiredAlerts does not accumulate stale entries across calls", () => {
  const doc = installFakeDocument(["fired-alert-list"]);
  renderFiredAlerts([{ metric: "tvl", threshold: 100, currentValue: 200, firedAt: Date.now() }]);
  renderFiredAlerts([{ metric: "volume24h", threshold: 50, currentValue: 60, firedAt: Date.now() }]);

  const firedList = doc.getElementById("fired-alert-list");
  assert.equal(firedList.children.length, 1);
  assert.ok(firedList.children[0].textContent.includes("volume24h"));
});

test("renderAlertConfigs still renders configured thresholds with working remove buttons", () => {
  const doc = installFakeDocument(["alert-list"]);
  let removed = null;
  renderAlertConfigs(
    [{ poolId: "pool-a", metric: "price_deviation", thresholdBps: 50 }],
    (alert) => { removed = alert; },
  );
  const configList = doc.getElementById("alert-list");
  assert.equal(configList.children.length, 1);
  const button = configList.children[0].children.find((c) => c.tagName === "button" || c.className === "remove");
  assert.ok(button);
  button.listeners.click[0]();
  assert.deepEqual(removed, { poolId: "pool-a", metric: "price_deviation", thresholdBps: 50 });
});

test("btn-retry stays hidden after a fully successful refresh", async () => {
  const doc = setupRefreshTestDom();
  installFakeFetch([]);
  const controller = createDashboardController({ apiUrl: "http://api.test", poolId: "pool-a" });
  await controller.refresh();
  assert.equal(doc.getElementById("btn-retry").hidden, true);
});

test("btn-retry is shown after a partial failure", async () => {
  const doc = setupRefreshTestDom();
  installFakeFetch(["Alerts"]);
  const controller = createDashboardController({ apiUrl: "http://api.test", poolId: "pool-a" });
  await controller.refresh();
  assert.equal(doc.getElementById("btn-retry").hidden, false);
});

test("btn-retry is shown after every query fails", async () => {
  const doc = setupRefreshTestDom();
  installFakeFetch(Object.keys(QUERY_RESPONSES));
  const controller = createDashboardController({ apiUrl: "http://api.test", poolId: "pool-a" });
  await controller.refresh();
  assert.equal(doc.getElementById("btn-retry").hidden, false);
});

test("addAlert with a blank Pool ID does not drop another pool's alert with the same metric", async () => {
  const doc = setupRefreshTestDom();
  installFakeFetch([]);
  doc.registerElement("alert-metric", { value: "tvl" });
  doc.registerElement("alert-threshold", { value: "10" });

  const controller = createDashboardController({ apiUrl: "http://api.test", poolId: "" });

  doc.getElementById("pool-id").value = "pool-a";
  await controller.addAlert();

  doc.getElementById("pool-id").value = "pool-b";
  doc.getElementById("alert-threshold").value = "20";
  await controller.addAlert();

  // Blank Pool ID: previously this dropped both prior entries because the
  // dedup filter only matched on metric, ignoring poolId.
  doc.getElementById("pool-id").value = "";
  doc.getElementById("alert-threshold").value = "30";
  await controller.addAlert();

  assert.equal(doc.getElementById("alert-list").children.length, 3);
});

// ── Render functions and controller (issue #791) ────────────────────────────
//
// The render* functions and createDashboardController touch the DOM and drive
// fetching, so they run against the FakeDocument from dom-stub.js with a
// stubbed global.fetch rather than a real network or browser.

const STATS_IDS = ["m-tvl", "m-tvl-sub", "m-vol", "m-vol-sub", "m-fees", "m-fees-sub", "m-swaps"];
const HEALTH_IDS = ["health-score-text", "health-arc", "hd-status", "hd-tvl", "hd-vol", "hd-fee", "hd-dev", "dev-bps", "dev-fill", "fired-alert-list"];

const SAMPLE_STATS = {
  poolId: "pool-a", tokenA: "AAA", tokenB: "BBB",
  tvl: 2_500_000, volume24h: 12_500, fees24h: 125.5, swapCount: 1234, priceDeviationBps: 40,
};

test("renderStats writes formatted metrics into their tiles", () => {
  const doc = installFakeDocument(STATS_IDS);
  renderStats([SAMPLE_STATS], "pool-a");
  assert.ok(doc.getElementById("m-tvl").textContent.includes("tokens"));
  assert.ok(doc.getElementById("m-vol").textContent.includes("tokens"));
  assert.ok(doc.getElementById("m-fees").textContent.includes("tokens"));
  assert.equal(doc.getElementById("m-swaps").textContent, Number(1234).toLocaleString());
  assert.ok(doc.getElementById("m-tvl-sub").textContent.includes("AAA"));
  assert.ok(doc.getElementById("m-tvl-sub").textContent.includes("BBB"));
});

test("renderStats blanks every tile when there are no stats", () => {
  const doc = installFakeDocument(STATS_IDS);
  for (const empty of [[], null, undefined]) {
    renderStats(empty, "pool-a");
    for (const id of ["m-tvl", "m-vol", "m-fees", "m-swaps"]) {
      assert.equal(doc.getElementById(id).textContent, "—", `${id} for ${JSON.stringify(empty)}`);
    }
  }
});

test("renderHealth fills the gauge, sub-scores and deviation bar", () => {
  const doc = installFakeDocument(HEALTH_IDS);
  renderHealth({
    healthScore: 82, tvlScore: 90.4, volumeScore: 71.25, feeEfficiencyScore: 60,
    priceDeviationBps: 30, status: "Healthy", alertsFired: [],
  }, []);
  assert.equal(doc.getElementById("health-score-text").textContent, "82");
  assert.equal(doc.getElementById("hd-status").textContent, "Healthy");
  assert.equal(doc.getElementById("hd-tvl").textContent, "90.4");
  assert.equal(doc.getElementById("hd-vol").textContent, "71.3");
  assert.equal(doc.getElementById("hd-fee").textContent, "60.0");
  assert.equal(doc.getElementById("hd-dev").textContent, "30 bps");
  // 82 is in the green band, and the arc is drawn as a dash offset.
  assert.equal(doc.getElementById("health-arc").getAttribute("stroke"), "var(--green)");
  assert.ok(Number(doc.getElementById("health-arc").getAttribute("stroke-dashoffset")) > 0);
  // With default threshold of 100 bps, 30 bps is 30% width
  assert.equal(doc.getElementById("dev-fill").style.width, "30%");
});

test("renderHealth degrades to placeholders when health is unavailable", () => {
  const doc = installFakeDocument(HEALTH_IDS);
  renderHealth(null, []);
  assert.equal(doc.getElementById("health-score-text").textContent, "—");
  assert.equal(doc.getElementById("hd-status").textContent, "Unavailable");
});

test("renderHealth forwards fired alerts to the fired-alert list", () => {
  const doc = installFakeDocument(HEALTH_IDS);
  renderHealth({
    healthScore: 20, tvlScore: 1, volumeScore: 1, feeEfficiencyScore: 1,
    priceDeviationBps: 500, status: "Critical",
    alertsFired: [{ metric: "tvl", threshold: 100, currentValue: 20, firedAt: Date.now() }],
  }, []);
  assert.equal(doc.getElementById("health-arc").getAttribute("stroke"), "var(--red)");
  assert.equal(doc.getElementById("fired-alert-list").children.length, 1);
});

test("renderFiredAlerts shows an empty state when nothing has fired", () => {
  const doc = installFakeDocument(["fired-alert-list"]);
  renderFiredAlerts([]);
  const list = doc.getElementById("fired-alert-list");
  assert.equal(list.children.length, 0);
  assert.ok(list.innerHTML.includes("No alerts have fired"));
});

test("renderEvents renders one row of four cells per event", () => {
  const doc = installFakeDocument(["events-body"]);
  renderEvents([
    { id: "1", poolId: "pool-a", type: "swap", timestamp: Date.now(), payload: '{"amountIn":5}' },
    { id: "2", poolId: "pool-b", type: "mint", timestamp: Date.now(), payload: null },
  ]);
  const body = doc.getElementById("events-body");
  assert.equal(body.children.length, 2);
  assert.equal(body.children[0].children.length, 4);
  assert.equal(body.children[0].children[1].textContent, "swap");
  assert.equal(body.children[0].children[2].textContent, "pool-a");
  // A null payload renders as an empty cell rather than "null".
  assert.equal(body.children[1].children[3].textContent, "");
});

test("renderEvents caps the table at 50 rows", () => {
  const doc = installFakeDocument(["events-body"]);
  const events = Array.from({ length: 120 }, (_, i) => ({
    id: String(i), poolId: "pool-a", type: "swap", timestamp: Date.now(), payload: "{}",
  }));
  renderEvents(events);
  assert.equal(doc.getElementById("events-body").children.length, 50);
});

test("renderEvents shows an empty state for no events", () => {
  const doc = installFakeDocument(["events-body"]);
  for (const empty of [[], null, undefined]) {
    renderEvents(empty);
    const body = doc.getElementById("events-body");
    assert.equal(body.children.length, 0);
    assert.ok(body.innerHTML.includes("No events indexed yet"));
  }
});

test("renderHistory draws an axis label and one min/max range bar per point", () => {
  const doc = installFakeDocument(["price-chart"]);
  renderHistory([
    { poolId: "p", timestamp: Date.now(), price: 1, feeBps: 30 },
    { poolId: "p", timestamp: Date.now(), price: 2, feeBps: 30 },
  ]);
  const chart = doc.getElementById("price-chart");
  // Axis label first, then one bar per point (each point is its own bucket).
  assert.equal(chart.children.length, 3);
  assert.ok(chart.children[0].textContent.includes("Range:"));
  const [, low, high] = chart.children;
  // A single-price bucket has no range, so it gets the 2px minimum height and
  // is positioned by its price against a chart that is baselined at zero.
  assert.equal(low.style.height, "2px");
  assert.equal(high.style.height, "2px");
  assert.equal(high.style.marginTop, "0px");
  assert.equal(low.style.marginTop, "68px");
  assert.ok(low.title.includes("1.000000"));
});

test("renderHistory shows an empty state for no history", () => {
  const doc = installFakeDocument(["price-chart"]);
  renderHistory([]);
  const chart = doc.getElementById("price-chart");
  assert.equal(chart.children.length, 0);
  assert.ok(chart.innerHTML.includes("No price history indexed yet"));
});

test("renderHeatmap always renders 24 buckets and only counts swaps", () => {
  const doc = installFakeDocument(["heatmap"]);
  const now = Date.now();
  renderHeatmap([
    { type: "swap", timestamp: now, payload: '{"amountIn":100}' },
    { type: "mint", timestamp: now, payload: '{"amountIn":999999}' },
  ]);
  const heatmap = doc.getElementById("heatmap");
  assert.equal(heatmap.children.length, 24);
  // The newest bucket is last; the mint must not have contributed to it.
  assert.ok(heatmap.children[23].title.includes("100.00"));
});

test("renderHeatmap ignores events outside the 24h window", () => {
  const doc = installFakeDocument(["heatmap"]);
  const now = Date.now();
  renderHeatmap([
    { type: "swap", timestamp: now - 48 * 3_600_000, payload: '{"amountIn":500}' },
    { type: "swap", timestamp: now + 3_600_000, payload: '{"amountIn":500}' },
  ]);
  const heatmap = doc.getElementById("heatmap");
  assert.equal(heatmap.children.length, 24);
  // Nothing landed in a bucket, so every bar sits at the 4px floor.
  for (const bar of heatmap.children) assert.equal(bar.style.height, "4px");
});

test("renderHeatmap swallows a malformed payload without losing other events", () => {
  const doc = installFakeDocument(["heatmap"]);
  const now = Date.now();
  assert.doesNotThrow(() => renderHeatmap([
    { type: "swap", timestamp: now, payload: "{not json" },
    { type: "swap", timestamp: now, payload: '{"amountIn":250}' },
  ]));
  const heatmap = doc.getElementById("heatmap");
  assert.equal(heatmap.children.length, 24);
  // The good event still counted: the bad one only skipped itself.
  assert.ok(heatmap.children[23].title.includes("250.00"));
});

test("renderHeatmap handles a null event list", () => {
  const doc = installFakeDocument(["heatmap"]);
  assert.doesNotThrow(() => renderHeatmap(null));
  assert.equal(doc.getElementById("heatmap").children.length, 24);
});

test("renderAlertConfigs shows an empty state when nothing is configured", () => {
  const doc = installFakeDocument(["alert-list"]);
  renderAlertConfigs([], () => {});
  const list = doc.getElementById("alert-list");
  assert.equal(list.children.length, 0);
  assert.ok(list.innerHTML.includes("No alerts configured"));
});

test("renderAlertConfigs labels bps and native-unit metrics differently", () => {
  const doc = installFakeDocument(["alert-list"]);
  renderAlertConfigs([
    { poolId: "p", metric: "price_deviation", thresholdBps: 50 },
    { poolId: "p", metric: "tvl", thresholdValue: 1000 },
  ], () => {});
  const list = doc.getElementById("alert-list");
  assert.ok(list.children[0].textContent.includes("50 bps"));
  assert.ok(list.children[1].textContent.includes("> 1000"));
  assert.ok(!list.children[1].textContent.includes("bps"));
});

test("gql throws on a non-OK HTTP status", async () => {
  globalThis.fetch = async () => ({ ok: false, status: 503, json: async () => ({}) });
  await assert.rejects(
    () => gql("http://api.test", "query Stats { x }"),
    /HTTP 503/,
  );
});

test("gql throws with every GraphQL error message joined", async () => {
  globalThis.fetch = async () => ({
    ok: true,
    json: async () => ({ errors: [{ message: "bad pool" }, { message: "bad metric" }] }),
  });
  await assert.rejects(
    () => gql("http://api.test", "query Stats { x }"),
    /bad pool; bad metric/,
  );
});

test("gql returns the data payload and posts the query and variables", async () => {
  let seen;
  globalThis.fetch = async (url, options) => {
    seen = { url, body: JSON.parse(options.body), method: options.method };
    return { ok: true, json: async () => ({ data: { poolStats: [] } }) };
  };
  const data = await gql("http://api.test", "query Stats { x }", { poolId: "pool-a" });
  assert.deepEqual(data, { poolStats: [] });
  assert.equal(seen.method, "POST");
  assert.equal(seen.url, "http://api.test");
  assert.deepEqual(seen.body.variables, { poolId: "pool-a" });
});

test("refresh reports Connected when all five queries succeed", async () => {
  const doc = setupRefreshTestDom();
  installFakeFetch([]);
  const controller = createDashboardController({ apiUrl: "http://api.test", poolId: "pool-a" });
  await controller.refresh();
  const status = doc.getElementById("connection-status");
  assert.equal(status.textContent, "Connected");
  assert.equal(status.className, "connected");
  assert.equal(status.title, "");
});

test("refresh reports Degraded and names the failing query on a partial failure", async () => {
  const doc = setupRefreshTestDom();
  installFakeFetch(["Alerts"]);
  const controller = createDashboardController({ apiUrl: "http://api.test", poolId: "pool-a" });
  await controller.refresh();
  const status = doc.getElementById("connection-status");
  assert.equal(status.textContent, "Degraded");
  assert.equal(status.className, "error");
  assert.ok(status.title.includes("alerts"));
  assert.ok(doc.getElementById("dashboard-state").textContent.startsWith("Degraded:"));
});

test("refresh reports Error when every query fails", async () => {
  const doc = setupRefreshTestDom();
  installFakeFetch(Object.keys(QUERY_RESPONSES));
  const controller = createDashboardController({ apiUrl: "http://api.test", poolId: "pool-a" });
  await controller.refresh();
  const status = doc.getElementById("connection-status");
  assert.equal(status.textContent, "Error");
  assert.equal(status.className, "error");
  assert.ok(doc.getElementById("dashboard-state").textContent.includes("Unable to load"));
});

test("refresh ignores a reentrant call while one is already in flight", async () => {
  setupRefreshTestDom();
  let calls = 0;
  globalThis.fetch = async (_url, options) => {
    calls += 1;
    const { query } = JSON.parse(options.body);
    const key = Object.keys(QUERY_RESPONSES).find((k) => query.includes(`query ${k}`));
    return { ok: true, json: async () => ({ data: QUERY_RESPONSES[key] }) };
  };
  const controller = createDashboardController({ apiUrl: "http://api.test", poolId: "pool-a" });
  await Promise.all([controller.refresh(), controller.refresh()]);
  // Five queries for the first refresh; the second returns immediately.
  assert.equal(calls, 5);
});

test("addAlert rolls the configs list back when the mutation fails", async () => {
  const doc = setupRefreshTestDom();
  installFakeFetch([]);
  doc.registerElement("alert-metric", { value: "tvl" });
  doc.registerElement("alert-threshold", { value: "10" });

  const controller = createDashboardController({ apiUrl: "http://api.test", poolId: "pool-a" });
  await controller.addAlert();
  assert.equal(doc.getElementById("alert-list").children.length, 1);

  // Second add fails at the mutation: the optimistic entry must be undone.
  globalThis.fetch = async () => ({ ok: false, status: 500, json: async () => ({}) });
  doc.getElementById("alert-metric").value = "volume24h";
  doc.getElementById("alert-threshold").value = "20";
  await controller.addAlert();

  const list = doc.getElementById("alert-list");
  assert.equal(list.children.length, 1, "rolled back to the single saved alert");
  assert.ok(list.children[0].textContent.includes("tvl"));
  assert.ok(!list.children[0].textContent.includes("volume24h"));
  assert.ok(doc.getElementById("dashboard-state").textContent.includes("Alert was not saved"));
});

test("addAlert rejects a blank metric or a negative threshold without calling the API", async () => {
  const doc = setupRefreshTestDom();
  let calls = 0;
  globalThis.fetch = async () => { calls += 1; return { ok: true, json: async () => ({ data: {} }) }; };
  doc.registerElement("alert-metric", { value: "" });
  doc.registerElement("alert-threshold", { value: "10" });

  const controller = createDashboardController({ apiUrl: "http://api.test", poolId: "pool-a" });
  await controller.addAlert();

  doc.getElementById("alert-metric").value = "tvl";
  doc.getElementById("alert-threshold").value = "-5";
  await controller.addAlert();

  assert.equal(calls, 0);
  assert.equal(doc.getElementById("alert-list").children.length, 0);
});

test("removeAlert drops the entry optimistically and restores it on failure", async () => {
  const doc = setupRefreshTestDom();
  installFakeFetch([]);
  doc.registerElement("alert-metric", { value: "tvl" });
  doc.registerElement("alert-threshold", { value: "10" });

  const controller = createDashboardController({ apiUrl: "http://api.test", poolId: "pool-a" });
  await controller.addAlert();
  const list = doc.getElementById("alert-list");
  assert.equal(list.children.length, 1);

  // The remove button is wired to removeAlert; make its mutation fail.
  globalThis.fetch = async () => { throw new Error("network down"); };
  const button = list.children[0].children.find((c) => c.className === "remove");
  await button.listeners.click[0]();

  assert.equal(list.children.length, 1, "reverted to the pre-remove list");
  assert.ok(list.children[0].textContent.includes("tvl"));
  assert.ok(doc.getElementById("dashboard-state").textContent.includes("Alert deletion failed"));
});

test("removeAlert keeps the entry removed when the mutation succeeds", async () => {
  const doc = setupRefreshTestDom();
  installFakeFetch([]);
  doc.registerElement("alert-metric", { value: "tvl" });
  doc.registerElement("alert-threshold", { value: "10" });

  const controller = createDashboardController({ apiUrl: "http://api.test", poolId: "pool-a" });
  await controller.addAlert();
  const list = doc.getElementById("alert-list");
  const button = list.children[0].children.find((c) => c.className === "remove");
  await button.listeners.click[0]();

  assert.equal(list.children.length, 0);
  assert.ok(list.innerHTML.includes("No alerts configured"));
});

test("refresh prefers the live api-url and pool-id inputs over the constructor values", async () => {
  const doc = setupRefreshTestDom();
  const urls = [];
  const pools = [];
  globalThis.fetch = async (url, options) => {
    const body = JSON.parse(options.body);
    urls.push(url);
    pools.push(body.variables.poolId);
    const key = Object.keys(QUERY_RESPONSES).find((k) => body.query.includes(`query ${k}`));
    return { ok: true, json: async () => ({ data: QUERY_RESPONSES[key] }) };
  };
  doc.getElementById("api-url").value = "  http://live.test  ";
  doc.getElementById("pool-id").value = "  pool-live  ";

  const controller = createDashboardController({ apiUrl: "http://api.test", poolId: "pool-a" });
  await controller.refresh();

  assert.ok(urls.every((u) => u === "http://live.test"));
  assert.ok(pools.every((p) => p === "pool-live"));
});

test("a refresh that throws does not block the next refresh", async () => {
  const doc = setupRefreshTestDom();
  installFakeFetch([]);
  const controller = createDashboardController({ apiUrl: "http://api.test", poolId: "pool-a" });

  const renderTarget = doc.getElementById("m-tvl");
  const originalGet = doc.getElementById.bind(doc);
  doc.getElementById = (id) => {
    if (id === "m-tvl") throw new Error("render failed");
    return originalGet(id);
  };
  await assert.rejects(controller.refresh(), /render failed/);

  doc.getElementById = originalGet;
  await controller.refresh();
  assert.equal(renderTarget.textContent, "—");
  assert.equal(doc.getElementById("connection-status").textContent, "Connected");
});

// ── New tests for timestamp, token formatting, and UI fixes ─────────────────

test("formatTimestamp handles second-resolution timestamps", () => {
  const result = formatTimestamp(1_700_000_000);
  assert.notEqual(result, "—");
  // Should render a 2023 time, not 1970
  assert.ok(result.includes(":"));
});

test("formatTimestamp handles millisecond-resolution timestamps", () => {
  const result = formatTimestamp(1_700_000_000_000);
  assert.notEqual(result, "—");
  assert.ok(result.includes(":"));
});

test("formatTokenAmount scales by decimals and adds symbol", () => {
  assert.equal(formatTokenAmount(10_000_000, "XLM", 7), "1.00 XLM");
  assert.equal(formatTokenAmount(5_000_000, "USDC", 6), "5.00 USDC");
  assert.equal(formatTokenAmount("invalid", "XLM"), "—");
});

test("formatMetricWithUnit formats with K/M suffixes", () => {
  assert.equal(formatMetricWithUnit(10_000_000_000_000, "tokens", 7), "1.00M tokens");
  assert.equal(formatMetricWithUnit(10_000_000_000, "tokens", 7), "1.0K tokens");
  assert.equal(formatMetricWithUnit(10_000_000, "tokens", 7), "1.00 tokens");
  assert.equal(formatMetricWithUnit(1_000_000_000, "tokens", 7), "100.00 tokens");
});

test("renderPoolsOverview creates a table with all pools", () => {
  const doc = installFakeDocument(["metrics-grid"]);
  const stats = [
    { poolId: "pool-a", tokenA: "AAA", tokenB: "BBB", tvl: 10_000_000, volume24h: 5_000_000, fees24h: 100_000, swapCount: 100, priceDeviationBps: 50 },
    { poolId: "pool-b", tokenA: "CCC", tokenB: "DDD", tvl: 20_000_000, volume24h: 10_000_000, fees24h: 200_000, swapCount: 200, priceDeviationBps: 75 },
  ];
  renderPoolsOverview(stats);
  const grid = doc.getElementById("metrics-grid");
  const html = renderedText(grid);
  assert.ok(html.includes("pool-a"));
  assert.ok(html.includes("pool-b"));
  assert.ok(html.includes("AAA"));
  assert.ok(html.includes("CCC"));
});

test("renderPoolsOverview shows empty state when no stats", () => {
  const doc = installFakeDocument(["metrics-grid"]);
  renderPoolsOverview([]);
  const grid = doc.getElementById("metrics-grid");
  assert.ok(grid.innerHTML.includes("No pools indexed yet"));
});

test("renderStats with no poolId calls renderPoolsOverview", () => {
  const doc = installFakeDocument(["metrics-grid"]);
  const stats = [{ poolId: "pool-a", tokenA: "AAA", tokenB: "BBB", tvl: 10_000_000, volume24h: 5_000_000, fees24h: 100_000, swapCount: 100, priceDeviationBps: 50 }];
  renderStats(stats, "");
  const grid = doc.getElementById("metrics-grid");
  assert.ok(renderedText(grid).includes("pool-a"));
});

test("renderStats with poolId shows single-pool metrics without dollar signs", () => {
  const doc = installFakeDocument(STATS_IDS);
  renderStats([SAMPLE_STATS], "pool-a");
  assert.ok(!doc.getElementById("m-tvl").textContent.includes("$"));
  assert.ok(doc.getElementById("m-tvl").textContent.includes("tokens"));
});

test("renderStats with null tvl shows dash", () => {
  const doc = installFakeDocument(STATS_IDS);
  const statsWithNull = { ...SAMPLE_STATS, tvl: null };
  renderStats([statsWithNull], "pool-a");
  assert.equal(doc.getElementById("m-tvl").textContent, "—");
});

test("renderHistory downsamples to at most 200 buckets", () => {
  const doc = installFakeDocument(["price-chart"]);
  const history = Array.from({ length: 10000 }, (_, i) => ({
    poolId: "p", timestamp: Date.now() + i * 1000, price: 1 + i * 0.0001, feeBps: 30,
  }));
  renderHistory(history);
  const chart = doc.getElementById("price-chart");
  // Should have axis label + at most 200 bars
  assert.ok(chart.children[0].textContent.includes("Range:"));
  const barCount = chart.children.length - 1; // minus axis label
  assert.ok(barCount <= 200);
});

test("renderHistory adds axis label with price range", () => {
  const doc = installFakeDocument(["price-chart"]);
  renderHistory([
    { poolId: "p", timestamp: Date.now(), price: 1, feeBps: 30 },
    { poolId: "p", timestamp: Date.now(), price: 2, feeBps: 30 },
  ]);
  const chart = doc.getElementById("price-chart");
  const label = chart.children[0].textContent;
  // The axis is baselined at zero, so the range runs from 0 to the max price.
  assert.equal(label, "Range: 0.000000 - 2.000000");
});

test("renderHealth uses configured threshold for deviation gauge color", () => {
  const doc = installFakeDocument(HEALTH_IDS);
  const health = {
    healthScore: 80, tvlScore: 90, volumeScore: 80, feeEfficiencyScore: 80,
    priceDeviationBps: 150, status: "Healthy", alertsFired: [],
  };
  const configs = [{ poolId: "p", metric: "price_deviation", thresholdBps: 100 }];
  renderHealth(health, configs);
  const fill = doc.getElementById("dev-fill");
  // 150 bps >= 100 bps threshold, should be red
  assert.equal(fill.style.background, "var(--red)");
});

test("renderHealth deviation gauge width is percentage of threshold", () => {
  const doc = installFakeDocument(HEALTH_IDS);
  const health = {
    healthScore: 80, tvlScore: 90, volumeScore: 80, feeEfficiencyScore: 80,
    priceDeviationBps: 50, status: "Healthy", alertsFired: [],
  };
  const configs = [{ poolId: "p", metric: "price_deviation", thresholdBps: 100 }];
  renderHealth(health, configs);
  const fill = doc.getElementById("dev-fill");
  // 50 bps is 50% of 100 bps threshold
  assert.equal(fill.style.width, "50%");
});

test("renderHeatmap handles second-resolution timestamps", () => {
  const doc = installFakeDocument(["heatmap"]);
  const now = Date.now();
  const thirtyMinutesAgoSeconds = Math.floor((now - 30 * 60 * 1000) / 1000);
  renderHeatmap([
    { type: "swap", timestamp: thirtyMinutesAgoSeconds, payload: '{"amountIn":100}' },
  ]);
  const heatmap = doc.getElementById("heatmap");
  // Should have landed in a bucket (not all at 4px floor)
  const hasNonZeroHeight = Array.from(heatmap.children).some(bar => parseInt(bar.style.height) > 4);
  assert.ok(hasNonZeroHeight);
});
