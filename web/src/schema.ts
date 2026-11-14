import {
  MAX_VALID_TPS,
  MIXED_ATTRIBUTION_KEY,
  QUOTA_COST_BUCKET_BOUNDS,
  QUOTA_COST_HISTOGRAM_EDGES,
  STATS_SCHEMA_VERSION,
  TPS_BUCKET_BOUNDS,
  TPS_HISTOGRAM_EDGES,
  type QuotaByPlanBucketModel,
  type StatsReport,
  type StatsWindow,
  type TpsByModelEffort,
  type UsageByPlanModel,
} from "../../shared/telemetry";

export {
  MAX_VALID_TPS,
  MIXED_ATTRIBUTION_KEY,
  QUOTA_COST_BUCKET_BOUNDS,
  STATS_SCHEMA_VERSION,
  TPS_BUCKET_BOUNDS,
  TPS_HISTOGRAM_EDGES,
  type QuotaByPlanBucketModel,
  type StatsReport,
  type StatsWindow,
  type TpsByModelEffort,
  type UsageByPlanModel,
};

export interface PercentileSpec {
  readonly q: number;
  readonly key: string;
  readonly label: string;
}

/**
 * Canonical set of percentiles displayed across all tables, charts, and selectors.
 * Edit this single list to change which quantiles are reported everywhere in the UI.
 */
export const REPORT_PERCENTILES = [
  { q: 0.5, key: "p50", label: "p50 (Median)" },
  { q: 0.9, key: "p90", label: "p90" },
  { q: 0.95, key: "p95", label: "p95" },
] as const satisfies readonly PercentileSpec[];

export type ReportPercentileKey = (typeof REPORT_PERCENTILES)[number]["key"];
export const PRIMARY_PERCENTILE = REPORT_PERCENTILES[0];
export const REPORT_PERCENTILE_SLASH = REPORT_PERCENTILES.map((p) => p.key).join(" / ");
export const REPORT_PERCENTILE_COMMA = REPORT_PERCENTILES.map((p) => p.key).join(", ");

export const TPS_BUCKET_LABELS: string[] = TPS_BUCKET_BOUNDS.map(([lo, hi]) => `${lo}-${hi}`);

export const QUOTA_COST_BUCKET_LABELS: string[] = [
  ...QUOTA_COST_HISTOGRAM_EDGES.map(
    (edge, idx) => `$${idx === 0 ? 0 : QUOTA_COST_HISTOGRAM_EDGES[idx - 1]}–${edge}`,
  ),
  `$${QUOTA_COST_HISTOGRAM_EDGES[QUOTA_COST_HISTOGRAM_EDGES.length - 1]}+`,
];

export interface DataManifest {
  updated_at: string;
  last_hour: string | null;
  available_hours: string[];
  rolling: {
    "24h"?: string;
    "7d"?: string;
    "30d"?: string;
  };
}

function isPlainObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function isFiniteNonNegative(value: unknown): value is number {
  return typeof value === "number" && Number.isFinite(value) && value >= 0;
}

function isFiniteNonNegativeInt(value: unknown): value is number {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= 0;
}

