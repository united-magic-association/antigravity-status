import { Database } from "bun:sqlite";
import { describe, expect, test } from "bun:test";
import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";

import worker, {
  Env,
  handleIngest,
  handleScheduled,
  handleStats,
  pruneExpiredRawRows,
} from "../src/index";
import { aggregateHourByContributors } from "../src/merge";
import {
  QUOTA_COST_HISTOGRAM_BUCKETS,
  STATS_SCHEMA_VERSION,
  StatsReport,
  TPS_HISTOGRAM_BUCKETS,
} from "../src/schema";

/** Minimal D1Database shim backed by bun:sqlite for fast local integration testing. */
function createTestD1(): D1Database {
  const sqlite = new Database(":memory:");
  const migrationsDir = join(import.meta.dir, "..", "migrations");
  const migrationFiles = readdirSync(migrationsDir)
    .filter((f) => f.endsWith(".sql"))
    .sort();
  for (const file of migrationFiles) {
    sqlite.exec(readFileSync(join(migrationsDir, file), "utf8"));
  }

  const makeStatement = (query: string, bound: unknown[] = []) => ({
    bind(...values: unknown[]) {
      return makeStatement(query, values);
    },
    async first<T = Record<string, unknown>>() {
      const row = sqlite.query(query).get(...(bound as never[]));
      return (row as T | undefined) ?? null;
    },
    async all<T = Record<string, unknown>>() {
      const results = sqlite.query(query).all(...(bound as never[])) as T[];
      return { results, success: true, meta: {} } as unknown as D1Result<T>;
    },
    async run() {
      const info = sqlite.query(query).run(...(bound as never[]));
      return { success: true, meta: { changes: info.changes } } as unknown as D1Result;
    },
    _execRaw(): D1Result {
      const info = sqlite.query(query).run(...(bound as never[]));
      return { success: true, meta: { changes: info.changes } } as unknown as D1Result;
    },
  });

  return {
    prepare(query: string) {
      return makeStatement(query) as unknown as D1PreparedStatement;
    },
    async batch(statements: D1PreparedStatement[]) {
      const results: D1Result[] = [];
      const tx = sqlite.transaction(() => {
        for (const stmt of statements) {
          results.push((stmt as unknown as { _execRaw(): D1Result })._execRaw());
        }
      });
      tx();
      return results;
    },
  } as unknown as D1Database;
}

function emptyTpsHist(): number[] {
  return new Array<number>(TPS_HISTOGRAM_BUCKETS).fill(0);
}

function sampleReport(start: string, end: string, events = 4): StatsReport {
  const durationSecs = Math.max(1, Math.round((Date.parse(end) - Date.parse(start)) / 1000));
  const histogram = emptyTpsHist();
  histogram[3] = 2;
  return {
    schema_version: STATS_SCHEMA_VERSION,
    scope: "local",
    window: { start, end, duration_secs: durationSecs },
    contributors: 1,
    usage_by_plan_model: [
      {
        plan_tier: "Google AI Ultra",
        model: "Claude Opus 4.6",
        effort: "high",
        events,
        input_tokens: 1000,
        cache_read_tokens: 2000,
        cache_write_tokens: 0,
        output_tokens: 500,
        reference_cost_usd: 0.0185,
      },
    ],
    tps_by_model_effort: [
      {
        model: "Claude Opus 4.6",
        effort: "high",
        sample_count: 2,
        output_tokens: 500,
        elapsed_ms: 10000,
        histogram,
      },
    ],
    quota_by_plan_bucket_model: [
      {
        plan_tier: "Google AI Ultra",
        bucket: "3p-5h",
        attributed_model: "Claude Opus 4.6",
        attributed_effort: "high",
        slices: 1,
        delta_quota_fraction: 0.01,
        input_tokens: 1000,
        cache_read_tokens: 2000,
        cache_write_tokens: 0,
        output_tokens: 500,
        reference_cost_usd: 0.0185,
      },
    ],
  };
}

