import {
  QuotaByPlanBucketModel,
  QUOTA_COST_HISTOGRAM_BUCKETS,
  STATS_SCHEMA_VERSION,
  StatsReport,
  StatsWindow,
  TPS_HISTOGRAM_BUCKETS,
  TpsByModelEffort,
  UsageByPlanModel,
  normalizeQuotaCostHistogram,
} from "./schema";

export const MAX_TPS_SAMPLES_PER_CONTRIBUTOR_HOUR = 200;
export const MAX_QUOTA_SLICES_PER_CONTRIBUTOR_HOUR = 200;

function compareStrings(a: string, b: string): number {
  return a < b ? -1 : a > b ? 1 : 0;
}

/** Proportionally scales histogram bucket counts to `targetTotal` via largest-remainder rounding. */
function scaleHistogramCounts(
  hist: readonly number[],
  buckets: number,
  sourceTotal: number,
  targetTotal: number,
): number[] {
  const scaledHist = new Array<number>(buckets).fill(0);
  if (targetTotal <= 0 || sourceTotal <= 0) {
    return scaledHist;
  }
  const ratio = targetTotal / sourceTotal;
  const remainders: Array<{ idx: number; frac: number }> = [];
  let assigned = 0;
  for (let i = 0; i < buckets; i++) {
    const exact = (hist[i] ?? 0) * ratio;
    const base = Math.floor(exact);
    scaledHist[i] = base;
    assigned += base;
    remainders.push({ idx: i, frac: exact - base });
  }
  remainders.sort((a, b) => b.frac - a.frac || a.idx - b.idx);
  for (let i = 0; i < targetTotal - assigned && i < remainders.length; i++) {
    scaledHist[remainders[i].idx] += 1;
  }
  return scaledHist;
}

/** Caps an individual contributor's TPS weight at `maxSamples` so one heavy client cannot dominate. */
function capTpsSampleWeight(
  row: TpsByModelEffort,
  maxSamples = MAX_TPS_SAMPLES_PER_CONTRIBUTOR_HOUR,
): TpsByModelEffort {
  const hist = Array.from({ length: TPS_HISTOGRAM_BUCKETS }, (_, i) => row.histogram[i] ?? 0);
  if (row.sample_count <= maxSamples || row.sample_count <= 0) {
    return { ...row, histogram: hist };
  }

  const ratio = maxSamples / row.sample_count;
  return {
    ...row,
    sample_count: maxSamples,
    output_tokens: Math.round(row.output_tokens * ratio),
    elapsed_ms: Math.round(row.elapsed_ms * ratio),
    histogram: scaleHistogramCounts(hist, TPS_HISTOGRAM_BUCKETS, row.sample_count, maxSamples),
  };
}

/** Caps an individual contributor's quota slice weight at `maxSlices` without inflating $/pct. */
function capQuotaSliceWeight(
  row: QuotaByPlanBucketModel,
  maxSlices = MAX_QUOTA_SLICES_PER_CONTRIBUTOR_HOUR,
): QuotaByPlanBucketModel {
  const hist = normalizeQuotaCostHistogram(row);
  if (row.slices <= maxSlices || row.slices <= 0) {
    return { ...row, cost_per_pct_histogram: hist };
  }

  const ratio = maxSlices / row.slices;
  const histTotal = hist.reduce((acc, v) => acc + v, 0);
  const targetHistTotal = Math.min(maxSlices, Math.round(histTotal * ratio));
  return {
    ...row,
    slices: maxSlices,
    delta_quota_fraction: row.delta_quota_fraction * ratio,
    input_tokens: Math.round(row.input_tokens * ratio),
    cache_read_tokens: Math.round(row.cache_read_tokens * ratio),
    cache_write_tokens: Math.round(row.cache_write_tokens * ratio),
    output_tokens: Math.round(row.output_tokens * ratio),
    reference_cost_usd: row.reference_cost_usd * ratio,
    cost_per_pct_histogram: scaleHistogramCounts(
      hist,
      QUOTA_COST_HISTOGRAM_BUCKETS,
      histTotal,
      targetHistTotal,
    ),
  };
}

/**
 * Selects a single canonical report per contributor within an hour (preferring the widest time span
 * and latest window end on ties) so retried or split sub-hour reports never double-count.
 */
function selectCanonicalContributorReport(reports: StatsReport[]): StatsReport[] {
  if (reports.length <= 1) return reports;

  let best = reports[0];
  let bestSpan = -1;
  let bestEnd = -1;
  for (const report of reports) {
    const startMs = Date.parse(report.window.start);
    const endMs = Date.parse(report.window.end);
    const span = Number.isFinite(startMs) && Number.isFinite(endMs) ? endMs - startMs : 0;
    const end = Number.isFinite(endMs) ? endMs : 0;
    if (span > bestSpan || (span === bestSpan && end >= bestEnd)) {
      best = report;
      bestSpan = span;
      bestEnd = end;
    }
  }
  return [best];
}

/**
 * Aggregates raw hourly slices cleanly per anonymous contributor (`contributor_hash`):
 * 1) Keeps one canonical report per contributor within the hour.
 * 2) Caps per-contributor TPS and quota sample weights so one heavy client cannot dominate others.
 */
