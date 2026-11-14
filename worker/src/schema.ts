import {
  MAX_VALID_TPS,
  QUOTA_COST_HISTOGRAM_BUCKETS,
  STATS_SCHEMA_VERSION,
  TPS_HISTOGRAM_BUCKETS,
  normalizeQuotaCostHistogram,
  type QuotaByPlanBucketModel,
  type StatsReport,
  type StatsWindow,
  type TpsByModelEffort,
  type UsageByPlanModel,
} from "../../shared/telemetry";

export {
  MAX_VALID_TPS,
  QUOTA_COST_HISTOGRAM_BUCKETS,
  STATS_SCHEMA_VERSION,
  TPS_HISTOGRAM_BUCKETS,
  normalizeQuotaCostHistogram,
  type QuotaByPlanBucketModel,
  type StatsReport,
  type StatsWindow,
  type TpsByModelEffort,
  type UsageByPlanModel,
};

export const MAX_ARRAY_ITEMS = 200;
export const MAX_TOKENS_PER_ROW = 1_000_000_000;
export const MAX_CACHE_READ_TOKENS_PER_ROW = 10_000_000_000;
export const MAX_COST_PER_ROW = 25_000;
export const MAX_QUOTA_DELTA_FRACTION = 5.0;
export const MAX_TPS_ELAPSED_MS = 360_000_000;
export const MAX_LABEL_LEN = 128;
export const MAX_INGEST_BATCH_SIZE = 50;

export interface ValidatedIngestItem {
  submission_id: string;
  client_token: string;
  report: StatsReport;
  hourBucket: string;
}

export type BatchValidationResult =
  | { ok: true; items: ValidatedIngestItem[] }
  | { ok: false; error: string };

export function toUtcHourBucket(iso: string): string | null {
  const ms = Date.parse(iso);
  if (!Number.isFinite(ms)) return null;
  const d = new Date(ms);
  d.setUTCMinutes(0, 0, 0);
  return d.toISOString().replace(".000Z", "Z");
}

function isNonEmptyBoundedString(val: unknown, maxLen = MAX_LABEL_LEN): val is string {
  return typeof val === "string" && val.length > 0 && val.length <= maxLen;
}

function isFiniteIntInRange(val: unknown, min: number, max: number): val is number {
  return typeof val === "number" && Number.isInteger(val) && val >= min && val <= max;
}

function isFiniteFloatInRange(val: unknown, min: number, max: number): val is number {
  return typeof val === "number" && Number.isFinite(val) && val >= min && val <= max;
}