describe("telemetry worker ingest, rollup, and stats", () => {
  test("deduplicates submissions, rolls up closed hours, and enforces TELEMETRY_API_TOKEN", async () => {
    const db = createTestD1();
    const env: Env = {
      DB: db,
      ANONYMIZATION_SALT: "test-salt",
      TELEMETRY_API_TOKEN: "secret-api-token",
    };

    const ingestTime = new Date("2026-11-14T15:30:00Z");
    const reportA = sampleReport("2026-11-14T15:00:00Z", "2026-11-14T15:30:00Z", 4);
    const reportB = sampleReport("2026-11-14T15:00:00Z", "2026-11-14T15:30:00Z", 6);

    const post = (body: unknown) =>
      handleIngest(
        new Request("https://worker.test/api/telemetry/ingest", {
          method: "POST",
          body: JSON.stringify(body),
          headers: { "content-type": "application/json" },
        }),
        env,
        ingestTime,
      );

    // User 1 submits once (202), then retries same submission_id (200 deduplicated).
    const r1 = await post({
      client_token: "user-a",
      submissions: [{ submission_id: "sub-1", report: reportA }],
    });
    expect(r1.status).toBe(202);
    const r1Dup = await post({
      client_token: "user-a",
      submissions: [{ submission_id: "sub-1", report: reportA }],
    });
    expect(r1Dup.status).toBe(200);
    expect(await r1Dup.json()).toEqual({
      ok: true,
      accepted: 0,
      deduplicated: 1,
      skipped_expired: 0,
    });

    // User 2 submits into same hour.
    const r2 = await post({
      client_token: "user-b",
      submissions: [{ submission_id: "sub-2", report: reportB }],
    });
    expect(r2.status).toBe(202);

    // Rejects poisoned TPS > MAX_VALID_TPS.
    const bad = sampleReport("2026-11-14T15:00:00Z", "2026-11-14T15:30:00Z", 1);
    bad.tps_by_model_effort[0].output_tokens = 3_000_000;
    bad.tps_by_model_effort[0].elapsed_ms = 500;
    const rBad = await post({
      client_token: "user-c",
      submissions: [{ submission_id: "sub-bad", report: bad }],
    });
    expect(rBad.status).toBe(400);

    // While still inside 15:xx, scheduled rollup does nothing and /api/telemetry/stats returns 0 closed hours.
    expect(await handleScheduled(env, new Date("2026-11-14T15:59:00Z"))).toBe(0);
    const stillOpen = await handleStats(
      new Request("https://worker.test/api/telemetry/stats?granularity=hourly", {
        headers: { authorization: "Bearer secret-api-token" },
      }),
      env,
      new Date("2026-11-14T15:59:00Z"),
    );
    expect(stillOpen.status).toBe(200);
    expect(((await stillOpen.json()) as { hours: unknown[] }).hours).toHaveLength(0);

    // Stats endpoint rejects missing or wrong token with 401.
    const unauth = await handleStats(new Request("https://worker.test/api/telemetry/stats"), env);
    expect(unauth.status).toBe(401);

    const wrongAuth = await handleStats(
      new Request("https://worker.test/api/telemetry/stats", {
        headers: { authorization: "Bearer wrong" },
      }),
      env,
    );
    expect(wrongAuth.status).toBe(401);

    // Once 16:00 passes, /api/telemetry/stats stays pure read-only until scheduled cron runs.
    const beforeScheduled = await handleStats(
      new Request("https://worker.test/api/telemetry/stats?granularity=hourly", {
        headers: { authorization: "Bearer secret-api-token" },
      }),
      env,
      new Date("2026-11-14T16:08:00Z"),
    );
    expect(beforeScheduled.status).toBe(200);
    expect(((await beforeScheduled.json()) as { hours: unknown[] }).hours).toHaveLength(0);

    // Worker scheduled() cron rolls up closed hours; /stats then serves the closed 15:00 hour read-only.
    await worker.scheduled(
      { scheduledTime: Date.parse("2026-11-14T16:10:00Z"), cron: "10 * * * *" } as ScheduledController,
      env,
    );
    const okHourly = await handleStats(
      new Request("https://worker.test/api/telemetry/stats?granularity=hourly", {
        headers: { authorization: "Bearer secret-api-token" },
      }),
      env,
      new Date("2026-11-14T16:15:00Z"),
    );
    expect(okHourly.status).toBe(200);
    const exported = (await okHourly.json()) as {
      ok: boolean;
      hours: Array<{ hour_bucket: string; submission_count: number; report: StatsReport }>;
    };
    expect(exported.hours).toHaveLength(1);
    expect(exported.hours[0].hour_bucket).toBe("2026-11-14T15:00:00Z");
    expect(exported.hours[0].submission_count).toBe(2);
    expect(exported.hours[0].report.scope).toBe("aggregated");
    expect(exported.hours[0].report.contributors).toBe(2);
    expect(exported.hours[0].report.usage_by_plan_model[0].events).toBe(10);
  });

  test("re-rolls and exports updated hours within 3 days via updated_after while ignoring >3d history", async () => {
    const db = createTestD1();
    const env: Env = {
      DB: db,
      ANONYMIZATION_SALT: "test-salt",
      TELEMETRY_API_TOKEN: "secret-api-token",
    };

    // First contributor uploads closed 15:00Z hour before Sync #1.
    const reportA = sampleReport("2026-11-14T15:00:00Z", "2026-11-14T16:00:00Z", 4);
    const resFirst = await handleIngest(
      new Request("https://worker.test/api/telemetry/ingest", {
        method: "POST",
        body: JSON.stringify({
          client_token: "user-a",
          submissions: [{ submission_id: "sub-user-a", report: reportA }],
        }),
        headers: { "content-type": "application/json" },
      }),
      env,
      new Date("2026-11-14T16:05:00Z"),
    );
    expect(resFirst.status).toBe(202);

    // Scheduled Cron #1 at 16:08:00Z rolls up 15:00Z with 4 events; /stats serves it read-only.
    expect(await handleScheduled(env, new Date("2026-11-14T16:08:00Z"))).toBe(1);
    const sync1 = await handleStats(
      new Request("https://worker.test/api/telemetry/stats?granularity=hourly", {
        headers: { authorization: "Bearer secret-api-token" },
      }),
      env,
      new Date("2026-11-14T16:08:00Z"),
    );
    expect(sync1.status).toBe(200);
    const body1 = (await sync1.json()) as {
      hours: Array<{ hour_bucket: string; aggregated_at: string; report: StatsReport }>;
    };
    expect(body1.hours).toHaveLength(1);
    expect(body1.hours[0].aggregated_at).toBe("2026-11-14T16:08:00Z");
    expect(body1.hours[0].report.usage_by_plan_model[0].events).toBe(4);

    // Late contributor submission arrives in the EXACT SAME SECOND (16:08:00Z) as Cron #1's rollup.
    const reportB = sampleReport("2026-11-14T15:00:00Z", "2026-11-14T16:00:00Z", 3);
    const resLate = await handleIngest(
      new Request("https://worker.test/api/telemetry/ingest", {
        method: "POST",
        body: JSON.stringify({
          client_token: "user-b",
          submissions: [{ submission_id: "sub-user-b", report: reportB }],
        }),
        headers: { "content-type": "application/json" },
      }),
      env,
      new Date("2026-11-14T16:08:00Z"),
    );
    expect(resLate.status).toBe(202);

    // Seed a >3d closed hour directly in hourly_rollup with newer aggregated_at.
    const oldBucket = "2026-11-10T10:00:00Z";
    const oldReport = sampleReport(oldBucket, "2026-11-10T11:00:00Z", 99);
    await db
      .prepare(
        `INSERT INTO hourly_rollup (hour_bucket, report_json, submission_count, aggregated_at)
         VALUES (?1, ?2, 1, ?3)`,
      )
      .bind(oldBucket, JSON.stringify(oldReport), "2026-11-14T16:59:00Z")
      .run();

    // Cron #2 at 17:08:00Z re-rolls 15:00Z (7 events); Sync #2 gets it via after & updated_after and ignores >3d hour.
    expect(await handleScheduled(env, new Date("2026-11-14T17:08:00Z"))).toBe(1);
    const sync2 = await handleStats(
      new Request(
        "https://worker.test/api/telemetry/stats?granularity=hourly&after=2026-11-14T15:00:00Z&updated_after=2026-11-14T16:08:00Z",
        {
          headers: { authorization: "Bearer secret-api-token" },
        },
      ),
      env,
      new Date("2026-11-14T17:08:00Z"),
    );
    expect(sync2.status).toBe(200);
    const body2 = (await sync2.json()) as {
      hours: Array<{ hour_bucket: string; submission_count: number; aggregated_at: string; report: StatsReport }>;
    };
    expect(body2.hours).toHaveLength(1);
    expect(body2.hours[0].hour_bucket).toBe("2026-11-14T15:00:00Z");
    expect(body2.hours[0].submission_count).toBe(2);
    expect(body2.hours[0].aggregated_at).toBe("2026-11-14T17:08:00Z");
    expect(body2.hours[0].report.usage_by_plan_model[0].events).toBe(7);
  });

  test("prunes >3d raw rows in bounded LIMIT batches while preserving recent rows", async () => {
    const db = createTestD1();
    const now = new Date("2026-11-14T17:08:00Z");
    const oldBucket = "2026-11-10T10:00:00Z";
    const freshBucket = "2026-11-14T15:00:00Z";
    const sampleJson = JSON.stringify(sampleReport(oldBucket, "2026-11-10T11:00:00Z", 1));

    for (let i = 0; i < 5; i++) {
      await db
        .prepare(
          "INSERT INTO raw_submission (hour_bucket, contributor_hash, report_json, received_at) VALUES (?1, ?2, ?3, ?4)",
        )
        .bind(oldBucket, `h-${i}`, sampleJson, "2026-11-10T10:15:00Z")
        .run();
      await db
        .prepare("INSERT INTO ingest_receipt (receipt_id, client_hour_key, created_at) VALUES (?1, ?2, ?3)")
        .bind(`old-r-${i}`, `h-${i}:${oldBucket}`, "2026-11-10T10:15:00Z")
        .run();
    }

    await db
      .prepare(
        "INSERT INTO raw_submission (hour_bucket, contributor_hash, report_json, received_at) VALUES (?1, ?2, ?3, ?4)",
      )
      .bind(freshBucket, "fresh-h", sampleJson, "2026-11-14T15:15:00Z")
      .run();
    await db
      .prepare("INSERT INTO ingest_receipt (receipt_id, client_hour_key, created_at) VALUES (?1, ?2, ?3)")
      .bind("fresh-r", `fresh-h:${freshBucket}`, "2026-11-14T15:15:00Z")
      .run();

    // First bounded pass (1 batch of 2 rows per table) removes 4 rows total without locking.
    const pass1 = await pruneExpiredRawRows(db, now, 1, 2);
    expect(pass1).toBe(4);
    const remainingOldAfterPass1 = await db
      .prepare("SELECT COUNT(*) AS cnt FROM raw_submission WHERE hour_bucket = ?1")
      .bind(oldBucket)
      .first<{ cnt: number }>();
    expect(remainingOldAfterPass1?.cnt).toBe(3);

    // Subsequent multi-batch pass drains the remaining 3+3 expired rows while keeping fresh rows intact.
    const pass2 = await pruneExpiredRawRows(db, now, 10, 2);
    expect(pass2).toBe(6);

    const rawCount = await db
      .prepare("SELECT COUNT(*) AS cnt FROM raw_submission")
      .first<{ cnt: number }>();
    const receiptCount = await db
      .prepare("SELECT COUNT(*) AS cnt FROM ingest_receipt")
      .first<{ cnt: number }>();
    expect(rawCount?.cnt).toBe(1);
    expect(receiptCount?.cnt).toBe(1);
  });

  test("handles batch ingest with deduplication, expired skip, and per-hour rate limiting", async () => {
    const db = createTestD1();
    const env: Env = {
      DB: db,
      ANONYMIZATION_SALT: "test-salt",
      TELEMETRY_API_TOKEN: "secret-api-token",
    };
    const now = new Date("2026-11-14T16:15:00Z");

    const postBatch = (body: unknown) =>
      handleIngest(
        new Request("https://worker.test/api/telemetry/ingest", {
          method: "POST",
          body: JSON.stringify(body),
          headers: { "content-type": "application/json" },
        }),
        env,
        now,
      );

    // Batch with: 1 expired (>3d), 2 valid closed hours, and 1 duplicate submission_id inside the batch.
    const res1 = await postBatch({
      client_token: "batch-user",
      submissions: [
        {
          submission_id: "sub-expired",
          report: sampleReport("2026-11-10T10:00:00Z", "2026-11-10T11:00:00Z", 2),
        },
        {
          submission_id: "sub-h14",
          report: sampleReport("2026-11-14T14:00:00Z", "2026-11-14T15:00:00Z", 3),
        },
        {
          submission_id: "sub-h14",
          report: sampleReport("2026-11-14T14:00:00Z", "2026-11-14T15:00:00Z", 3),
        },
        {
          submission_id: "sub-h15",
          report: sampleReport("2026-11-14T15:00:00Z", "2026-11-14T16:00:00Z", 5),
        },
      ],
    });
    expect(res1.status).toBe(202);
    expect(await res1.json()).toEqual({
      ok: true,
      accepted: 2,
      deduplicated: 1,
      skipped_expired: 1,
    });

    // Re-sending the same batch returns 200 with full deduplication.
    const resDup = await postBatch({
      client_token: "batch-user",
      submissions: [
        {
          submission_id: "sub-h14",
          report: sampleReport("2026-11-14T14:00:00Z", "2026-11-14T15:00:00Z", 3),
        },
        {
          submission_id: "sub-h15",
          report: sampleReport("2026-11-14T15:00:00Z", "2026-11-14T16:00:00Z", 5),
        },
      ],
    });
    expect(resDup.status).toBe(200);
    expect(await resDup.json()).toEqual({
      ok: true,
      accepted: 0,
      deduplicated: 2,
      skipped_expired: 0,
    });

    // Exceeding MAX_SUBMISSIONS_PER_CLIENT_HOUR (6) within one batch rejects with 429.
    const spamSubmissions = Array.from({ length: 7 }, (_, i) => ({
      submission_id: `spam-${i}`,
      report: sampleReport("2026-11-14T14:00:00Z", "2026-11-14T15:00:00Z", 1),
    }));
    const res429 = await postBatch({
      client_token: "spammer",
      submissions: spamSubmissions,
    });
    expect(res429.status).toBe(429);

    // High cache_read (2B) and cross-reset quota delta (1.6) pass relaxed sanity bounds.
    const heavyReport = sampleReport("2026-11-14T14:00:00Z", "2026-11-14T15:00:00Z", 50);
    heavyReport.usage_by_plan_model[0].cache_read_tokens = 2_000_000_000;
    heavyReport.quota_by_plan_bucket_model[0].delta_quota_fraction = 1.6;
    const resHeavy = await postBatch({
      client_token: "heavy-user",
      submissions: [{ submission_id: "heavy-ok", report: heavyReport }],
    });
    expect(resHeavy.status).toBe(202);
  });

  test("caps single contributor TPS and quota weights without dropping legitimate cohort contributors", () => {
    const window = {
      start: "2026-11-14T15:00:00Z",
      end: "2026-11-14T16:00:00Z",
      duration_secs: 3600,
    };

    const makeContrib = (events: number, tpsSamples: number, outTok: number, elapsedMs: number, histBucket: number, quotaCost: number) => {
      const r = sampleReport(window.start, window.end, events);
      const hist = emptyTpsHist();
      hist[histBucket] = tpsSamples;
      r.tps_by_model_effort[0].sample_count = tpsSamples;
      r.tps_by_model_effort[0].output_tokens = outTok;
      r.tps_by_model_effort[0].elapsed_ms = elapsedMs;
      r.tps_by_model_effort[0].histogram = hist;
      r.quota_by_plan_bucket_model[0].delta_quota_fraction = 0.01;
      r.quota_by_plan_bucket_model[0].reference_cost_usd = quotaCost;
      return r;
    };

    const cohort = [
      { contributorHash: "u1", report: makeContrib(8, 10, 5_000, 100_000, 3, 0.04) },
      { contributorHash: "u2", report: makeContrib(10, 10, 5_000, 100_000, 3, 0.05) },
      { contributorHash: "u3", report: makeContrib(12, 10, 5_500, 100_000, 3, 0.05) },
      { contributorHash: "u4", report: makeContrib(14, 10, 6_000, 100_000, 3, 0.06) },
      { contributorHash: "u5", report: makeContrib(16, 10, 6_500, 100_000, 3, 0.07) },
    ];
    const rolled = aggregateHourByContributors(cohort, window);
    expect(rolled.contributors).toBe(5);
    expect(rolled.usage_by_plan_model[0].events).toBe(8 + 10 + 12 + 14 + 16);
    expect(rolled.tps_by_model_effort[0].sample_count).toBe(50);

    // Cohort >= 20 keeps all contributors while capping an extreme sender's TPS/quota sample weight at 200.
    const topBucket = TPS_HISTOGRAM_BUCKETS - 1;
    const twentyCohort = [
      { contributorHash: "low-outlier", report: makeContrib(1, 10, 500, 100_000, 0, 0.01) },
      ...Array.from({ length: 18 }, (_, i) => ({
        contributorHash: `norm-${i}`,
        report: makeContrib(10, 10, 5_000, 100_000, 3, 0.05),
      })),
      { contributorHash: "heavy", report: makeContrib(20, 20_000, 4_000_000, 200_000, topBucket, 0.2) },
    ];
    const twentyRolled = aggregateHourByContributors(twentyCohort, window);
    expect(twentyRolled.contributors).toBe(20);
    expect(twentyRolled.usage_by_plan_model[0].events).toBe(1 + 18 * 10 + 20);
    expect(twentyRolled.tps_by_model_effort[0].sample_count).toBe(10 + 18 * 10 + 200);
    expect(twentyRolled.tps_by_model_effort[0].histogram[topBucket]).toBe(200);
    expect(twentyRolled.quota_by_plan_bucket_model[0].reference_cost_usd).toBeCloseTo(
      0.01 + 18 * 0.05 + 0.2,
      6,
    );

    // Single contributor submitting 1000 TPS samples and 1000 quota slices is capped to 200 each.
    const heavySolo = makeContrib(20, 1000, 50_000, 1_000_000, 3, 10.0);
    heavySolo.quota_by_plan_bucket_model[0].slices = 1000;
    const smallGroup = aggregateHourByContributors(
      [{ contributorHash: "solo", report: heavySolo }],
      window,
    );
    expect(smallGroup.tps_by_model_effort[0].sample_count).toBe(200);
    expect(smallGroup.tps_by_model_effort[0].histogram[3]).toBe(200);
    expect(smallGroup.quota_by_plan_bucket_model[0].slices).toBe(200);
    expect(smallGroup.quota_by_plan_bucket_model[0].reference_cost_usd).toBeCloseTo(2.0, 6);
  });

  test("enforces edge per-IP rate limiting before reading body or touching D1", async () => {
    const db = createTestD1();
    const seenKeys: string[] = [];
    let calls = 0;
    const limiter: RateLimit = {
      async limit(opts) {
        seenKeys.push(opts.key);
        calls += 1;
        return { success: calls <= 2 };
      },
    };
    const env: Env = {
      DB: db,
      INGEST_RATE_LIMITER: limiter,
      ANONYMIZATION_SALT: "test-salt",
      TELEMETRY_API_TOKEN: "secret-api-token",
    };

    const sendFromIp = (id: string, ip: string) =>
      handleIngest(
        new Request("https://worker.test/api/telemetry/ingest", {
          method: "POST",
          body: JSON.stringify({
            client_token: "ip-user",
            submissions: [
              {
                submission_id: id,
                report: sampleReport("2026-11-14T15:00:00Z", "2026-11-14T16:00:00Z", 1),
              },
            ],
          }),
          headers: {
            "content-type": "application/json",
            "cf-connecting-ip": ip,
          },
        }),
        env,
        new Date("2026-11-14T16:57:05Z"),
      );

    expect((await sendFromIp("ip-1", "203.0.113.9")).status).toBe(202);
    expect((await sendFromIp("ip-2", "203.0.113.9")).status).toBe(202);
    const blocked = await sendFromIp("ip-3", "203.0.113.9");
    expect(blocked.status).toBe(429);
    expect(seenKeys).toEqual(["ingest:203.0.113.9", "ingest:203.0.113.9", "ingest:203.0.113.9"]);
  });

  test("rejects zero-length, duration-mismatched, cross-hour, and future-end submission windows", async () => {
    const db = createTestD1();
    const env: Env = {
      DB: db,
      ANONYMIZATION_SALT: "test-salt",
      TELEMETRY_API_TOKEN: "secret-api-token",
    };
    const now = new Date("2026-11-14T15:30:00Z");

    const submit = (id: string, report: StatsReport, at = now) =>
      handleIngest(
        new Request("https://worker.test/api/telemetry/ingest", {
          method: "POST",
          body: JSON.stringify({
            client_token: "window-tester",
            submissions: [{ submission_id: id, report }],
          }),
          headers: { "content-type": "application/json" },
        }),
        env,
        at,
      );

    // Zero-length window (start === end).
    const zeroLen = sampleReport("2026-11-14T15:00:00Z", "2026-11-14T15:00:00Z", 1);
    expect((await submit("w-zero", zeroLen)).status).toBe(400);

    // Mismatched duration_secs (60s elapsed vs claimed 3600s).
    const mismatch = sampleReport("2026-11-14T15:00:00Z", "2026-11-14T15:01:00Z", 1);
    mismatch.window.duration_secs = 3600;
    expect((await submit("w-mismatch", mismatch)).status).toBe(400);

    // Cross-hour window spanning 15:xx into 16:xx.
    const crossHour = sampleReport("2026-11-14T15:59:00Z", "2026-11-14T16:05:00Z", 1);
    expect((await submit("w-cross", crossHour, new Date("2026-11-14T16:10:00Z"))).status).toBe(400);

    // Far-future end timestamp (even with valid start in current hour).
    const farFuture = sampleReport("2026-11-14T15:59:00Z", "2099-01-01T00:00:00Z", 1);
    farFuture.window.duration_secs = 60;
    expect((await submit("w-2099", farFuture)).status).toBe(400);

    // Same-hour window whose end is > 15m in the future relative to ingest clock.
    const futureEnd = sampleReport("2026-11-14T15:59:00Z", "2026-11-14T16:00:00Z", 1);
    expect((await submit("w-future-end", futureEnd, new Date("2026-11-14T15:30:00Z"))).status).toBe(400);

    // Full hour exact boundary [15:00:00Z, 16:00:00Z] is accepted once clock passes 15:45:00Z.
    expect((await submit("w-full-hour", futureEnd, new Date("2026-11-14T16:00:05Z"))).status).toBe(202);
  });

  test("retries concurrent duplicate ingest conflict into idempotent deduplication", async () => {
    const baseDb = createTestD1();
    const ingestAt = new Date("2026-11-14T15:30:00Z");
    const report = sampleReport("2026-11-14T15:00:00Z", "2026-11-14T15:30:00Z", 4);

    let onBeforeFirstBatch: (() => Promise<void>) | null = null;
    const racedDb = {
      ...baseDb,
      prepare: baseDb.prepare.bind(baseDb),
      async batch(statements: D1PreparedStatement[]) {
        if (onBeforeFirstBatch) {
          const hook = onBeforeFirstBatch;
          onBeforeFirstBatch = null;
          await hook();
        }
        return baseDb.batch(statements);
      },
    } as unknown as D1Database;

    const envDirect: Env = {
      DB: baseDb,
      ANONYMIZATION_SALT: "test-salt",
      TELEMETRY_API_TOKEN: "secret-api-token",
    };
    const envRaced: Env = {
      ...envDirect,
      DB: racedDb,
    };

    const postIngest = (targetEnv: Env, submissions: Array<{ submission_id: string; report: StatsReport }>) =>
      handleIngest(
        new Request("https://worker.test/api/telemetry/ingest", {
          method: "POST",
          body: JSON.stringify({
            client_token: "race-client",
            submissions,
          }),
          headers: { "content-type": "application/json" },
        }),
        targetEnv,
        ingestAt,
      );

    // 1. Exact duplicate race: inner request commits race-sub-1 right before outer batch executes.
    let innerExactRes: Response | null = null;
    onBeforeFirstBatch = async () => {
      innerExactRes = await postIngest(envDirect, [{ submission_id: "race-sub-1", report }]);
    };

    const outerExactRes = await postIngest(envRaced, [{ submission_id: "race-sub-1", report }]);
    expect(innerExactRes).not.toBeNull();
    expect(innerExactRes!.status).toBe(202);
    expect(await innerExactRes!.json()).toMatchObject({ ok: true, accepted: 1, deduplicated: 0 });

    expect(outerExactRes.status).toBe(200);
    expect(await outerExactRes.json()).toEqual({
      ok: true,
      accepted: 0,
      deduplicated: 1,
      skipped_expired: 0,
    });

    // 2. Partial batch race: inner commits race-sub-2 while outer sends [race-sub-2, race-sub-3].
    let innerPartialRes: Response | null = null;
    onBeforeFirstBatch = async () => {
      innerPartialRes = await postIngest(envDirect, [{ submission_id: "race-sub-2", report }]);
    };

    const outerPartialRes = await postIngest(envRaced, [
      { submission_id: "race-sub-2", report },
      { submission_id: "race-sub-3", report },
    ]);
    expect(innerPartialRes).not.toBeNull();
    expect(innerPartialRes!.status).toBe(202);

    expect(outerPartialRes.status).toBe(202);
    expect(await outerPartialRes.json()).toEqual({
      ok: true,
      accepted: 1,
      deduplicated: 1,
      skipped_expired: 0,
    });

    const rawCount = await baseDb.prepare("SELECT COUNT(*) AS cnt FROM raw_submission").first<{ cnt: number }>();
    expect(rawCount?.cnt).toBe(3);
  });

  test("prevents concurrent hourly rate limit TOCTOU bypass via atomic quota CHECK and validates TPS bounds", async () => {
    const baseDb = createTestD1();
    const ingestAt = new Date("2026-11-14T15:30:00Z");
    const report = sampleReport("2026-11-14T15:00:00Z", "2026-11-14T15:30:00Z", 4);

    let onBeforeFirstBatch: (() => Promise<void>) | null = null;
    const racedDb = {
      ...baseDb,
      prepare: baseDb.prepare.bind(baseDb),
      async batch(statements: D1PreparedStatement[]) {
        if (onBeforeFirstBatch) {
          const hook = onBeforeFirstBatch;
          onBeforeFirstBatch = null;
          await hook();
        }
        return baseDb.batch(statements);
      },
    } as unknown as D1Database;

    const envDirect: Env = {
      DB: baseDb,
      ANONYMIZATION_SALT: "test-salt",
      TELEMETRY_API_TOKEN: "secret-api-token",
    };
    const envRaced: Env = {
      ...envDirect,
      DB: racedDb,
    };

    const postIngest = (targetEnv: Env, ids: string[], customReport = report) =>
      handleIngest(
        new Request("https://worker.test/api/telemetry/ingest", {
          method: "POST",
          body: JSON.stringify({
            client_token: "quota-race-client",
            submissions: ids.map((submission_id) => ({ submission_id, report: customReport })),
          }),
          headers: { "content-type": "application/json" },
        }),
        targetEnv,
        ingestAt,
      );

    // 1. Both overlapping requests pass pre-check at count=0 with 4 items each (4 + 4 = 8 > 6).
    let innerRes: Response | null = null;
    onBeforeFirstBatch = async () => {
      innerRes = await postIngest(envDirect, ["lim-a1", "lim-a2", "lim-a3", "lim-a4"]);
    };

    const outerRes = await postIngest(envRaced, ["lim-b1", "lim-b2", "lim-b3", "lim-b4"]);
    expect(innerRes).not.toBeNull();
    expect(innerRes!.status).toBe(202);
    expect(await innerRes!.json()).toMatchObject({ ok: true, accepted: 4 });

    // Outer transaction hits hourly_quota CHECK constraint and rolls back completely.
    expect(outerRes.status).toBe(429);
    const receiptCnt = await baseDb
      .prepare("SELECT COUNT(*) AS cnt FROM ingest_receipt")
      .first<{ cnt: number }>();
    const rawCnt = await baseDb
      .prepare("SELECT COUNT(*) AS cnt FROM raw_submission")
      .first<{ cnt: number }>();
    const quotaRow = await baseDb
      .prepare("SELECT submission_count FROM hourly_quota WHERE quota_key LIKE 'c:%'")
      .first<{ submission_count: number }>();
    expect(receiptCnt?.cnt).toBe(4);
    expect(rawCnt?.cnt).toBe(4);
    expect(quotaRow?.submission_count).toBe(4);

    // 2. Concurrent duplicate + remaining quota slots: [lim-a4, lim-c1, lim-c2] when 4 exist
    //    must fail UNIQUE first, retry cleanly to deduplicate lim-a4, and accept lim-c1 & lim-c2 (total 6).
    const fifthRes = await postIngest(envDirect, ["lim-a4", "lim-c1", "lim-c2"]);
    expect(fifthRes.status).toBe(202);
    expect(await fifthRes.json()).toEqual({
      ok: true,
      accepted: 2,
      deduplicated: 1,
      skipped_expired: 0,
    });
    const finalQuotaRow = await baseDb
      .prepare("SELECT submission_count FROM hourly_quota WHERE quota_key LIKE 'c:%'")
      .first<{ submission_count: number }>();
    expect(finalQuotaRow?.submission_count).toBe(6);

    // 3. Schema v1 & MAX_VALID_TPS=2000 alignment checks.
    const valid2k = sampleReport("2026-11-14T15:00:00Z", "2026-11-14T15:30:00Z", 1);
    const hist2k = emptyTpsHist();
    hist2k[TPS_HISTOGRAM_BUCKETS - 1] = 1;
    valid2k.tps_by_model_effort[0].sample_count = 1;
    valid2k.tps_by_model_effort[0].output_tokens = 2_000;
    valid2k.tps_by_model_effort[0].elapsed_ms = 1_000;
    valid2k.tps_by_model_effort[0].histogram = hist2k;
    expect(
      (await postIngest(
        { ...envDirect },
        ["tps-2k-other-client"],
        valid2k,
      ).then((r) => r.status)),
    ).toBe(429); // same client already at 6/6

    const otherClientRes = await handleIngest(
      new Request("https://worker.test/api/telemetry/ingest", {
        method: "POST",
        body: JSON.stringify({
          client_token: "tps-tester",
          submissions: [{ submission_id: "tps-2k", report: valid2k }],
        }),
        headers: { "content-type": "application/json" },
      }),
      envDirect,
      ingestAt,
    );
    expect(otherClientRes.status).toBe(202);

    const invalidOver2k = sampleReport("2026-11-14T15:00:00Z", "2026-11-14T15:30:00Z", 1);
    invalidOver2k.tps_by_model_effort[0].sample_count = 1;
    invalidOver2k.tps_by_model_effort[0].output_tokens = 2_001;
    invalidOver2k.tps_by_model_effort[0].elapsed_ms = 1_000;
    invalidOver2k.tps_by_model_effort[0].histogram = hist2k;
    const rejectedOver2k = await handleIngest(
      new Request("https://worker.test/api/telemetry/ingest", {
        method: "POST",
        body: JSON.stringify({
          client_token: "tps-tester",
          submissions: [{ submission_id: "tps-2001", report: invalidOver2k }],
        }),
        headers: { "content-type": "application/json" },
      }),
      envDirect,
      ingestAt,
    );
    expect(rejectedOver2k.status).toBe(400);

    const wrongSchemaVersion = {
      ...valid2k,
      schema_version: 99,
    };
    const rejectedVersion = await handleIngest(
      new Request("https://worker.test/api/telemetry/ingest", {
        method: "POST",
        body: JSON.stringify({
          client_token: "tps-tester",
          submissions: [{ submission_id: "tps-bad-ver", report: wrongSchemaVersion }],
        }),
        headers: { "content-type": "application/json" },
      }),
      envDirect,
      ingestAt,
    );
    expect(rejectedVersion.status).toBe(400);

    const wrongBucketLen = {
      ...valid2k,
      tps_by_model_effort: [
        { ...valid2k.tps_by_model_effort[0], histogram: [0, 0, 0, 0, 0, 0, 0, 0, 1] },
      ],
    };
    const rejectedBucketLen = await handleIngest(
      new Request("https://worker.test/api/telemetry/ingest", {
        method: "POST",
        body: JSON.stringify({
          client_token: "tps-tester",
          submissions: [{ submission_id: "tps-bad-hist", report: wrongBucketLen }],
        }),
        headers: { "content-type": "application/json" },
      }),
      envDirect,
      ingestAt,
    );
    expect(rejectedBucketLen.status).toBe(400);

    // 4. Prune deletes expired hourly_quota entries alongside raw_submission & ingest_receipt.
    const deletedAfterRetention = await pruneExpiredRawRows(
      baseDb,
      new Date("2026-11-18T16:00:00Z"),
    );
    expect(deletedAfterRetention).toBeGreaterThan(0);
    const remainingQuota = await baseDb
      .prepare("SELECT COUNT(*) AS cnt FROM hourly_quota")
      .first<{ cnt: number }>();
    expect(remainingQuota?.cnt).toBe(0);
  });

  test("keeps a single canonical slice per contributor hour and rejects double-counting sub-hour splits", () => {
    const window = {
      start: "2026-11-14T15:00:00Z",
      end: "2026-11-14T16:00:00Z",
      duration_secs: 3_600,
    };

    // Contributor A submitted a partial [15:00, 15:30) slice (4 events) and a full [15:00, 16:00) slice (7 events).
    // Contributor B submitted two split sub-slices [15:00, 15:40) (4 events) and [15:40, 16:00) (3 events);
    // only the single canonical slice (widest span) per contributor is retained.
    const rolled = aggregateHourByContributors(
      [
        {
          contributorHash: "contrib-a",
          report: sampleReport("2026-11-14T15:00:00Z", "2026-11-14T15:30:00Z", 4),
        },
        {
          contributorHash: "contrib-a",
          report: sampleReport("2026-11-14T15:00:00Z", "2026-11-14T16:00:00Z", 7),
        },
        {
          contributorHash: "contrib-b",
          report: sampleReport("2026-11-14T15:00:00Z", "2026-11-14T15:40:00Z", 4),
        },
        {
          contributorHash: "contrib-b",
          report: sampleReport("2026-11-14T15:40:00Z", "2026-11-14T16:00:00Z", 3),
        },
      ],
      window,
    );

    expect(rolled.contributors).toBe(2);
    expect(rolled.usage_by_plan_model[0].events).toBe(11);
  });

  test("rejects per-submission client_token overrides and enforces atomic per-IP hourly item quotas across rotated tokens", async () => {
    const db = createTestD1();
    const env: Env = { DB: db, ANONYMIZATION_SALT: "ip-quota-test-salt" };
    const ingestAt = new Date("2026-11-14T20:30:00Z");

    const postWithIp = (ip: string, body: unknown, now = ingestAt) =>
      handleIngest(
        new Request("https://worker.test/api/telemetry/ingest", {
          method: "POST",
          body: JSON.stringify(body),
          headers: {
            "content-type": "application/json",
            "cf-connecting-ip": ip,
          },
        }),
        env,
        now,
      );

    // 1. Per-submission client_token override and missing top-level token are rejected (400).
    const overrideRes = await postWithIp("198.51.100.1", {
      client_token: "top-level-token",
      submissions: [
        {
          submission_id: "forged-1",
          client_token: "forged-sub-token",
          report: sampleReport("2026-11-14T15:00:00Z", "2026-11-14T16:00:00Z", 1),
        },
      ],
    });
    expect(overrideRes.status).toBe(400);
    expect(await overrideRes.json()).toEqual({
      ok: false,
      error: "submissions[0]: per-submission client_token override is not allowed",
    });

    const missingTopTokenRes = await postWithIp("198.51.100.1", {
      submissions: [
        {
          submission_id: "missing-token-1",
          report: sampleReport("2026-11-14T15:00:00Z", "2026-11-14T16:00:00Z", 1),
        },
      ],
    });
    expect(missingTopTokenRes.status).toBe(400);

    // 2. Per-IP target-hour cap (24): 4 rotated tokens x 6 submissions = 24 accepted, 25th fails 429 atomically.
    const targetStart = "2026-11-14T15:00:00Z";
    const targetEnd = "2026-11-14T16:00:00Z";
    for (let t = 0; t < 4; t++) {
      const subs = Array.from({ length: 6 }, (_, i) => ({
        submission_id: `ip1-h15-t${t}-${i}`,
        report: sampleReport(targetStart, targetEnd, 1),
      }));
      const res = await postWithIp("198.51.100.1", {
        client_token: `rotated-token-${t}`,
        submissions: subs,
      });
      expect(res.status).toBe(202);
    }

    const blockedTargetRes = await postWithIp("198.51.100.1", {
      client_token: "rotated-token-4",
      submissions: [
        {
          submission_id: "ip1-h15-blocked",
          report: sampleReport(targetStart, targetEnd, 1),
        },
      ],
    });
    expect(blockedTargetRes.status).toBe(429);

    // Duplicate retry of an existing receipt from the capped IP still succeeds (200) without consuming quota.
    const dupRetryRes = await postWithIp("198.51.100.1", {
      client_token: "rotated-token-0",
      submissions: [
        {
          submission_id: "ip1-h15-t0-0",
          report: sampleReport(targetStart, targetEnd, 1),
        },
      ],
    });
    expect(dupRetryRes.status).toBe(200);
    expect(await dupRetryRes.json()).toEqual({
      ok: true,
      accepted: 0,
      deduplicated: 1,
      skipped_expired: 0,
    });

    // Another IP targeting the same hour is unaffected.
    const otherIpRes = await postWithIp("198.51.100.2", {
      client_token: "other-ip-token",
      submissions: [
        {
          submission_id: "ip2-h15-0",
          report: sampleReport(targetStart, targetEnd, 1),
        },
      ],
    });
    expect(otherIpRes.status).toBe(202);

    // 3. Concurrent requests hitting the same IP target-hour cap race safely via CHECK rollback.
    const raceStart = "2026-11-14T16:00:00Z";
    const raceEnd = "2026-11-14T17:00:00Z";
    for (let t = 0; t < 3; t++) {
      const res = await postWithIp("198.51.100.1", {
        client_token: `race-seed-${t}`,
        submissions: Array.from({ length: 6 }, (_, i) => ({
          submission_id: `ip1-h16-seed-${t}-${i}`,
          report: sampleReport(raceStart, raceEnd, 1),
        })),
      });
      expect(res.status).toBe(202);
    }
    const seedTwo = await postWithIp("198.51.100.1", {
      client_token: "race-seed-3",
      submissions: [
        { submission_id: "ip1-h16-seed-3-0", report: sampleReport(raceStart, raceEnd, 1) },
        { submission_id: "ip1-h16-seed-3-1", report: sampleReport(raceStart, raceEnd, 1) },
      ],
    });
    expect(seedTwo.status).toBe(202); // 20 of 24 used

    const [raceA, raceB] = await Promise.all([
      postWithIp("198.51.100.1", {
        client_token: "race-contender-a",
        submissions: Array.from({ length: 4 }, (_, i) => ({
          submission_id: `ip1-h16-a-${i}`,
          report: sampleReport(raceStart, raceEnd, 1),
        })),
      }),
      postWithIp("198.51.100.1", {
        client_token: "race-contender-b",
        submissions: Array.from({ length: 4 }, (_, i) => ({
          submission_id: `ip1-h16-b-${i}`,
          report: sampleReport(raceStart, raceEnd, 1),
        })),
      }),
    ]);
    const raceStatuses = [raceA.status, raceB.status].sort((a, b) => a - b);
    expect(raceStatuses).toEqual([202, 429]);

    const h16Count = await db
      .prepare("SELECT COUNT(*) AS cnt FROM raw_submission WHERE hour_bucket = ?1")
      .bind(raceStart)
      .first<{ cnt: number }>();
    expect(h16Count?.cnt).toBe(24);

    // 4. Retention cleanup prunes expired hourly_quota rows too.
    await pruneExpiredRawRows(db, new Date("2026-11-18T00:00:00Z"));
    const remainingIpQuota = await db
      .prepare("SELECT COUNT(*) AS cnt FROM hourly_quota")
      .first<{ cnt: number }>();
    expect(remainingIpQuota?.cnt).toBe(0);
  });

  test("fails closed with 503 when ANONYMIZATION_SALT is missing or blank", async () => {
    const db = createTestD1();
    const validBody = JSON.stringify({
      client_token: "salt-check-client",
      submissions: [
        {
          submission_id: "salt-check-1",
          report: sampleReport("2026-11-14T15:00:00Z", "2026-11-14T16:00:00Z", 1),
        },
      ],
    });
    const sendWithEnv = (env: Env) =>
      handleIngest(
        new Request("https://worker.test/api/telemetry/ingest", {
          method: "POST",
          body: validBody,
          headers: {
            "content-type": "application/json",
            "cf-connecting-ip": "198.51.100.77",
          },
        }),
        env,
        new Date("2026-11-14T16:15:00Z"),
      );

    for (const badEnv of [
      { DB: db },
      { DB: db, ANONYMIZATION_SALT: "" },
      { DB: db, ANONYMIZATION_SALT: "   " },
    ] satisfies Env[]) {
      const res = await sendWithEnv(badEnv);
      expect(res.status).toBe(503);
      expect(await res.json()).toEqual({
        ok: false,
        error: "anonymization salt not configured",
      });
    }

    const quotaRows = await db
      .prepare("SELECT COUNT(*) AS cnt FROM hourly_quota")
      .first<{ cnt: number }>();
    const rawRows = await db
      .prepare("SELECT COUNT(*) AS cnt FROM raw_submission")
      .first<{ cnt: number }>();
    expect(quotaRows?.cnt).toBe(0);
    expect(rawRows?.cnt).toBe(0);
  });

  test("validates histogram sums, strips extra properties, isolates receipts per contributor, and groups IPv6 /64 prefixes", async () => {
    const db = createTestD1();
    const env: Env = {
      DB: db,
      ANONYMIZATION_SALT: "test-salt",
      TELEMETRY_API_TOKEN: "secret-api-token",
    };
    const now = new Date("2026-11-14T16:15:00Z");

    // 1. TPS histogram sum != sample_count is rejected (400).
    const badTpsSum = sampleReport("2026-11-14T15:00:00Z", "2026-11-14T16:00:00Z", 1);
    badTpsSum.tps_by_model_effort[0].histogram = emptyTpsHist();
    badTpsSum.tps_by_model_effort[0].histogram[TPS_HISTOGRAM_BUCKETS - 1] = 1;
    const resBadTps = await handleIngest(
      new Request("https://worker.test/api/telemetry/ingest", {
        method: "POST",
        body: JSON.stringify({
          client_token: "bad-tps-user",
          submissions: [{ submission_id: "bad-tps-1", report: badTpsSum }],
        }),
        headers: { "content-type": "application/json" },
      }),
      env,
      now,
    );
    expect(resBadTps.status).toBe(400);

    // 2. Implausible sample floor (20 samples with only 500 output_tokens < 20*100) rejected.
    const badFloor = sampleReport("2026-11-14T15:00:00Z", "2026-11-14T16:00:00Z", 1);
    badFloor.tps_by_model_effort[0].sample_count = 20;
    badFloor.tps_by_model_effort[0].histogram = emptyTpsHist();
    badFloor.tps_by_model_effort[0].histogram[3] = 20;
    const resBadFloor = await handleIngest(
      new Request("https://worker.test/api/telemetry/ingest", {
        method: "POST",
        body: JSON.stringify({
          client_token: "bad-floor-user",
          submissions: [{ submission_id: "bad-floor-1", report: badFloor }],
        }),
        headers: { "content-type": "application/json" },
      }),
      env,
      now,
    );
    expect(resBadFloor.status).toBe(400);

    // 3. Quota cost_per_pct_histogram sum > slices rejected (400).
    const badQuotaHist = sampleReport("2026-11-14T15:00:00Z", "2026-11-14T16:00:00Z", 1);
    const badCostHist = new Array<number>(QUOTA_COST_HISTOGRAM_BUCKETS).fill(0);
    badCostHist[QUOTA_COST_HISTOGRAM_BUCKETS - 1] = 2;
    badQuotaHist.quota_by_plan_bucket_model[0].cost_per_pct_histogram = badCostHist;
    const resBadQuota = await handleIngest(
      new Request("https://worker.test/api/telemetry/ingest", {
        method: "POST",
        body: JSON.stringify({
          client_token: "bad-q-user",
          submissions: [{ submission_id: "bad-q-1", report: badQuotaHist }],
        }),
        headers: { "content-type": "application/json" },
      }),
      env,
      now,
    );
    expect(resBadQuota.status).toBe(400);

    // 4. Two distinct contributors using identical submission_id must both succeed (no cross-tenant receipt collision),
    // and extra unknown fields inside rows are stripped before persisting raw_submission.
    const noisyReport = sampleReport("2026-11-14T15:00:00Z", "2026-11-14T16:00:00Z", 2);
    (noisyReport.usage_by_plan_model[0] as unknown as Record<string, unknown>).secret_note = "drop-me";

    const tenantA = await handleIngest(
      new Request("https://worker.test/api/telemetry/ingest", {
        method: "POST",
        body: JSON.stringify({
          client_token: "tenant-alpha",
          submissions: [{ submission_id: "shared-id-001", report: noisyReport }],
        }),
        headers: { "content-type": "application/json" },
      }),
      env,
      now,
    );
    const tenantB = await handleIngest(
      new Request("https://worker.test/api/telemetry/ingest", {
        method: "POST",
        body: JSON.stringify({
          client_token: "tenant-beta",
          submissions: [{ submission_id: "shared-id-001", report: noisyReport }],
        }),
        headers: { "content-type": "application/json" },
      }),
      env,
      now,
    );
    expect(tenantA.status).toBe(202);
    expect(await tenantB.json()).toMatchObject({ ok: true, accepted: 1, deduplicated: 0 });

    const storedRaw = await db
      .prepare("SELECT report_json FROM raw_submission ORDER BY id DESC LIMIT 1")
      .first<{ report_json: string }>();
    expect(storedRaw?.report_json).not.toContain("secret_note");

    // 5. Rotating IPv6 interface IDs within the same /64 prefix share IP hourly quota (24 limit).
    for (let t = 0; t < 4; t++) {
      const res = await handleIngest(
        new Request("https://worker.test/api/telemetry/ingest", {
          method: "POST",
          body: JSON.stringify({
            client_token: `v6-token-${t}`,
            submissions: Array.from({ length: 6 }, (_, i) => ({
              submission_id: `v6-${t}-${i}`,
              report: sampleReport("2026-11-14T14:00:00Z", "2026-11-14T15:00:00Z", 1),
            })),
          }),
          headers: {
            "content-type": "application/json",
            "cf-connecting-ip": `2001:db8:abcd:0012::${t + 10}`,
          },
        }),
        env,
        now,
      );
      expect(res.status).toBe(202);
    }

    const blockedV6 = await handleIngest(
      new Request("https://worker.test/api/telemetry/ingest", {
        method: "POST",
        body: JSON.stringify({
          client_token: "v6-token-4",
          submissions: [
            {
              submission_id: "v6-4-0",
              report: sampleReport("2026-11-14T14:00:00Z", "2026-11-14T15:00:00Z", 1),
            },
          ],
        }),
        headers: {
          "content-type": "application/json",
          "cf-connecting-ip": "2001:0db8:abcd:12:ffff::99",
        },
      }),
      env,
      now,
    );
    expect(blockedV6.status).toBe(429);
  });

  test("bounds Cron D1 queries strictly below 50 across 72h backfill and multi-batch pruning", async () => {
    const rawDb = createTestD1();
    const unwrapMap = new WeakMap<D1PreparedStatement, D1PreparedStatement>();
    let d1Queries = 0;

    // Count D1 queries under Cloudflare Free plan rules: each first/all/run or batch() = 1 query.
    const wrapStmt = (inner: D1PreparedStatement): D1PreparedStatement => {
      const proxy = {
        bind(...values: unknown[]) {
          return wrapStmt(inner.bind(...values));
        },
        first<T = Record<string, unknown>>() {
          d1Queries += 1;
          return inner.first<T>();
        },
        all<T = Record<string, unknown>>() {
          d1Queries += 1;
          return inner.all<T>();
        },
        run() {
          d1Queries += 1;
          return inner.run();
        },
        raw<T = unknown[]>() {
          d1Queries += 1;
          return inner.raw<T>();
        },
      } as unknown as D1PreparedStatement;
      unwrapMap.set(proxy, inner);
      return proxy;
    };

    const countedDb = {
      prepare(query: string) {
        return wrapStmt(rawDb.prepare(query));
      },
      batch(statements: D1PreparedStatement[]) {
        d1Queries += 1;
        return rawDb.batch(statements.map((s) => unwrapMap.get(s) ?? s));
      },
    } as unknown as D1Database;

    const now = new Date("2026-11-14T16:00:00Z");
    const env: Env = {
      DB: countedDb,
      ANONYMIZATION_SALT: "test-salt",
      TELEMETRY_API_TOKEN: "secret-api-token",
    };

    // Seed all 72 closed hours within the 3-day raw retention window (`[now-72h, now)`).
    const seedHours: D1PreparedStatement[] = [];
    for (let h = 1; h <= 72; h++) {
      const start = new Date(now.getTime() - h * 3_600_000).toISOString().replace(".000Z", "Z");
      const end = new Date(now.getTime() - (h - 1) * 3_600_000).toISOString().replace(".000Z", "Z");
      seedHours.push(
        rawDb
          .prepare(
            "INSERT INTO raw_submission (hour_bucket, contributor_hash, report_json, received_at) VALUES (?1, ?2, ?3, ?4)",
          )
          .bind(start, `contrib-${h}`, JSON.stringify(sampleReport(start, end, 1)), start),
      );
    }
    await rawDb.batch(seedHours);

    // Seed 12,000 expired rows (24 x 500-row prune batches) plus expired raw_submission and hourly_quota rows.
    await rawDb
      .prepare(
        `WITH RECURSIVE cnt(x) AS (
           SELECT 1 UNION ALL SELECT x + 1 FROM cnt WHERE x < 12000
         )
         INSERT INTO ingest_receipt (receipt_id, client_hour_key, created_at)
         SELECT 'expired-r-' || x, 'expired-c-' || x || ':2026-11-10T10:00:00Z', '2026-11-10T10:15:00Z'
         FROM cnt`,
      )
      .run();
    await rawDb
      .prepare(
        "INSERT INTO raw_submission (hour_bucket, contributor_hash, report_json, received_at) VALUES (?1, ?2, ?3, ?4)",
      )
      .bind(
        "2026-11-10T10:00:00Z",
        "expired-c",
        JSON.stringify(sampleReport("2026-11-10T10:00:00Z", "2026-11-10T11:00:00Z", 1)),
        "2026-11-10T10:15:00Z",
      )
      .run();
    await rawDb
      .prepare(
        "INSERT INTO hourly_quota (quota_key, submission_count, max_limit, updated_at) VALUES (?1, 1, 6, ?2)",
      )
      .bind("expired-quota-key", "2026-11-10T10:15:00Z")
      .run();

    const perRunCounts: number[] = [];
    const perRunQueries: number[] = [];
    for (let runIdx = 0; runIdx < 5; runIdx++) {
      d1Queries = 0;
      const rolled = await handleScheduled(env, now);
      perRunCounts.push(rolled);
      perRunQueries.push(d1Queries);
      expect(d1Queries).toBeLessThan(50);
    }

    expect(perRunCounts).toEqual([20, 20, 20, 12, 0]);
    expect(perRunQueries[0]).toBe(45);
    expect(perRunQueries[4]).toBe(2);

    const rollupCount = await rawDb
      .prepare("SELECT COUNT(*) AS cnt FROM hourly_rollup")
      .first<{ cnt: number }>();
    const expiredReceipts = await rawDb
      .prepare("SELECT COUNT(*) AS cnt FROM ingest_receipt")
      .first<{ cnt: number }>();
    const expiredRaw = await rawDb
      .prepare("SELECT COUNT(*) AS cnt FROM raw_submission WHERE hour_bucket < '2026-11-11T16:00:00Z'")
      .first<{ cnt: number }>();
    const expiredQuota = await rawDb
      .prepare("SELECT COUNT(*) AS cnt FROM hourly_quota")
      .first<{ cnt: number }>();

    expect(rollupCount?.cnt).toBe(72);
    expect(expiredReceipts?.cnt).toBe(0);
    expect(expiredRaw?.cnt).toBe(0);
    expect(expiredQuota?.cnt).toBe(0);
  });
});
