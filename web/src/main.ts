import { Chart, registerables } from "chart.js";
import {
  deriveModelScorecard,
  deriveQuotaRows,
  formatWindowDuration,
  histogramPercentile,
  summarizeReport,
  weightedTps,
} from "./metrics";
import {
  MIXED_ATTRIBUTION_KEY,
  PRIMARY_PERCENTILE,
  QUOTA_COST_BUCKET_LABELS,
  REPORT_PERCENTILES,
  REPORT_PERCENTILE_COMMA,
  REPORT_PERCENTILE_SLASH,
  StatsReport,
  TPS_BUCKET_LABELS,
  validateDataManifest,
  validateStatsReport,
} from "./schema";
import "./style.css";

Chart.register(...registerables);

type WindowKey = "24h" | "7d" | "30d";

const PALETTE = ["#5b9dff", "#40d19e", "#f6b73c", "#f472b6", "#a78bfa", "#38bdf8"];

const state: {
  community: StatsReport | null;
  hourly: StatsReport[];
  local: StatsReport | null;
  activeWindow: WindowKey;
  trendQuantile: number;
  compareMode: boolean;
  charts: Record<string, Chart | undefined>;
} = {
  community: null,
  hourly: [],
  local: null,
  activeWindow: "24h",
  trendQuantile: PRIMARY_PERCENTILE.q,
  compareMode: true,
  charts: {},
};

const bannerEl = document.getElementById("status-banner") as HTMLDivElement;
const kpiGridEl = document.getElementById("kpi-grid") as HTMLDivElement;
const clearLocalBtnEl = document.getElementById("clear-local-btn") as HTMLButtonElement;
const compareWrapEl = document.getElementById("compare-wrap") as HTMLLabelElement;
const compareToggleEl = document.getElementById("compare-toggle") as HTMLInputElement;
const subTpsTrendEl = document.getElementById("sub-tps-trend") as HTMLSpanElement;
const trendPctSelectEl = document.getElementById("trend-pct-select") as HTMLSelectElement;
const histSelectEl = document.getElementById("hist-model-select") as HTMLSelectElement;
const quotaHistSelectEl = document.getElementById("quota-hist-select") as HTMLSelectElement;
const fileInputEl = document.getElementById("file-input") as HTMLInputElement;
const dropZoneEl = document.getElementById("drop-zone") as HTMLElement;

function initPercentileLabelsAndControls() {
  const thTps = document.getElementById("th-tps-percentiles");
  if (thTps) thTps.textContent = `TPS (${REPORT_PERCENTILE_SLASH})`;
  const thQuota = document.getElementById("th-quota-percentiles");
  if (thQuota) thQuota.textContent = `$/1% (${REPORT_PERCENTILE_SLASH})`;
  const thRelWeight = document.getElementById("th-quota-rel-weight");
  if (thRelWeight) thRelWeight.textContent = `Rel Weight (${PRIMARY_PERCENTILE.key})`;
  const subTps = document.getElementById("sub-tps-model");
  if (subTps) subTps.textContent = `Estimated ${REPORT_PERCENTILE_COMMA} output tokens/s`;
  const subQuota = document.getElementById("sub-quota-model");
  if (subQuota)
    subQuota.textContent = `Estimated ${REPORT_PERCENTILE_COMMA} Reference USD per 1% Quota`;

  trendPctSelectEl.replaceChildren();
  REPORT_PERCENTILES.forEach((spec, idx) => {
    const opt = document.createElement("option");
    opt.value = String(spec.q);
    opt.textContent = spec.label;
    if (idx === 0) opt.selected = true;
    trendPctSelectEl.appendChild(opt);
  });
}

initPercentileLabelsAndControls();

const NO_ONLINE_DATA_BANNER =
  'No online ./data/manifest.json found yet — click "Import Stats JSON" or drop an `antigravity-status stats --json` file here to inspect.';

