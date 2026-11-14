import {
  normalizeQuotaCostHistogram,
  quotaCostHistogramPercentile as quotaHistogramPercentile,
  tpsHistogramPercentile as histogramPercentile,
} from "../../shared/telemetry";
import {
  MIXED_ATTRIBUTION_KEY,
  PRIMARY_PERCENTILE,
  QUOTA_COST_BUCKET_BOUNDS,
  QuotaByPlanBucketModel,
  REPORT_PERCENTILES,
  ReportPercentileKey,
  StatsReport,
  TpsByModelEffort,
} from "./schema";

export { histogramPercentile, normalizeQuotaCostHistogram, quotaHistogramPercentile };

export function weightedTps(row: TpsByModelEffort): number {
  if (!(row.elapsed_ms > 0) || !(row.output_tokens >= 0)) return 0;
  const tps = (row.output_tokens * 1_000) / row.elapsed_ms;
  return Number.isFinite(tps) ? tps : 0;
}

export function formatWindowDuration(secs: number): string {
  if (!Number.isFinite(secs) || secs <= 0) return "All";
  if (secs % 86_400 === 0) return `${secs / 86_400}d`;
  if (secs % 3_600 === 0) return `${secs / 3_600}h`;
  if (secs >= 3_600) return `${(secs / 3_600).toFixed(1)}h`;
  if (secs >= 60) return `${Math.round(secs / 60)}m`;
  return `${Math.round(secs)}s`;
}

function hasExplicitCostHistogram(row: QuotaByPlanBucketModel): row is QuotaByPlanBucketModel & {
  cost_per_pct_histogram: number[];
} {
  return (
    Array.isArray(row.cost_per_pct_histogram) &&
    row.cost_per_pct_histogram.length === QUOTA_COST_BUCKET_BOUNDS.length
  );
}

export interface QuotaDerivedRow {
  row: QuotaByPlanBucketModel;
  pctDrop: number;
  outputTokensPer1Pct: number;
  totalTokensPer1Pct: number;
  costPer1PctUsd: number;
  percentiles: Record<ReportPercentileKey, number | null>;
  p50CostPer1PctUsd: number | null;
  p90CostPer1PctUsd: number | null;
  p95CostPer1PctUsd: number | null;
  histogram: number[];
  relativeWeight: number | null;
}

export function deriveQuotaRows(rows: QuotaByPlanBucketModel[]): QuotaDerivedRow[] {
  const precomputed = rows.map((row) => {
    const pctDrop = row.delta_quota_fraction * 100;
    const totalTokens =
      row.input_tokens + row.cache_read_tokens + row.cache_write_tokens + row.output_tokens;
    const outputTokensPer1Pct = pctDrop > 0 ? row.output_tokens / pctDrop : 0;
    const totalTokensPer1Pct = pctDrop > 0 ? totalTokens / pctDrop : 0;
    const costPer1PctUsd = pctDrop > 0 ? row.reference_cost_usd / pctDrop : 0;
    const histogram = normalizeQuotaCostHistogram(row);
    const hasExplicitHist = hasExplicitCostHistogram(row);
    const percentiles = {} as Record<ReportPercentileKey, number | null>;
    for (const spec of REPORT_PERCENTILES) {
      percentiles[spec.key] = hasExplicitHist
        ? quotaHistogramPercentile(histogram, spec.q)
        : costPer1PctUsd > 0
          ? costPer1PctUsd
          : null;
    }
    return {
      row,
      pctDrop,
      outputTokensPer1Pct,
      totalTokensPer1Pct,
      costPer1PctUsd,
      percentiles,
      p50CostPer1PctUsd: percentiles.p50,
      p90CostPer1PctUsd: percentiles.p90,
      p95CostPer1PctUsd: percentiles.p95,
      histogram,
    };
  });

  const baselines = new Map<string, number>();
  for (const item of precomputed) {
    if (item.row.attributed_model === MIXED_ATTRIBUTION_KEY || item.row.delta_quota_fraction <= 0) {
      continue;
    }
    const metricCost = item.percentiles[PRIMARY_PERCENTILE.key] ?? item.costPer1PctUsd;
    if (metricCost <= 0) continue;
    const key = `${item.row.plan_tier}\u0000${item.row.bucket}`;
    const prev = baselines.get(key);
    if (prev === undefined || metricCost < prev) {
      baselines.set(key, metricCost);
    }
  }

  return precomputed.map((item) => {
    let relativeWeight: number | null = null;
    const metricCost = item.percentiles[PRIMARY_PERCENTILE.key] ?? item.costPer1PctUsd;
    if (item.row.attributed_model !== MIXED_ATTRIBUTION_KEY && metricCost > 0) {
      const base = baselines.get(`${item.row.plan_tier}\u0000${item.row.bucket}`);
      if (base !== undefined) {
        relativeWeight = base / metricCost;
      }
    }
    return {
      ...item,
      relativeWeight,
    };
  });
}

