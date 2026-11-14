use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};

use crate::config::TokenRates;
use crate::payload::StatusPayload;
use crate::stats::{
    MIXED_ATTRIBUTION_KEY, QUOTA_COST_HISTOGRAM_BUCKETS, QuotaByPlanBucketModel,
    STATS_SCHEMA_VERSION, StatsReport, StatsWindow, TPS_HISTOGRAM_BUCKETS, TpsByModelEffort,
    UsageByPlanModel, quota_cost_bucket_index, reference_cost_usd, tps_bucket_index,
};
use crate::tracker::{
    SessionMetrics, SessionTrackerState, TpsTurnSample, UsageSnapshot, current_time_ms,
    read_transcript_totals, resolve_metrics_with_transcript,
};

const SCHEMA_VERSION: i64 = 1;

const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS session (
    session_id             TEXT PRIMARY KEY,
    bucket_group           TEXT NOT NULL,
    model_id               TEXT NOT NULL,
    input_token            INTEGER NOT NULL DEFAULT 0,
    cache_read_token       INTEGER NOT NULL DEFAULT 0,
    cache_write_token      INTEGER NOT NULL DEFAULT 0,
    output_token           INTEGER NOT NULL DEFAULT 0,
    reported_cost_usd      REAL,
    last_total_input       INTEGER NOT NULL DEFAULT 0,
    last_total_output      INTEGER NOT NULL DEFAULT 0,
    last_usage_input       INTEGER,
    last_usage_output      INTEGER,
    last_usage_cache_read  INTEGER,
    last_usage_cache_write INTEGER,
    sum_input              INTEGER NOT NULL DEFAULT 0,
    sum_cache_read         INTEGER NOT NULL DEFAULT 0,
    sum_cache_write        INTEGER NOT NULL DEFAULT 0,
    sum_output             INTEGER NOT NULL DEFAULT 0,
    working_started_at_ms  INTEGER,
    tps_output_tokens      INTEGER NOT NULL DEFAULT 0,
    tps_elapsed_ms         INTEGER NOT NULL DEFAULT 0,
    consumed_planner_count INTEGER NOT NULL DEFAULT 0,
    created_at             TEXT NOT NULL,
    updated_at             TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS usage_event (
    id                      INTEGER PRIMARY KEY AUTOINCREMENT,
    recorded_at             TEXT NOT NULL,
    session_id              TEXT NOT NULL,
    bucket_group            TEXT NOT NULL,
    model_id                TEXT NOT NULL,
    delta_input_token       INTEGER NOT NULL,
    delta_cache_read_token  INTEGER NOT NULL,
    delta_cache_write_token INTEGER NOT NULL,
    delta_output_token      INTEGER NOT NULL,
    delta_reported_cost_usd REAL,
    plan_tier               TEXT NOT NULL DEFAULT 'unknown',
    model                   TEXT NOT NULL DEFAULT 'unknown',
    effort                  TEXT NOT NULL DEFAULT 'default',
    ref_cost_usd            REAL NOT NULL DEFAULT 0.0
);

CREATE INDEX IF NOT EXISTS idx_usage_event_recorded_at ON usage_event (recorded_at);
CREATE INDEX IF NOT EXISTS idx_usage_event_session_id ON usage_event (session_id);

CREATE TABLE IF NOT EXISTS tps_sample (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    recorded_at   TEXT NOT NULL,
    session_id    TEXT NOT NULL,
    plan_tier     TEXT NOT NULL,
    model         TEXT NOT NULL,
    effort        TEXT NOT NULL,
    output_tokens INTEGER NOT NULL,
    elapsed_ms    INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_tps_sample_recorded_at ON tps_sample (recorded_at);

CREATE TABLE IF NOT EXISTS quota_bucket_cursor (
    bucket                    TEXT PRIMARY KEY,
    bucket_group              TEXT NOT NULL,
    reset_time                TEXT NOT NULL,
    last_remaining_fraction   REAL NOT NULL,
    pending_input_token       INTEGER NOT NULL DEFAULT 0,
    pending_cache_read_token  INTEGER NOT NULL DEFAULT 0,
    pending_cache_write_token INTEGER NOT NULL DEFAULT 0,
    pending_output_token      INTEGER NOT NULL DEFAULT 0,
    pending_ref_cost_usd      REAL NOT NULL DEFAULT 0.0,
    pending_model             TEXT,
    pending_effort            TEXT,
    pending_mixed             INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS quota_attribution (
    id                   INTEGER PRIMARY KEY AUTOINCREMENT,
    recorded_at          TEXT NOT NULL,
    plan_tier            TEXT NOT NULL,
    bucket               TEXT NOT NULL,
    attributed_model     TEXT NOT NULL,
    attributed_effort    TEXT NOT NULL,
    delta_quota_fraction REAL NOT NULL,
    input_tokens         INTEGER NOT NULL,
    cache_read_tokens    INTEGER NOT NULL,
    cache_write_tokens   INTEGER NOT NULL,
    output_tokens        INTEGER NOT NULL,
    ref_cost_usd         REAL NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_quota_attribution_recorded_at ON quota_attribution (recorded_at);

CREATE TABLE IF NOT EXISTS telemetry_state (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
"#;

struct UsageDelta {
    input_tokens: i64,
    cache_read_tokens: i64,
    cache_write_tokens: i64,
    output_tokens: i64,
    ref_cost_usd: f64,
}

/// Resolves session metrics atomically against SQLite `session` state and persists deltas,
/// TPS turn samples, and quota attribution slices in a single `IMMEDIATE` transaction.
pub fn resolve_and_record(
    db_path: &Path,
    payload: &StatusPayload,
    fallback_rates: Option<&TokenRates>,
) -> rusqlite::Result<SessionMetrics> {
    let now_ms = current_time_ms();
    let now_iso = iso8601_from_unix_secs(now_ms / 1_000);
    resolve_and_record_at(db_path, payload, fallback_rates, now_ms, &now_iso)
}

pub fn resolve_and_record_at(
    db_path: &Path,
    payload: &StatusPayload,
    fallback_rates: Option<&TokenRates>,
    now_ms: u64,
    now_iso: &str,
) -> rusqlite::Result<SessionMetrics> {
    // Parse transcript outside the SQLite IMMEDIATE lock to keep write lock hold times short.
    let transcript = read_transcript_totals(payload);
    let mut conn = open_and_migrate(db_path)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let metrics = persist_tick_tx(&tx, payload, now_iso, |prev| {
        let mut tracker_state = prev.map(|s| s.tracker.clone()).unwrap_or_default();
        let metrics = resolve_metrics_with_transcript(
            payload,
            transcript.as_ref(),
            Some(&mut tracker_state),
            fallback_rates,
            now_ms,
        );
        let tps_sample = metrics.last_turn_tps_sample;
        (metrics, tracker_state, tps_sample)
    })?;
    tx.commit()?;
    Ok(metrics)
}

#[cfg(test)]
pub fn record_snapshot(
    db_path: &Path,
    payload: &StatusPayload,
    metrics: &SessionMetrics,
) -> rusqlite::Result<()> {
    let now = iso8601_from_unix_secs(current_unix_secs());
    record_snapshot_at(db_path, payload, metrics, &now)
}

#[cfg(test)]
pub fn record_snapshot_at(
    db_path: &Path,
    payload: &StatusPayload,
    metrics: &SessionMetrics,
    now: &str,
) -> rusqlite::Result<()> {
    let mut conn = open_and_migrate(db_path)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let cur_out = to_i64(metrics.output_tokens);
    persist_tick_tx(&tx, payload, now, |prev| {
        let tracker_state = prev.map(|s| s.tracker.clone()).unwrap_or_default();
        let tps_sample = metrics
            .last_turn_tps_sample
            .filter(|_| prev.is_none_or(|p| cur_out != p.output_token));
        (metrics.clone(), tracker_state, tps_sample)
    })?;
    tx.commit()
}

fn persist_tick_tx<F>(
    tx: &Transaction<'_>,
    payload: &StatusPayload,
    now: &str,
    build_metrics: F,
) -> rusqlite::Result<SessionMetrics>
where
    F: FnOnce(
        Option<&StoredSession>,
    ) -> (SessionMetrics, SessionTrackerState, Option<TpsTurnSample>),
{
    let plan_tier = crate::payload::normalize_label(payload.normalized_plan_tier(), "unknown", 128);
    let (raw_model, raw_effort) = payload.normalized_model_and_effort();
    let model = crate::payload::normalize_label(&raw_model, "unknown", 128);
    let effort = crate::payload::normalize_label(&raw_effort, "default", 128);
    let bucket_group = payload.active_quota_group().as_str();

    let reset_or_new_buckets = ensure_and_reset_quota_cursors(tx, payload)?;

    let metrics = if let Some(sid) = payload.effective_session_id() {
        let prev = load_session(tx, sid)?;
        let (metrics, tracker_state, tps_sample) = build_metrics(prev.as_ref());
        let recorded_delta = persist_session_usage(
            tx,
            now,
            sid,
            payload,
            &metrics,
            prev.as_ref(),
            &tracker_state,
            &plan_tier,
            &model,
            &effort,
        )?;
        if let Some(ref delta) = recorded_delta {
            accumulate_usage_into_cursors(
                tx,
                bucket_group,
                &model,
                &effort,
                delta,
                &reset_or_new_buckets,
            )?;
        }
        if let Some(sample) = tps_sample
            && tps_bucket_index(sample.output_tokens, sample.elapsed_ms).is_some()
        {
            record_tps_sample(tx, now, sid, &plan_tier, &model, &effort, sample)?;
        }
        metrics
    } else {
        build_metrics(None).0
    };

    settle_quota_attributions(tx, now, payload, &plan_tier)?;
    Ok(metrics)
}

/// Queries a consolidated `StatsReport` across `[start_unix, end_unix]`.
/// Passing `start_unix = None` spans all recorded history up to `end_unix`.
pub fn query_stats_report(
    db_path: &Path,
    start_unix: Option<u64>,
    end_unix: u64,
) -> rusqlite::Result<StatsReport> {
    query_stats_report_with_bounds(db_path, start_unix, end_unix, true, true)
}

/// Queries a consolidated `StatsReport` across half-open `[start_unix, end_unix_exclusive)`.
pub fn query_stats_report_half_open(
    db_path: &Path,
    start_unix: u64,
    end_unix_exclusive: u64,
) -> rusqlite::Result<StatsReport> {
    query_stats_report_with_bounds(db_path, Some(start_unix), end_unix_exclusive, false, true)
}

/// Queries a consolidated `StatsReport` across half-open `[start_unix, end_unix_exclusive)`
/// strictly read-only without creating or migrating the database file.
pub fn query_stats_report_half_open_readonly(
    db_path: &Path,
    start_unix: u64,
    end_unix_exclusive: u64,
) -> rusqlite::Result<StatsReport> {
    query_stats_report_with_bounds(db_path, Some(start_unix), end_unix_exclusive, false, false)
}

fn query_stats_report_with_bounds(
    db_path: &Path,
    start_unix: Option<u64>,
    end_unix: u64,
    inclusive_end: bool,
    allow_write_migration: bool,
) -> rusqlite::Result<StatsReport> {
    let end_iso = iso8601_from_unix_secs(end_unix);
    let empty_report = || {
        let start_iso = start_unix.map_or_else(|| end_iso.clone(), iso8601_from_unix_secs);
        let duration_secs = start_unix.map_or(0, |s| end_unix.saturating_sub(s));
        StatsReport::empty_local(start_iso, end_iso.clone(), duration_secs)
    };
    if !db_path.exists() {
        return Ok(empty_report());
    }

    let ro_conn = open_readonly(db_path)?;
    let conn = match ro_conn {
        Some(c) if is_schema_current(&c)? => c,
        _ => {
            if !allow_write_migration {
                return Ok(empty_report());
            }
            open_and_migrate(db_path)?
        }
    };

    let (start_iso, duration_secs) = match start_unix {
        Some(s) => (iso8601_from_unix_secs(s), end_unix.saturating_sub(s)),
        None => match earliest_recorded_timestamp(&conn)? {
            Some(ts) => {
                let dur = unix_secs_from_iso8601(&ts)
                    .map_or(0, |start_u| end_unix.saturating_sub(start_u));
                (ts, dur)
            }
            None => (end_iso.clone(), 0),
        },
    };

    let end_bound_iso = if inclusive_end {
        iso8601_from_unix_secs(end_unix.saturating_add(1))
    } else {
        end_iso.clone()
    };

    query_report_from_conn(&conn, &start_iso, &end_iso, &end_bound_iso, duration_secs)
}

fn query_report_from_conn(
    conn: &Connection,
    start_iso: &str,
    end_iso: &str,
    end_bound_iso: &str,
    duration_secs: u64,
) -> rusqlite::Result<StatsReport> {
    let usage_by_plan_model = query_usage_by_plan_model(conn, start_iso, end_bound_iso)?;
    let tps_by_model_effort = query_tps_by_model_effort(conn, start_iso, end_bound_iso)?;
    let quota_by_plan_bucket_model =
        query_quota_by_plan_bucket_model(conn, start_iso, end_bound_iso)?;

    Ok(StatsReport {
        schema_version: STATS_SCHEMA_VERSION,
        scope: "local".to_string(),
        window: StatsWindow {
            start: start_iso.to_string(),
            end: end_iso.to_string(),
            duration_secs,
        },
        contributors: 1,
        usage_by_plan_model,
        tps_by_model_effort,
        quota_by_plan_bucket_model,
    })
}

fn open_readonly(db_path: &Path) -> rusqlite::Result<Option<Connection>> {
    if !db_path.exists() {
        return Ok(None);
    }
    let conn = Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(Duration::from_millis(5000))?;
    Ok(Some(conn))
}

fn table_exists(conn: &Connection, table: &str) -> rusqlite::Result<bool> {
    let exists: i64 = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
        params![table],
        |row| row.get(0),
    )?;
    Ok(exists != 0)
}

fn is_schema_current(conn: &Connection) -> rusqlite::Result<bool> {
    let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    Ok(version == SCHEMA_VERSION)
}

pub fn get_telemetry_state_readonly(db_path: &Path, key: &str) -> rusqlite::Result<Option<String>> {
    let Some(conn) = open_readonly(db_path)? else {
        return Ok(None);
    };
    if !table_exists(&conn, "telemetry_state")? {
        return Ok(None);
    }
    conn.query_row(
        "SELECT value FROM telemetry_state WHERE key = ?1",
        params![key],
        |row| row.get(0),
    )
    .optional()
}

pub fn earliest_recorded_unix_secs_readonly(db_path: &Path) -> rusqlite::Result<Option<u64>> {
    let Some(conn) = open_readonly(db_path)? else {
        return Ok(None);
    };
    if !is_schema_current(&conn)? {
        return Ok(None);
    }
    Ok(earliest_recorded_timestamp(&conn)?.and_then(|ts| unix_secs_from_iso8601(&ts)))
}

pub fn count_active_stats_slices_readonly(
    db_path: &Path,
    ranges: &[(u64, u64)],
) -> rusqlite::Result<usize> {
    if ranges.is_empty() {
        return Ok(0);
    }
    let Some(conn) = open_readonly(db_path)? else {
        return Ok(0);
    };
    if !is_schema_current(&conn)? {
        return Ok(0);
    }

    let mut count = 0;
    for &(start_unix, end_unix) in ranges {
        let start_iso = iso8601_from_unix_secs(start_unix);
        let end_iso = iso8601_from_unix_secs(end_unix);
        let mut active = false;
        for table in ["usage_event", "tps_sample", "quota_attribution"] {
            let sql = format!(
                "SELECT EXISTS(SELECT 1 FROM {table} WHERE recorded_at >= ?1 AND recorded_at < ?2)"
            );
            let exists: i64 =
                conn.query_row(&sql, params![start_iso, end_iso], |row| row.get(0))?;
            if exists != 0 {
                active = true;
                break;
            }
        }
        if active {
            count += 1;
        }
    }
    Ok(count)
}

pub fn set_telemetry_state(db_path: &Path, key: &str, value: &str) -> rusqlite::Result<()> {
    let conn = open_and_migrate(db_path)?;
    conn.execute(
        "INSERT INTO telemetry_state (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

/// Atomically inspects upload scheduling cursors and claims `attempt_key = now_unix` inside an
/// `IMMEDIATE` transaction so concurrent statusLine invocations spawn at most one background worker.
pub fn try_claim_auto_upload_slot<F>(
    db_path: &Path,
    now_unix: u64,
    uploaded_key: &str,
    attempt_key: &str,
    should_trigger: F,
) -> rusqlite::Result<bool>
where
    F: FnOnce(u64, u64) -> bool,
{
    let mut conn = open_and_migrate(db_path)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let read_u64 = |key: &str| -> rusqlite::Result<Option<u64>> {
        let raw: Option<String> = tx
            .query_row(
                "SELECT value FROM telemetry_state WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()?;
        Ok(raw.and_then(|v| v.parse::<u64>().ok()))
    };
    let last_uploaded = read_u64(uploaded_key)?.unwrap_or(0);
    let last_attempt = read_u64(attempt_key)?.unwrap_or(0);
    if !should_trigger(last_uploaded, last_attempt) {
        return Ok(false);
    }
    tx.execute(
        "INSERT INTO telemetry_state (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![attempt_key, now_unix.to_string()],
    )?;
    tx.commit()?;
    Ok(true)
}

pub fn get_or_create_client_token(db_path: &Path) -> rusqlite::Result<String> {
    let mut conn = open_and_migrate(db_path)?;
    // IMMEDIATE acquires the write lock before reading so concurrent first-time
    // callers serialize and converge on the single persisted client_token.
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if let Some(existing) = tx
        .query_row(
            "SELECT value FROM telemetry_state WHERE key = 'client_token'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        && !existing.is_empty()
    {
        return Ok(existing);
    }
    let token = generate_random_hex_token();
    tx.execute(
        "INSERT INTO telemetry_state (key, value) VALUES ('client_token', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value WHERE telemetry_state.value = ''",
        params![token],
    )?;
    let persisted: String = tx.query_row(
        "SELECT value FROM telemetry_state WHERE key = 'client_token'",
        [],
        |row| row.get::<_, String>(0),
    )?;
    tx.commit()?;
    Ok(persisted)
}

fn generate_random_hex_token() -> String {
    let mut buf = [0u8; 16];
    if let Ok(mut f) = fs::File::open("/dev/urandom")
        && std::io::Read::read_exact(&mut f, &mut buf).is_ok()
    {
        return encode_hex(&buf);
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let pid = u128::from(std::process::id());
    let mixed = nanos ^ (pid.rotate_left(37));
    buf = mixed.to_be_bytes();
    encode_hex(&buf)
}

fn encode_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(out, "{b:02x}");
    }
    out
}

fn is_sqlite_busy(err: &rusqlite::Error) -> bool {
    matches!(
        err.sqlite_error_code(),
        Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
    )
}

fn open_and_migrate(db_path: &Path) -> rusqlite::Result<Connection> {
    if let Some(parent) = db_path.parent()
        && !parent.as_os_str().is_empty()
    {
        let _ = fs::create_dir_all(parent);
    }

    // Switching a new DB to WAL and running initial DDL concurrently can fail immediately
    // with SQLITE_BUSY (SHARED -> EXCLUSIVE/RESERVED upgrade deadlock bypasses busy_timeout).
    const MAX_ATTEMPTS: usize = 40;
    for attempt in 0..MAX_ATTEMPTS {
        match try_open_and_migrate(db_path) {
            Ok(conn) => return Ok(conn),
            Err(err) if is_sqlite_busy(&err) && attempt + 1 < MAX_ATTEMPTS => {
                std::thread::sleep(Duration::from_millis(((attempt + 1) as u64).min(15) * 5));
            }
            Err(err) => return Err(err),
        }
    }
    unreachable!()
}

fn try_open_and_migrate(db_path: &Path) -> rusqlite::Result<Connection> {
    let mut conn = Connection::open(db_path)?;
    conn.busy_timeout(Duration::from_millis(5000))?;

    let journal_mode: String = conn.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
    if !journal_mode.eq_ignore_ascii_case("wal") {
        conn.pragma_update(None, "journal_mode", "WAL")?;
    }
    conn.pragma_update(None, "synchronous", "NORMAL")?;

    if !is_schema_current(&conn)? {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if !is_schema_current(&tx)? {
            tx.execute_batch(SCHEMA_SQL)?;
            tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        }
        tx.commit()?;
    }

    Ok(conn)
}

struct StoredSession {
    input_token: i64,
    cache_read_token: i64,
    cache_write_token: i64,
    output_token: i64,
    reported_cost_usd: Option<f64>,
    tracker: SessionTrackerState,
}

fn load_session(tx: &Transaction<'_>, session_id: &str) -> rusqlite::Result<Option<StoredSession>> {
    tx.query_row(
        "SELECT input_token, cache_read_token, cache_write_token, output_token,
                reported_cost_usd, last_total_input, last_total_output,
                last_usage_input, last_usage_output, last_usage_cache_read, last_usage_cache_write,
                sum_input, sum_cache_read, sum_cache_write, sum_output,
                working_started_at_ms, tps_output_tokens, tps_elapsed_ms, consumed_planner_count
         FROM session WHERE session_id = ?1",
        params![session_id],
        |row| {
            let last_usage = match (
                row.get::<_, Option<i64>>(7)?,
                row.get::<_, Option<i64>>(8)?,
                row.get::<_, Option<i64>>(9)?,
                row.get::<_, Option<i64>>(10)?,
            ) {
                (Some(i), Some(o), Some(cr), Some(cw)) => Some(UsageSnapshot {
                    input_tokens: to_u64(i),
                    output_tokens: to_u64(o),
                    cache_read_input_tokens: to_u64(cr),
                    cache_creation_input_tokens: to_u64(cw),
                }),
                _ => None,
            };
            Ok(StoredSession {
                input_token: row.get(0)?,
                cache_read_token: row.get(1)?,
                cache_write_token: row.get(2)?,
                output_token: row.get(3)?,
                reported_cost_usd: row.get(4)?,
                tracker: SessionTrackerState {
                    last_total_input: to_u64(row.get(5)?),
                    last_total_output: to_u64(row.get(6)?),
                    last_usage,
                    sum_input: to_u64(row.get(11)?),
                    sum_cache_read: to_u64(row.get(12)?),
                    sum_cache_write: to_u64(row.get(13)?),
                    sum_output: to_u64(row.get(14)?),
                    working_started_at_ms: row.get::<_, Option<i64>>(15)?.map(to_u64),
                    tps_output_tokens: to_u64(row.get(16)?),
                    tps_elapsed_ms: to_u64(row.get(17)?),
                    consumed_planner_count: to_u64(row.get(18)?),
                },
            })
        },
    )
    .optional()
}

#[allow(clippy::too_many_arguments)]
fn persist_session_usage(
    tx: &Transaction<'_>,
    now: &str,
    session_id: &str,
    payload: &StatusPayload,
    metrics: &SessionMetrics,
    prev: Option<&StoredSession>,
    tracker_state: &SessionTrackerState,
    plan_tier: &str,
    model: &str,
    effort: &str,
) -> rusqlite::Result<Option<UsageDelta>> {
    let bucket_group = payload.active_quota_group().as_str();
    let model_id = payload.effective_model_name().unwrap_or("unknown");
    let reported_cost = if metrics.cost_is_local_estimate {
        None
    } else {
        metrics.cost_usd.filter(|c| c.is_finite() && *c >= 0.0)
    };

    let cur_in = to_i64(metrics.input_tokens);
    let cur_cr = to_i64(metrics.cache_read_tokens);
    let cur_cw = to_i64(metrics.cache_write_tokens);
    let cur_out = to_i64(metrics.output_tokens);

    let (d_in, d_cr, d_cw, d_out, d_cost, next_in, next_cr, next_cw, next_out, next_cost) =
        match prev {
            Some(p) if cur_out < p.output_token => {
                // Counter rewound (e.g., conversation restarted with same ID).
                (
                    cur_in,
                    cur_cr,
                    cur_cw,
                    cur_out,
                    reported_cost,
                    cur_in,
                    cur_cr,
                    cur_cw,
                    cur_out,
                    reported_cost,
                )
            }
            Some(p) => {
                let d_in = to_i64(to_u64(cur_in).saturating_sub(to_u64(p.input_token)));
                let d_cr = to_i64(to_u64(cur_cr).saturating_sub(to_u64(p.cache_read_token)));
                let d_cw = to_i64(to_u64(cur_cw).saturating_sub(to_u64(p.cache_write_token)));
                let d_out = to_i64(to_u64(cur_out).saturating_sub(to_u64(p.output_token)));
                let d_cost = reported_cost.map(|c| {
                    (c - p
                        .reported_cost_usd
                        .filter(|v| v.is_finite() && *v >= 0.0)
                        .unwrap_or(0.0))
                    .max(0.0)
                });
                (
                    d_in,
                    d_cr,
                    d_cw,
                    d_out,
                    d_cost,
                    cur_in.max(p.input_token),
                    cur_cr.max(p.cache_read_token),
                    cur_cw.max(p.cache_write_token),
                    cur_out,
                    reported_cost.or(p.reported_cost_usd),
                )
            }
            None => (
                cur_in,
                cur_cr,
                cur_cw,
                cur_out,
                reported_cost,
                cur_in,
                cur_cr,
                cur_cw,
                cur_out,
                reported_cost,
            ),
        };

    let mut recorded_delta = None;
    if d_in > 0 || d_cr > 0 || d_cw > 0 || d_out > 0 {
        let ref_cost = reference_cost_usd(
            model,
            to_u64(d_in),
            to_u64(d_cr),
            to_u64(d_cw),
            to_u64(d_out),
        );
        tx.execute(
            "INSERT INTO usage_event (
                recorded_at, session_id, bucket_group, model_id,
                delta_input_token, delta_cache_read_token, delta_cache_write_token,
                delta_output_token, delta_reported_cost_usd,
                plan_tier, model, effort, ref_cost_usd
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                now,
                session_id,
                bucket_group,
                model_id,
                d_in,
                d_cr,
                d_cw,
                d_out,
                d_cost,
                plan_tier,
                model,
                effort,
                ref_cost
            ],
        )?;
        recorded_delta = Some(UsageDelta {
            input_tokens: d_in,
            cache_read_tokens: d_cr,
            cache_write_tokens: d_cw,
            output_tokens: d_out,
            ref_cost_usd: ref_cost,
        });
    }

    let (lu_in, lu_out, lu_cr, lu_cw) = match tracker_state.last_usage {
        Some(u) => (
            Some(to_i64(u.input_tokens)),
            Some(to_i64(u.output_tokens)),
            Some(to_i64(u.cache_read_input_tokens)),
            Some(to_i64(u.cache_creation_input_tokens)),
        ),
        None => (None, None, None, None),
    };

    tx.execute(
        "INSERT INTO session (
            session_id, bucket_group, model_id,
            input_token, cache_read_token, cache_write_token, output_token,
            reported_cost_usd,
            last_total_input, last_total_output,
            last_usage_input, last_usage_output, last_usage_cache_read, last_usage_cache_write,
            sum_input, sum_cache_read, sum_cache_write, sum_output,
            working_started_at_ms, tps_output_tokens, tps_elapsed_ms, consumed_planner_count,
            created_at, updated_at
         ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8,
            ?9, ?10, ?11, ?12, ?13, ?14,
            ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?23
         )
         ON CONFLICT(session_id) DO UPDATE SET
            bucket_group = excluded.bucket_group,
            model_id = excluded.model_id,
            input_token = excluded.input_token,
            cache_read_token = excluded.cache_read_token,
            cache_write_token = excluded.cache_write_token,
            output_token = excluded.output_token,
            reported_cost_usd = excluded.reported_cost_usd,
            last_total_input = excluded.last_total_input,
            last_total_output = excluded.last_total_output,
            last_usage_input = excluded.last_usage_input,
            last_usage_output = excluded.last_usage_output,
            last_usage_cache_read = excluded.last_usage_cache_read,
            last_usage_cache_write = excluded.last_usage_cache_write,
            sum_input = excluded.sum_input,
            sum_cache_read = excluded.sum_cache_read,
            sum_cache_write = excluded.sum_cache_write,
            sum_output = excluded.sum_output,
            working_started_at_ms = excluded.working_started_at_ms,
            tps_output_tokens = excluded.tps_output_tokens,
            tps_elapsed_ms = excluded.tps_elapsed_ms,
            consumed_planner_count = excluded.consumed_planner_count,
            updated_at = excluded.updated_at",
        params![
            session_id,
            bucket_group,
            model_id,
            next_in,
            next_cr,
            next_cw,
            next_out,
            next_cost,
            to_i64(tracker_state.last_total_input),
            to_i64(tracker_state.last_total_output),
            lu_in,
            lu_out,
            lu_cr,
            lu_cw,
            to_i64(tracker_state.sum_input),
            to_i64(tracker_state.sum_cache_read),
            to_i64(tracker_state.sum_cache_write),
            to_i64(tracker_state.sum_output),
            tracker_state.working_started_at_ms.map(to_i64),
            to_i64(tracker_state.tps_output_tokens),
            to_i64(tracker_state.tps_elapsed_ms),
            to_i64(tracker_state.consumed_planner_count),
            now
        ],
    )?;

    Ok(recorded_delta)
}

fn record_tps_sample(
    tx: &Transaction<'_>,
    now: &str,
    session_id: &str,
    plan_tier: &str,
    model: &str,
    effort: &str,
    sample: crate::tracker::TpsTurnSample,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO tps_sample (
            recorded_at, session_id, plan_tier, model, effort, output_tokens, elapsed_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            now,
            session_id,
            plan_tier,
            model,
            effort,
            to_i64(sample.output_tokens),
            to_i64(sample.elapsed_ms)
        ],
    )?;
    Ok(())
}

fn bucket_group_for_bucket(bucket: &str, payload: &StatusPayload) -> &'static str {
    if bucket.starts_with("gemini") {
        "gemini"
    } else if bucket.starts_with("3p") {
        "3p"
    } else {
        payload.active_quota_group().as_str()
    }
}

const QUOTA_FRACTION_EPSILON: f64 = 1e-6;

fn ensure_and_reset_quota_cursors(
    tx: &Transaction<'_>,
    payload: &StatusPayload,
) -> rusqlite::Result<Vec<String>> {
    let mut reset_or_new_buckets = Vec::new();
    for (raw_bucket, info) in &payload.quota {
        let bucket = crate::payload::normalize_label(raw_bucket, "", 128);
        if bucket.is_empty() {
            continue;
        }
        let Some(fraction) = info
            .remaining_fraction
            .filter(|f| f.is_finite() && (0.0..=1.0).contains(f))
        else {
            continue;
        };
        let Some(reset_time) = info
            .reset_time
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let bucket_group = bucket_group_for_bucket(&bucket, payload);

        let existing: Option<(String, f64)> = tx
            .query_row(
                "SELECT reset_time, last_remaining_fraction FROM quota_bucket_cursor WHERE bucket = ?1",
                params![&bucket],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;

        match existing {
            None => {
                tx.execute(
                    "INSERT INTO quota_bucket_cursor (
                        bucket, bucket_group, reset_time, last_remaining_fraction
                     ) VALUES (?1, ?2, ?3, ?4)",
                    params![&bucket, bucket_group, reset_time, fraction],
                )?;
                reset_or_new_buckets.push(bucket);
            }
            Some((prev_reset, prev_fraction))
                if prev_reset != reset_time
                    || fraction - prev_fraction >= QUOTA_FRACTION_EPSILON =>
            {
                // Re-baseline cursor and discard unattributed pending usage across either a window
                // reset or a non-monotonic upward quota adjustment within the same reset window.
                tx.execute(
                    "UPDATE quota_bucket_cursor
                     SET bucket_group = ?2,
                         reset_time = ?3,
                         last_remaining_fraction = ?4,
                         pending_input_token = 0,
                         pending_cache_read_token = 0,
                         pending_cache_write_token = 0,
                         pending_output_token = 0,
                         pending_ref_cost_usd = 0.0,
                         pending_model = NULL,
                         pending_effort = NULL,
                         pending_mixed = 0
                     WHERE bucket = ?1",
                    params![bucket, bucket_group, reset_time, fraction],
                )?;
                reset_or_new_buckets.push(bucket.clone());
            }
            Some(_) => {}
        }
    }
    Ok(reset_or_new_buckets)
}

fn accumulate_usage_into_cursors(
    tx: &Transaction<'_>,
    bucket_group: &str,
    model: &str,
    effort: &str,
    delta: &UsageDelta,
    skip_buckets: &[String],
) -> rusqlite::Result<()> {
    let mut rows = Vec::new();
    {
        let mut stmt = tx.prepare(
            "SELECT bucket, pending_model, pending_effort, pending_mixed,
                    pending_input_token, pending_cache_read_token,
                    pending_cache_write_token, pending_output_token, pending_ref_cost_usd
             FROM quota_bucket_cursor
             WHERE bucket_group = ?1",
        )?;
        let iter = stmt.query_map(params![bucket_group], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, i64>(7)?,
                row.get::<_, f64>(8)?,
            ))
        })?;
        for item in iter {
            rows.push(item?);
        }
    }

    for (bucket, p_model, p_effort, p_mixed, p_in, p_cr, p_cw, p_out, p_cost) in rows {
        if skip_buckets.iter().any(|b| b == &bucket) {
            continue;
        }
        let (next_model, next_effort, next_mixed) = if p_mixed != 0 {
            (
                MIXED_ATTRIBUTION_KEY.to_string(),
                MIXED_ATTRIBUTION_KEY.to_string(),
                1_i64,
            )
        } else {
            match (p_model, p_effort) {
                (None, _) | (_, None) => (model.to_string(), effort.to_string(), 0_i64),
                (Some(prev_m), Some(prev_e)) if prev_m == model && prev_e == effort => {
                    (prev_m, prev_e, 0_i64)
                }
                _ => (
                    MIXED_ATTRIBUTION_KEY.to_string(),
                    MIXED_ATTRIBUTION_KEY.to_string(),
                    1_i64,
                ),
            }
        };

        let next_in = to_i64(to_u64(p_in).saturating_add(to_u64(delta.input_tokens)));
        let next_cr = to_i64(to_u64(p_cr).saturating_add(to_u64(delta.cache_read_tokens)));
        let next_cw = to_i64(to_u64(p_cw).saturating_add(to_u64(delta.cache_write_tokens)));
        let next_out = to_i64(to_u64(p_out).saturating_add(to_u64(delta.output_tokens)));
        let next_cost = p_cost + delta.ref_cost_usd.max(0.0);

        tx.execute(
            "UPDATE quota_bucket_cursor
             SET pending_input_token = ?1,
                 pending_cache_read_token = ?2,
                 pending_cache_write_token = ?3,
                 pending_output_token = ?4,
                 pending_ref_cost_usd = ?5,
                 pending_model = ?6,
                 pending_effort = ?7,
                 pending_mixed = ?8
             WHERE bucket = ?9",
            params![
                next_in,
                next_cr,
                next_cw,
                next_out,
                next_cost,
                next_model,
                next_effort,
                next_mixed,
                bucket
            ],
        )?;
    }

    Ok(())
}

