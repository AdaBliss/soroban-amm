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

// Token decimals map for Stellar assets (7 decimals for most)
const TOKEN_DECIMALS = {
  "default": 7,
};

export function formatTokenAmount(amount, symbol, decimals = TOKEN_DECIMALS.default) {
  const value = Number(amount);
  if (!Number.isFinite(value)) return "—";
  const scaled = value / Math.pow(10, decimals);
  return `${scaled.toFixed(2)} ${symbol}`;
}

export function formatMetricWithUnit(value, symbol, decimals = TOKEN_DECIMALS.default) {
  const num = Number(value);
  if (!Number.isFinite(num)) return "—";
  const scaled = num / Math.pow(10, decimals);
  if (scaled >= 1_000_000) return `${(scaled / 1_000_000).toFixed(2)}M ${symbol}`;
  if (scaled >= 1_000) return `${(scaled / 1_000).toFixed(1)}K ${symbol}`;
  return `${scaled.toFixed(2)} ${symbol}`;
}

export function formatTimestamp(timestamp) {
  const ts = Number(timestamp);
  if (!Number.isFinite(ts)) return "—";
  // Auto-detect: timestamps < 1e10 are seconds (Unix epoch), >= 1e10 are milliseconds
  const ms = ts < 1_000_000_00000 ? ts * 1000 : ts;
  const date = new Date(ms);
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

export function renderStats(stats, poolId) {
  // When no pool is selected, show overview table instead of single-pool stats
  if (!poolId) {
    renderPoolsOverview(stats);
    return;
  }
  const value = stats?.[0];
  if (!value) { ["m-tvl", "m-vol", "m-fees", "m-swaps"].forEach((id) => setText(id, "—")); return; }
  // Show raw token amounts with symbols, not dollar signs
  const tvlDisplay = value.tvl === null ? "—" : formatMetricWithUnit(value.tvl, "tokens");
  const volDisplay = value.volume24h === null ? "—" : formatMetricWithUnit(value.volume24h, "tokens");
  const feesDisplay = value.fees24h === null ? "—" : formatMetricWithUnit(value.fees24h, "tokens");
  setText("m-tvl", tvlDisplay);
  setText("m-tvl-sub", `Token A: ${value.tokenA} / Token B: ${value.tokenB}`);
  setText("m-vol", volDisplay);
  setText("m-vol-sub", `Fee income: ${feesDisplay}`);
  setText("m-fees", feesDisplay);
  setText("m-fees-sub", `Swap count: ${Number(value.swapCount).toLocaleString()}`);
  setText("m-swaps", Number(value.swapCount).toLocaleString());
}

export function renderPoolsOverview(stats) {
  const grid = $("metrics-grid");
  if (!grid) return;
  grid.replaceChildren();
  if (!stats?.length) {
    grid.innerHTML = '<div class="empty-state" style="grid-column:1/-1">No pools indexed yet. Select a Pool ID to view single-pool metrics.</div>';
    return;
  }
  // Create a table for all pools
  const table = document.createElement("table");
  table.className = "pools-table";
  const thead = document.createElement("thead");
  thead.innerHTML = '<tr><th>Pool ID</th><th>Token A</th><th>Token B</th><th>TVL</th><th>24h Volume</th><th>24h Fees</th><th>Swaps</th><th>Price Deviation</th></tr>';
  table.appendChild(thead);
  const tbody = document.createElement("tbody");
  for (const pool of stats) {
    const row = document.createElement("tr");
    const tvlDisplay = pool.tvl === null ? "—" : formatMetricWithUnit(pool.tvl, "tokens");
    const volDisplay = pool.volume24h === null ? "—" : formatMetricWithUnit(pool.volume24h, "tokens");
    const feesDisplay = pool.fees24h === null ? "—" : formatMetricWithUnit(pool.fees24h, "tokens");
    row.innerHTML = `<td>${pool.poolId}</td><td>${pool.tokenA}</td><td>${pool.tokenB}</td><td>${tvlDisplay}</td><td>${volDisplay}</td><td>${feesDisplay}</td><td>${Number(pool.swapCount).toLocaleString()}</td><td>${pool.priceDeviationBps} bps</td>`;
    tbody.appendChild(row);
  }
  table.appendChild(tbody);
  const wrapper = document.createElement("div");
  wrapper.style.gridColumn = "1 / -1";
  wrapper.appendChild(table);
  grid.appendChild(wrapper);
  // Show prompt for single-pool panels
  const prompt = document.createElement("div");
  prompt.className = "empty-state";
  prompt.style.gridColumn = "1 / -1";
  prompt.style.marginTop = "24px";
  prompt.textContent = "Select a Pool ID above to view health score, price history, and alerts for a specific pool.";
  grid.appendChild(prompt);
}

export function renderHealth(health, alertConfigs = []) {
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
  
  // Find configured price_deviation threshold (default to 100 bps if not configured)
  const priceAlert = alertConfigs.find(c => c.metric === "price_deviation");
  const thresholdBps = priceAlert?.thresholdBps || 100;
  const currentBps = Number(health.priceDeviationBps);
  
  const fill = $("dev-fill");
  if (fill) {
    // Width is percentage of threshold (capped at 100%)
    const widthPercent = Math.min(100, (currentBps / thresholdBps) * 100);
    fill.style.width = `${widthPercent}%`;
    // Color flips at threshold: green/yellow below, red at or above
    fill.style.background = currentBps >= thresholdBps ? "var(--red)" : healthColor(100 - (currentBps / thresholdBps) * 100);
  }
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
  
  // Downsample to at most 200 buckets with min/max per bucket
  const MAX_BUCKETS = 200;
  const bucketSize = Math.ceil(history.length / MAX_BUCKETS);
  const buckets = [];
  
  for (let i = 0; i < history.length; i += bucketSize) {
    const slice = history.slice(i, i + bucketSize);
    const prices = slice.map(p => Number(p.price));
    buckets.push({
      min: Math.min(...prices),
      max: Math.max(...prices),
      timestamp: slice[0].timestamp,
      price: slice[Math.floor(slice.length / 2)].price, // representative price
    });
  }
  
  const globalMax = Math.max(...buckets.map(b => b.max), 1);
  const globalMin = Math.min(...buckets.map(b => b.min), 0);
  
  // Add axis label showing price range
  const axisLabel = document.createElement("div");
  axisLabel.className = "chart-axis-label";
  axisLabel.textContent = `Range: ${globalMin.toFixed(6)} - ${globalMax.toFixed(6)}`;
  chart.appendChild(axisLabel);
  
  // Render bars representing min/max range for each bucket
  for (const bucket of buckets) {
    const bar = document.createElement("div");
    bar.className = "chart-bar";
    const height = Math.max(2, (bucket.max - bucket.min) / (globalMax - globalMin || 1) * 136);
    const top = ((globalMax - bucket.max) / (globalMax - globalMin || 1)) * 136;
    bar.style.height = `${height}px`;
    bar.style.marginTop = `${top}px`;
    bar.title = `${bucket.min.toFixed(6)} - ${bucket.max.toFixed(6)} @ ${formatTimestamp(bucket.timestamp)}`;
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
    try {
      const ts = Number(event.timestamp);
      const ms = ts < 1_000_000_00000 ? ts * 1000 : ts;
      const hoursAgo = Math.floor((now - ms) / 3_600_000);
      if (hoursAgo >= 0 && hoursAgo < 24) buckets[23 - hoursAgo] += Number(JSON.parse(event.payload || "{}").amountIn || 0);
    } catch { /* malformed payloads do not break the dashboard */ }
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
      renderStats(data.stats?.poolStats, currentPool); renderHealth(currentPool ? data.health?.poolHealth : null, configs); renderEvents(currentPool ? data.events?.poolEvents : []); renderHistory(currentPool ? data.history?.priceHistory : []); renderHeatmap(currentPool ? data.events?.poolEvents : []); if (data.alerts?.alertConfigs) { configs = data.alerts.alertConfigs; renderAlertConfigs(configs, removeAlert); }
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
