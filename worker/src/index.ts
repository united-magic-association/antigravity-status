import { aggregateHourByContributors } from "./merge";
import {
  STATS_SCHEMA_VERSION,
  StatsReport,
  toUtcHourBucket,
  ValidatedIngestItem,
  validateIngestBatchPayload,
} from "./schema";

export interface Env {
  DB: D1Database;
  INGEST_RATE_LIMITER?: RateLimit;
  ANONYMIZATION_SALT?: string;
  TELEMETRY_API_TOKEN?: string;
}

const MAX_INGEST_BYTES = 512 * 1024;
// Must stay aligned with `hourly_quota` CHECK constraint in migrations/0001_init.sql.
const MAX_SUBMISSIONS_PER_CLIENT_HOUR = 6;
const MAX_SUBMISSIONS_PER_IP_HOUR = 24;
const MAX_INGEST_CONFLICT_RETRIES = 3;
const RAW_RETENTION_DAYS = 3;
const RAW_PRUNE_BATCH_SIZE = 500;
const RAW_PRUNE_DEFAULT_BATCHES = 20;
// Keep a single Cron invocation strictly below Cloudflare Workers Free plan's 50 D1 query limit.
const MAX_CRON_D1_QUERIES = 45;
const MAX_ROLLUP_HOURS_PER_RUN = 20;

const MAX_REPORT_JSON_BYTES = 256 * 1024;
const SHARED_MISSING_IP_KEY = "missing-ip";

function errorTextAndCode(err: unknown): string {
  const parts: string[] = [];
  let current: unknown = err;
  for (let depth = 0; depth < 4 && current && typeof current === "object"; depth++) {
    const code = (current as { code?: unknown }).code;
    const message = (current as { message?: unknown }).message;
    if (typeof code === "string" && code) parts.push(code);
    if (typeof message === "string" && message) parts.push(message);
    current = (current as { cause?: unknown }).cause;
  }
  return parts.join(" ");
}

function isHourlyQuotaConstraintError(err: unknown): boolean {
  return /CHECK constraint failed|SQLITE_CONSTRAINT_CHECK/i.test(errorTextAndCode(err));
}

function isReceiptUniqueConstraintError(err: unknown): boolean {
  return /UNIQUE constraint failed|SQLITE_CONSTRAINT_UNIQUE|SQLITE_CONSTRAINT_PRIMARYKEY|SQLITE_CONSTRAINT/i.test(
    errorTextAndCode(err),
  );
}

function toIsoSeconds(date: Date): string {
  return date.toISOString().replace(/\.\d{3}Z$/, "Z");
}

function sqlPlaceholders(count: number): string {
  return Array.from({ length: count }, (_, i) => `?${i + 1}`).join(", ");
}

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: {
      "content-type": "application/json; charset=utf-8",
      "cache-control": "no-store",
    },
  });
}

async function sha256Hex(input: string): Promise<string> {
  const bytes = new TextEncoder().encode(input);
  const digest = await crypto.subtle.digest("SHA-256", bytes);
  return Array.from(new Uint8Array(digest))
    .map((b) => b.toString(16).padStart(2, "0"))
    .join("");
}

function timingSafeEqual(a: string, b: string): boolean {
  const encoder = new TextEncoder();
  const ab = encoder.encode(a);
  const bb = encoder.encode(b);
  if (ab.length !== bb.length) return false;
  let diff = 0;
  for (let i = 0; i < ab.length; i++) {
    diff |= ab[i] ^ bb[i];
  }
  return diff === 0;
}

function extractBearerToken(request: Request): string | null {
  const auth = request.headers.get("authorization");
  if (!auth) return null;
  const match = auth.match(/^Bearer\s+(.+)$/i);
  return match ? match[1].trim() : null;
}

function addHoursIso(hourBucket: string, hours: number): string {
  const ms = Date.parse(hourBucket) + hours * 3_600_000;
  return new Date(ms).toISOString().replace(".000Z", "Z");
}