fn settle_quota_attributions(
    tx: &Transaction<'_>,
    now: &str,
    payload: &StatusPayload,
    plan_tier: &str,
) -> rusqlite::Result<()> {
    for (raw_bucket, info) in &payload.quota {
        let bucket = crate::payload::normalize_label(raw_bucket, "", 128);
        if bucket.is_empty() {
            continue;
        }
        let Some(fraction) = info
            .remaining_fraction
            .filter(|f| f.is_finite() && (0.0..=1.0).contains(f))
        else {
            continue;
        };
        let Some(reset_time) = info
            .reset_time
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };

        #[allow(clippy::type_complexity)]
        let cursor: Option<(
            String,
            f64,
            i64,
            i64,
            i64,
            i64,
            f64,
            Option<String>,
            Option<String>,
            i64,
        )> = tx
            .query_row(
                "SELECT reset_time, last_remaining_fraction,
                        pending_input_token, pending_cache_read_token,
                        pending_cache_write_token, pending_output_token,
                        pending_ref_cost_usd, pending_model, pending_effort, pending_mixed
                 FROM quota_bucket_cursor
                 WHERE bucket = ?1",
                params![&bucket],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                        row.get(8)?,
                        row.get(9)?,
                    ))
                },
            )
            .optional()?;

        if let Some((
            cursor_reset,
            last_fraction,
            p_in,
            p_cr,
            p_cw,
            p_out,
            p_cost,
            p_model,
            p_effort,
            p_mixed,
        )) = cursor
            && cursor_reset == reset_time
            && last_fraction - fraction >= QUOTA_FRACTION_EPSILON
        {
            let delta_q = last_fraction - fraction;
            let total_pending_tokens = to_u64(p_in)
                .saturating_add(to_u64(p_cr))
                .saturating_add(to_u64(p_cw))
                .saturating_add(to_u64(p_out));
            if delta_q.is_finite() && delta_q > 0.0 && total_pending_tokens > 0 {
                let (attr_model, attr_effort) = if p_mixed != 0 {
                    (MIXED_ATTRIBUTION_KEY, MIXED_ATTRIBUTION_KEY)
                } else {
                    (
                        p_model.as_deref().unwrap_or(MIXED_ATTRIBUTION_KEY),
                        p_effort.as_deref().unwrap_or(MIXED_ATTRIBUTION_KEY),
                    )
                };
                tx.execute(
                    "INSERT INTO quota_attribution (
                        recorded_at, plan_tier, bucket, attributed_model, attributed_effort,
                        delta_quota_fraction, input_tokens, cache_read_tokens,
                        cache_write_tokens, output_tokens, ref_cost_usd
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                    params![
                        now,
                        plan_tier,
                        &bucket,
                        attr_model,
                        attr_effort,
                        delta_q,
                        p_in,
                        p_cr,
                        p_cw,
                        p_out,
                        p_cost.max(0.0)
                    ],
                )?;
            }

            tx.execute(
                "UPDATE quota_bucket_cursor
                 SET last_remaining_fraction = ?1,
                     pending_input_token = 0,
                     pending_cache_read_token = 0,
                     pending_cache_write_token = 0,
                     pending_output_token = 0,
                     pending_ref_cost_usd = 0.0,
                     pending_model = NULL,
                     pending_effort = NULL,
                     pending_mixed = 0
                 WHERE bucket = ?2",
                params![fraction, &bucket],
            )?;
        }
    }
    Ok(())
}

