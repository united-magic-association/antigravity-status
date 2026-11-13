use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};

use crate::payload::StatusPayload;
use crate::tracker::SessionMetrics;

const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS session (
    session_id        TEXT PRIMARY KEY,
    bucket_group      TEXT NOT NULL,
    model_id          TEXT NOT NULL,
    input_token       INTEGER NOT NULL DEFAULT 0,
    cache_read_token  INTEGER NOT NULL DEFAULT 0,
    cache_write_token INTEGER NOT NULL DEFAULT 0,
    output_token      INTEGER NOT NULL DEFAULT 0,
    reported_cost_usd REAL,
    created_at        TEXT NOT NULL,
    updated_at        TEXT NOT NULL
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
    delta_reported_cost_usd REAL
);

CREATE INDEX IF NOT EXISTS idx_usage_event_recorded_at ON usage_event (recorded_at);
CREATE INDEX IF NOT EXISTS idx_usage_event_session_id ON usage_event (session_id);

CREATE TABLE IF NOT EXISTS quota_window (
    bucket                  TEXT NOT NULL,
    reset_time              TEXT NOT NULL,
    first_seen_at           TEXT NOT NULL,
    last_seen_at            TEXT NOT NULL,
    max_remaining_fraction  REAL NOT NULL,
    min_remaining_fraction  REAL NOT NULL,
    last_remaining_fraction REAL NOT NULL,
    PRIMARY KEY (bucket, reset_time)
);
"#;

/// Persists session token deltas and quota window observations into SQLite (WAL mode).
pub fn record_snapshot(
    db_path: &Path,
    payload: &StatusPayload,
    metrics: &SessionMetrics,
) -> rusqlite::Result<()> {
    if let Some(parent) = db_path.parent()
        && !parent.as_os_str().is_empty()
    {
        let _ = fs::create_dir_all(parent);
    }

    let mut conn = Connection::open(db_path)?;
    conn.busy_timeout(Duration::from_millis(5000))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.execute_batch(SCHEMA_SQL)?;

    let now = utc_now_iso8601();
    let tx = conn.transaction()?;

    if let Some(sid) = payload.effective_session_id() {
        record_session_usage(&tx, &now, sid, payload, metrics)?;
    }

    record_quota_windows(&tx, &now, payload)?;

    tx.commit()
}

fn record_session_usage(
    tx: &rusqlite::Transaction<'_>,
    now: &str,
    session_id: &str,
    payload: &StatusPayload,
    metrics: &SessionMetrics,
) -> rusqlite::Result<()> {
    let bucket_group = payload.active_quota_group().as_str();
    let model_id = payload.effective_model_name().unwrap_or("unknown");
    let reported_cost = if metrics.cost_is_local_estimate {
        None
    } else {
        metrics.cost_usd
    };

    let prev: Option<(i64, i64, i64, i64, Option<f64>)> = tx
        .query_row(
            "SELECT input_token, cache_read_token, cache_write_token, output_token, reported_cost_usd
             FROM session WHERE session_id = ?1",
            params![session_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
        )
        .optional()?;

    let cur_in = to_i64(metrics.input_tokens);
    let cur_cr = to_i64(metrics.cache_read_tokens);
    let cur_cw = to_i64(metrics.cache_write_tokens);
    let cur_out = to_i64(metrics.output_tokens);

    let (d_in, d_cr, d_cw, d_out, d_cost) = match prev {
        Some((p_in, p_cr, p_cw, p_out, p_cost)) => {
            if cur_out < p_out {
                // Counter rewound (e.g., conversation restarted with same ID).
                (cur_in, cur_cr, cur_cw, cur_out, reported_cost)
            } else {
                let d_cost = reported_cost.map(|c| (c - p_cost.unwrap_or(0.0)).max(0.0));
                (
                    (cur_in - p_in).max(0),
                    (cur_cr - p_cr).max(0),
                    (cur_cw - p_cw).max(0),
                    (cur_out - p_out).max(0),
                    d_cost,
                )
            }
        }
        None => (cur_in, cur_cr, cur_cw, cur_out, reported_cost),
    };

    if d_in > 0 || d_cr > 0 || d_cw > 0 || d_out > 0 {
        tx.execute(
            "INSERT INTO usage_event (
                recorded_at, session_id, bucket_group, model_id,
                delta_input_token, delta_cache_read_token, delta_cache_write_token,
                delta_output_token, delta_reported_cost_usd
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![now, session_id, bucket_group, model_id, d_in, d_cr, d_cw, d_out, d_cost],
        )?;
    }

    tx.execute(
        "INSERT INTO session (
            session_id, bucket_group, model_id,
            input_token, cache_read_token, cache_write_token, output_token,
            reported_cost_usd, created_at, updated_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)
         ON CONFLICT(session_id) DO UPDATE SET
            bucket_group = excluded.bucket_group,
            model_id = excluded.model_id,
            input_token = excluded.input_token,
            cache_read_token = excluded.cache_read_token,
            cache_write_token = excluded.cache_write_token,
            output_token = excluded.output_token,
            reported_cost_usd = COALESCE(excluded.reported_cost_usd, session.reported_cost_usd),
            updated_at = excluded.updated_at",
        params![
            session_id,
            bucket_group,
            model_id,
            cur_in,
            cur_cr,
            cur_cw,
            cur_out,
            reported_cost,
            now
        ],
    )?;

    Ok(())
}