function normalizeIpIdentity(rawIp: string): string {
  const withoutZone = rawIp.trim().split("%")[0].toLowerCase();
  if (!withoutZone.includes(":")) {
    return withoutZone;
  }
  if (withoutZone.startsWith("::ffff:") && !withoutZone.slice(7).includes(":")) {
    return withoutZone.slice(7);
  }
  const halves = withoutZone.split("::");
  if (halves.length > 2) return withoutZone;
  const left = halves[0] ? halves[0].split(":").filter(Boolean) : [];
  const right = halves.length === 2 && halves[1] ? halves[1].split(":").filter(Boolean) : [];
  const missing = halves.length === 2 ? Math.max(0, 8 - left.length - right.length) : 0;
  const groups = [...left, ...new Array<string>(missing).fill("0"), ...right];
  if (groups.length < 4) return withoutZone;
  const prefix = groups.slice(0, 4).map((g) => Number.parseInt(g, 16).toString(16));
  if (prefix.some((p) => p === "nan")) return withoutZone;
  return `${prefix.join(":")}::/64`;
}

function extractClientIp(request: Request): string {
  const cfIp = request.headers.get("cf-connecting-ip")?.trim();
  if (cfIp) return normalizeIpIdentity(cfIp);
  return SHARED_MISSING_IP_KEY;
}