fn earliest_recorded_timestamp(conn: &Connection) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT MIN(ts) FROM (
            SELECT MIN(recorded_at) AS ts FROM usage_event
            UNION ALL
            SELECT MIN(recorded_at) AS ts FROM tps_sample
            UNION ALL
            SELECT MIN(recorded_at) AS ts FROM quota_attribution
         )",
        [],
        |row| row.get(0),
    )
}

fn query_usage_by_plan_model(
    conn: &Connection,
    start_iso: &str,
    end_iso: &str,
) -> rusqlite::Result<Vec<UsageByPlanModel>> {
    let mut stmt = conn.prepare(
        "SELECT plan_tier, model, effort,
                delta_input_token, delta_cache_read_token,
                delta_cache_write_token, delta_output_token, ref_cost_usd
         FROM usage_event
         WHERE recorded_at >= ?1 AND recorded_at < ?2",
    )?;
    #[allow(clippy::type_complexity)]
    let rows = stmt.query_map(params![start_iso, end_iso], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            to_u64(row.get(3)?),
            to_u64(row.get(4)?),
            to_u64(row.get(5)?),
            to_u64(row.get(6)?),
            row.get::<_, f64>(7)?,
        ))
    })?;

    let mut grouped: BTreeMap<(String, String, String), UsageByPlanModel> = BTreeMap::new();
    for item in rows {
        let (raw_plan, raw_model, raw_effort, d_in, d_cr, d_cw, d_out, ref_cost) = item?;
        let plan_tier = crate::payload::normalize_label(&raw_plan, "unknown", 128);
        let model = crate::payload::normalize_label(&raw_model, "unknown", 128);
        let effort = crate::payload::normalize_label(&raw_effort, "default", 128);
        let entry = grouped
            .entry((plan_tier.clone(), model.clone(), effort.clone()))
            .or_insert_with(|| UsageByPlanModel {
                plan_tier,
                model,
                effort,
                events: 0,
                input_tokens: 0,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                output_tokens: 0,
                reference_cost_usd: 0.0,
            });
        entry.events = entry.events.saturating_add(1);
        entry.input_tokens = entry.input_tokens.saturating_add(d_in);
        entry.cache_read_tokens = entry.cache_read_tokens.saturating_add(d_cr);
        entry.cache_write_tokens = entry.cache_write_tokens.saturating_add(d_cw);
        entry.output_tokens = entry.output_tokens.saturating_add(d_out);
        if ref_cost.is_finite() && ref_cost >= 0.0 {
            entry.reference_cost_usd += ref_cost;
        }
    }
    Ok(grouped.into_values().collect())
}

