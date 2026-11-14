use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::db;
use crate::stats::StatsReport;

const TELEMETRY_LOOKBACK_SECS: u64 = 3 * 86_400;
const MAX_INGEST_BATCH_SIZE: usize = 50;
const MAX_INGEST_BATCH_BYTES: usize = 384 * 1024;
const KEY_LAST_UPLOADED_UNTIL_UNIX: &str = "last_uploaded_until_unix";
const KEY_LAST_AUTO_ATTEMPT_UNIX: &str = "last_auto_attempt_unix";
const PREVIEW_EPHEMERAL_TOKEN: &str = "preview-ephemeral-token";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IngestSubmission {
    pub submission_id: String,
    pub client_token: String,
    pub report: StatsReport,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BatchIngestItem {
    pub submission_id: String,
    pub report: StatsReport,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BatchIngestPayload {
    pub client_token: String,
    pub submissions: Vec<BatchIngestItem>,
}

fn build_upload_batches(submissions: &[IngestSubmission]) -> Vec<BatchIngestPayload> {
    let Some(first) = submissions.first() else {
        return Vec::new();
    };
    let token_json = serde_json::to_string(&first.client_token).unwrap_or_default();
    let envelope_bytes = format!(r#"{{"client_token":{token_json},"submissions":[]}}"#).len();

    let mut batches = Vec::new();
    let mut current: Vec<BatchIngestItem> = Vec::new();
    let mut current_bytes = envelope_bytes;

    for sub in submissions {
        let item = BatchIngestItem {
            submission_id: sub.submission_id.clone(),
            report: sub.report.clone(),
        };
        let item_bytes = serde_json::to_vec(&item).map(|v| v.len()).unwrap_or(0);
        let sep = usize::from(!current.is_empty());

        if !current.is_empty()
            && (current.len() >= MAX_INGEST_BATCH_SIZE
                || current_bytes.saturating_add(sep).saturating_add(item_bytes)
                    > MAX_INGEST_BATCH_BYTES)
        {
            batches.push(BatchIngestPayload {
                client_token: first.client_token.clone(),
                submissions: std::mem::take(&mut current),
            });
            current_bytes = envelope_bytes;
        }

        let sep = usize::from(!current.is_empty());
        current_bytes = current_bytes.saturating_add(sep).saturating_add(item_bytes);
        current.push(item);
    }

    if !current.is_empty() {
        batches.push(BatchIngestPayload {
            client_token: first.client_token.clone(),
            submissions: current,
        });
    }

    batches
}

#[derive(Debug, Default)]
pub struct PendingUploads {
    pub submissions: Vec<IngestSubmission>,
    pub uploaded_until_unix: Option<u64>,
}

fn read_u64_state_readonly(db_path: &Path, key: &str, label: &str) -> Result<Option<u64>, String> {
    db::get_telemetry_state_readonly(db_path, key)
        .map_err(|e| format!("failed to read {label}: {e}"))
        .map(|opt| opt.and_then(|s| s.parse::<u64>().ok()))
}

fn read_uploaded_cursor(db_path: &Path) -> Result<Option<u64>, String> {
    read_u64_state_readonly(db_path, KEY_LAST_UPLOADED_UNTIL_UNIX, "telemetry cursor")
}

struct PendingWindowPlan {
    slices: Vec<(u64, u64)>,
    uploaded_until_unix: Option<u64>,
}

fn plan_pending_window(db_path: &Path, now_unix: u64) -> Result<PendingWindowPlan, String> {
    let target_end_unix = (now_unix / 3_600) * 3_600;
    let lookback_floor = (now_unix.saturating_sub(TELEMETRY_LOOKBACK_SECS) / 3_600) * 3_600;
    let cursor = read_uploaded_cursor(db_path)?;

    // Always advance an older (or unset) cursor up to `target_end_unix`, even when the DB is
    // empty or only has open current-hour activity, so background uploads do not respawn forever.
    let advance_until = (cursor.unwrap_or(0) < target_end_unix).then_some(target_end_unix);

    let start_unix = match cursor {
        Some(prev) => prev.max(lookback_floor),
        None => {
            let earliest = db::earliest_recorded_unix_secs_readonly(db_path)
                .map_err(|e| format!("failed to read earliest timestamp: {e}"))?;
            match earliest {
                Some(ts) => ((ts / 3_600) * 3_600).max(lookback_floor),
                None => {
                    return Ok(PendingWindowPlan {
                        slices: Vec::new(),
                        uploaded_until_unix: advance_until,
                    });
                }
            }
        }
    };

    if start_unix >= target_end_unix {
        return Ok(PendingWindowPlan {
            slices: Vec::new(),
            uploaded_until_unix: advance_until,
        });
    }

    let mut slices = Vec::new();
    let mut pos = start_unix;
    while pos < target_end_unix {
        let hour_end = ((pos / 3_600) + 1).saturating_mul(3_600);
        let slice_end = target_end_unix.min(hour_end);
        slices.push((pos, slice_end));
        pos = slice_end;
    }

    Ok(PendingWindowPlan {
        slices,
        uploaded_until_unix: Some(target_end_unix),
    })
}

pub fn count_pending_slices(db_path: &Path) -> Result<usize, String> {
    count_pending_slices_at(db_path, db::current_unix_secs())
}

pub fn count_pending_slices_at(db_path: &Path, now_unix: u64) -> Result<usize, String> {
    let plan = plan_pending_window(db_path, now_unix)?;
    db::count_active_stats_slices_readonly(db_path, &plan.slices)
        .map_err(|e| format!("failed to count pending telemetry slices: {e}"))
}

pub fn collect_pending_submissions(db_path: &Path) -> Result<PendingUploads, String> {
    collect_pending_submissions_at(db_path, db::current_unix_secs())
}

pub fn collect_pending_submissions_at(
    db_path: &Path,
    now_unix: u64,
) -> Result<PendingUploads, String> {
    collect_pending_submissions_at_mode(db_path, now_unix, false)
}

fn collect_pending_submissions_at_mode(
    db_path: &Path,
    now_unix: u64,
    readonly: bool,
) -> Result<PendingUploads, String> {
    let plan = plan_pending_window(db_path, now_unix)?;
    if plan.slices.is_empty() {
        return Ok(PendingUploads {
            submissions: Vec::new(),
            uploaded_until_unix: plan.uploaded_until_unix,
        });
    }

    let mut client_token: Option<String> = None;
    let mut submissions = Vec::new();
    for &(slice_start, slice_end) in &plan.slices {
        let report = if readonly {
            db::query_stats_report_half_open_readonly(db_path, slice_start, slice_end)
        } else {
            db::query_stats_report_half_open(db_path, slice_start, slice_end)
        }
        .map_err(|e| format!("failed to query hourly stats slice: {e}"))?;
        if !report.is_empty() {
            let token = match client_token.as_ref() {
                Some(tok) => tok,
                None => {
                    let resolved = if readonly {
                        db::get_telemetry_state_readonly(db_path, "client_token")
                            .map_err(|e| format!("failed to read anonymous client token: {e}"))?
                            .unwrap_or_else(|| PREVIEW_EPHEMERAL_TOKEN.to_string())
                    } else {
                        db::get_or_create_client_token(db_path)
                            .map_err(|e| format!("failed to resolve anonymous client token: {e}"))?
                    };
                    client_token.insert(resolved)
                }
            };
            let submission_id = format!("{}-{}", report.window.start, report.window.end);
            submissions.push(IngestSubmission {
                submission_id,
                client_token: token.clone(),
                report,
            });
        }
    }

    Ok(PendingUploads {
        submissions,
        uploaded_until_unix: plan.uploaded_until_unix,
    })
}

pub fn print_status(cfg: &Config, db_path: &Path) -> Result<(), String> {
    let pending_slices = count_pending_slices(db_path)?;
    let last_uploaded = read_uploaded_cursor(db_path)?.map(db::iso8601_from_unix_secs);
    let last_attempt =
        read_u64_state_readonly(db_path, KEY_LAST_AUTO_ATTEMPT_UNIX, "attempt timestamp")?
            .map(db::iso8601_from_unix_secs);

    println!("telemetry.enabled: {}", cfg.telemetry.enabled);
    println!("telemetry.auto_upload: {}", cfg.telemetry.auto_upload);
    println!(
        "telemetry.upload_interval_secs: {}",
        cfg.telemetry.upload_interval_secs
    );
    println!(
        "telemetry.endpoint: {}",
        cfg.telemetry
            .resolved_ingest_url()
            .as_deref()
            .unwrap_or("(not configured)")
    );
    println!("database: {}", db_path.display());
    println!(
        "last_uploaded_until: {}",
        last_uploaded.as_deref().unwrap_or("(never)")
    );
    println!(
        "last_auto_attempt: {}",
        last_attempt.as_deref().unwrap_or("(never)")
    );
    println!("pending_slices: {pending_slices}");
    Ok(())
}

pub fn print_preview(db_path: &Path) -> Result<(), String> {
    let pending = collect_pending_submissions_at_mode(db_path, db::current_unix_secs(), true)?;
    let batches = build_upload_batches(&pending.submissions);
    let json = serde_json::to_string_pretty(&batches)
        .map_err(|e| format!("failed to serialize telemetry preview: {e}"))?;
    println!("{json}");
    Ok(())
}

pub fn upload_pending(
    cfg: &Config,
    db_path: &Path,
    dry_run: bool,
    quiet: bool,
) -> Result<(), String> {
    if dry_run {
        let pending = collect_pending_submissions_at_mode(db_path, db::current_unix_secs(), true)?;
        let batches = build_upload_batches(&pending.submissions);
        let json = serde_json::to_string_pretty(&batches)
            .map_err(|e| format!("failed to serialize telemetry dry-run: {e}"))?;
        if !quiet {
            println!("{json}");
        }
        return Ok(());
    }

    let pending = collect_pending_submissions(db_path)?;
    let batches = build_upload_batches(&pending.submissions);

    if !cfg.telemetry.enabled {
        return Err(
            "telemetry is disabled (set `[telemetry] enabled = true` in config.toml)".to_string(),
        );
    }

    let endpoint = cfg.telemetry.resolved_ingest_url().ok_or_else(|| {
        "telemetry.endpoint is empty (configure Worker URL in config.toml)".to_string()
    })?;

    if batches.is_empty() {
        if let Some(until) = pending.uploaded_until_unix {
            let _ =
                db::set_telemetry_state(db_path, KEY_LAST_UPLOADED_UNTIL_UNIX, &until.to_string());
        }
        if !quiet {
            println!("No pending telemetry slices to upload.");
        }
        return Ok(());
    }

    let mut uploaded = 0usize;
    let mut last_ok_end: Option<u64> = None;
    for batch in &batches {
        let payload = serde_json::to_vec(batch)
            .map_err(|e| format!("failed to serialize telemetry batch: {e}"))?;
        if let Err(err) = post_json_via_curl(&endpoint, &payload) {
            if let Some(ok_end) = last_ok_end {
                let _ = db::set_telemetry_state(
                    db_path,
                    KEY_LAST_UPLOADED_UNTIL_UNIX,
                    &ok_end.to_string(),
                );
            }
            return Err(err);
        }
        uploaded += batch.submissions.len();
        if let Some(last_item) = batch.submissions.last() {
            last_ok_end = db::unix_secs_from_iso8601(&last_item.report.window.end);
        }
    }

    if let Some(until) = pending.uploaded_until_unix.or(last_ok_end) {
        db::set_telemetry_state(db_path, KEY_LAST_UPLOADED_UNTIL_UNIX, &until.to_string())
            .map_err(|e| format!("uploaded {uploaded} slices but failed updating cursor: {e}"))?;
    }

    if !quiet {
        println!(
            "Uploaded {uploaded} telemetry slice(s) in {} batch(es) to {endpoint}.",
            batches.len()
        );
    }
    Ok(())
}

/// Evaluates whether a background upload should spawn at `now_unix`:
/// triggers once per closed UTC hour (`last_uploaded_until < current_hour_start`),
/// throttled by cooldown.
pub(crate) fn should_trigger_auto_upload(
    now_unix: u64,
    last_uploaded_until: u64,
    last_attempt: u64,
    cooldown_secs: u64,
) -> bool {
    let min_interval = cooldown_secs.max(60);
    if now_unix.saturating_sub(last_attempt) < min_interval {
        return false;
    }

    let current_hour_start = (now_unix / 3_600) * 3_600;
    last_uploaded_until < current_hour_start
}

/// Spawns a detached background upload process when due (non-blocking).
pub fn maybe_spawn_background_upload(db_path: &Path, cfg: &Config, config_path: Option<&Path>) {
    if !cfg.telemetry.enabled
        || !cfg.telemetry.auto_upload
        || cfg.telemetry.resolved_ingest_url().is_none()
    {
        return;
    }

    let now_unix = db::current_unix_secs();
    let cooldown_secs = cfg.telemetry.upload_interval_secs;
    let claimed = db::try_claim_auto_upload_slot(
        db_path,
        now_unix,
        KEY_LAST_UPLOADED_UNTIL_UNIX,
        KEY_LAST_AUTO_ATTEMPT_UNIX,
        |last_uploaded, last_attempt| {
            should_trigger_auto_upload(now_unix, last_uploaded, last_attempt, cooldown_secs)
        },
    )
    .unwrap_or(false);
    if !claimed {
        return;
    }

    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let mut cmd = build_background_upload_command(&exe, db_path, config_path);
    let _ = cmd.spawn();
}

fn build_background_upload_command(
    exe: &Path,
    db_path: &Path,
    config_path: Option<&Path>,
) -> Command {
    let mut cmd = Command::new(exe);
    cmd.args(["telemetry", "upload", "--quiet", "--db"])
        .arg(db_path);
    if let Some(path) = config_path {
        cmd.arg("--config").arg(path);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd
}

fn post_json_via_curl(url: &str, body: &[u8]) -> Result<(), String> {
    let (allowed_proto, validated_url) = crate::config::validate_endpoint_url(url)
        .ok_or_else(|| format!("rejected unsafe telemetry endpoint URL: {url}"))?;
    let mut child = Command::new("curl")
        .args([
            "-sS",
            "-X",
            "POST",
            "--proto",
            allowed_proto,
            "-H",
            "Content-Type: application/json",
            "--max-time",
            "15",
            "-w",
            "\n%{http_code}",
            "--data-binary",
            "@-",
            "--",
            validated_url,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to launch curl: {e}"))?;

    if let Some(mut stdin) = child.stdin.take()
        && let Err(err) = stdin.write_all(body)
    {
        drop(stdin);
        let _ = child.wait();
        return Err(format!("failed to pipe telemetry payload to curl: {err}"));
    }

    let output = child
        .wait_with_output()
        .map_err(|e| format!("failed waiting for curl: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "curl exited with {}: {}",
            output.status,
            stderr.trim()
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let status_line = stdout.lines().last().unwrap_or("").trim();
    let status: u16 = status_line
        .parse()
        .map_err(|_| format!("unexpected HTTP status from curl: '{status_line}'"))?;
    if !(200..300).contains(&status) {
        return Err(format!("telemetry ingest returned HTTP {status}: {stdout}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::payload::StatusPayload;
    use crate::tracker::SessionMetrics;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_db_path(tag: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        std::env::temp_dir().join(format!("agy-status-telemetry-{tag}-{nanos}.db"))
    }

    #[test]
    fn collects_only_closed_hours_and_strips_all_pii() {
        let db = temp_db_path("pii-audit");
        let payload: StatusPayload = serde_json::from_str(
            r#"{
              "session_id": "secret-session-uuid-999",
              "cwd": "/Users/secret-name/private-project",
              "transcript_path": "/Users/secret-name/.gemini/transcript.jsonl",
              "plan_tier": "Google AI Ultra",
              "model": { "id": "Claude Opus 4.6", "effort": "high" }
            }"#,
        )
        .unwrap();
        let metrics = SessionMetrics {
            input_tokens: 1_500,
            output_tokens: 300,
            ..Default::default()
        };

        // Closed hour 15:00Z + open hour 16:00Z when now = 16:15Z.
        db::record_snapshot_at(&db, &payload, &metrics, "2026-11-14T15:20:00Z").unwrap();
        let m2 = SessionMetrics {
            input_tokens: 2_000,
            output_tokens: 500,
            ..Default::default()
        };
        db::record_snapshot_at(&db, &payload, &m2, "2026-11-14T16:10:00Z").unwrap();

        let now_unix = db::unix_secs_from_iso8601("2026-11-14T16:15:00Z").unwrap();
        let pending = collect_pending_submissions_at(&db, now_unix).unwrap();
        assert_eq!(pending.submissions.len(), 1);
        assert_eq!(
            pending.submissions[0].report.window.start,
            "2026-11-14T15:00:00Z"
        );
        assert_eq!(
            pending.submissions[0].report.window.end,
            "2026-11-14T16:00:00Z"
        );

        let serialized = serde_json::to_string(&pending.submissions).unwrap();
        assert!(!serialized.contains("secret-session-uuid-999"));
        assert!(!serialized.contains("secret-name"));
        assert!(!serialized.contains("private-project"));
        assert!(!serialized.contains("transcript.jsonl"));

        let _ = std::fs::remove_file(&db);
        let _ = std::fs::remove_file(format!("{}-wal", db.display()));
        let _ = std::fs::remove_file(format!("{}-shm", db.display()));
    }

    #[test]
    fn uploads_closed_whole_utc_hours_only_and_retries_idempotently() {
        let db = temp_db_path("whole-hour");
        let payload: StatusPayload = serde_json::from_str(
            r#"{
              "session_id": "sess-whole-hour",
              "plan_tier": "Google AI Ultra",
              "model": { "id": "Claude Opus 4.6", "effort": "high" }
            }"#,
        )
        .unwrap();

        let m1 = SessionMetrics {
            input_tokens: 1_000,
            output_tokens: 200,
            ..Default::default()
        };
        db::record_snapshot_at(&db, &payload, &m1, "2026-11-14T15:30:00Z").unwrap();

        let h15 = db::unix_secs_from_iso8601("2026-11-14T15:00:00Z").unwrap();
        let h16 = db::unix_secs_from_iso8601("2026-11-14T16:00:00Z").unwrap();

        // While hour 15 is still open (even late at 15:58:45), nothing triggers or uploads.
        let t_1558 = db::unix_secs_from_iso8601("2026-11-14T15:58:45Z").unwrap();
        assert!(!should_trigger_auto_upload(t_1558, h15, 0, 300));
        let p_open = collect_pending_submissions_at(&db, t_1558).unwrap();
        assert!(p_open.submissions.is_empty());

        // Once hour 16 opens at 16:00:05, full closed [15:00:00Z, 16:00:00Z) becomes uploadable.
        let t_1600 = db::unix_secs_from_iso8601("2026-11-14T16:00:05Z").unwrap();
        assert!(should_trigger_auto_upload(t_1600, 0, 0, 300));
        let p1 = collect_pending_submissions_at(&db, t_1600).unwrap();
        assert_eq!(p1.submissions.len(), 1);
        assert_eq!(
            p1.submissions[0].report.window.start,
            "2026-11-14T15:00:00Z"
        );
        assert_eq!(p1.submissions[0].report.window.end, "2026-11-14T16:00:00Z");
        assert_eq!(p1.submissions[0].report.window.duration_secs, 3_600);
        assert_eq!(p1.uploaded_until_unix, Some(h16));

        // Retrying before cursor is persisted produces identical window and submission_id.
        let t_1606 = db::unix_secs_from_iso8601("2026-11-14T16:06:00Z").unwrap();
        let p1_retry = collect_pending_submissions_at(&db, t_1606).unwrap();
        assert_eq!(p1_retry.submissions.len(), 1);
        assert_eq!(
            p1_retry.submissions[0].report.window,
            p1.submissions[0].report.window
        );
        assert_eq!(
            p1_retry.submissions[0].submission_id,
            p1.submissions[0].submission_id
        );

        // Persist cursor at hour 16 start; open hour 16 activity does not trigger until 17:00.
        db::set_telemetry_state(&db, KEY_LAST_UPLOADED_UNTIL_UNIX, &h16.to_string()).unwrap();
        let m2 = SessionMetrics {
            input_tokens: 2_000,
            output_tokens: 500,
            ..Default::default()
        };
        db::record_snapshot_at(&db, &payload, &m2, "2026-11-14T16:30:00Z").unwrap();
        let t_1658 = db::unix_secs_from_iso8601("2026-11-14T16:58:00Z").unwrap();
        assert!(!should_trigger_auto_upload(t_1658, h16, 0, 300));
        assert!(
            collect_pending_submissions_at(&db, t_1658)
                .unwrap()
                .submissions
                .is_empty()
        );

        // Once hour 17 opens, closed [16:00:00Z, 17:00:00Z) triggers and collects cleanly.
        let t_1701 = db::unix_secs_from_iso8601("2026-11-14T17:01:00Z").unwrap();
        assert!(should_trigger_auto_upload(t_1701, h16, 0, 300));
        let p2 = collect_pending_submissions_at(&db, t_1701).unwrap();
        assert_eq!(p2.submissions.len(), 1);
        assert_eq!(
            p2.submissions[0].report.window.start,
            "2026-11-14T16:00:00Z"
        );
        assert_eq!(p2.submissions[0].report.window.end, "2026-11-14T17:00:00Z");

        let _ = std::fs::remove_file(&db);
        let _ = std::fs::remove_file(format!("{}-wal", db.display()));
        let _ = std::fs::remove_file(format!("{}-shm", db.display()));
    }

    #[test]
    fn chunks_upload_submissions_into_batches_of_up_to_50() {
        let sample = crate::stats::StatsReport::empty_local(
            "2026-11-14T15:00:00Z".to_string(),
            "2026-11-14T16:00:00Z".to_string(),
            3600,
        );
        let subs: Vec<IngestSubmission> = (0..105)
            .map(|i| IngestSubmission {
                submission_id: format!("sub-{i}"),
                client_token: "tok-1".to_string(),
                report: sample.clone(),
            })
            .collect();

        let batches = build_upload_batches(&subs);
        assert_eq!(batches.len(), 3);
        assert_eq!(batches[0].submissions.len(), 50);
        assert_eq!(batches[1].submissions.len(), 50);
        assert_eq!(batches[2].submissions.len(), 5);
        assert_eq!(batches[0].client_token, "tok-1");
        assert_eq!(batches[2].submissions[4].submission_id, "sub-104");
    }

    #[test]
    fn background_upload_command_forwards_resolved_config_and_db_path() {
        let exe = std::path::PathBuf::from("/usr/local/bin/antigravity-status");
        let db = std::path::PathBuf::from("/tmp/proj/default.db");
        let cfg = std::path::PathBuf::from("/tmp/proj/.antigravity-status.toml");
        let cmd = build_background_upload_command(&exe, &db, Some(&cfg));
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            vec![
                "telemetry",
                "upload",
                "--quiet",
                "--db",
                "/tmp/proj/default.db",
                "--config",
                "/tmp/proj/.antigravity-status.toml",
            ]
        );
    }

    #[test]
    fn empty_and_open_hour_only_databases_advance_upload_cursor_without_creating_token() {
        // 1. Missing database advances target horizon on check without creating any file.
        let missing_db = temp_db_path("empty-missing");
        let t_1620 = db::unix_secs_from_iso8601("2026-11-14T16:20:00Z").unwrap();
        let h16 = db::unix_secs_from_iso8601("2026-11-14T16:00:00Z").unwrap();
        let p_missing = collect_pending_submissions_at(&missing_db, t_1620).unwrap();
        assert!(p_missing.submissions.is_empty());
        assert_eq!(p_missing.uploaded_until_unix, Some(h16));
        assert!(!missing_db.exists());

        // 2. Empty DB running upload_pending persists cursor so auto-upload stops respawning.
        let empty_db = temp_db_path("empty-upload");
        let mut cfg = Config::default();
        cfg.telemetry.enabled = true;
        cfg.telemetry.endpoint = "https://telemetry.invalid".to_string();
        let cur_hour_before = (db::current_unix_secs() / 3_600) * 3_600;
        upload_pending(&cfg, &empty_db, false, true).unwrap();

        let cursor_after_empty = read_uploaded_cursor(&empty_db).unwrap().unwrap();
        assert!(cursor_after_empty >= cur_hour_before);
        assert!(
            db::get_telemetry_state_readonly(&empty_db, "client_token")
                .unwrap()
                .is_none()
        );
        // No trigger occurs within the cursor's UTC hour once caught up.
        let in_current_hour = (cursor_after_empty / 3_600) * 3_600 + 58 * 60;
        assert!(!should_trigger_auto_upload(
            in_current_hour,
            cursor_after_empty,
            0,
            300,
        ));

        // 3. DB containing only open current-hour usage also advances cursor to `current_hour_start`.
        let open_db = temp_db_path("open-hour-only");
        let payload: StatusPayload = serde_json::from_str(
            r#"{
              "session_id": "sess-open-only",
              "plan_tier": "Google AI Ultra",
              "model": { "id": "Claude Opus 4.6", "effort": "high" }
            }"#,
        )
        .unwrap();
        let metrics = SessionMetrics {
            input_tokens: 500,
            output_tokens: 100,
            ..Default::default()
        };
        db::record_snapshot_at(&open_db, &payload, &metrics, "2026-11-14T16:10:00Z").unwrap();

        let p_open = collect_pending_submissions_at(&open_db, t_1620).unwrap();
        assert!(p_open.submissions.is_empty());
        assert_eq!(p_open.uploaded_until_unix, Some(h16));
        assert!(
            db::get_telemetry_state_readonly(&open_db, "client_token")
                .unwrap()
                .is_none()
        );

        // Once cursor is already at current_hour_start, checking again does not regress or re-emit.
        db::set_telemetry_state(&open_db, KEY_LAST_UPLOADED_UNTIL_UNIX, &h16.to_string()).unwrap();
        let p_open_repeat = collect_pending_submissions_at(&open_db, t_1620).unwrap();
        assert!(p_open_repeat.submissions.is_empty());
        assert_eq!(p_open_repeat.uploaded_until_unix, None);

        for path in [empty_db, open_db] {
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_file(format!("{}-wal", path.display()));
            let _ = std::fs::remove_file(format!("{}-shm", path.display()));
        }
    }

    #[cfg(unix)]
    #[test]
    fn status_query_is_strictly_readonly_on_missing_and_chmod_0444_databases() {
        use std::os::unix::fs::PermissionsExt;

        let cfg = Config::default();

        // 1. Missing DB succeeds with 0 pending slices and does not create any file.
        let missing_db = temp_db_path("status-missing");
        assert_eq!(count_pending_slices(&missing_db).unwrap(), 0);
        print_status(&cfg, &missing_db).unwrap();
        assert!(!missing_db.exists());

        // 2. Populated DB with a pending closed hour and no client_token succeeds under chmod 0444.
        let ro_db = temp_db_path("status-readonly");
        let payload: StatusPayload = serde_json::from_str(
            r#"{
              "session_id": "sess-ro",
              "plan_tier": "Google AI Ultra",
              "model": { "id": "Claude Opus 4.6", "effort": "high" }
            }"#,
        )
        .unwrap();
        let metrics = SessionMetrics {
            input_tokens: 1_200,
            output_tokens: 250,
            ..Default::default()
        };
        let closed_ts =
            db::iso8601_from_unix_secs((db::current_unix_secs() / 3_600) * 3_600 - 1_800);
        db::record_snapshot_at(&ro_db, &payload, &metrics, &closed_ts).unwrap();

        let ro_perms = std::fs::Permissions::from_mode(0o444);
        std::fs::set_permissions(&ro_db, ro_perms).unwrap();

        assert_eq!(count_pending_slices(&ro_db).unwrap(), 1);
        print_status(&cfg, &ro_db).unwrap();
        assert!(
            db::get_telemetry_state_readonly(&ro_db, "client_token")
                .unwrap()
                .is_none()
        );

        // `preview` and `upload --dry-run` must also remain strictly read-only on chmod 0444 DB.
        print_preview(&ro_db).unwrap();
        upload_pending(&cfg, &ro_db, true, true).unwrap();
        assert!(
            db::get_telemetry_state_readonly(&ro_db, "client_token")
                .unwrap()
                .is_none()
        );

        let rw_perms = std::fs::Permissions::from_mode(0o644);
        let _ = std::fs::set_permissions(&ro_db, rw_perms);
        let _ = std::fs::remove_file(&ro_db);
        let _ = std::fs::remove_file(format!("{}-wal", ro_db.display()));
        let _ = std::fs::remove_file(format!("{}-shm", ro_db.display()));
    }

    #[test]
    fn build_upload_batches_enforces_byte_limit_and_url_validation_blocks_unsafe_schemes() {
        let mut large_report = StatsReport::empty_local(
            "2026-11-15T10:00:00Z".to_string(),
            "2026-11-15T11:00:00Z".to_string(),
            3_600,
        );
        for idx in 0..80 {
            large_report
                .usage_by_plan_model
                .push(crate::stats::UsageByPlanModel {
                    plan_tier: "Google AI Ultra".to_string(),
                    model: format!("Model-{idx}-{}", "x".repeat(80)),
                    effort: "high".to_string(),
                    events: 1,
                    input_tokens: 1_000,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                    output_tokens: 100,
                    reference_cost_usd: 0.01,
                });
        }

        // 40 submissions < 50 item cap, yet total bytes exceed MAX_INGEST_BATCH_BYTES.
        let subs: Vec<IngestSubmission> = (0..40)
            .map(|i| IngestSubmission {
                submission_id: format!("{i:032x}"),
                client_token: "11111111111111111111111111111111".to_string(),
                report: large_report.clone(),
            })
            .collect();

        let batches = build_upload_batches(&subs);
        assert!(
            batches.len() > 1,
            "should split batches before exceeding byte limit"
        );
        for batch in &batches {
            let encoded = serde_json::to_vec(batch).unwrap();
            assert!(
                encoded.len() <= MAX_INGEST_BATCH_BYTES,
                "batch size {} exceeded byte cap {}",
                encoded.len(),
                MAX_INGEST_BATCH_BYTES
            );
        }

        assert!(post_json_via_curl("http://evil.com/api/telemetry/ingest", b"{}").is_err());
        assert!(post_json_via_curl("-K/etc/passwd", b"{}").is_err());
    }
}