export interface ModelScorecardRow {
  planTier: string;
  model: string;
  effort: string;
  events: number;
  tpsSamples: number;
  inputTokens: number;
  cacheReadTokens: number;
  outputTokens: number;
  totalTokens: number;
  cacheHitRate: number;
  tpsPercentiles: Record<ReportPercentileKey, number | null>;
  weightedAvgTps: number;
  refCostUsd: number;
}

function computeTpsPercentiles(
  t: TpsByModelEffort | undefined,
): Record<ReportPercentileKey, number | null> {
  const out = {} as Record<ReportPercentileKey, number | null>;
  for (const spec of REPORT_PERCENTILES) {
    out[spec.key] = t ? histogramPercentile(t.histogram, spec.q) : null;
  }
  return out;
}

export function deriveModelScorecard(report: StatsReport): ModelScorecardRow[] {
  const tpsByKey = new Map<string, TpsByModelEffort>();
  for (const t of report.tps_by_model_effort) {
    tpsByKey.set(`${t.model}\u0000${t.effort}`, t);
  }

  const usageByModelEffort = new Map<
    string,
    {
      plans: Set<string>;
      model: string;
      effort: string;
      events: number;
      inputTokens: number;
      cacheReadTokens: number;
      cacheWriteTokens: number;
      outputTokens: number;
      refCostUsd: number;
    }
  >();

  for (const u of report.usage_by_plan_model) {
    const meKey = `${u.model}\u0000${u.effort}`;
    const existing = usageByModelEffort.get(meKey);
    if (!existing) {
      usageByModelEffort.set(meKey, {
        plans: new Set([u.plan_tier]),
        model: u.model,
        effort: u.effort,
        events: u.events,
        inputTokens: u.input_tokens,
        cacheReadTokens: u.cache_read_tokens,
        cacheWriteTokens: u.cache_write_tokens,
        outputTokens: u.output_tokens,
        refCostUsd: u.reference_cost_usd,
      });
    } else {
      existing.plans.add(u.plan_tier);
      existing.events += u.events;
      existing.inputTokens += u.input_tokens;
      existing.cacheReadTokens += u.cache_read_tokens;
      existing.cacheWriteTokens += u.cache_write_tokens;
      existing.outputTokens += u.output_tokens;
      existing.refCostUsd += u.reference_cost_usd;
    }
  }

  const seenModelEffort = new Set<string>();
  const rows: ModelScorecardRow[] = [];

  for (const [meKey, u] of usageByModelEffort.entries()) {
    seenModelEffort.add(meKey);
    const t = tpsByKey.get(meKey);
    const denom = u.inputTokens + u.cacheReadTokens;
    const totalTokens =
      u.inputTokens + u.cacheReadTokens + u.cacheWriteTokens + u.outputTokens;
    const tpsPercentiles = computeTpsPercentiles(t);
    const planTier = Array.from(u.plans).sort().join(", ");
    rows.push({
      planTier,
      model: u.model,
      effort: u.effort,
      events: u.events,
      tpsSamples: t ? t.sample_count : 0,
      inputTokens: u.inputTokens,
      cacheReadTokens: u.cacheReadTokens,
      outputTokens: u.outputTokens,
      totalTokens,
      cacheHitRate: denom > 0 ? (u.cacheReadTokens * 100) / denom : 0,
      tpsPercentiles,
      weightedAvgTps: t ? weightedTps(t) : 0,
      refCostUsd: u.refCostUsd,
    });
  }

  for (const t of report.tps_by_model_effort) {
    const meKey = `${t.model}\u0000${t.effort}`;
    if (seenModelEffort.has(meKey)) continue;
    const tpsPercentiles = computeTpsPercentiles(t);
    rows.push({
      planTier: "unknown",
      model: t.model,
      effort: t.effort,
      events: 0,
      tpsSamples: t.sample_count,
      inputTokens: 0,
      cacheReadTokens: 0,
      outputTokens: t.output_tokens,
      totalTokens: t.output_tokens,
      cacheHitRate: 0,
      tpsPercentiles,
      weightedAvgTps: weightedTps(t),
      refCostUsd: 0,
    });
  }

  rows.sort((a, b) => {
    if (b.totalTokens !== a.totalTokens) return b.totalTokens - a.totalTokens;
    return `${a.model} (${a.effort})`.localeCompare(`${b.model} (${b.effort})`);
  });

  return rows;
}

