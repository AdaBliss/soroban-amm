// @ts-check
export const DEFAULT_API_URL = "http://localhost:4000/graphql";
export const DEFAULT_POLL_INTERVAL_MS = 5000;

export function formatMetric(value) {
  const number = Number(value);
  if (!Number.isFinite(number)) return "—";
  if (number >= 1_000_000) return `${(number / 1_000_000).toFixed(2)}M`;
  if (number >= 1_000) return `${(number / 1_000).toFixed(1)}K`;
  return number.toFixed(2);
}

export function formatTimestamp(timestamp) {
  const date = new Date(Number(timestamp));
  return Number.isNaN(date.getTime()) ? "—" : date.toLocaleTimeString([], {
    hour: "2-digit", minute: "2-digit", second: "2-digit",
  });
}

export function healthColor(score) {
  const value = Number(score);
  return value >= 70 ? "var(--green)" : value >= 40 ? "var(--yellow)" : "var(--red)";
}

export function compareThreshold(value, threshold, direction = "above") {
  const current = Number(value);
  const limit = Number(threshold);
  if (!Number.isFinite(current) || !Number.isFinite(limit)) return false;
  return direction === "below" ? current < limit : current > limit;
}

export function formatError(error) {
  return error instanceof Error ? error.message : String(error || "Unknown error");
}

export async function gql(apiUrl, query, variables = {}) {
  const response = await fetch(apiUrl, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ query, variables }),
  });
  if (!response.ok) throw new Error(`GraphQL API returned HTTP ${response.status}`);
  const payload = await response.json();
  if (payload.errors?.length) throw new Error(payload.errors.map((item) => item.message).join("; "));
  return payload.data;
}

const QUERY = {
  stats: `query Stats($poolId: ID) { poolStats(poolId: $poolId) { poolId tokenA tokenB tvl volume24h fees24h swapCount priceDeviationBps } }`,
  health: `query Health($poolId: ID!) { poolHealth(poolId: $poolId) { poolId healthScore tvlScore volumeScore feeEfficiencyScore priceDeviationBps status alertsFired { poolId metric threshold currentValue firedAt } } }`,
  alerts: `query Alerts($poolId: ID) { alertConfigs(poolId: $poolId) { poolId metric thresholdBps thresholdValue } }`,
  history: `query History($poolId: ID!) { priceHistory(poolId: $poolId, from: 0) { poolId timestamp price feeBps } }`,
  events: `query Events($poolId: ID) { poolEvents(poolId: $poolId, limit: 200) { id poolId type timestamp payload } }`,
};

function $(id) { return document.getElementById(id); }
/** @param {string} id @returns {HTMLInputElement | HTMLSelectElement | null} */
function field(id) { return /** @type {HTMLInputElement | HTMLSelectElement | null} */ ($(id)); }
function setText(id, value) { const element = $(id); if (element) element.textContent = value; }
function setSectionState(message) {
  const state = $("dashboard-state");
  if (state) { state.textContent = message; state.hidden = !message; }
}

export function renderStats(stats) {
  const value = stats?.[0];
  if (!value) { ["m-tvl", "m-vol", "m-fees", "m-swaps"].forEach((id) => setText(id, "—")); return; }
  setText("m-tvl", `$${formatMetric(value.tvl)}`);
  setText("m-tvl-sub", `Token A: ${value.tokenA} / Token B: ${value.tokenB}`);
  setText("m-vol", `$${formatMetric(value.volume24h)}`);
  setText("m-vol-sub", `Fee income: $${formatMetric(value.fees24h)}`);
  setText("m-fees", `$${formatMetric(value.fees24h)}`);
  setText("m-fees-sub", `Swap count: ${Number(value.swapCount).toLocaleString()}`);
  setText("m-swaps", Number(value.swapCount).toLocaleString());
}