function formatCompact(n: number): string {
  if (n >= 1_000_000_000) return `${(n / 1_000_000_000).toFixed(2)}B`;
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(2)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}K`;
  return String(Math.round(n));
}

function formatUsd(n: number): string {
  if (!Number.isFinite(n)) return "$0.00";
  if (n > 0 && Math.abs(n) < 0.1) return `$${n.toFixed(4)}`;
  return `$${n.toFixed(2)}`;
}

function formatQuotaRateUsd(n: number): string {
  if (!Number.isFinite(n)) return "$0.0000";
  return Math.abs(n) < 1 ? `$${n.toFixed(4)}` : `$${n.toFixed(2)}`;
}

function formatUtcTimestamp(iso: string, includeYear = true): string {
  const ms = Date.parse(iso);
  if (!Number.isFinite(ms)) {
    return includeYear ? iso.slice(0, 16) : iso.slice(5, 16).replace("T", " ");
  }
  const d = new Date(ms);
  const pad2 = (n: number) => String(n).padStart(2, "0");
  const mmddhhmm = `${pad2(d.getUTCMonth() + 1)}-${pad2(d.getUTCDate())} ${pad2(d.getUTCHours())}:${pad2(d.getUTCMinutes())}`;
  return includeYear ? `${d.getUTCFullYear()}-${mmddhhmm} UTC` : mmddhhmm;
}

function setBanner(msg: string | null) {
  if (!msg) {
    bannerEl.hidden = true;
    bannerEl.textContent = "";
  } else {
    bannerEl.hidden = false;
    bannerEl.textContent = msg;
  }
}

function activePrimaryReport(): StatsReport | null {
  return state.local ?? state.community;
}

function destroyChart(id: string) {
  state.charts[id]?.destroy();
  state.charts[id] = undefined;
}

function appendKpi(label: string, value: string, sub: string) {
  const card = document.createElement("div");
  card.className = "kpi";
  const l = document.createElement("div");
  l.className = "label";
  l.textContent = label;
  const v = document.createElement("div");
  v.className = "value";
  v.textContent = value;
  const s = document.createElement("div");
  s.className = "sub";
  s.textContent = sub;
  card.append(l, v, s);
  kpiGridEl.appendChild(card);
}

function renderKpis(primary: StatsReport) {
  kpiGridEl.replaceChildren();
  const s = summarizeReport(primary);
  const contributorLabel =
    primary.scope === "local" ? "Contributors" : "Peak/hr contributors";
  const winLabel = formatWindowDuration(primary.window.duration_secs);

  appendKpi(
    "Window & Scope",
    winLabel,
    `${formatUtcTimestamp(primary.window.start)} -> ${formatUtcTimestamp(primary.window.end)} · ${primary.scope} (${contributorLabel}: ${primary.contributors})`,
  );
  const tokenBreakdown =
    s.cacheWriteTokens > 0
      ? `In ${formatCompact(s.inputTokens)} · CacheR ${formatCompact(s.cacheReadTokens)} · CacheW ${formatCompact(s.cacheWriteTokens)} · Out ${formatCompact(s.outputTokens)}`
      : `In ${formatCompact(s.inputTokens)} · CacheR ${formatCompact(s.cacheReadTokens)} · Out ${formatCompact(s.outputTokens)}`;
  appendKpi("Total Tokens", formatCompact(s.totalTokens), tokenBreakdown);
  appendKpi(
    "Activity & Active Models",
    `${s.events} ev · ${s.tpsSamples} turns`,
    s.topModelLabel
      ? `${s.activeModels} model(s) · Top: ${s.topModelLabel} (${s.topModelSharePct.toFixed(1)}%)`
      : `${s.activeModels} model(s)`,
  );
  appendKpi(
    "Reference Cost",
    formatUsd(s.refCostUsd),
    `Usage API eq. · ${s.quotaSlices} settled quota slice(s) (${formatUsd(s.quotaRefCostUsd)})`,
  );
}

function renderTpsTrendChart() {
  destroyChart("trend");
  if (state.local && !state.compareMode) {
    subTpsTrendEl.textContent = "Community hourly comparison disabled";
    return;
  }
  subTpsTrendEl.textContent = "Per-model throughput across finalized hours (latest 24h / 48h, UTC)";
  const canvas = document.getElementById("chart-tps-trend") as HTMLCanvasElement;
  const hours = state.hourly;
  if (hours.length === 0) return;

  const q = state.trendQuantile;
  const qLabel =
    REPORT_PERCENTILES.find((spec) => Math.abs(spec.q - q) < 1e-6)?.key ??
    `p${Math.round(q * 100)}`;
  const modelKeys = Array.from(
    new Set(
      hours.flatMap((h) => h.tps_by_model_effort.map((t) => `${t.model} (${t.effort})`)),
    ),
  );
  const labels = hours.map((h) => formatUtcTimestamp(h.window.start, false));
  const datasets = modelKeys.map((label, i) => ({
    label: `${label} ${qLabel}`,
    data: hours.map((h) => {
      const row = h.tps_by_model_effort.find((t) => `${t.model} (${t.effort})` === label);
      if (!row) return null;
      const pct = histogramPercentile(row.histogram, q);
      return Number((pct ?? weightedTps(row)).toFixed(2));
    }),
    borderColor: PALETTE[i % PALETTE.length],
    backgroundColor: PALETTE[i % PALETTE.length],
    tension: 0.25,
    spanGaps: true,
  }));

  state.charts["trend"] = new Chart(canvas, {
    type: "line",
    data: { labels, datasets },
    options: {
      responsive: true,
      maintainAspectRatio: false,
      scales: { y: { beginAtZero: true, title: { display: true, text: `${qLabel} tokens/s` } } },
    },
  });
}

function renderTpsModelChart(primary: StatsReport, baseline: StatsReport | null) {
  destroyChart("modelTps");
  const canvas = document.getElementById("chart-tps-model") as HTMLCanvasElement;
  const rows = primary.tps_by_model_effort;
  const labels = rows.map((r) => `${r.model} (${r.effort})`);
  const pctPalette = [PALETTE[0], PALETTE[2], PALETTE[3], PALETTE[4]];
  const datasets: Array<{ label: string; data: number[]; backgroundColor: string }> =
    REPORT_PERCENTILES.map((spec, idx) => ({
      label: state.local ? `Local ${spec.key} TPS` : `Est. ${spec.key} TPS`,
      data: rows.map((r) => {
        const val =
          histogramPercentile(r.histogram, spec.q) ??
          (idx === 0 ? weightedTps(r) : null);
        return val ? Number(val.toFixed(1)) : 0;
      }),
      backgroundColor: pctPalette[idx % pctPalette.length],
    }));

  if (baseline && state.local && state.compareMode) {
    const baseData = rows.map((r) => {
      const match = baseline.tps_by_model_effort.find(
        (b) => b.model === r.model && b.effort === r.effort,
      );
      if (!match) return 0;
      const val =
        histogramPercentile(match.histogram, PRIMARY_PERCENTILE.q) ??
        weightedTps(match);
      return val ? Number(val.toFixed(1)) : 0;
    });
    datasets.push({
      label: `Community ${PRIMARY_PERCENTILE.key} TPS`,
      data: baseData,
      backgroundColor: PALETTE[1],
    });
  }

  state.charts["modelTps"] = new Chart(canvas, {
    type: "bar",
    data: { labels, datasets },
    options: {
      responsive: true,
      maintainAspectRatio: false,
      scales: { y: { beginAtZero: true, title: { display: true, text: "tokens/s" } } },
    },
  });
}

function renderHistogram(primary: StatsReport) {
  destroyChart("hist");
  const canvas = document.getElementById("chart-tps-hist") as HTMLCanvasElement;
  const rows = primary.tps_by_model_effort;

  const prevValue = histSelectEl.value;
  histSelectEl.replaceChildren();
  rows.forEach((r, idx) => {
    const opt = document.createElement("option");
    const key = `${r.model} (${r.effort})`;
    opt.value = key;
    opt.textContent = `${key} (${r.sample_count})`;
    if (key === prevValue || (!prevValue && idx === 0)) opt.selected = true;
    histSelectEl.appendChild(opt);
  });

  const selected = rows.find((r) => `${r.model} (${r.effort})` === histSelectEl.value) ?? rows[0];
  if (!selected) return;

  state.charts["hist"] = new Chart(canvas, {
    type: "bar",
    data: {
      labels: TPS_BUCKET_LABELS,
      datasets: [
        {
          label: `${selected.model} (${selected.effort}) samples`,
          data: selected.histogram,
          backgroundColor: PALETTE[1],
        },
      ],
    },
    options: {
      responsive: true,
      maintainAspectRatio: false,
      scales: {
        x: { title: { display: true, text: "tokens/s bucket" } },
        y: { beginAtZero: true, title: { display: true, text: "samples" } },
      },
    },
  });
}

function quotaRowLabel(row: {
  plan_tier: string;
  bucket: string;
  attributed_model: string;
  attributed_effort: string;
}): string {
  return `${row.plan_tier} · ${row.bucket} · ${row.attributed_model} (${row.attributed_effort})`;
}

function renderQuotaChartAndTable(primary: StatsReport) {
  destroyChart("quota");
  const canvas = document.getElementById("chart-quota") as HTMLCanvasElement;
  const derived = deriveQuotaRows(primary.quota_by_plan_bucket_model);

  const labels = derived.map((d) => quotaRowLabel(d.row));
  const pctPalette = [PALETTE[0], PALETTE[2], PALETTE[3], PALETTE[4]];
  const datasets = REPORT_PERCENTILES.map((spec, idx) => ({
    label: `${spec.label} ($/1% Quota)`,
    data: derived.map((d) =>
      Number((d.percentiles[spec.key] ?? d.costPer1PctUsd).toFixed(4)),
    ),
    backgroundColor: pctPalette[idx % pctPalette.length],
  }));

  state.charts["quota"] = new Chart(canvas, {
    type: "bar",
    data: {
      labels,
      datasets,
    },
    options: {
      responsive: true,
      maintainAspectRatio: false,
      scales: { y: { beginAtZero: true, title: { display: true, text: "USD / 1% Quota" } } },
    },
  });

  renderQuotaHistogram(primary);

  const tbody = document.querySelector("#quota-table tbody") as HTMLTableSectionElement;
  tbody.replaceChildren();
  for (const d of derived) {
    const tr = document.createElement("tr");
    const hasAnyPct = REPORT_PERCENTILES.some((spec) => d.percentiles[spec.key] !== null);
    const pctBandStr = hasAnyPct
      ? REPORT_PERCENTILES.map((spec) => {
          const val = d.percentiles[spec.key];
          return val !== null ? formatQuotaRateUsd(val) : "--";
        }).join(" / ")
      : "--";
    const cells: Array<[string, boolean]> = [
      [d.row.plan_tier, false],
      [d.row.bucket, false],
      [d.row.attributed_model, false],
      [d.row.attributed_effort, false],
      [String(d.row.slices), true],
      [`${d.pctDrop.toFixed(2)}%`, true],
      [pctBandStr, true],
      [formatQuotaRateUsd(d.costPer1PctUsd), true],
      [formatCompact(d.outputTokensPer1Pct), true],
      [formatCompact(d.totalTokensPer1Pct), true],
      [formatUsd(d.row.reference_cost_usd), true],
      [d.relativeWeight !== null ? `${d.relativeWeight.toFixed(2)}x` : "--", true],
    ];
    for (const [text, isNum] of cells) {
      const td = document.createElement("td");
      td.textContent = text;
      if (isNum) td.className = "num";
      tr.appendChild(td);
    }
    tbody.appendChild(tr);
  }
}

function renderQuotaHistogram(primary: StatsReport) {
  destroyChart("quotaHist");
  const canvas = document.getElementById("chart-quota-hist") as HTMLCanvasElement;
  const derived = deriveQuotaRows(primary.quota_by_plan_bucket_model);

  const prevValue = quotaHistSelectEl.value;
  quotaHistSelectEl.replaceChildren();
  derived.forEach((d, idx) => {
    const opt = document.createElement("option");
    const key = quotaRowLabel(d.row);
    opt.value = key;
    opt.textContent = `${key} (${d.row.slices})`;
    if (key === prevValue || (!prevValue && idx === 0)) opt.selected = true;
    quotaHistSelectEl.appendChild(opt);
  });

  const selected =
    derived.find((d) => quotaRowLabel(d.row) === quotaHistSelectEl.value) ?? derived[0];
  if (!selected) return;

  state.charts["quotaHist"] = new Chart(canvas, {
    type: "bar",
    data: {
      labels: QUOTA_COST_BUCKET_LABELS,
      datasets: [
        {
          label: `${quotaRowLabel(selected.row)} slices`,
          data: selected.histogram,
          backgroundColor: PALETTE[4],
        },
      ],
    },
    options: {
      responsive: true,
      maintainAspectRatio: false,
      scales: {
        x: { title: { display: true, text: "Reference USD per 1% Quota bucket" } },
        y: { beginAtZero: true, title: { display: true, text: "slices" } },
      },
    },
  });
}

function renderUsageTable(primary: StatsReport) {
  const tbody = document.querySelector("#usage-table tbody") as HTMLTableSectionElement;
  tbody.replaceChildren();
  const scorecard = deriveModelScorecard(primary);

  for (const row of scorecard) {
    const tr = document.createElement("tr");
    const hasAnyTpsPct = REPORT_PERCENTILES.some((spec) => row.tpsPercentiles[spec.key] !== null);
    const tpsPctText = hasAnyTpsPct
      ? `${REPORT_PERCENTILES.map((spec) => {
          const val = row.tpsPercentiles[spec.key];
          return val !== null ? val.toFixed(1) : "--";
        }).join(" / ")} t/s`
      : "--";
    const meanTpsText = row.weightedAvgTps > 0 ? `${row.weightedAvgTps.toFixed(1)} t/s` : "--";
    const cells: Array<[string, boolean]> = [
      [row.model, false],
      [row.effort, false],
      [row.planTier, false],
      [`${row.events} / ${row.tpsSamples}`, true],
      [formatCompact(row.inputTokens), true],
      [formatCompact(row.cacheReadTokens), true],
      [formatCompact(row.outputTokens), true],
      [`${row.cacheHitRate.toFixed(1)}%`, true],
      [tpsPctText, true],
      [meanTpsText, true],
      [`$${row.refCostUsd.toFixed(4)}`, true],
    ];
    for (const [text, isNum] of cells) {
      const td = document.createElement("td");
      td.textContent = text;
      if (isNum) td.className = "num";
      tr.appendChild(td);
    }
    tbody.appendChild(tr);
  }
}

function clearDashboard() {
  kpiGridEl.replaceChildren();
  document.querySelector("#usage-table tbody")?.replaceChildren();
  document.querySelector("#quota-table tbody")?.replaceChildren();
  histSelectEl.replaceChildren();
  quotaHistSelectEl.replaceChildren();
  compareWrapEl.hidden = true;
  clearLocalBtnEl.hidden = !state.local;
  for (const id of Object.keys(state.charts)) {
    destroyChart(id);
  }
}

function renderAll() {
  clearLocalBtnEl.hidden = !state.local;
  const primary = activePrimaryReport();
  if (!primary) {
    clearDashboard();
    return;
  }

  compareWrapEl.hidden = !(state.local && state.community);
  renderKpis(primary);
  renderUsageTable(primary);
  renderTpsTrendChart();
  renderTpsModelChart(primary, state.community);
  renderHistogram(primary);
  renderQuotaChartAndTable(primary);
}

let communityRequestSeq = 0;
let communityAbortController: AbortController | null = null;
let localLoadSeq = 0;

async function fetchJson<T>(url: string, signal?: AbortSignal): Promise<T> {
  const res = await fetch(url, { signal });
  if (!res.ok) throw new Error(`HTTP ${res.status} loading ${url}`);
  return (await res.json()) as T;
}

async function loadCommunityWindow(win: WindowKey) {
  state.activeWindow = win;
  document.querySelectorAll<HTMLButtonElement>(".pill[data-window]").forEach((btn) => {
    btn.classList.toggle("active", btn.dataset.window === win);
  });

  const requestId = ++communityRequestSeq;
  communityAbortController?.abort();
  const controller = new AbortController();
  communityAbortController = controller;

  try {
    const manifestRaw = await fetchJson<unknown>("./data/manifest.json", controller.signal);
    if (requestId !== communityRequestSeq) return;
    const manifest = validateDataManifest(manifestRaw);
    if (!manifest) throw new Error("Invalid ./data/manifest.json payload");

    const rollingPath = manifest.rolling[win] ?? `./data/rolling/${win}.json`;
    const rollingRaw = await fetchJson<unknown>(rollingPath, controller.signal);
    if (requestId !== communityRequestSeq) return;
    const rolling = validateStatsReport(rollingRaw);
    if (!rolling) throw new Error(`Invalid rolling StatsReport payload: ${rollingPath}`);

    const limitHours = win === "24h" ? 24 : 48;
    const recentPaths = manifest.available_hours.slice(-limitHours);
    const settled = await Promise.allSettled(
      recentPaths.map((p) => fetchJson<unknown>(p, controller.signal)),
    );
    if (requestId !== communityRequestSeq) return;

    const hourly: StatsReport[] = [];
    for (const res of settled) {
      if (res.status === "fulfilled") {
        const valid = validateStatsReport(res.value);
        if (valid) hourly.push(valid);
      }
    }

    state.community = rolling;
    state.hourly = hourly;
    setBanner(
      state.local
        ? `Comparing imported stats JSON against online community (${win}, updated ${manifest.updated_at}).`
        : null,
    );
    renderAll();
  } catch {
    if (controller.signal.aborted || requestId !== communityRequestSeq) return;
    state.community = null;
    state.hourly = [];
    renderAll();
    if (!state.local) {
      setBanner(NO_ONLINE_DATA_BANNER);
    }
  }
}

async function loadLocalFile(file: File) {
  const requestId = ++localLoadSeq;
  const prevLocal = state.local;
  try {
    const raw: unknown = JSON.parse(await file.text());
    if (requestId !== localLoadSeq) return;
    const parsed = validateStatsReport(raw);
    if (!parsed) {
      setBanner(`Invalid StatsReport v1 JSON file: ${file.name}`);
      return;
    }
    state.local = parsed;
    renderAll();
    setBanner(
      `Loaded stats report: ${file.name} (${formatUtcTimestamp(parsed.window.start)} -> ${formatUtcTimestamp(parsed.window.end)})`,
    );
  } catch (err) {
    if (requestId !== localLoadSeq) return;
    state.local = prevLocal;
    setBanner(`Failed to load ${file.name}: ${String(err)}`);
    try {
      renderAll();
    } catch {
      state.local = null;
      clearDashboard();
    }
  } finally {
    fileInputEl.value = "";
  }
}

document.querySelectorAll<HTMLButtonElement>(".pill[data-window]").forEach((btn) => {
  btn.addEventListener("click", () => {
    const win = (btn.dataset.window as WindowKey) || "24h";
    void loadCommunityWindow(win);
  });
});

clearLocalBtnEl.addEventListener("click", () => {
  ++localLoadSeq;
  state.local = null;
  renderAll();
  setBanner(state.community ? null : NO_ONLINE_DATA_BANNER);
});

compareToggleEl.addEventListener("change", () => {
  state.compareMode = compareToggleEl.checked;
  renderAll();
});

trendPctSelectEl.addEventListener("change", () => {
  const parsed = Number(trendPctSelectEl.value);
  state.trendQuantile = Number.isFinite(parsed) && parsed > 0 ? parsed : PRIMARY_PERCENTILE.q;
  renderTpsTrendChart();
});

histSelectEl.addEventListener("change", () => {
  const primary = activePrimaryReport();
  if (primary) renderHistogram(primary);
});

quotaHistSelectEl.addEventListener("change", () => {
  const primary = activePrimaryReport();
  if (primary) renderQuotaHistogram(primary);
});

fileInputEl.addEventListener("change", () => {
  const file = fileInputEl.files?.[0];
  if (file) void loadLocalFile(file);
});

dropZoneEl.addEventListener("dragover", (e) => {
  e.preventDefault();
  dropZoneEl.classList.add("dragover");
});

dropZoneEl.addEventListener("dragleave", () => {
  dropZoneEl.classList.remove("dragover");
});

dropZoneEl.addEventListener("drop", (e) => {
  e.preventDefault();
  dropZoneEl.classList.remove("dragover");
  const file = e.dataTransfer?.files?.[0];
  if (file) void loadLocalFile(file);
});

void loadCommunityWindow("24h");