fn query_tps_by_model_effort(
    conn: &Connection,
    start_iso: &str,
    end_iso: &str,
) -> rusqlite::Result<Vec<TpsByModelEffort>> {
    let mut stmt = conn.prepare(
        "SELECT model, effort, output_tokens, elapsed_ms
         FROM tps_sample
         WHERE recorded_at >= ?1 AND recorded_at < ?2",
    )?;
    let rows = stmt.query_map(params![start_iso, end_iso], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            to_u64(row.get(2)?),
            to_u64(row.get(3)?),
        ))
    })?;

    let mut grouped: BTreeMap<(String, String), TpsByModelEffort> = BTreeMap::new();
    for item in rows {
        let (raw_model, raw_effort, out_tok, elapsed_ms) = item?;
        let Some(bucket_idx) = tps_bucket_index(out_tok, elapsed_ms) else {
            continue;
        };
        let model = crate::payload::normalize_label(&raw_model, "unknown", 128);
        let effort = crate::payload::normalize_label(&raw_effort, "default", 128);
        let entry = grouped
            .entry((model.clone(), effort.clone()))
            .or_insert_with(|| TpsByModelEffort {
                model,
                effort,
                sample_count: 0,
                output_tokens: 0,
                elapsed_ms: 0,
                histogram: [0; TPS_HISTOGRAM_BUCKETS],
            });
        entry.sample_count = entry.sample_count.saturating_add(1);
        entry.output_tokens = entry.output_tokens.saturating_add(out_tok);
        entry.elapsed_ms = entry.elapsed_ms.saturating_add(elapsed_ms);
        entry.histogram[bucket_idx] = entry.histogram[bucket_idx].saturating_add(1);
    }

    Ok(grouped.into_values().collect())
}