export function renderHealth(health) {
  if (!health) { setText("health-score-text", "—"); setText("hd-status", "Unavailable"); return; }
  const score = Number(health.healthScore);
  const circumference = 2 * Math.PI * 34;
  const arc = $("health-arc");
  if (arc) { arc.setAttribute("stroke-dashoffset", (circumference * (1 - score / 100)).toFixed(1)); arc.setAttribute("stroke", healthColor(score)); }
  setText("health-score-text", Number.isFinite(score) ? score.toFixed(0) : "—");
  setText("hd-status", health.status || "Unknown");
  setText("hd-tvl", Number(health.tvlScore).toFixed(1));
  setText("hd-vol", Number(health.volumeScore).toFixed(1));
  setText("hd-fee", Number(health.feeEfficiencyScore).toFixed(1));
  setText("hd-dev", `${health.priceDeviationBps} bps`);
  setText("dev-bps", `${health.priceDeviationBps} bps`);
  const fill = $("dev-fill");
  if (fill) { fill.style.width = `${Math.min(100, Number(health.priceDeviationBps) / 2)}%`; fill.style.background = healthColor(100 - Number(health.priceDeviationBps)); }
  renderFiredAlerts(health.alertsFired || []);
}

export function renderFiredAlerts(alerts) {
  const list = $("fired-alert-list");
  if (!list) return;
  list.replaceChildren();
  if (!alerts.length) { list.innerHTML = '<div class="empty-state">No alerts have fired for this pool</div>'; return; }
  for (const alert of alerts) {
    const item = document.createElement("div");
    item.className = "alert-item alert-fired";
    item.textContent = `${alert.metric} exceeded ${alert.threshold}; current ${Number(alert.currentValue).toFixed(0)} at ${formatTimestamp(alert.firedAt)}`;
    list.prepend(item);
  }
}

export function renderEvents(events) {
  const tbody = $("events-body");
  if (!tbody) return;
  tbody.replaceChildren();
  if (!events?.length) { tbody.innerHTML = '<tr><td colspan="4" class="empty-state">No events indexed yet</td></tr>'; return; }
  for (const event of events.slice(0, 50)) {
    const row = document.createElement("tr");
    [formatTimestamp(event.timestamp), event.type, event.poolId, event.payload || ""].forEach((value) => { const cell = document.createElement("td"); cell.textContent = value; row.appendChild(cell); });
    tbody.appendChild(row);
  }
}

export function renderHistory(history) {
  const chart = $("price-chart");
  if (!chart) return;
  chart.replaceChildren();
  if (!history?.length) { chart.innerHTML = '<div class="empty-state" style="width:100%">No price history indexed yet</div>'; return; }
  const max = Math.max(...history.map((point) => Number(point.price)), 1);
  for (const point of history) {
    const bar = document.createElement("div");
    bar.className = "chart-bar";
    bar.style.height = `${Math.max(2, Number(point.price) / max * 136)}px`;
    bar.title = `${Number(point.price).toFixed(6)} @ ${formatTimestamp(point.timestamp)}`;
    chart.appendChild(bar);
  }
}

export function renderHeatmap(events) {
  const heatmap = $("heatmap");
  if (!heatmap) return;
  heatmap.replaceChildren();
  const buckets = new Array(24).fill(0);
  const now = Date.now();
  for (const event of events || []) {
    if (event.type !== "swap") continue;
    try { const hoursAgo = Math.floor((now - Number(event.timestamp)) / 3_600_000); if (hoursAgo >= 0 && hoursAgo < 24) buckets[23 - hoursAgo] += Number(JSON.parse(event.payload || "{}").amountIn || 0); } catch { /* malformed payloads do not break the dashboard */ }
  }
  const max = Math.max(...buckets, 1);
  buckets.forEach((amount, index) => { const bar = document.createElement("div"); bar.className = "heatmap-bar"; bar.style.height = `${Math.max(4, amount / max * 56)}px`; bar.title = `${23 - index}h ago: ${formatMetric(amount)}`; heatmap.appendChild(bar); });
}