function validateSubmissionEntry(
  submissionIdRaw: unknown,
  clientToken: string,
  reportRaw: unknown,
): { ok: true; value: ValidatedIngestItem } | { ok: false; error: string } {
  if (!isNonEmptyBoundedString(submissionIdRaw, 128)) {
    return { ok: false, error: "submission_id must be a non-empty string <= 128 chars" };
  }
  if (!reportRaw || typeof reportRaw !== "object") {
    return { ok: false, error: "report must be a JSON object" };
  }
  const r = reportRaw as Record<string, unknown>;
  if (r.schema_version !== STATS_SCHEMA_VERSION) {
    return { ok: false, error: `unsupported schema_version: ${String(r.schema_version)}` };
  }
  if (!r.window || typeof r.window !== "object") {
    return { ok: false, error: "report.window is required" };
  }
  const win = r.window as Record<string, unknown>;
  if (!isNonEmptyBoundedString(win.start, 40) || !isNonEmptyBoundedString(win.end, 40)) {
    return { ok: false, error: "report.window.start and end must be ISO-8601 strings" };
  }
  const startMs = Date.parse(win.start);
  const endMs = Date.parse(win.end);
  if (!Number.isFinite(startMs) || !Number.isFinite(endMs) || endMs <= startMs) {
    return { ok: false, error: "report.window timestamps are invalid or non-positive length" };
  }
  if (!isFiniteIntInRange(win.duration_secs, 1, 3_600)) {
    return { ok: false, error: "ingest report.window.duration_secs must be within 1..3600" };
  }
  if (Math.abs(endMs - startMs - win.duration_secs * 1_000) >= 1_000) {
    return { ok: false, error: "report.window.duration_secs must match start/end duration" };
  }
  const hourBucket = toUtcHourBucket(win.start);
  if (!hourBucket) {
    return { ok: false, error: "cannot derive UTC hour_bucket from window.start" };
  }
  if (endMs > Date.parse(hourBucket) + 3_600_000) {
    return { ok: false, error: "report.window must stay within a single UTC hour slice" };
  }
  if (
    !Array.isArray(r.usage_by_plan_model) ||
    !Array.isArray(r.tps_by_model_effort) ||
    !Array.isArray(r.quota_by_plan_bucket_model)
  ) {
    return { ok: false, error: "report arrays must all be present" };
  }
  if (
    r.usage_by_plan_model.length > MAX_ARRAY_ITEMS ||
    r.tps_by_model_effort.length > MAX_ARRAY_ITEMS ||
    r.quota_by_plan_bucket_model.length > MAX_ARRAY_ITEMS
  ) {
    return { ok: false, error: "report array exceeds maximum allowed item count" };
  }
  if (
    r.usage_by_plan_model.length === 0 &&
    r.tps_by_model_effort.length === 0 &&
    r.quota_by_plan_bucket_model.length === 0
  ) {
    return { ok: false, error: "empty telemetry report ignored" };
  }

  const sanitizedUsage: UsageByPlanModel[] = [];
  for (const item of r.usage_by_plan_model) {
    if (!item || typeof item !== "object") return { ok: false, error: "invalid usage row" };
    const u = item as Record<string, unknown>;
    if (
      !isNonEmptyBoundedString(u.plan_tier) ||
      !isNonEmptyBoundedString(u.model) ||
      !isNonEmptyBoundedString(u.effort) ||
      !isFiniteIntInRange(u.events, 1, 100_000) ||
      !isFiniteIntInRange(u.input_tokens, 0, MAX_TOKENS_PER_ROW) ||
      !isFiniteIntInRange(u.cache_read_tokens, 0, MAX_CACHE_READ_TOKENS_PER_ROW) ||
      !isFiniteIntInRange(u.cache_write_tokens, 0, MAX_TOKENS_PER_ROW) ||
      !isFiniteIntInRange(u.output_tokens, 0, MAX_TOKENS_PER_ROW) ||
      !isFiniteFloatInRange(u.reference_cost_usd, 0, MAX_COST_PER_ROW)
    ) {
      return { ok: false, error: "invalid fields in usage_by_plan_model" };
    }
    const totalTokens =
      (u.input_tokens as number) +
      (u.cache_read_tokens as number) +
      (u.cache_write_tokens as number) +
      (u.output_tokens as number);
    if (totalTokens === 0 && (u.reference_cost_usd as number) > 0) {
      return { ok: false, error: "usage reference_cost_usd must be zero when tokens are zero" };
    }
    if ((u.reference_cost_usd as number) > totalTokens * 0.001 + 1e-6) {
      return { ok: false, error: "usage reference_cost_usd exceeds plausible token pricing" };
    }
    sanitizedUsage.push({
      plan_tier: u.plan_tier,
      model: u.model,
      effort: u.effort,
      events: u.events as number,
      input_tokens: u.input_tokens as number,
      cache_read_tokens: u.cache_read_tokens as number,
      cache_write_tokens: u.cache_write_tokens as number,
      output_tokens: u.output_tokens as number,
      reference_cost_usd: u.reference_cost_usd as number,
    });
  }

  const sanitizedTps: TpsByModelEffort[] = [];
  for (const item of r.tps_by_model_effort) {
    if (!item || typeof item !== "object") return { ok: false, error: "invalid tps row" };
    const t = item as Record<string, unknown>;
    if (
      !isNonEmptyBoundedString(t.model) ||
      !isNonEmptyBoundedString(t.effort) ||
      !isFiniteIntInRange(t.sample_count, 1, 50_000) ||
      !Array.isArray(t.histogram) ||
      t.histogram.length !== TPS_HISTOGRAM_BUCKETS
    ) {
      return { ok: false, error: "invalid fields in tps_by_model_effort" };
    }
    const sampleCount = t.sample_count as number;
    if (
      !isFiniteIntInRange(t.output_tokens, 100 * sampleCount, MAX_TOKENS_PER_ROW) ||
      !isFiniteIntInRange(t.elapsed_ms, 500 * sampleCount, MAX_TPS_ELAPSED_MS)
    ) {
      return { ok: false, error: "invalid fields in tps_by_model_effort" };
    }
    let histSum = 0;
    const cleanHistogram: number[] = [];
    for (let idx = 0; idx < TPS_HISTOGRAM_BUCKETS; idx++) {
      const bucketCount = t.histogram[idx];
      if (!isFiniteIntInRange(bucketCount, 0, 50_000)) {
        return { ok: false, error: "invalid tps histogram bucket count" };
      }
      const count = bucketCount as number;
      cleanHistogram.push(count);
      histSum += count;
    }
    if (histSum !== sampleCount) {
      return { ok: false, error: "tps histogram sum must match sample_count" };
    }
    const outputTokens = t.output_tokens as number;
    const elapsedMs = t.elapsed_ms as number;
    if (outputTokens * 1_000 > MAX_VALID_TPS * elapsedMs) {
      return { ok: false, error: "implied TPS out of sane bounds" };
    }
    sanitizedTps.push({
      model: t.model,
      effort: t.effort,
      sample_count: sampleCount,
      output_tokens: outputTokens,
      elapsed_ms: elapsedMs,
      histogram: cleanHistogram,
    });
  }

  const sanitizedQuota: QuotaByPlanBucketModel[] = [];
  for (const item of r.quota_by_plan_bucket_model) {
    if (!item || typeof item !== "object") return { ok: false, error: "invalid quota row" };
    const q = item as Record<string, unknown>;
    if (
      !isNonEmptyBoundedString(q.plan_tier) ||
      !isNonEmptyBoundedString(q.bucket) ||
      !isNonEmptyBoundedString(q.attributed_model) ||
      !isNonEmptyBoundedString(q.attributed_effort) ||
      !isFiniteIntInRange(q.slices, 1, 10_000) ||
      !isFiniteFloatInRange(q.delta_quota_fraction, 1e-6, MAX_QUOTA_DELTA_FRACTION) ||
      !isFiniteIntInRange(q.input_tokens, 0, MAX_TOKENS_PER_ROW) ||
      !isFiniteIntInRange(q.cache_read_tokens, 0, MAX_CACHE_READ_TOKENS_PER_ROW) ||
      !isFiniteIntInRange(q.cache_write_tokens, 0, MAX_TOKENS_PER_ROW) ||
      !isFiniteIntInRange(q.output_tokens, 0, MAX_TOKENS_PER_ROW) ||
      !isFiniteFloatInRange(q.reference_cost_usd, 0, MAX_COST_PER_ROW)
    ) {
      return { ok: false, error: "invalid fields in quota_by_plan_bucket_model" };
    }
    const quotaTokens =
      (q.input_tokens as number) +
      (q.cache_read_tokens as number) +
      (q.cache_write_tokens as number) +
      (q.output_tokens as number);
    const refCostUsd = q.reference_cost_usd as number;
    if (quotaTokens === 0 && refCostUsd > 0) {
      return { ok: false, error: "quota reference_cost_usd must be zero when tokens are zero" };
    }
    if (refCostUsd > quotaTokens * 0.001 + 1e-6) {
      return { ok: false, error: "quota reference_cost_usd exceeds plausible token pricing" };
    }

    let cleanCostHistogram: number[];
    if (q.cost_per_pct_histogram !== undefined) {
      if (
        !Array.isArray(q.cost_per_pct_histogram) ||
        q.cost_per_pct_histogram.length !== QUOTA_COST_HISTOGRAM_BUCKETS
      ) {
        return { ok: false, error: "invalid cost_per_pct_histogram length" };
      }
      let histSum = 0;
      cleanCostHistogram = [];
      for (let idx = 0; idx < QUOTA_COST_HISTOGRAM_BUCKETS; idx++) {
        const bucketCount = q.cost_per_pct_histogram[idx];
        if (!isFiniteIntInRange(bucketCount, 0, 10_000)) {
          return { ok: false, error: "invalid cost_per_pct_histogram count" };
        }
        const count = bucketCount as number;
        cleanCostHistogram.push(count);
        histSum += count;
      }
      if (histSum > (q.slices as number)) {
        return { ok: false, error: "cost_per_pct_histogram sum exceeds slices" };
      }
    } else {
      cleanCostHistogram = normalizeQuotaCostHistogram({
        slices: q.slices as number,
        delta_quota_fraction: q.delta_quota_fraction as number,
        reference_cost_usd: refCostUsd,
      });
    }

    sanitizedQuota.push({
      plan_tier: q.plan_tier,
      bucket: q.bucket,
      attributed_model: q.attributed_model,
      attributed_effort: q.attributed_effort,
      slices: q.slices as number,
      delta_quota_fraction: q.delta_quota_fraction as number,
      input_tokens: q.input_tokens as number,
      cache_read_tokens: q.cache_read_tokens as number,
      cache_write_tokens: q.cache_write_tokens as number,
      output_tokens: q.output_tokens as number,
      reference_cost_usd: refCostUsd,
      cost_per_pct_histogram: cleanCostHistogram,
    });
  }

  const sanitizedReport: StatsReport = {
    schema_version: STATS_SCHEMA_VERSION,
    scope: "local",
    window: {
      start: win.start as string,
      end: win.end as string,
      duration_secs: win.duration_secs as number,
    },
    contributors: 1,
    usage_by_plan_model: sanitizedUsage,
    tps_by_model_effort: sanitizedTps,
    quota_by_plan_bucket_model: sanitizedQuota,
  };

  return {
    ok: true,
    value: {
      submission_id: submissionIdRaw,
      client_token: clientToken,
      report: sanitizedReport,
      hourBucket,
    },
  };
}