export function summarizeReport(report: StatsReport) {
  let inputTokens = 0;
  let cacheReadTokens = 0;
  let cacheWriteTokens = 0;
  let outputTokens = 0;
  let refCostUsd = 0;
  let events = 0;

  const modelTokenTotals = new Map<string, number>();
  const distinctModels = new Set<string>();

  for (const u of report.usage_by_plan_model) {
    events += u.events;
    inputTokens += u.input_tokens;
    cacheReadTokens += u.cache_read_tokens;
    cacheWriteTokens += u.cache_write_tokens;
    outputTokens += u.output_tokens;
    refCostUsd += u.reference_cost_usd;
    distinctModels.add(`${u.model} (${u.effort})`);
    const rowTokens =
      u.input_tokens + u.cache_read_tokens + u.cache_write_tokens + u.output_tokens;
    const label = `${u.model} (${u.effort})`;
    modelTokenTotals.set(label, (modelTokenTotals.get(label) ?? 0) + rowTokens);
  }

  let tpsOut = 0;
  let tpsMs = 0;
  let tpsSamples = 0;
  for (const t of report.tps_by_model_effort) {
    tpsSamples += t.sample_count;
    tpsOut += t.output_tokens;
    tpsMs += t.elapsed_ms;
    distinctModels.add(`${t.model} (${t.effort})`);
  }

  let quotaSlices = 0;
  let quotaRefCostUsd = 0;
  for (const q of report.quota_by_plan_bucket_model) {
    quotaSlices += q.slices;
    quotaRefCostUsd += q.reference_cost_usd;
  }

  const totalTokens = inputTokens + cacheReadTokens + cacheWriteTokens + outputTokens;
  let topModelLabel: string | null = null;
  let topModelTokens = 0;
  for (const [label, tok] of modelTokenTotals.entries()) {
    if (tok > topModelTokens) {
      topModelTokens = tok;
      topModelLabel = label;
    }
  }
  const topModelSharePct = totalTokens > 0 ? (topModelTokens * 100) / totalTokens : 0;

  const cacheDenom = inputTokens + cacheReadTokens;
  return {
    events,
    tpsSamples,
    activeModels: distinctModels.size,
    topModelLabel,
    topModelSharePct,
    quotaSlices,
    quotaRefCostUsd,
    inputTokens,
    cacheReadTokens,
    cacheWriteTokens,
    outputTokens,
    totalTokens,
    cacheHitRate: cacheDenom > 0 ? (cacheReadTokens * 100) / cacheDenom : 0,
    refCostUsd,
    weightedAvgTps: tpsMs > 0 ? tpsOut / (tpsMs / 1000) : 0,
  };
}