export function renderAlertConfigs(configs, onRemove) {
  const list = $("alert-list");
  if (!list) return;
  list.replaceChildren();
  if (!configs.length) { list.innerHTML = '<div class="empty-state">No alerts configured for this pool</div>'; return; }
  configs.forEach((alert) => { const item = document.createElement("div"); item.className = "alert-item"; const label = alert.metric === "price_deviation" ? `${alert.metric} > ${alert.thresholdBps} bps` : `${alert.metric} > ${alert.thresholdValue}`; item.textContent = `${label} `; const button = document.createElement("button"); button.className = "remove"; button.type = "button"; button.textContent = "×"; button.setAttribute("aria-label", `Remove ${alert.metric} alert`); button.addEventListener("click", () => onRemove(alert)); item.appendChild(button); list.appendChild(item); });
}

function addDashboardMessages() {
  const toolbar = document.querySelector(".toolbar");
  if (!$("last-updated")) { const label = document.createElement("span"); label.id = "last-updated"; label.className = "last-updated"; toolbar?.appendChild(label); }
  if (!$("btn-retry")) { const button = document.createElement("button"); button.id = "btn-retry"; button.className = "secondary"; button.type = "button"; button.textContent = "Retry"; button.hidden = true; toolbar?.appendChild(button); }
  if (!$("dashboard-state")) { const state = document.createElement("div"); state.id = "dashboard-state"; state.className = "dashboard-state"; state.setAttribute("role", "status"); document.querySelector("main")?.prepend(state); }
}

