export const STATS_SCHEMA_VERSION = 1;
export const MAX_VALID_TPS = 2_000;
export const MIXED_ATTRIBUTION_KEY = "__mixed__";

export const TPS_HISTOGRAM_EDGES = [
  15, 30, 45, 60, 80, 100, 130, 160, 220, 320, 500, 800, 1300,
] as const;
export const TPS_HISTOGRAM_BUCKETS = TPS_HISTOGRAM_EDGES.length + 1;

export const QUOTA_COST_HISTOGRAM_EDGES = [
  0.005, 0.01, 0.02, 0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 4.0, 8.0, 16.0, 32.0,
] as const;
export const QUOTA_COST_HISTOGRAM_BUCKETS = QUOTA_COST_HISTOGRAM_EDGES.length + 1;
export const QUOTA_COST_HISTOGRAM_MAX = 64.0;

export function buildBucketBounds(edges: readonly number[], topBound: number): Array<[number, number]> {
  return [
    ...edges.map((edge, idx): [number, number] => [idx === 0 ? 0 : edges[idx - 1], edge]),
    [edges[edges.length - 1], topBound],
  ];
}

export const TPS_BUCKET_BOUNDS: Array<[number, number]> = buildBucketBounds(
  TPS_HISTOGRAM_EDGES,
  MAX_VALID_TPS,
);

export const QUOTA_COST_BUCKET_BOUNDS: Array<[number, number]> = buildBucketBounds(
  QUOTA_COST_HISTOGRAM_EDGES,
  QUOTA_COST_HISTOGRAM_MAX,
);

export interface StatsWindow {
  start: string;
  end: string;
  duration_secs: number;
}

export interface UsageByPlanModel {
  plan_tier: string;
  model: string;
  effort: string;
  events: number;
  input_tokens: number;
  cache_read_tokens: number;
  cache_write_tokens: number;
  output_tokens: number;
  reference_cost_usd: number;
}

export interface TpsByModelEffort {
  model: string;
  effort: string;
  sample_count: number;
  output_tokens: number;
  elapsed_ms: number;
  histogram: number[];
}

export interface QuotaByPlanBucketModel {
  plan_tier: string;
  bucket: string;
  attributed_model: string;
  attributed_effort: string;
  slices: number;
  delta_quota_fraction: number;
  input_tokens: number;
  cache_read_tokens: number;
  cache_write_tokens: number;
  output_tokens: number;
  reference_cost_usd: number;
  cost_per_pct_histogram?: number[];
}

export interface StatsReport {
  schema_version: number;
  scope: "local" | "aggregated" | string;
  window: StatsWindow;
  contributors: number;
  usage_by_plan_model: UsageByPlanModel[];
  tps_by_model_effort: TpsByModelEffort[];
  quota_by_plan_bucket_model: QuotaByPlanBucketModel[];
}

export function bucketIndexFromEdges(value: number, edges: readonly number[]): number {
  for (let i = 0; i < edges.length; i++) {
    if (value < edges[i]) return i;
  }
  return edges.length;
}

export function quotaCostBucketIndex(
  deltaQuotaFraction: number,
  refCostUsd: number,
): number | null {
  if (
    !(deltaQuotaFraction > 0) ||
    !Number.isFinite(deltaQuotaFraction) ||
    !(refCostUsd >= 0) ||
    !Number.isFinite(refCostUsd)
  ) {
    return null;
  }
  const costPerPct = refCostUsd / (deltaQuotaFraction * 100);
  if (!Number.isFinite(costPerPct) || costPerPct < 0) return null;
  return bucketIndexFromEdges(costPerPct, QUOTA_COST_HISTOGRAM_EDGES);
}

export function normalizeQuotaCostHistogram(
  q: Pick<
    QuotaByPlanBucketModel,
    "slices" | "delta_quota_fraction" | "reference_cost_usd" | "cost_per_pct_histogram"
  >,
): number[] {
  if (
    Array.isArray(q.cost_per_pct_histogram) &&
    q.cost_per_pct_histogram.length === QUOTA_COST_HISTOGRAM_BUCKETS
  ) {
    return q.cost_per_pct_histogram.map((v) => Math.max(0, Math.trunc(v)));
  }
  const hist = new Array<number>(QUOTA_COST_HISTOGRAM_BUCKETS).fill(0);
  const idx = quotaCostBucketIndex(q.delta_quota_fraction, q.reference_cost_usd);
  if (idx !== null && q.slices > 0) {
    hist[idx] = Math.trunc(q.slices);
  }
  return hist;
}

export function bucketHistogramPercentile(
  histogram: readonly number[],
  bounds: readonly (readonly [number, number])[],
  q: number,
): number | null {
  const total = histogram.reduce((acc, v) => acc + Math.max(0, v), 0);
  if (total === 0) return null;
  const target = total * Math.min(1, Math.max(0, q));
  let cumulative = 0;
  for (let i = 0; i < bounds.length; i++) {
    const count = Math.max(0, histogram[i] ?? 0);
    const [lo, hi] = bounds[i];
    if (count > 0 && cumulative + count >= target) {
      const ratio = Math.min(1, Math.max(0, (target - cumulative) / count));
      return lo + ratio * (hi - lo);
    }
    cumulative += count;
  }
  return bounds[bounds.length - 1][1];
}

export function tpsHistogramPercentile(histogram: readonly number[], q: number): number | null {
  return bucketHistogramPercentile(histogram, TPS_BUCKET_BOUNDS, q);
}

export function quotaCostHistogramPercentile(
  histogram: readonly number[],
  q: number,
): number | null {
  return bucketHistogramPercentile(histogram, QUOTA_COST_BUCKET_BOUNDS, q);
}