function isSafeRelativeDataPath(value: unknown): value is string {
  if (typeof value !== "string" || value.length === 0) return false;
  if (value.startsWith("/") || value.startsWith("\\") || value.includes("\\")) return false;
  if (/[?#\s\u0000-\u001f\u007f]/.test(value)) return false;
  if (/^[a-zA-Z][a-zA-Z0-9+.-]*:/.test(value)) return false;
  const trimmed = value.startsWith("./") ? value.slice(2) : value;
  if (!trimmed.endsWith(".json")) return false;
  const segments = trimmed.split("/");
  return segments.every((seg) => seg.length > 0 && seg !== "." && seg !== "..");
}

function isHistogram(value: unknown, expectedLen: number): value is number[] {
  return (
    Array.isArray(value) &&
    value.length === expectedLen &&
    value.every((n) => isFiniteNonNegative(n))
  );
}

export function validateStatsReport(raw: unknown): StatsReport | null {
  if (!isPlainObject(raw)) return null;
  if (raw.schema_version !== STATS_SCHEMA_VERSION) return null;
  if (typeof raw.scope !== "string") return null;
  if (!isFiniteNonNegativeInt(raw.contributors)) return null;

  if (!isPlainObject(raw.window)) return null;
  const win = raw.window;
  if (typeof win.start !== "string" || typeof win.end !== "string") return null;
  const startMs = Date.parse(win.start);
  const endMs = Date.parse(win.end);
  if (!Number.isFinite(startMs) || !Number.isFinite(endMs) || endMs < startMs) return null;
  if (!isFiniteNonNegative(win.duration_secs)) return null;

  if (
    !Array.isArray(raw.usage_by_plan_model) ||
    !Array.isArray(raw.tps_by_model_effort) ||
    !Array.isArray(raw.quota_by_plan_bucket_model)
  ) {
    return null;
  }

  for (const row of raw.usage_by_plan_model) {
    if (!isPlainObject(row)) return null;
    if (
      typeof row.plan_tier !== "string" ||
      typeof row.model !== "string" ||
      typeof row.effort !== "string" ||
      !isFiniteNonNegativeInt(row.events) ||
      !isFiniteNonNegativeInt(row.input_tokens) ||
      !isFiniteNonNegativeInt(row.cache_read_tokens) ||
      !isFiniteNonNegativeInt(row.cache_write_tokens) ||
      !isFiniteNonNegativeInt(row.output_tokens) ||
      !isFiniteNonNegative(row.reference_cost_usd)
    ) {
      return null;
    }
  }

  for (const row of raw.tps_by_model_effort) {
    if (!isPlainObject(row)) return null;
    if (
      typeof row.model !== "string" ||
      typeof row.effort !== "string" ||
      !isFiniteNonNegativeInt(row.sample_count) ||
      !isFiniteNonNegativeInt(row.output_tokens) ||
      !isFiniteNonNegativeInt(row.elapsed_ms) ||
      !isHistogram(row.histogram, TPS_BUCKET_LABELS.length) ||
      !row.histogram.every(isFiniteNonNegativeInt)
    ) {
      return null;
    }
  }

  for (const row of raw.quota_by_plan_bucket_model) {
    if (!isPlainObject(row)) return null;
    if (
      typeof row.plan_tier !== "string" ||
      typeof row.bucket !== "string" ||
      typeof row.attributed_model !== "string" ||
      typeof row.attributed_effort !== "string" ||
      !isFiniteNonNegativeInt(row.slices) ||
      !isFiniteNonNegative(row.delta_quota_fraction) ||
      !isFiniteNonNegativeInt(row.input_tokens) ||
      !isFiniteNonNegativeInt(row.cache_read_tokens) ||
      !isFiniteNonNegativeInt(row.cache_write_tokens) ||
      !isFiniteNonNegativeInt(row.output_tokens) ||
      !isFiniteNonNegative(row.reference_cost_usd)
    ) {
      return null;
    }
    if (
      row.cost_per_pct_histogram !== undefined &&
      (!isHistogram(row.cost_per_pct_histogram, QUOTA_COST_BUCKET_BOUNDS.length) ||
        !row.cost_per_pct_histogram.every(isFiniteNonNegativeInt))
    ) {
      return null;
    }
  }

  return raw as unknown as StatsReport;
}

export function validateDataManifest(raw: unknown): DataManifest | null {
  if (!isPlainObject(raw)) return null;
  if (typeof raw.updated_at !== "string") return null;
  if (raw.last_hour !== null && typeof raw.last_hour !== "string") return null;
  if (!Array.isArray(raw.available_hours) || !raw.available_hours.every(isSafeRelativeDataPath)) {
    return null;
  }
  if (!isPlainObject(raw.rolling)) return null;
  const rolling: DataManifest["rolling"] = {};
  for (const key of ["24h", "7d", "30d"] as const) {
    const val = raw.rolling[key];
    if (val !== undefined) {
      if (!isSafeRelativeDataPath(val)) return null;
      rolling[key] = val;
    }
  }
  return {
    updated_at: raw.updated_at,
    last_hour: raw.last_hour,
    available_hours: [...raw.available_hours],
    rolling,
  };
}
