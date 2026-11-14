import { describe, expect, test } from "bun:test";
import {
  deriveModelScorecard,
  deriveQuotaRows,
  formatWindowDuration,
  histogramPercentile,
  normalizeQuotaCostHistogram,
  summarizeReport,
  weightedTps,
} from "../src/metrics";
import {
  MIXED_ATTRIBUTION_KEY,
  QUOTA_COST_BUCKET_BOUNDS,
  STATS_SCHEMA_VERSION,
  StatsReport,
  TPS_BUCKET_BOUNDS,
  validateDataManifest,
  validateStatsReport,
} from "../src/schema";

function makeHist(length: number, buckets: Record<number, number>): number[] {
  const hist = new Array<number>(length).fill(0);
  for (const [idx, count] of Object.entries(buckets)) {
    hist[Number(idx)] = count;
  }
  return hist;
}

function tpsHist(buckets: Record<number, number>): number[] {
  return makeHist(TPS_BUCKET_BOUNDS.length, buckets);
}

function quotaHist(buckets: Record<number, number>): number[] {
  return makeHist(QUOTA_COST_BUCKET_BOUNDS.length, buckets);
}

describe("web metrics helpers", () => {
  test("computes weighted TPS, histogram percentiles, model scorecard, and quota token/cost weights", () => {
    const tps = weightedTps({
      model: "Claude Opus 4.6",
      effort: "high",
      sample_count: 10,
      output_tokens: 5_000,
      elapsed_ms: 100_000,
      histogram: tpsHist({ 2: 2, 3: 6, 4: 2 }),
    });
    expect(tps).toBe(50);

    const p50 = histogramPercentile(tpsHist({ 2: 2, 3: 6, 4: 2 }), 0.5);
    expect(p50).not.toBeNull();
    expect(p50!).toBeGreaterThan(45);
    expect(p50!).toBeLessThan(60);

    const quotaRows = deriveQuotaRows([
      {
        plan_tier: "Google AI Ultra",
        bucket: "3p-5h",
        attributed_model: "Claude Sonnet 4.6",
        attributed_effort: "medium",
        slices: 4,
        delta_quota_fraction: 0.04,
        input_tokens: 20_000,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        output_tokens: 2_000,
        reference_cost_usd: 8.0, // $2.00 per 1% quota mean
        cost_per_pct_histogram: quotaHist({ 8: 2, 9: 2 }), // p50 = $2.00
      },
      {
        plan_tier: "Google AI Ultra",
        bucket: "3p-5h",
        attributed_model: "Claude Opus 4.6",
        attributed_effort: "high",
        slices: 4,
        delta_quota_fraction: 0.08,
        input_tokens: 20_000,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        output_tokens: 2_000,
        reference_cost_usd: 8.0, // $1.00 per 1% quota mean
        cost_per_pct_histogram: quotaHist({ 7: 2, 8: 2 }), // p50 = $1.00 (2x weight)
      },
      {
        plan_tier: "Google AI Ultra",
        bucket: "3p-5h",
        attributed_model: MIXED_ATTRIBUTION_KEY,
        attributed_effort: MIXED_ATTRIBUTION_KEY,
        slices: 1,
        delta_quota_fraction: 0.01,
        input_tokens: 1_000,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        output_tokens: 100,
        reference_cost_usd: 0.01,
      },
    ]);

    expect(quotaRows[0].costPer1PctUsd).toBeCloseTo(2.0, 6);
    expect(quotaRows[0].percentiles.p50).toBeCloseTo(2.0, 6);
    expect(quotaRows[0].percentiles.p90).toBeCloseTo(3.6, 6);
    expect(quotaRows[0].percentiles.p95).toBeCloseTo(3.8, 6);
    expect(quotaRows[0].p50CostPer1PctUsd).toBeCloseTo(2.0, 6);
    expect(quotaRows[0].p90CostPer1PctUsd).toBeCloseTo(3.6, 6);
    expect(quotaRows[0].p95CostPer1PctUsd).toBeCloseTo(3.8, 6);
    expect(quotaRows[0].outputTokensPer1Pct).toBeCloseTo(500, 6);
    expect(quotaRows[0].totalTokensPer1Pct).toBeCloseTo(5500, 6);
    expect(quotaRows[1].costPer1PctUsd).toBeCloseTo(1.0, 6);
    expect(quotaRows[1].p50CostPer1PctUsd).toBeCloseTo(1.0, 6);
    expect(quotaRows[1].relativeWeight).toBeCloseTo(1.0, 6);
    expect(quotaRows[0].relativeWeight).toBeCloseTo(0.5, 6);
    expect(quotaRows[2].relativeWeight).toBeNull();

    const report: StatsReport = {
      schema_version: STATS_SCHEMA_VERSION,
      scope: "local",
      window: {
        start: "2026-11-14T15:00:00Z",
        end: "2026-11-14T16:00:00Z",
        duration_secs: 3600,
      },
      contributors: 1,
      usage_by_plan_model: [
        {
          plan_tier: "Google AI Ultra",
          model: "Claude Opus 4.6",
          effort: "high",
          events: 5,
          input_tokens: 1000,
          cache_read_tokens: 9000,
          cache_write_tokens: 2000,
          output_tokens: 500,
          reference_cost_usd: 0.05,
        },
        {
          plan_tier: "Google AI Pro",
          model: "Claude Opus 4.6",
          effort: "high",
          events: 3,
          input_tokens: 1000,
          cache_read_tokens: 9000,
          cache_write_tokens: 0,
          output_tokens: 500,
          reference_cost_usd: 0.05,
        },
        {
          plan_tier: "Google AI Ultra",
          model: "Gemini 4 Argon",
          effort: "medium",
          events: 2,
          input_tokens: 500,
          cache_read_tokens: 500,
          cache_write_tokens: 0,
          output_tokens: 100,
          reference_cost_usd: 0.01,
        },
      ],
      tps_by_model_effort: [
        {
          model: "Claude Opus 4.6",
          effort: "high",
          sample_count: 10,
          output_tokens: 5_000,
          elapsed_ms: 100_000,
          histogram: tpsHist({ 2: 2, 3: 6, 4: 1, 5: 1 }),
        },
      ],
      quota_by_plan_bucket_model: [],
    };
    const summary = summarizeReport(report);
    expect(summary.cacheWriteTokens).toBe(2000);
    expect(summary.totalTokens).toBe(24100);
    expect(summary.activeModels).toBe(2);
    expect(summary.tpsSamples).toBe(10);
    expect(summary.topModelLabel).toBe("Claude Opus 4.6 (high)");
    expect(summary.topModelSharePct).toBeCloseTo((23000 * 100) / 24100, 5);
    expect(summary.topModelSharePct).toBeLessThanOrEqual(100);

    // Same model across multiple plan tiers is consolidated into 1 scorecard row without duplicating TPS samples.
    const scorecard = deriveModelScorecard(report);
    expect(scorecard.length).toBe(2);
    expect(scorecard[0].model).toBe("Claude Opus 4.6");
    expect(scorecard[0].planTier).toBe("Google AI Pro, Google AI Ultra");
    expect(scorecard[0].events).toBe(8);
    expect(scorecard[0].tpsSamples).toBe(10);
    expect(scorecard[0].cacheHitRate).toBeCloseTo(90, 6);
    expect(scorecard[0].tpsPercentiles.p50!).toBeGreaterThan(45);
    expect(scorecard[1].model).toBe("Gemini 4 Argon");
    expect(scorecard[1].cacheHitRate).toBeCloseTo(50, 6);
    expect(scorecard[1].tpsPercentiles.p50).toBeNull();
  });

  test("matches Rust top quota cost bucket percentiles ($32-$64) and strictly validates StatsReport JSON", () => {
    const [topRow] = deriveQuotaRows([
      {
        plan_tier: "Google AI Ultra",
        bucket: "3p-5h",
        attributed_model: "Claude Opus 4.6",
        attributed_effort: "high",
        slices: 10,
        delta_quota_fraction: 0.1,
        input_tokens: 10_000,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        output_tokens: 1_000,
        reference_cost_usd: 48.0,
        cost_per_pct_histogram: quotaHist({ 13: 10 }),
      },
    ]);
    expect(topRow.percentiles.p50).toBeCloseTo(48.0, 6);
    expect(topRow.percentiles.p90).toBeCloseTo(60.8, 6);
    expect(topRow.percentiles.p95).toBeCloseTo(62.4, 6);

    const validReport: StatsReport = {
      schema_version: STATS_SCHEMA_VERSION,
      scope: "local",
      window: {
        start: "2026-11-14T15:00:00Z",
        end: "2026-11-14T16:00:00Z",
        duration_secs: 3600,
      },
      contributors: 1,
      usage_by_plan_model: [],
      tps_by_model_effort: [
        {
          model: "Claude Opus 4.6",
          effort: "high",
          sample_count: 1,
          output_tokens: 50,
          elapsed_ms: 1000,
          histogram: tpsHist({ 3: 1 }),
        },
      ],
      quota_by_plan_bucket_model: [],
    };
    expect(validateStatsReport(validReport)).not.toBeNull();

    // High-TPS bucket [800, 1300) (1000 t/s sample) resolves to ~1050 p50 and ~1275 p95 rather than capping at 200.
    const highTpsHist = tpsHist({ 12: 1 });
    expect(histogramPercentile(highTpsHist, 0.5)).toBeCloseTo(1050, 6);
    expect(histogramPercentile(highTpsHist, 0.95)).toBeCloseTo(1275, 6);
    expect(histogramPercentile(tpsHist({ 13: 1 }), 0.5)).toBeCloseTo(1650, 6);

    const wrongVersion = { ...validReport, schema_version: 99 };
    expect(validateStatsReport(wrongVersion)).toBeNull();

    const missingWindow = { ...validReport, window: undefined };
    expect(validateStatsReport(missingWindow)).toBeNull();

    const missingTps = { ...validReport, tps_by_model_effort: undefined };
    expect(validateStatsReport(missingTps)).toBeNull();

    const missingQuota = { ...validReport, quota_by_plan_bucket_model: undefined };
    expect(validateStatsReport(missingQuota)).toBeNull();

    const badHistogram = {
      ...validReport,
      tps_by_model_effort: [{ ...validReport.tps_by_model_effort[0], histogram: [1, 2] }],
    };
    expect(validateStatsReport(badHistogram)).toBeNull();

    expect(formatWindowDuration(0)).toBe("All");
    expect(formatWindowDuration(900)).toBe("15m");
    expect(formatWindowDuration(3600)).toBe("1h");
    expect(formatWindowDuration(86400)).toBe("1d");
  });

  test("rejects fractional integers in StatsReport and unsafe paths in DataManifest", () => {
    const baseReport: StatsReport = {
      schema_version: STATS_SCHEMA_VERSION,
      scope: "local",
      window: {
        start: "2026-11-14T10:00:00Z",
        end: "2026-11-14T11:00:00Z",
        duration_secs: 3600,
      },
      contributors: 1,
      usage_by_plan_model: [],
      tps_by_model_effort: [
        {
          model: "Claude Opus 4.6",
          effort: "high",
          sample_count: 1,
          output_tokens: 50,
          elapsed_ms: 1000,
          histogram: tpsHist({ 3: 1 }),
        },
      ],
      quota_by_plan_bucket_model: [],
    };

    expect(validateStatsReport({ ...baseReport, contributors: 1.5 })).toBeNull();
    expect(
      validateStatsReport({
        ...baseReport,
        tps_by_model_effort: [{ ...baseReport.tps_by_model_effort[0], sample_count: 1.5 }],
      }),
    ).toBeNull();
    const fractionalHist = tpsHist({});
    fractionalHist[3] = 0.5;
    expect(
      validateStatsReport({
        ...baseReport,
        tps_by_model_effort: [{ ...baseReport.tps_by_model_effort[0], histogram: fractionalHist }],
      }),
    ).toBeNull();

    const validManifest = validateDataManifest({
      updated_at: "2026-11-14T12:00:00Z",
      last_hour: "2026-11-14T11:00:00Z",
      available_hours: ["./data/hourly/2026-11-14T11.json", "data/hourly/2026-11-14T12.json"],
      rolling: {
        "24h": "./data/rolling/24h.json",
        "7d": "data/rolling/7d.json",
      },
      extraField: "ignored",
    });
    expect(validManifest).toEqual({
      updated_at: "2026-11-14T12:00:00Z",
      last_hour: "2026-11-14T11:00:00Z",
      available_hours: ["./data/hourly/2026-11-14T11.json", "data/hourly/2026-11-14T12.json"],
      rolling: {
        "24h": "./data/rolling/24h.json",
        "7d": "data/rolling/7d.json",
      },
    });

    for (const unsafePath of [
      "https://evil.example/24h.json",
      "//evil.example/24h.json",
      "../secret.json",
      "./data/../secret.json",
      "/abs/path/24h.json",
      "./data/24h.json?cache=bust",
      "./data/24h.txt",
    ]) {
      expect(
        validateDataManifest({
          updated_at: "2026-11-14T12:00:00Z",
          last_hour: null,
          available_hours: [unsafePath],
          rolling: {},
        }),
      ).toBeNull();
      expect(
        validateDataManifest({
          updated_at: "2026-11-14T12:00:00Z",
          last_hour: null,
          available_hours: [],
          rolling: { "24h": unsafePath },
        }),
      ).toBeNull();
    }
  });

  test("preserves explicit all-zero quota histograms and guards weightedTps", () => {
    const emptyCostHist = new Array<number>(QUOTA_COST_BUCKET_BOUNDS.length).fill(0);
    const rowWithExplicitEmpty = {
      plan_tier: "pro",
      bucket: "Claude",
      attributed_model: "Claude Sonnet 4.6",
      attributed_effort: "high",
      slices: 4,
      delta_quota_fraction: 0.2,
      input_tokens: 1000,
      cache_read_tokens: 0,
      cache_write_tokens: 0,
      output_tokens: 500,
      reference_cost_usd: 2.0,
      cost_per_pct_histogram: emptyCostHist,
    };

    expect(normalizeQuotaCostHistogram(rowWithExplicitEmpty)).toEqual(emptyCostHist);
    const [derived] = deriveQuotaRows([rowWithExplicitEmpty]);
    expect(derived.percentiles.p50).toBeNull();
    expect(derived.percentiles.p90).toBeNull();
    expect(derived.percentiles.p95).toBeNull();

    expect(
      weightedTps({
        model: "m",
        effort: "e",
        sample_count: 1,
        output_tokens: 500,
        elapsed_ms: 250,
        histogram: tpsHist({}),
      }),
    ).toBeCloseTo(2000, 6);
    expect(
      weightedTps({
        model: "m",
        effort: "e",
        sample_count: 1,
        output_tokens: 500,
        elapsed_ms: 0,
        histogram: tpsHist({}),
      }),
    ).toBe(0);
  });

  test("resolves sub-cent quota percentiles without collapsing RelWt and separates quotaRefCostUsd", () => {
    const quotaRows = deriveQuotaRows([
      {
        plan_tier: "Google AI Pro",
        bucket: "Claude",
        attributed_model: "Claude Opus 4.6",
        attributed_effort: "high",
        slices: 4,
        delta_quota_fraction: 0.4,
        input_tokens: 1000,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        output_tokens: 500,
        reference_cost_usd: 0.3, // $0.0075 / 1%
        cost_per_pct_histogram: quotaHist({ 1: 4 }), // [0.005, 0.01)
      },
      {
        plan_tier: "Google AI Pro",
        bucket: "Claude",
        attributed_model: "Claude Sonnet 4.6",
        attributed_effort: "medium",
        slices: 4,
        delta_quota_fraction: 0.4,
        input_tokens: 1000,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        output_tokens: 500,
        reference_cost_usd: 0.1, // $0.0025 / 1%
        cost_per_pct_histogram: quotaHist({ 0: 4 }), // [0, 0.005)
      },
    ]);

    expect(quotaRows[0].percentiles.p50).toBeCloseTo(0.0075, 6);
    expect(quotaRows[1].percentiles.p50).toBeCloseTo(0.0025, 6);
    expect(quotaRows[0].relativeWeight).toBeCloseTo(1 / 3, 6);
    expect(quotaRows[1].relativeWeight).toBeCloseTo(1.0, 6);

    const summary = summarizeReport({
      schema_version: STATS_SCHEMA_VERSION,
      scope: "local",
      window: {
        start: "2026-11-15T00:00:00Z",
        end: "2026-11-15T01:00:00Z",
        duration_secs: 3600,
      },
      contributors: 1,
      usage_by_plan_model: [
        {
          plan_tier: "Google AI Pro",
          model: "Claude Opus 4.6",
          effort: "high",
          events: 4,
          input_tokens: 1000,
          cache_read_tokens: 0,
          cache_write_tokens: 0,
          output_tokens: 500,
          reference_cost_usd: 0.14,
        },
      ],
      tps_by_model_effort: [],
      quota_by_plan_bucket_model: [
        {
          plan_tier: "Google AI Pro",
          bucket: "Claude",
          attributed_model: "Claude Opus 4.6",
          attributed_effort: "high",
          slices: 3,
          delta_quota_fraction: 0.3,
          input_tokens: 800,
          cache_read_tokens: 0,
          cache_write_tokens: 0,
          output_tokens: 400,
          reference_cost_usd: 0.072,
          cost_per_pct_histogram: quotaHist({ 0: 3 }),
        },
        {
          plan_tier: "Google AI Pro",
          bucket: "Claude",
          attributed_model: "Claude Sonnet 4.6",
          attributed_effort: "medium",
          slices: 1,
          delta_quota_fraction: 0.1,
          input_tokens: 200,
          cache_read_tokens: 0,
          cache_write_tokens: 0,
          output_tokens: 100,
          reference_cost_usd: 0.028,
          cost_per_pct_histogram: quotaHist({ 0: 1 }),
        },
      ],
    });
    expect(summary.quotaRefCostUsd).toBeCloseTo(0.1, 6);
    expect(summary.quotaSlices).toBe(4);
  });
});