fn query_quota_by_plan_bucket_model(
    conn: &Connection,
    start_iso: &str,
    end_iso: &str,
) -> rusqlite::Result<Vec<QuotaByPlanBucketModel>> {
    let mut stmt = conn.prepare(
        "SELECT plan_tier, bucket, attributed_model, attributed_effort,
                delta_quota_fraction, input_tokens, cache_read_tokens,
                cache_write_tokens, output_tokens, ref_cost_usd
         FROM quota_attribution
         WHERE recorded_at >= ?1 AND recorded_at < ?2",
    )?;
    #[allow(clippy::type_complexity)]
    let rows = stmt.query_map(params![start_iso, end_iso], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, f64>(4)?,
            to_u64(row.get(5)?),
            to_u64(row.get(6)?),
            to_u64(row.get(7)?),
            to_u64(row.get(8)?),
            row.get::<_, f64>(9)?,
        ))
    })?;

    let mut grouped: BTreeMap<(String, String, String, String), QuotaByPlanBucketModel> =
        BTreeMap::new();
    for item in rows {
        let (
            raw_plan,
            raw_bucket,
            raw_model,
            raw_effort,
            delta_q,
            in_tok,
            cr_tok,
            cw_tok,
            out_tok,
            ref_cost,
        ) = item?;
        if !delta_q.is_finite() || delta_q <= 0.0 || delta_q > 1.0 {
            continue;
        }
        let bucket = crate::payload::normalize_label(&raw_bucket, "", 128);
        if bucket.is_empty() {
            continue;
        }
        let plan_tier = crate::payload::normalize_label(&raw_plan, "unknown", 128);
        let attributed_model = crate::payload::normalize_label(&raw_model, "unknown", 128);
        let attributed_effort = crate::payload::normalize_label(&raw_effort, "default", 128);
        let safe_ref_cost = if ref_cost.is_finite() && ref_cost >= 0.0 {
            ref_cost
        } else {
            0.0
        };
        let key = (
            plan_tier.clone(),
            bucket.clone(),
            attributed_model.clone(),
            attributed_effort.clone(),
        );
        let entry = grouped
            .entry(key)
            .or_insert_with(|| QuotaByPlanBucketModel {
                plan_tier,
                bucket,
                attributed_model,
                attributed_effort,
                slices: 0,
                delta_quota_fraction: 0.0,
                input_tokens: 0,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                output_tokens: 0,
                reference_cost_usd: 0.0,
                cost_per_pct_histogram: [0; QUOTA_COST_HISTOGRAM_BUCKETS],
            });
        entry.slices = entry.slices.saturating_add(1);
        entry.delta_quota_fraction += delta_q;
        entry.input_tokens = entry.input_tokens.saturating_add(in_tok);
        entry.cache_read_tokens = entry.cache_read_tokens.saturating_add(cr_tok);
        entry.cache_write_tokens = entry.cache_write_tokens.saturating_add(cw_tok);
        entry.output_tokens = entry.output_tokens.saturating_add(out_tok);
        entry.reference_cost_usd += safe_ref_cost;
        if let Some(bucket_idx) = quota_cost_bucket_index(delta_q, safe_ref_cost) {
            entry.cost_per_pct_histogram[bucket_idx] =
                entry.cost_per_pct_histogram[bucket_idx].saturating_add(1);
        }
    }

    Ok(grouped.into_values().collect())
}