/** Validates a batch ingest payload `{ client_token, submissions: [...] }`. */
export function validateIngestBatchPayload(raw: unknown): BatchValidationResult {
  if (!raw || typeof raw !== "object") {
    return { ok: false, error: "payload must be a JSON object" };
  }
  const obj = raw as Record<string, unknown>;
  if (!isNonEmptyBoundedString(obj.client_token, 128)) {
    return { ok: false, error: "client_token must be a non-empty string <= 128 chars" };
  }
  if (!Array.isArray(obj.submissions) || obj.submissions.length === 0) {
    return { ok: false, error: "submissions must be a non-empty array" };
  }
  if (obj.submissions.length > MAX_INGEST_BATCH_SIZE) {
    return {
      ok: false,
      error: `submissions batch exceeds maximum of ${MAX_INGEST_BATCH_SIZE} items`,
    };
  }

  const items: ValidatedIngestItem[] = [];
  for (let i = 0; i < obj.submissions.length; i++) {
    const entry = obj.submissions[i];
    if (!entry || typeof entry !== "object") {
      return { ok: false, error: `submissions[${i}] must be a JSON object` };
    }
    const item = entry as Record<string, unknown>;
    if (item.client_token !== undefined) {
      return {
        ok: false,
        error: `submissions[${i}]: per-submission client_token override is not allowed`,
      };
    }
    const res = validateSubmissionEntry(item.submission_id, obj.client_token, item.report);
    if (!res.ok) {
      return { ok: false, error: `submissions[${i}]: ${res.error}` };
    }
    items.push(res.value);
  }

  return { ok: true, items };
}