async function readRequestTextLimited(
  request: Request,
  maxBytes: number,
): Promise<{ ok: true; text: string } | { ok: false }> {
  const contentLengthHeader = request.headers.get("content-length");
  if (contentLengthHeader) {
    const len = Number(contentLengthHeader);
    if (Number.isFinite(len) && len > maxBytes) {
      return { ok: false };
    }
  }
  if (!request.body) {
    const text = await request.text();
    return new TextEncoder().encode(text).byteLength <= maxBytes
      ? { ok: true, text }
      : { ok: false };
  }
  const reader = request.body.getReader();
  const chunks: Uint8Array[] = [];
  let totalBytes = 0;
  while (true) {
    const { done, value } = await reader.read();
    if (done) break;
    if (value) {
      totalBytes += value.byteLength;
      if (totalBytes > maxBytes) {
        await reader.cancel().catch(() => {});
        return { ok: false };
      }
      chunks.push(value);
    }
  }
  const merged = new Uint8Array(totalBytes);
  let offset = 0;
  for (const chunk of chunks) {
    merged.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return { ok: true, text: new TextDecoder().decode(merged) };
}

export async function handleIngest(
  request: Request,
  env: Env,
  now = new Date(),
): Promise<Response> {
  const salt = env.ANONYMIZATION_SALT?.trim();
  if (!salt) {
    return jsonResponse({ ok: false, error: "anonymization salt not configured" }, 503);
  }

  const clientIp = extractClientIp(request);
  if (env.INGEST_RATE_LIMITER) {
    const { success } = await env.INGEST_RATE_LIMITER.limit({ key: `ingest:${clientIp}` });
    if (!success) {
      return jsonResponse({ ok: false, error: "too many requests from IP" }, 429);
    }
  }

  const bodyRead = await readRequestTextLimited(request, MAX_INGEST_BYTES);
  if (!bodyRead.ok) {
    return jsonResponse({ ok: false, error: "payload too large" }, 413);
  }
  const rawText = bodyRead.text;

  let parsed: unknown;
  try {
    parsed = JSON.parse(rawText);
  } catch {
    return jsonResponse({ ok: false, error: "invalid JSON body" }, 400);
  }

  const validation = validateIngestBatchPayload(parsed);
  if (!validation.ok) {
    return jsonResponse({ ok: false, error: validation.error }, 400);
  }

  const { items } = validation;
  const nowMs = now.getTime();
  const minAcceptedMs = nowMs - RAW_RETENTION_DAYS * 86_400_000;
  const maxAcceptedMs = nowMs + 15 * 60_000;

  let skippedExpired = 0;
  let inBatchDupes = 0;
  const seenIds = new Set<string>();
  const candidates: ValidatedIngestItem[] = [];

  for (const item of items) {
    const bucketMs = Date.parse(item.hourBucket);
    const endMs = Date.parse(item.report.window.end);
    if (bucketMs > maxAcceptedMs || endMs > maxAcceptedMs) {
      return jsonResponse({ ok: false, error: "submission window is in the future" }, 400);
    }
    if (bucketMs < minAcceptedMs) {
      skippedExpired += 1;
      continue;
    }
    if (seenIds.has(item.submission_id)) {
      inBatchDupes += 1;
      continue;
    }
    seenIds.add(item.submission_id);
    candidates.push(item);
  }

  if (candidates.length === 0) {
    return jsonResponse(
      { ok: true, accepted: 0, deduplicated: inBatchDupes, skipped_expired: skippedExpired },
      200,
    );
  }

  const hashCache = new Map<string, string>();
  const textEncoder = new TextEncoder();
  const enrichedCandidates: Array<
    ValidatedIngestItem & {
      contributorHash: string;
      receiptId: string;
      clientHourKey: string;
      ipHourKey: string;
      reportJson: string;
    }
  > = [];

  for (const item of candidates) {
    const dayKey = item.hourBucket.slice(0, 10);
    // Keep contributor hash input format unchanged (`${dayKey}:${salt}:${client_token}`) for consistency across rollups.
    const contributorCacheKey = `${dayKey}\u0000token\u0000${item.client_token}`;
    let contributorHash = hashCache.get(contributorCacheKey);
    if (!contributorHash) {
      contributorHash = await sha256Hex(`${dayKey}:${salt}:${item.client_token}`);
      hashCache.set(contributorCacheKey, contributorHash);
    }
    const ipCacheKey = `${dayKey}\u0000ip\u0000${clientIp}`;
    let ipHash = hashCache.get(ipCacheKey);
    if (!ipHash) {
      ipHash = await sha256Hex(`${dayKey}:${salt}:ip:${clientIp}`);
      hashCache.set(ipCacheKey, ipHash);
    }
    const receiptId = await sha256Hex(`rcpt:${contributorHash}:${item.submission_id}`);
    const reportJson = JSON.stringify(item.report);
    if (textEncoder.encode(reportJson).byteLength > MAX_REPORT_JSON_BYTES) {
      return jsonResponse({ ok: false, error: "serialized submission report too large" }, 413);
    }
    enrichedCandidates.push({
      ...item,
      contributorHash,
      receiptId,
      clientHourKey: `${contributorHash}:${item.hourBucket}`,
      ipHourKey: `ip:${ipHash}:${item.hourBucket}`,
      reportJson,
    });
  }

  const candidateIds = enrichedCandidates.map((c) => c.receiptId);
  const nowIso = toIsoSeconds(now);

  for (let attempt = 0; attempt < MAX_INGEST_CONFLICT_RETRIES; attempt++) {
    const { results: existingRows } = await env.DB.prepare(
      `SELECT receipt_id FROM ingest_receipt WHERE receipt_id IN (${sqlPlaceholders(candidateIds.length)})`,
    )
      .bind(...candidateIds)
      .all<{ receipt_id: string }>();

    const existingIdSet = new Set((existingRows ?? []).map((r) => r.receipt_id));
    const freshItems = enrichedCandidates.filter((c) => !existingIdSet.has(c.receiptId));
    const deduplicatedCount = inBatchDupes + (enrichedCandidates.length - freshItems.length);

    if (freshItems.length === 0) {
      return jsonResponse(
        { ok: true, accepted: 0, deduplicated: deduplicatedCount, skipped_expired: skippedExpired },
        200,
      );
    }

    const pendingQuotaByKey = new Map<string, { count: number; maxLimit: number }>();
    const bumpQuota = (quotaKey: string, maxLimit: number) => {
      const entry = pendingQuotaByKey.get(quotaKey);
      pendingQuotaByKey.set(quotaKey, {
        count: (entry?.count ?? 0) + 1,
        maxLimit,
      });
    };
    for (const item of freshItems) {
      bumpQuota(`c:${item.clientHourKey}`, MAX_SUBMISSIONS_PER_CLIENT_HOUR);
      bumpQuota(item.ipHourKey, MAX_SUBMISSIONS_PER_IP_HOUR);
    }

    const distinctQuotaKeys = Array.from(pendingQuotaByKey.keys());
    const { results: quotaRows } = await env.DB.prepare(
      `SELECT quota_key, submission_count AS cnt
       FROM hourly_quota
       WHERE quota_key IN (${sqlPlaceholders(distinctQuotaKeys.length)})`,
    )
      .bind(...distinctQuotaKeys)
      .all<{ quota_key: string; cnt: number }>();

    const existingQuotaMap = new Map((quotaRows ?? []).map((r) => [r.quota_key, r.cnt]));
    for (const [quotaKey, pending] of pendingQuotaByKey) {
      if ((existingQuotaMap.get(quotaKey) ?? 0) + pending.count > pending.maxLimit) {
        return jsonResponse({ ok: false, error: "hourly submission rate limit exceeded" }, 429);
      }
    }

    // Order receipt uniqueness checks before the atomic hourly quota counters and payload writes
    // so concurrent duplicates surface as UNIQUE conflicts while quota overflows roll back with CHECK.
    const statements: D1PreparedStatement[] = [];
    for (const item of freshItems) {
      statements.push(
        env.DB.prepare(
          "INSERT INTO ingest_receipt (receipt_id, client_hour_key, created_at) VALUES (?1, ?2, ?3)",
        ).bind(item.receiptId, item.clientHourKey, nowIso),
      );
    }
    for (const [quotaKey, pending] of pendingQuotaByKey) {
      statements.push(
        env.DB.prepare(
          `INSERT INTO hourly_quota (quota_key, submission_count, max_limit, updated_at)
           VALUES (?1, ?2, ?3, ?4)
           ON CONFLICT(quota_key) DO UPDATE SET
             submission_count = hourly_quota.submission_count + excluded.submission_count,
             updated_at = excluded.updated_at`,
        ).bind(quotaKey, pending.count, pending.maxLimit, nowIso),
      );
    }
    for (const item of freshItems) {
      statements.push(
        env.DB.prepare(
          "INSERT INTO raw_submission (hour_bucket, contributor_hash, report_json, received_at) VALUES (?1, ?2, ?3, ?4)",
        ).bind(item.hourBucket, item.contributorHash, item.reportJson, nowIso),
      );
    }

    try {
      await env.DB.batch(statements);
      return jsonResponse(
        {
          ok: true,
          accepted: freshItems.length,
          deduplicated: deduplicatedCount,
          skipped_expired: skippedExpired,
        },
        202,
      );
    } catch (err) {
      if (isHourlyQuotaConstraintError(err)) {
        return jsonResponse({ ok: false, error: "hourly submission rate limit exceeded" }, 429);
      }
      if (isReceiptUniqueConstraintError(err) && attempt + 1 < MAX_INGEST_CONFLICT_RETRIES) {
        continue;
      }
      throw err;
    }
  }

  return jsonResponse(
    { ok: false, error: "concurrent ingest conflict retry limit exceeded" },
    409,
  );
}

export async function handleStats(
  request: Request,
  env: Env,
  now = new Date(),
): Promise<Response> {
  const configuredToken = env.TELEMETRY_API_TOKEN?.trim();
  if (!configuredToken) {
    return jsonResponse({ ok: false, error: "api token not configured" }, 503);
  }

  const provided = extractBearerToken(request);
  if (!provided || !timingSafeEqual(provided, configuredToken)) {
    return jsonResponse({ ok: false, error: "unauthorized" }, 401);
  }

  const url = new URL(request.url);
  const after = url.searchParams.get("after") ?? "";
  const updatedAfter = url.searchParams.get("updated_after") ?? "";
  const cutoffIso = retentionCutoffIso(now);
  const rawLimit = Number(url.searchParams.get("limit") ?? "168");
  const limit = Number.isInteger(rawLimit) ? Math.min(Math.max(rawLimit, 1), 720) : 168;

  const { results } = await env.DB.prepare(
    `SELECT hour_bucket, report_json, submission_count, aggregated_at
     FROM hourly_rollup
     WHERE (
       hour_bucket > ?1
       OR (?2 != '' AND hour_bucket >= ?3 AND aggregated_at > ?2)
     )
     ORDER BY hour_bucket ASC
     LIMIT ?4`,
  )
    .bind(after, updatedAfter, cutoffIso, limit)
    .all<{
      hour_bucket: string;
      report_json: string;
      submission_count: number;
      aggregated_at: string;
    }>();

  const hours = (results ?? []).map((row) => ({
    hour_bucket: row.hour_bucket,
    submission_count: row.submission_count,
    aggregated_at: row.aggregated_at,
    report: JSON.parse(row.report_json) as StatsReport,
  }));

  return jsonResponse({ ok: true, hours }, 200);
}

function retentionCutoffIso(now: Date): string {
  return toIsoSeconds(new Date(now.getTime() - RAW_RETENTION_DAYS * 86_400_000));
}

const PRUNE_TARGETS = [
  { table: "raw_submission", column: "hour_bucket" },
  { table: "ingest_receipt", column: "created_at" },
  { table: "hourly_quota", column: "updated_at" },
] as const;

export async function rollupFinalizedHours(db: D1Database, now = new Date()): Promise<number> {
  const currentUtcHour = toUtcHourBucket(now.toISOString());
  if (!currentUtcHour) return 0;
  const aggregatedAt = toIsoSeconds(now);
  const cutoffIso = retentionCutoffIso(now);
  let queriesUsed = 0;

  // Re-rollup closed hours within the 3-day raw retention window when new or updated.
  // Checking `COUNT(r.id) != MAX(h.submission_count)` ensures late submissions arriving in the
  // exact same second as `h.aggregated_at` are never missed.
  // Cap hours per run and share the D1 query budget with pruning to stay under the Free plan limit (50).
  queriesUsed += 1;
  const { results: pendingHours } = await db
    .prepare(
      `SELECT r.hour_bucket, MAX(h.aggregated_at) AS prev_aggregated_at
       FROM raw_submission r
       LEFT JOIN hourly_rollup h ON r.hour_bucket = h.hour_bucket
       WHERE r.hour_bucket < ?1
         AND r.hour_bucket >= ?2
       GROUP BY r.hour_bucket
       HAVING MAX(h.hour_bucket) IS NULL
           OR COUNT(r.id) != MAX(h.submission_count)
           OR MAX(r.received_at) > MAX(h.aggregated_at)
       ORDER BY r.hour_bucket ASC
       LIMIT ?3`,
    )
    .bind(currentUtcHour, cutoffIso, MAX_ROLLUP_HOURS_PER_RUN)
    .all<{ hour_bucket: string; prev_aggregated_at: string | null }>();

  const upsertStmts: D1PreparedStatement[] = [];
  for (const { hour_bucket: hourBucket, prev_aggregated_at: prevAggregatedAt } of pendingHours ?? []) {
    queriesUsed += 1;
    const { results: submissions } = await db
      .prepare(
        `SELECT contributor_hash, report_json
         FROM raw_submission
         WHERE hour_bucket = ?1
         ORDER BY received_at ASC, id ASC`,
      )
      .bind(hourBucket)
      .all<{ contributor_hash: string; report_json: string }>();

    const rows = submissions ?? [];
    if (rows.length === 0) continue;

    const perContributorInput: Array<{ contributorHash: string; report: StatsReport }> = [];
    for (const row of rows) {
      try {
        const parsed = JSON.parse(row.report_json) as StatsReport;
        if (parsed && parsed.schema_version === STATS_SCHEMA_VERSION) {
          perContributorInput.push({
            contributorHash: row.contributor_hash,
            report: parsed,
          });
        }
      } catch {
        // Ignore malformed legacy rows if any ever appear.
      }
    }
    if (perContributorInput.length === 0) continue;

    const merged = aggregateHourByContributors(perContributorInput, {
      start: hourBucket,
      end: addHoursIso(hourBucket, 1),
      duration_secs: 3600,
    });

    // Ensure `aggregated_at` strictly advances past any prior rollup timestamp so downstream
    // incremental `updated_after` queries never miss a same-second re-rollup.
    let nextAggregatedAt = aggregatedAt;
    if (prevAggregatedAt && nextAggregatedAt <= prevAggregatedAt) {
      const bumpedMs = Date.parse(prevAggregatedAt) + 1000;
      if (Number.isFinite(bumpedMs)) {
        nextAggregatedAt = toIsoSeconds(new Date(bumpedMs));
      }
    }

    upsertStmts.push(
      db
        .prepare(
          `INSERT INTO hourly_rollup (hour_bucket, report_json, submission_count, aggregated_at)
           VALUES (?1, ?2, ?3, ?4)
           ON CONFLICT(hour_bucket) DO UPDATE SET
             report_json = excluded.report_json,
             submission_count = excluded.submission_count,
             aggregated_at = excluded.aggregated_at
           WHERE excluded.submission_count >= hourly_rollup.submission_count`,
        )
        .bind(hourBucket, JSON.stringify(merged), rows.length, nextAggregatedAt),
    );
  }

  if (upsertStmts.length > 0) {
    queriesUsed += 1;
    await db.batch(upsertStmts);
  }

  const remainingPruneQueries = Math.max(0, MAX_CRON_D1_QUERIES - queriesUsed);
  if (remainingPruneQueries > 0) {
    await pruneExpiredRawRows(db, now, remainingPruneQueries);
  }

  return upsertStmts.length;
}

export async function pruneExpiredRawRows(
  db: D1Database,
  now: Date,
  maxBatches = RAW_PRUNE_DEFAULT_BATCHES,
  batchSize = RAW_PRUNE_BATCH_SIZE,
): Promise<number> {
  if (maxBatches <= 0 || batchSize <= 0) return 0;
  // Keep a 1-hour grace margin beyond RAW_RETENTION_DAYS so boundary hours being rolled up cannot be deleted early.
  const cutoffIso = toIsoSeconds(new Date(now.getTime() - (RAW_RETENTION_DAYS * 24 + 1) * 3_600_000));
  let deleted = 0;

  for (let i = 0; i < maxBatches; i++) {
    // Batch all three table deletes into one D1 query so pruning stays within the shared Cron budget.
    const stmts = PRUNE_TARGETS.map(({ table, column }) =>
      db
        .prepare(
          `DELETE FROM ${table}
           WHERE rowid IN (
             SELECT rowid FROM ${table} WHERE ${column} < ?1 LIMIT ?2
           )`,
        )
        .bind(cutoffIso, batchSize),
    );
    const results = await db.batch(stmts);
    let anyFullBatch = false;
    for (const res of results) {
      const changes = typeof res.meta?.changes === "number" ? res.meta.changes : 0;
      deleted += changes;
      if (changes >= batchSize) {
        anyFullBatch = true;
      }
    }
    if (!anyFullBatch) break;
  }

  return deleted;
}

export async function handleScheduled(env: Env, now = new Date()): Promise<number> {
  return rollupFinalizedHours(env.DB, now);
}

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);
    if (request.method === "GET" && url.pathname === "/api/telemetry/health") {
      return jsonResponse({ ok: true, service: "antigravity-status-telemetry" });
    }
    if (request.method === "POST" && url.pathname === "/api/telemetry/ingest") {
      return handleIngest(request, env, new Date());
    }
    if (request.method === "GET" && url.pathname === "/api/telemetry/stats") {
      return handleStats(request, env, new Date());
    }
    return jsonResponse({ ok: false, error: "not found" }, 404);
  },
  async scheduled(controller: ScheduledController, env: Env): Promise<void> {
    const now = new Date(controller.scheduledTime || Date.now());
    await handleScheduled(env, now);
  },
};