export function aggregateHourByContributors(
  submissions: Array<{ contributorHash: string; report: StatsReport }>,
  window: StatsWindow,
): StatsReport {
  const byContributor = new Map<string, StatsReport[]>();
  for (const sub of submissions) {
    const list = byContributor.get(sub.contributorHash);
    if (list) {
      list.push(sub.report);
    } else {
      byContributor.set(sub.contributorHash, [sub.report]);
    }
  }

  if (byContributor.size === 0) {
    return mergeReports([], "aggregated", window, 0);
  }

  const cappedReports = Array.from(byContributor.values(), (reports) => {
    const perUser = mergeReports(selectCanonicalContributorReport(reports), "local", window, 1);
    return {
      ...perUser,
      tps_by_model_effort: perUser.tps_by_model_effort.map((t) => capTpsSampleWeight(t)),
      quota_by_plan_bucket_model: perUser.quota_by_plan_bucket_model.map((q) =>
        capQuotaSliceWeight(q),
      ),
    };
  });

  return mergeReports(cappedReports, "aggregated", window, byContributor.size);
}

export function mergeReports(
  reports: StatsReport[],
  scope: "local" | "aggregated" | string,
  fallbackWindow: StatsWindow,
  contributorsOverride?: number,
): StatsReport {
  if (reports.length === 0) {
    return {
      schema_version: STATS_SCHEMA_VERSION,
      scope,
      window: fallbackWindow,
      contributors: contributorsOverride ?? 0,
      usage_by_plan_model: [],
      tps_by_model_effort: [],
      quota_by_plan_bucket_model: [],
    };
  }

  let minStart = reports[0].window.start;
  let maxEnd = reports[0].window.end;
  let sumContributors = 0;

  const usageMap = new Map<string, UsageByPlanModel>();
  const tpsMap = new Map<string, TpsByModelEffort>();
  const quotaMap = new Map<string, QuotaByPlanBucketModel>();

  for (const report of reports) {
    if (report.window.start && (!minStart || report.window.start < minStart)) {
      minStart = report.window.start;
    }
    if (report.window.end > maxEnd) {
      maxEnd = report.window.end;
    }
    sumContributors += report.contributors || 0;

    for (const u of report.usage_by_plan_model) {
      const key = `${u.plan_tier}\u0000${u.model}\u0000${u.effort}`;
      const existing = usageMap.get(key);
      if (!existing) {
        usageMap.set(key, { ...u });
      } else {
        existing.events += u.events;
        existing.input_tokens += u.input_tokens;
        existing.cache_read_tokens += u.cache_read_tokens;
        existing.cache_write_tokens += u.cache_write_tokens;
        existing.output_tokens += u.output_tokens;
        existing.reference_cost_usd += u.reference_cost_usd;
      }
    }

    for (const t of report.tps_by_model_effort) {
      const key = `${t.model}\u0000${t.effort}`;
      const existing = tpsMap.get(key);
      if (!existing) {
        tpsMap.set(key, {
          ...t,
          histogram: Array.from({ length: TPS_HISTOGRAM_BUCKETS }, (_, i) => t.histogram[i] ?? 0),
        });
      } else {
        existing.sample_count += t.sample_count;
        existing.output_tokens += t.output_tokens;
        existing.elapsed_ms += t.elapsed_ms;
        for (let i = 0; i < TPS_HISTOGRAM_BUCKETS; i++) {
          existing.histogram[i] += t.histogram[i] ?? 0;
        }
      }
    }

    for (const q of report.quota_by_plan_bucket_model) {
      const key = `${q.plan_tier}\u0000${q.bucket}\u0000${q.attributed_model}\u0000${q.attributed_effort}`;
      const existing = quotaMap.get(key);
      const qHist = normalizeQuotaCostHistogram(q);
      if (!existing) {
        quotaMap.set(key, {
          ...q,
          cost_per_pct_histogram: [...qHist],
        });
      } else {
        existing.slices += q.slices;
        existing.delta_quota_fraction += q.delta_quota_fraction;
        existing.input_tokens += q.input_tokens;
        existing.cache_read_tokens += q.cache_read_tokens;
        existing.cache_write_tokens += q.cache_write_tokens;
        existing.output_tokens += q.output_tokens;
        existing.reference_cost_usd += q.reference_cost_usd;
        const existHist = normalizeQuotaCostHistogram(existing);
        for (let i = 0; i < QUOTA_COST_HISTOGRAM_BUCKETS; i++) {
          existHist[i] += qHist[i];
        }
        existing.cost_per_pct_histogram = existHist;
      }
    }
  }

  const usageSorted = Array.from(usageMap.values()).sort(
    (a, b) =>
      compareStrings(a.plan_tier, b.plan_tier) ||
      compareStrings(a.model, b.model) ||
      compareStrings(a.effort, b.effort),
  );
  const tpsSorted = Array.from(tpsMap.values()).sort(
    (a, b) => compareStrings(a.model, b.model) || compareStrings(a.effort, b.effort),
  );
  const quotaSorted = Array.from(quotaMap.values()).sort(
    (a, b) =>
      compareStrings(a.plan_tier, b.plan_tier) ||
      compareStrings(a.bucket, b.bucket) ||
      compareStrings(a.attributed_model, b.attributed_model) ||
      compareStrings(a.attributed_effort, b.attributed_effort),
  );

  return {
    schema_version: STATS_SCHEMA_VERSION,
    scope,
    window: {
      start: fallbackWindow.start || minStart,
      end: fallbackWindow.end || maxEnd,
      duration_secs: fallbackWindow.duration_secs,
    },
    contributors: contributorsOverride ?? sumContributors,
    usage_by_plan_model: usageSorted,
    tps_by_model_effort: tpsSorted,
    quota_by_plan_bucket_model: quotaSorted,
  };
}