fn record_quota_windows(
    tx: &rusqlite::Transaction<'_>,
    now: &str,
    payload: &StatusPayload,
) -> rusqlite::Result<()> {
    for (bucket, info) in &payload.quota {
        let Some(fraction) = info.remaining_fraction else {
            continue;
        };
        let Some(reset_time) = info.reset_time.as_deref().filter(|s| !s.is_empty()) else {
            continue;
        };

        tx.execute(
            "INSERT INTO quota_window (
                bucket, reset_time, first_seen_at, last_seen_at,
                max_remaining_fraction, min_remaining_fraction, last_remaining_fraction
             ) VALUES (?1, ?2, ?3, ?3, ?4, ?4, ?4)
             ON CONFLICT(bucket, reset_time) DO UPDATE SET
                last_seen_at = excluded.last_seen_at,
                max_remaining_fraction = MAX(quota_window.max_remaining_fraction, excluded.max_remaining_fraction),
                min_remaining_fraction = MIN(quota_window.min_remaining_fraction, excluded.min_remaining_fraction),
                last_remaining_fraction = excluded.last_remaining_fraction",
            params![bucket, reset_time, now, fraction],
        )?;
    }
    Ok(())
}

fn to_i64(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

fn utc_now_iso8601() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let hour = rem / 3_600;
    let minute = (rem % 3_600) / 60;
    let second = rem % 60;
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
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

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db_path(tag: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        std::env::temp_dir().join(format!("agy-status-db-{tag}-{nanos}.db"))
    }

    #[test]
    fn records_singular_tables_and_deduplicates_identical_ticks() {
        let db = temp_db_path("dedup");
        let payload: StatusPayload = serde_json::from_str(
            r#"{
              "session_id": "sess-db-1",
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

        let (max_f, min_f, last_f): (f64, f64, f64) = conn
            .query_row(
                "SELECT max_remaining_fraction, min_remaining_fraction, last_remaining_fraction
                 FROM quota_window WHERE bucket = 'gemini-5h'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert!((max_f - 0.95).abs() < 1e-9);
        assert!((min_f - 0.91).abs() < 1e-9);
        assert!((last_f - 0.91).abs() < 1e-9);

        let _ = fs::remove_file(&db);
        let _ = fs::remove_file(format!("{}-wal", db.display()));
        let _ = fs::remove_file(format!("{}-shm", db.display()));
    }
}