/** @param {{ apiUrl?: string, poolId?: string, pollIntervalMs?: number }} [options] */
export function createDashboardController({ apiUrl, poolId, pollIntervalMs = DEFAULT_POLL_INTERVAL_MS } = {}) {
  /** @type {Array<{ poolId: string, metric: string, thresholdBps?: number, thresholdValue?: number }>} */
  let configs = [];
  /** @type {ReturnType<typeof setInterval> | undefined} */
  let timer;
  let refreshing = false;
  const url = apiUrl || DEFAULT_API_URL;
  const pool = poolId || "";
  const status = $("connection-status");
  const setStatus = (text, className, error = "") => { if (status) { status.textContent = text; status.className = className; status.title = error; } };

  async function refresh() {
    if (refreshing) return;
    refreshing = true;
    try {
      setStatus("Connecting…", "");
      setSectionState("Loading pool data…");
      ["m-tvl", "m-vol", "m-fees", "m-swaps", "health-score-text"].forEach((id) => setText(id, "—"));
      const currentUrl = field("api-url")?.value.trim() || url;
      const currentPool = field("pool-id")?.value.trim() || pool;
      const entries = Object.entries(QUERY).map(async ([key, query]) => [key, await gql(currentUrl, query, { poolId: currentPool })]);
      const results = await Promise.allSettled(entries);
      /** @type {Record<string, any>} */ const data = {};
      /** @type {string[]} */ const failures = [];
      results.forEach((result, index) => { const key = Object.keys(QUERY)[index]; if (result.status === "fulfilled") data[key] = result.value; else failures.push(`${key}: ${formatError(result.reason)}`); });
      renderStats(data.stats?.poolStats); renderHealth(data.health?.poolHealth); renderEvents(data.events?.poolEvents); renderHistory(data.history?.priceHistory); renderHeatmap(data.events?.poolEvents); if (data.alerts?.alertConfigs) { configs = data.alerts.alertConfigs; renderAlertConfigs(configs, removeAlert); }
      if (failures.length === Object.keys(QUERY).length) { setStatus("Error", "error", failures.join("; ")); setSectionState(`Unable to load dashboard data. ${failures[0]}`); const retry = $("btn-retry"); if (retry) retry.hidden = false; }
      // Retry is only useful when something failed; hide it on a fully successful refresh.
      else { setStatus(failures.length ? "Degraded" : "Connected", failures.length ? "error" : "connected", failures.join("; ")); setSectionState(failures.length ? `Degraded: ${failures.join("; ")}` : data.stats?.poolStats?.length ? "" : "No pools indexed yet"); const retry = $("btn-retry"); if (retry) retry.hidden = !failures.length; setText("last-updated", `Last updated ${new Date().toLocaleTimeString()}`); }
    } finally {
      // Released even if rendering throws, so one bad response cannot wedge the dashboard.
      refreshing = false;
    }
  }

  async function addAlert() {
    const metric = field("alert-metric")?.value; const thresholdInput = field("alert-threshold"); const threshold = Number(thresholdInput?.value); if (!metric || !Number.isFinite(threshold) || threshold < 0) return;
    // "price_deviation" is basis points; "tvl" and "volume24h" are raw values in the metric's native units.
    const thresholdBps = metric === "price_deviation" ? threshold : undefined;
    const thresholdValue = metric === "price_deviation" ? undefined : threshold;
    const currentPool = field("pool-id")?.value.trim() || pool;
    // Dedup by metric AND poolId — filtering by metric alone would drop other pools' alerts
    // for the same metric when the Pool ID field is blank (alertConfigs(poolId: undefined) spans all pools).
    const previous = configs; const next = [...configs.filter((item) => !(item.metric === metric && item.poolId === currentPool)), { poolId: currentPool, metric, thresholdBps, thresholdValue }]; configs = next; renderAlertConfigs(configs, removeAlert); if (thresholdInput) thresholdInput.value = "";
    const currentUrl = field("api-url")?.value.trim() || url;
    try { await gql(currentUrl, `mutation SetAlert($poolId: ID!, $metric: String!, $thresholdBps: Int, $thresholdValue: Float) { setAlertConfig(poolId: $poolId, metric: $metric, thresholdBps: $thresholdBps, thresholdValue: $thresholdValue) { poolId metric thresholdBps thresholdValue } }`, { poolId: currentPool, metric, thresholdBps, thresholdValue }); } catch (error) { configs = previous; renderAlertConfigs(configs, removeAlert); setSectionState(`Alert was not saved: ${formatError(error)}`); }
  }
  async function removeAlert(alert) {
    const previous = configs; configs = configs.filter((item) => !(item.metric === alert.metric && item.poolId === alert.poolId)); renderAlertConfigs(configs, removeAlert);
    const currentUrl = field("api-url")?.value.trim() || url;
    try { await gql(currentUrl, `mutation RemoveAlert($poolId: ID!, $metric: String!) { removeAlertConfig(poolId: $poolId, metric: $metric) }`, { poolId: alert.poolId, metric: alert.metric }); } catch (error) { configs = previous; renderAlertConfigs(configs, removeAlert); setSectionState(`Alert deletion failed: ${formatError(error)}`); }
  }

  // Listeners and timers drop the promise a handler returns, so route every
  // background task through here and surface failures in the dashboard.
  /** @param {() => Promise<void>} task */
  const background = (task) => () => { task().catch((error) => setSectionState(`Dashboard error: ${formatError(error)}`)); };
  const refreshInBackground = background(refresh);

  return { refresh, addAlert, start() { addDashboardMessages(); const apiInput = field("api-url"); if (apiInput) apiInput.value = url; const poolInput = field("pool-id"); if (poolInput) poolInput.value = pool; $("btn-refresh")?.addEventListener("click", refreshInBackground); $("btn-retry")?.addEventListener("click", refreshInBackground); $("btn-add-alert")?.addEventListener("click", background(addAlert)); const auto = $("btn-auto"); auto?.addEventListener("click", () => { const on = auto.dataset.on !== "true"; auto.dataset.on = String(on); auto.textContent = `Auto-refresh: ${on ? "ON" : "OFF"}`; if (on) { refreshInBackground(); timer = setInterval(refreshInBackground, pollIntervalMs); } else clearInterval(timer); }); refreshInBackground(); renderAlertConfigs(configs, removeAlert); } };
}

if (typeof window !== "undefined") {
  const params = new URLSearchParams(window.location.search);
  const controller = createDashboardController({ apiUrl: params.get("api") || params.get("apiUrl") || DEFAULT_API_URL, poolId: params.get("pool") || "", pollIntervalMs: Math.max(1000, Number(params.get("interval")) || DEFAULT_POLL_INTERVAL_MS) });
  controller.start();
}