fn to_i64(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

fn to_u64(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

pub fn current_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

pub fn iso8601_from_unix_secs(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let hour = rem / 3_600;
    let minute = (rem % 3_600) / 60;
    let second = rem % 60;
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

pub fn unix_secs_from_iso8601(ts: &str) -> Option<u64> {
    unix_millis_from_iso8601(ts).map(|ms| ms / 1_000)
}

/// Parses an ISO-8601 UTC (`Z`) or numeric-offset (`+HH:MM`/`-HH:MM`) timestamp with optional
/// fractional seconds into Unix milliseconds.
pub fn unix_millis_from_iso8601(ts: &str) -> Option<u64> {
    let trimmed = ts.trim();
    let (core, offset_secs) = if let Some(rest) = trimmed
        .strip_suffix('Z')
        .or_else(|| trimmed.strip_suffix('z'))
    {
        (rest, 0_i64)
    } else if trimmed.len() >= 6
        && matches!(trimmed.as_bytes()[trimmed.len() - 6], b'+' | b'-')
        && trimmed.as_bytes()[trimmed.len() - 3] == b':'
    {
        let (head, off) = trimmed.split_at(trimmed.len() - 6);
        let sign = if off.starts_with('-') { -1_i64 } else { 1_i64 };
        let oh: i64 = off[1..3].parse().ok()?;
        let om: i64 = off[4..6].parse().ok()?;
        if oh >= 24 || om >= 60 {
            return None;
        }
        (head, sign * (oh * 3_600 + om * 60))
    } else {
        return None;
    };

    let (date_part, time_part) = core.split_once('T')?;
    let mut d_parts = date_part.split('-');
    let year: i64 = d_parts.next()?.parse().ok()?;
    let month: u32 = d_parts.next()?.parse().ok()?;
    let day: u32 = d_parts.next()?.parse().ok()?;
    if d_parts.next().is_some()
        || !(1..=12).contains(&month)
        || day == 0
        || day > max_day_in_month(year, month)
    {
        return None;
    }

    let mut t_parts = time_part.split(':');
    let hour: u64 = t_parts.next()?.parse().ok()?;
    let minute: u64 = t_parts.next()?.parse().ok()?;
    let second_raw = t_parts.next()?;
    let (sec_str, ms_part) = match second_raw.split_once('.') {
        Some((sec_s, frac)) => {
            if frac.is_empty() || !frac.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let mut ms = 0_u64;
            let mut mult = 100_u64;
            for b in frac.bytes().take(3) {
                ms += u64::from(b - b'0') * mult;
                mult /= 10;
            }
            (sec_s, ms)
        }
        None => (second_raw, 0_u64),
    };
    let second: u64 = sec_str.parse().ok()?;
    if t_parts.next().is_some() || hour >= 24 || minute >= 60 || second >= 60 {
        return None;
    }

    let days = days_from_civil(year, month, day);
    let local_secs = days
        .checked_mul(86_400)?
        .checked_add((hour * 3_600 + minute * 60 + second) as i64)?;
    let utc_secs = local_secs.checked_sub(offset_secs)?;
    if utc_secs < 0 {
        return None;
    }
    (utc_secs as u64).checked_mul(1_000)?.checked_add(ms_part)
}

/// Howard Hinnant's civil-from-days algorithm (days since 1970-01-01).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = y + if m <= 2 { 1 } else { 0 };
    (year, m, d)
}

fn max_day_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            let is_leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
            if is_leap { 29 } else { 28 }
        }
        _ => 0,
    }
}

/// Howard Hinnant's days-from-civil inverse algorithm (days since 1970-01-01).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = year - if month <= 2 { 1 } else { 0 };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u32;
    let m = month;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + (doe as i64) - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tracker::TpsTurnSample;

    fn temp_db_path(tag: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        std::env::temp_dir().join(format!("agy-status-db-{tag}-{nanos}.db"))
    }

    fn cleanup_db(db: &Path) {
        let _ = fs::remove_file(db);
        let _ = fs::remove_file(format!("{}-wal", db.display()));
        let _ = fs::remove_file(format!("{}-shm", db.display()));
    }

    #[test]
    fn records_singular_tables_and_deduplicates_identical_ticks() {
        let db = temp_db_path("dedup");
        let payload: StatusPayload = serde_json::from_str(
            r#"{
              "session_id": "sess-db-1",
              "plan_tier": "Google AI Ultra",
              "model": { "id": "Gemini 4 Hydrogen (High)" },
              "quota": {
                "gemini-5h": { "remaining_fraction": 0.95, "reset_time": "2026-11-14T00:00:00Z" },
                "gemini-weekly": { "remaining_fraction": 0.90, "reset_time": "2026-11-20T00:00:00Z" }
              }
            }"#,
        )
        .unwrap();

        let m1 = SessionMetrics {
            input_tokens: 1_000,
            cache_read_tokens: 500,
            cache_write_tokens: 0,
            output_tokens: 50,
            ..Default::default()
        };
        record_snapshot(&db, &payload, &m1).unwrap();
        // Repeat identical snapshot: should not create a second usage_event row.
        record_snapshot(&db, &payload, &m1).unwrap();

        let mut payload2: StatusPayload = serde_json::from_str(
            r#"{
              "session_id": "sess-db-1",
              "plan_tier": "Google AI Ultra",
              "model": { "id": "Gemini 4 Hydrogen (High)" },
              "quota": {
                "gemini-5h": { "remaining_fraction": 0.91, "reset_time": "2026-11-14T00:00:00Z" },
                "gemini-weekly": { "remaining_fraction": 0.89, "reset_time": "2026-11-20T00:00:00Z" }
              }
            }"#,
        )
        .unwrap();
        payload2.version = Some("1.5.8".to_string());
        let m2 = SessionMetrics {
            input_tokens: 1_400,
            cache_read_tokens: 1_500,
            cache_write_tokens: 0,
            output_tokens: 120,
            ..Default::default()
        };
        record_snapshot(&db, &payload2, &m2).unwrap();

        let conn = Connection::open(&db).unwrap();
        let event_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM usage_event", [], |r| r.get(0))
            .unwrap();
        assert_eq!(event_count, 2);

        let (sum_in, sum_cr, sum_out): (i64, i64, i64) = conn
            .query_row(
                "SELECT SUM(delta_input_token), SUM(delta_cache_read_token), SUM(delta_output_token) FROM usage_event",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!((sum_in, sum_cr, sum_out), (1_400, 1_500, 120));

        let user_version: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(user_version, SCHEMA_VERSION);

        cleanup_db(&db);
    }

    #[test]
    fn queries_stats_with_quota_attribution() {
        let db = temp_db_path("stats-attribution");

        // Tick 0: an earlier hour event to verify half-open / closed hour boundary filtering.
        let p0: StatusPayload = serde_json::from_str(
            r#"{
              "session_id": "old-sess",
              "model": { "id": "Claude Opus 4.6 (High)" }
            }"#,
        )
        .unwrap();
        let m0 = SessionMetrics {
            input_tokens: 10_000,
            output_tokens: 1_000,
            ..Default::default()
        };
        record_snapshot_at(&db, &p0, &m0, "2026-11-14T14:30:00Z").unwrap();

        // Tick 1: baseline quota = 0.90 (new session).
        let p1: StatusPayload = serde_json::from_str(
            r#"{
              "session_id": "sess-stat-1",
              "plan_tier": "Google AI Ultra",
              "model": { "id": "Claude Opus 4.6", "effort": "high" },
              "quota": {
                "3p-5h": { "remaining_fraction": 0.90, "reset_time": "2026-11-14T20:00:00Z" }
              }
            }"#,
        )
        .unwrap();
        let m1 = SessionMetrics {
            input_tokens: 1_000,
            cache_read_tokens: 2_000,
            output_tokens: 250,
            last_turn_tps_sample: Some(TpsTurnSample {
                output_tokens: 250,
                elapsed_ms: 5_000,
            }),
            ..Default::default()
        };
        record_snapshot_at(&db, &p1, &m1, "2026-11-14T15:10:00Z").unwrap();

        // Tick 2: quota drops to 0.88 on same model -> single-model clean attribution.
        let p2: StatusPayload = serde_json::from_str(
            r#"{
              "session_id": "sess-stat-1",
              "plan_tier": "Google AI Ultra",
              "model": { "id": "Claude Opus 4.6 (High)", "effort": "high" },
              "quota": {
                "3p-5h": { "remaining_fraction": 0.88, "reset_time": "2026-11-14T20:00:00Z" }
              }
            }"#,
        )
        .unwrap();
        let m2 = SessionMetrics {
            input_tokens: 2_000,
            cache_read_tokens: 4_000,
            output_tokens: 500,
            last_turn_tps_sample: Some(TpsTurnSample {
                output_tokens: 250,
                elapsed_ms: 4_000,
            }),
            ..Default::default()
        };
        record_snapshot_at(&db, &p2, &m2, "2026-11-14T15:20:00Z").unwrap();

        // Tick 3 & 4: mix two models before next quota drop -> __mixed__.
        let p3: StatusPayload = serde_json::from_str(
            r#"{
              "session_id": "sess-stat-1",
              "plan_tier": "Google AI Ultra",
              "model": { "id": "Claude Sonnet 4.6", "effort": "medium" },
              "quota": {
                "3p-5h": { "remaining_fraction": 0.88, "reset_time": "2026-11-14T20:00:00Z" }
              }
            }"#,
        )
        .unwrap();
        let m3 = SessionMetrics {
            input_tokens: 2_500,
            cache_read_tokens: 4_000,
            output_tokens: 650,
            ..Default::default()
        };
        record_snapshot_at(&db, &p3, &m3, "2026-11-14T15:25:00Z").unwrap();

        let p4: StatusPayload = serde_json::from_str(
            r#"{
              "session_id": "sess-stat-1",
              "plan_tier": "Google AI Ultra",
              "model": { "id": "Claude Opus 4.6", "effort": "high" },
              "quota": {
                "3p-5h": { "remaining_fraction": 0.86, "reset_time": "2026-11-14T20:00:00Z" }
              }
            }"#,
        )
        .unwrap();
        let m4 = SessionMetrics {
            input_tokens: 3_000,
            cache_read_tokens: 4_000,
            output_tokens: 800,
            ..Default::default()
        };
        record_snapshot_at(&db, &p4, &m4, "2026-11-14T15:30:00Z").unwrap();

        let start_15 = unix_secs_from_iso8601("2026-11-14T15:00:00Z").unwrap();
        let end_16 = unix_secs_from_iso8601("2026-11-14T16:00:00Z").unwrap();
        let report_1h = query_stats_report(&db, Some(start_15), end_16).unwrap();

        // Excludes 14:30 legacy event in 15:00-16:00 query, merges normalized Opus rows.
        let opus_usage = report_1h
            .usage_by_plan_model
            .iter()
            .find(|u| u.model == "Claude Opus 4.6")
            .unwrap();
        assert_eq!(opus_usage.effort, "high");
        assert_eq!(opus_usage.events, 3);
        assert_eq!(opus_usage.input_tokens, 2_500);

        let opus_tps = &report_1h.tps_by_model_effort[0];
        assert_eq!(opus_tps.model, "Claude Opus 4.6");
        assert_eq!(opus_tps.sample_count, 2);
        assert_eq!(opus_tps.output_tokens, 500);
        assert_eq!(opus_tps.elapsed_ms, 9_000);

        let clean_quota = report_1h
            .quota_by_plan_bucket_model
            .iter()
            .find(|q| q.attributed_model == "Claude Opus 4.6")
            .unwrap();
        assert!((clean_quota.delta_quota_fraction - 0.02).abs() < 1e-6);
        // Pre-baseline tokens from Tick 1 (1_000 in, 2_000 cache_read, 250 out) must NOT be absorbed
        // into the 0.90 -> 0.88 drop slice; only Tick 2's delta is attributed.
        assert_eq!(clean_quota.input_tokens, 1_000);
        assert_eq!(clean_quota.cache_read_tokens, 2_000);
        assert_eq!(clean_quota.output_tokens, 250);

        let mixed_quota = report_1h
            .quota_by_plan_bucket_model
            .iter()
            .find(|q| q.attributed_model == MIXED_ATTRIBUTION_KEY)
            .unwrap();
        assert!((mixed_quota.delta_quota_fraction - 0.02).abs() < 1e-6);

        // Full history includes the backfilled legacy row too.
        let all_report = query_stats_report(&db, None, end_16).unwrap();
        let opus_all = all_report
            .usage_by_plan_model
            .iter()
            .find(|u| u.plan_tier == "unknown" && u.model == "Claude Opus 4.6")
            .unwrap();
        assert_eq!(opus_all.effort, "high");
        assert!(opus_all.reference_cost_usd > 0.0);

        cleanup_db(&db);
    }

    #[test]
    fn concurrent_first_time_client_token_creation_converges_on_single_token() {
        let db = temp_db_path("concurrent-client-token");
        let worker_count = 8;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(worker_count));
        let handles: Vec<_> = (0..worker_count)
            .map(|_| {
                let db_path = db.clone();
                let gate = barrier.clone();
                std::thread::spawn(move || {
                    gate.wait();
                    get_or_create_client_token(&db_path).unwrap()
                })
            })
            .collect();

        let tokens: Vec<String> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let first = &tokens[0];
        assert!(!first.is_empty());
        assert!(tokens.iter().all(|t| t == first));

        cleanup_db(&db);
    }

    #[test]
    fn concurrent_auto_upload_claim_grants_only_one_winner_and_iso8601_preserves_millis() {
        let base = unix_secs_from_iso8601("2026-11-14T19:00:40Z").unwrap();
        assert_eq!(
            unix_millis_from_iso8601("2026-11-14T19:00:40.95Z"),
            Some(base * 1_000 + 950)
        );
        assert_eq!(
            unix_millis_from_iso8601("2026-11-14T19:00:40.005Z"),
            Some(base * 1_000 + 5)
        );
        assert_eq!(
            unix_millis_from_iso8601("2026-11-15T03:00:40.250+08:00"),
            Some(base * 1_000 + 250)
        );
        assert_eq!(unix_millis_from_iso8601("2026-11-14T19:00:40.Z"), None);

        let db = temp_db_path("concurrent-auto-claim");
        let worker_count = 8;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(worker_count));
        let now_unix = 1_794_682_620; // arbitrary second
        let handles: Vec<_> = (0..worker_count)
            .map(|_| {
                let db_path = db.clone();
                let gate = barrier.clone();
                std::thread::spawn(move || {
                    gate.wait();
                    try_claim_auto_upload_slot(
                        &db_path,
                        now_unix,
                        "last_uploaded_until_unix",
                        "last_auto_attempt_unix",
                        |_, last_attempt| now_unix.saturating_sub(last_attempt) >= 300,
                    )
                    .unwrap()
                })
            })
            .collect();

        let winners: usize = handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .filter(|&won| won)
            .count();
        assert_eq!(winners, 1);

        cleanup_db(&db);
    }

    #[test]
    fn non_monotonic_quota_upward_jump_rebaselines_cursor_and_clears_pending() {
        let db = temp_db_path("quota-upward-rebaseline");
        let make_payload = |fraction: f64| -> StatusPayload {
            serde_json::from_str(&format!(
                r#"{{
                  "session_id": "sess-quota-up",
                  "plan_tier": "Google AI Ultra",
                  "model": {{ "id": "Claude Opus 4.6 (High)" }},
                  "quota": {{
                    "3p-5h": {{ "remaining_fraction": {fraction}, "reset_time": "2026-11-15T18:00:00Z" }}
                  }}
                }}"#
            ))
            .unwrap()
        };
        let make_metrics = |tokens: u64| SessionMetrics {
            input_tokens: tokens,
            output_tokens: tokens,
            ..Default::default()
        };

        // 1. Initial tick establishes 0.80 baseline; initial tokens create no pending backlog.
        record_snapshot_at(
            &db,
            &make_payload(0.80),
            &make_metrics(100),
            "2026-11-15T14:00:00Z",
        )
        .unwrap();

        // 2. Same quota (0.80) with new usage puts 100 in / 100 out into pending.
        record_snapshot_at(
            &db,
            &make_payload(0.80),
            &make_metrics(200),
            "2026-11-15T14:01:00Z",
        )
        .unwrap();

        // 3. Upward quota adjustment (0.80 -> 0.90) in the same reset window must clear stale
        //    pending tokens and re-baseline the cursor to 0.90.
        record_snapshot_at(
            &db,
            &make_payload(0.90),
            &make_metrics(300),
            "2026-11-15T14:02:00Z",
        )
        .unwrap();

        let conn = Connection::open(&db).unwrap();
        let (last_frac, p_in, p_out): (f64, i64, i64) = conn
            .query_row(
                "SELECT last_remaining_fraction, pending_input_token, pending_output_token
                 FROM quota_bucket_cursor WHERE bucket = '3p-5h'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert!((last_frac - 0.90).abs() < 1e-9);
        assert_eq!((p_in, p_out), (0, 0));

        // 4. Subsequent drop (0.90 -> 0.85, still above old 0.80) must settle delta=0.05 against
        //    the 100/100 tokens consumed after the 0.90 baseline was established.
        record_snapshot_at(
            &db,
            &make_payload(0.85),
            &make_metrics(400),
            "2026-11-15T14:03:00Z",
        )
        .unwrap();

        let (slice_count, settled_delta, settled_in, settled_out): (i64, f64, i64, i64) = conn
            .query_row(
                "SELECT COUNT(*), COALESCE(SUM(delta_quota_fraction), 0.0),
                        COALESCE(SUM(input_tokens), 0), COALESCE(SUM(output_tokens), 0)
                 FROM quota_attribution",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(slice_count, 1);
        assert!((settled_delta - 0.05).abs() < 1e-9);
        assert_eq!((settled_in, settled_out), (100, 100));

        cleanup_db(&db);
    }

    #[test]
    fn tps_sample_is_idempotent_across_repeated_and_concurrent_identical_ticks() {
        let db = temp_db_path("tps-idempotent");
        let base_ms = unix_secs_from_iso8601("2026-11-15T14:10:00Z").unwrap() * 1_000;
        let working_payload: StatusPayload = serde_json::from_str(
            r#"{
              "session_id": "sess-tps-dedup",
              "plan_tier": "Google AI Ultra",
              "agent_state": "working",
              "model": { "id": "Claude Opus 4.6 (High)" },
              "context_window": { "total_input_tokens": 1000, "total_output_tokens": 0 }
            }"#,
        )
        .unwrap();
        let done_payload: StatusPayload = serde_json::from_str(
            r#"{
              "session_id": "sess-tps-dedup",
              "plan_tier": "Google AI Ultra",
              "agent_state": "idle",
              "model": { "id": "Claude Opus 4.6 (High)" },
              "context_window": {
                "total_input_tokens": 1000,
                "total_output_tokens": 250,
                "current_usage": { "input_tokens": 1000, "output_tokens": 250 }
              }
            }"#,
        )
        .unwrap();

        resolve_and_record_at(&db, &working_payload, None, base_ms, "2026-11-15T14:10:00Z")
            .unwrap();

        // 8 concurrent statusLine processes finish simultaneously; SQLite IMMEDIATE must admit 1 TPS row.
        let worker_count = 8;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(worker_count));
        std::thread::scope(|s| {
            for _ in 0..worker_count {
                s.spawn(|| {
                    barrier.wait();
                    resolve_and_record_at(
                        &db,
                        &done_payload,
                        None,
                        base_ms + 5_000,
                        "2026-11-15T14:10:05Z",
                    )
                    .unwrap();
                });
            }
        });

        // Subsequent repeated idle tick must also avoid duplicate usage/TPS rows.
        resolve_and_record_at(
            &db,
            &done_payload,
            None,
            base_ms + 10_000,
            "2026-11-15T14:10:10Z",
        )
        .unwrap();

        let conn = Connection::open(&db).unwrap();
        let usage_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM usage_event", [], |r| r.get(0))
            .unwrap();
        let tps_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM tps_sample", [], |r| r.get(0))
            .unwrap();
        assert_eq!(usage_count, 2); // working tick (1000 in) + done tick (250 out)
        assert_eq!(tps_count, 1);

        cleanup_db(&db);
    }

    #[test]
    fn tps_sample_records_when_transcript_output_led_prior_working_tick_and_ignores_invalid_quota()
    {
        let db = temp_db_path("skewed-tps-and-quota");
        let transcript = db.with_extension("transcript.jsonl");
        fs::write(
            &transcript,
            "{\"type\":\"PLANNER_RESPONSE\",\"input_tokens\":1000,\"output_tokens\":650}\n",
        )
        .unwrap();
        let base_ms = unix_secs_from_iso8601("2026-11-15T14:10:00Z").unwrap() * 1_000;

        let working_payload: StatusPayload = serde_json::from_str(&format!(
            r#"{{
              "session_id": "sess-skew",
              "plan_tier": "Google AI Ultra",
              "agent_state": "working",
              "transcript_path": {},
              "model": {{ "id": "Claude Opus 4.6 (High)" }},
              "context_window": {{ "total_input_tokens": 1000, "total_output_tokens": 500 }},
              "quota": {{
                "gemini-5h": {{ "remainingFraction": 1.25, "resetTime": "2026-11-15T20:00:00Z" }},
                "3p-5h": {{ "remainingFraction": 0.5, "resetTime": "2026-11-15T20:00:00Z" }}
              }}
            }}"#,
            serde_json::to_string(&transcript.to_string_lossy().into_owned()).unwrap()
        ))
        .unwrap();

        let idle_payload: StatusPayload = serde_json::from_str(&format!(
            r#"{{
              "session_id": "sess-skew",
              "plan_tier": "Google AI Ultra",
              "agent_state": "idle",
              "transcript_path": {},
              "model": {{ "id": "Claude Opus 4.6 (High)" }},
              "context_window": {{ "total_input_tokens": 1000, "total_output_tokens": 650 }},
              "quota": {{
                "gemini-5h": {{ "remainingFraction": 1.25, "resetTime": "2026-11-15T20:00:00Z" }},
                "3p-5h": {{ "remainingFraction": 0.5, "resetTime": "2026-11-15T20:00:00Z" }}
              }}
            }}"#,
            serde_json::to_string(&transcript.to_string_lossy().into_owned()).unwrap()
        ))
        .unwrap();

        // Tick 1 ("working"): transcript already reports output_tokens=650 while cw is 500.
        let m1 =
            resolve_and_record_at(&db, &working_payload, None, base_ms, "2026-11-15T14:10:00Z")
                .unwrap();
        assert_eq!(m1.output_tokens, 650);
        assert_eq!(m1.last_turn_tps_sample, None);

        // Tick 2 ("idle"): cw catches up to 650 (+150 output delta in 2000ms), even though total
        // output_tokens remains 650 (usage delta output == 0), TPS turn sample still records once.
        let m2 = resolve_and_record_at(
            &db,
            &idle_payload,
            None,
            base_ms + 2_000,
            "2026-11-15T14:10:02Z",
        )
        .unwrap();
        assert_eq!(
            m2.last_turn_tps_sample,
            Some(TpsTurnSample {
                output_tokens: 150,
                elapsed_ms: 2_000,
            })
        );

        // Repeat identical tick -> must remain deduplicated.
        resolve_and_record_at(
            &db,
            &idle_payload,
            None,
            base_ms + 5_000,
            "2026-11-15T14:10:05Z",
        )
        .unwrap();

        let conn = Connection::open(&db).unwrap();
        let tps_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM tps_sample", [], |r| r.get(0))
            .unwrap();
        assert_eq!(tps_count, 1);

        // Out-of-range remainingFraction (1.25) must not create cursor rows.
        let bad_cursor_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM quota_bucket_cursor WHERE bucket = 'gemini-5h'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(bad_cursor_count, 0);

        // Calendar validation must reject impossible dates cleanly.
        assert_eq!(unix_millis_from_iso8601("2026-02-30T12:00:00Z"), None);
        assert_eq!(unix_millis_from_iso8601("2026-13-01T12:00:00Z"), None);
        assert!(unix_millis_from_iso8601("2024-02-29T12:00:00Z").is_some());

        let _ = fs::remove_file(&transcript);
        cleanup_db(&db);
    }

    #[test]
    fn counter_rewind_resets_session_cost_and_allows_new_tps_turn() {
        let db = temp_db_path("rewind-cost-tps");
        let payload: StatusPayload = serde_json::from_str(
            r#"{
              "session_id": "sess-rewind",
              "plan_tier": "Google AI Pro",
              "model": { "id": "Gemini 3 Pro" }
            }"#,
        )
        .unwrap();

        let m_before = SessionMetrics {
            input_tokens: 2_000,
            output_tokens: 500,
            cost_usd: Some(0.50),
            last_turn_tps_sample: Some(TpsTurnSample {
                output_tokens: 500,
                elapsed_ms: 10_000,
            }),
            ..Default::default()
        };
        record_snapshot_at(&db, &payload, &m_before, "2026-11-15T14:00:00Z").unwrap();

        // Rewind to lower output_tokens without cost reported: stale 0.50 must clear.
        let m_rewound = SessionMetrics {
            input_tokens: 300,
            output_tokens: 120,
            cost_usd: None,
            last_turn_tps_sample: Some(TpsTurnSample {
                output_tokens: 120,
                elapsed_ms: 3_000,
            }),
            ..Default::default()
        };
        record_snapshot_at(&db, &payload, &m_rewound, "2026-11-15T14:05:00Z").unwrap();

        let conn = Connection::open(&db).unwrap();
        let (stored_cost, tps_count): (Option<f64>, i64) = (
            conn.query_row(
                "SELECT reported_cost_usd FROM session WHERE session_id = 'sess-rewind'",
                [],
                |r| r.get(0),
            )
            .unwrap(),
            conn.query_row("SELECT COUNT(*) FROM tps_sample", [], |r| r.get(0))
                .unwrap(),
        );
        assert_eq!(stored_cost, None);
        assert_eq!(tps_count, 2);

        cleanup_db(&db);
    }
}
