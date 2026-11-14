use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;

use crate::config::TokenRates;
use crate::payload::{CurrentUsage, StatusPayload};

/// Completed turn throughput sample captured when transitioning out of `"working"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TpsTurnSample {
    pub output_tokens: u64,
    pub elapsed_ms: u64,
}

/// Resolved session metrics ready for rendering.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct SessionMetrics {
    pub used_percentage: Option<f64>,
    pub context_window_size: u64,
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub output_tokens: u64,
    pub session_estimated_tps: Option<f64>,
    pub last_turn_tps_sample: Option<TpsTurnSample>,
    pub cost_usd: Option<f64>,
    pub subagent_cost_usd: Option<f64>,
    pub cost_is_local_estimate: bool,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SessionTrackerState {
    pub last_total_input: u64,
    pub last_total_output: u64,
    pub last_usage: Option<UsageSnapshot>,
    pub sum_input: u64,
    pub sum_output: u64,
    pub sum_cache_read: u64,
    pub sum_cache_write: u64,
    pub working_started_at_ms: Option<u64>,
    pub tps_output_tokens: u64,
    pub tps_elapsed_ms: u64,
    pub consumed_planner_count: u64,
}

impl SessionTrackerState {
    fn session_estimated_tps(&self) -> Option<f64> {
        if self.tps_output_tokens == 0 || self.tps_elapsed_ms == 0 {
            return None;
        }
        let tps = (self.tps_output_tokens as f64) / (self.tps_elapsed_ms as f64 / 1000.0);
        if tps.is_finite() && tps > 0.0 {
            Some(tps)
        } else {
            None
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct UsageSnapshot {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
}

impl From<CurrentUsage> for UsageSnapshot {
    fn from(u: CurrentUsage) -> Self {
        Self {
            input_tokens: u.input_tokens,
            output_tokens: u.output_tokens,
            cache_read_input_tokens: u.cache_read_input_tokens,
            cache_creation_input_tokens: u.cache_creation_input_tokens,
        }
    }
}

#[derive(Debug, Default)]
pub struct TranscriptTotals {
    pub input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub output: u64,
    pub planner_response_count: u64,
    pub last_planner_start_ms: Option<u64>,
}

/// Reads and aggregates token usage and planner timestamps from `payload.transcript_path`.
pub fn read_transcript_totals(payload: &StatusPayload) -> Option<TranscriptTotals> {
    payload
        .transcript_path
        .as_deref()
        .filter(|p| !p.is_empty())
        .and_then(resolve_transcript_path)
        .and_then(|p| sum_transcript_tokens(&p))
}

/// Resolves single-tick metrics without persistent session state (used when storage is disabled).
pub fn resolve_metrics_stateless(
    payload: &StatusPayload,
    fallback_rates: Option<&TokenRates>,
) -> SessionMetrics {
    let transcript = read_transcript_totals(payload);
    resolve_metrics_with_transcript(
        payload,
        transcript.as_ref(),
        None,
        fallback_rates,
        current_time_ms(),
    )
}

/// Resolves session metrics from `payload`, optional pre-parsed transcript totals, and optional
/// mutable persistent `SessionTrackerState` (held inside SQLite `session`).
pub fn resolve_metrics_with_transcript(
    payload: &StatusPayload,
    transcript: Option<&TranscriptTotals>,
    state: Option<&mut SessionTrackerState>,
    fallback_rates: Option<&TokenRates>,
    now_ms: u64,
) -> SessionMetrics {
    let cw = payload.context_window.as_ref();
    let cw_total_input = cw.map_or(0, |c| c.total_input_tokens);
    let cw_total_output = cw.map_or(0, |c| c.total_output_tokens);
    let current_usage = cw.and_then(|c| c.current_usage);
    let transcript_planner =
        transcript.map(|t| (t.planner_response_count, t.last_planner_start_ms));

    let mut fallback_state = SessionTrackerState::default();
    let (tracked_tps, last_turn_tps_sample, sum_input, sum_cache_read, sum_cache_write, sum_output) =
        match (payload.effective_session_id(), state) {
            (Some(_), Some(s)) => {
                let sample = advance_session_state(
                    s,
                    cw_total_input,
                    cw_total_output,
                    current_usage,
                    payload.agent_state.as_deref(),
                    transcript_planner,
                    now_ms,
                );
                (
                    s.session_estimated_tps(),
                    sample,
                    s.sum_input,
                    s.sum_cache_read,
                    s.sum_cache_write,
                    s.sum_output,
                )
            }
            _ => {
                if let Some(u) = current_usage {
                    fallback_state.sum_input = u.input_tokens;
                    fallback_state.sum_output = u.output_tokens;
                    fallback_state.sum_cache_read = u.cache_read_input_tokens;
                    fallback_state.sum_cache_write = u.cache_creation_input_tokens;
                }
                (
                    None,
                    None,
                    fallback_state.sum_input,
                    fallback_state.sum_cache_read,
                    fallback_state.sum_cache_write,
                    fallback_state.sum_output,
                )
            }
        };

    let (input_tokens, cache_read_tokens, cache_write_tokens, output_tokens) = match transcript {
        Some(tr) => (
            if tr.input > 0 {
                tr.input
            } else {
                sum_input.max(cw_total_input)
            },
            tr.cache_read.max(sum_cache_read),
            tr.cache_write.max(sum_cache_write),
            tr.output.max(cw_total_output).max(sum_output),
        ),
        None => (
            if sum_input > 0 {
                sum_input
            } else {
                cw_total_input
            },
            sum_cache_read,
            sum_cache_write,
            cw_total_output.max(sum_output),
        ),
    };

    let reported_cost = payload.cost.as_ref().and_then(|c| c.effective_total_usd());
    let subagent_cost_usd = payload.cost.as_ref().and_then(|c| c.subagent_usd);

    let (cost_usd, cost_is_local_estimate) = match reported_cost {
        Some(usd) => (Some(usd), false),
        None => match fallback_rates {
            Some(rates)
                if input_tokens + cache_read_tokens + cache_write_tokens + output_tokens > 0 =>
            {
                let est = rates.cost_usd(
                    input_tokens,
                    cache_read_tokens,
                    cache_write_tokens,
                    output_tokens,
                );
                (Some(est), true)
            }
            _ => (None, false),
        },
    };

    SessionMetrics {
        used_percentage: cw.and_then(|c| c.used_percentage),
        context_window_size: cw.map_or(0, |c| c.context_window_size),
        input_tokens,
        cache_read_tokens,
        cache_write_tokens,
        output_tokens,
        session_estimated_tps: tracked_tps,
        last_turn_tps_sample,
        cost_usd,
        subagent_cost_usd,
        cost_is_local_estimate,
    }
}

pub(crate) fn current_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// `agy` CLI may report `~/.gemini/antigravity/brain/...` in `transcript_path` even when
/// the file lives under `~/.gemini/antigravity-cli/brain/...`. Check both.
fn resolve_transcript_path(raw: &str) -> Option<PathBuf> {
    let direct = PathBuf::from(raw);
    if direct.is_file() {
        return Some(direct);
    }

    if raw.contains("/.gemini/antigravity/") {
        let cli_path =
            PathBuf::from(raw.replace("/.gemini/antigravity/", "/.gemini/antigravity-cli/"));
        if cli_path.is_file() {
            return Some(cli_path);
        }
    }

    if raw.contains("\\.gemini\\antigravity\\") {
        let cli_path =
            PathBuf::from(raw.replace("\\.gemini\\antigravity\\", "\\.gemini\\antigravity-cli\\"));
        if cli_path.is_file() {
            return Some(cli_path);
        }
    }

    None
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct TranscriptStep {
    #[serde(rename = "type")]
    step_type: Option<String>,
    created_at: Option<String>,
    input_tokens: Option<u64>,
    cache_read_tokens: Option<u64>,
    cache_creation_tokens: Option<u64>,
    cache_write_tokens: Option<u64>,
    output_tokens: Option<u64>,
}

fn sum_transcript_tokens(path: &Path) -> Option<TranscriptTotals> {
    let file = fs::File::open(path).ok()?;
    let reader = BufReader::new(file);
    let mut totals = TranscriptTotals::default();

    for line in reader.lines().map_while(Result::ok) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(step) = serde_json::from_str::<TranscriptStep>(trimmed) {
            totals.input = totals.input.saturating_add(step.input_tokens.unwrap_or(0));
            totals.cache_read = totals
                .cache_read
                .saturating_add(step.cache_read_tokens.unwrap_or(0));
            let write = step
                .cache_creation_tokens
                .or(step.cache_write_tokens)
                .unwrap_or(0);
            totals.cache_write = totals.cache_write.saturating_add(write);
            totals.output = totals
                .output
                .saturating_add(step.output_tokens.unwrap_or(0));
            if step.step_type.as_deref() == Some("PLANNER_RESPONSE") {
                totals.planner_response_count = totals.planner_response_count.saturating_add(1);
                if let Some(ts) = step.created_at.as_deref()
                    && let Some(ms) = crate::db::unix_millis_from_iso8601(ts)
                {
                    totals.last_planner_start_ms = Some(ms);
                }
            }
        }
    }

    Some(totals)
}

fn advance_session_state(
    state: &mut SessionTrackerState,
    total_input: u64,
    total_output: u64,
    current_usage: Option<CurrentUsage>,
    agent_state: Option<&str>,
    transcript_planner: Option<(u64, Option<u64>)>,
    now_ms: u64,
) -> Option<TpsTurnSample> {
    // Reset accumulated totals if counters moved backwards (e.g., rewound/new session with same ID).
    if total_output < state.last_total_output {
        *state = SessionTrackerState::default();
    }

    let prev_total_output = state.last_total_output;
    let output_delta = total_output.saturating_sub(prev_total_output);

    if let Some(curr) = current_usage {
        let snap = UsageSnapshot::from(curr);
        let totals_advanced =
            total_input != state.last_total_input || total_output > state.last_total_output;
        let usage_changed = state.last_usage.as_ref() != Some(&snap);

        if state.last_usage.is_none() || totals_advanced || usage_changed {
            state.sum_input = state.sum_input.saturating_add(curr.input_tokens);
            state.sum_output = state.sum_output.saturating_add(curr.output_tokens);
            state.sum_cache_read = state
                .sum_cache_read
                .saturating_add(curr.cache_read_input_tokens);
            state.sum_cache_write = state
                .sum_cache_write
                .saturating_add(curr.cache_creation_input_tokens);
            state.last_total_input = total_input;
            state.last_total_output = total_output;
            state.last_usage = Some(snap);
        }
    } else if total_output > state.last_total_output {
        state.last_total_output = total_output;
    }

    let is_working = agent_state.is_some_and(|s| s.eq_ignore_ascii_case("working"));
    let mut completed_sample = None;
    if output_delta > 0
        && let Some(start_ms) = state.working_started_at_ms
    {
        // Only trim tool-wait when at most one new PLANNER_RESPONSE landed since the previous
        // turn; if multiple turns finished between polls, keep `start_ms` so total tokens align
        // with total elapsed duration across all of them.
        let effective_start_ms = match transcript_planner {
            Some((planner_count, Some(planner_ms)))
                if planner_count <= state.consumed_planner_count.saturating_add(1)
                    && planner_ms > start_ms
                    && planner_ms < now_ms =>
            {
                planner_ms
            }
            _ => start_ms,
        };
        let elapsed_ms = now_ms.saturating_sub(effective_start_ms);
        if elapsed_ms > 0 {
            state.tps_output_tokens = state.tps_output_tokens.saturating_add(output_delta);
            state.tps_elapsed_ms = state.tps_elapsed_ms.saturating_add(elapsed_ms);
            completed_sample = Some(TpsTurnSample {
                output_tokens: output_delta,
                elapsed_ms,
            });
        }
        state.working_started_at_ms = if is_working { Some(now_ms) } else { None };
    } else if is_working {
        if state.working_started_at_ms.is_none() {
            state.working_started_at_ms = Some(now_ms);
        }
    } else if state.working_started_at_ms.is_some() {
        state.working_started_at_ms = None;
    }

    if (output_delta > 0 || (state.last_total_output == 0 && !is_working))
        && let Some((planner_count, _)) = transcript_planner
        && planner_count > state.consumed_planner_count
    {
        state.consumed_planner_count = planner_count;
    }

    completed_sample
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn resolve_metrics(
        payload: &StatusPayload,
        state: Option<&mut SessionTrackerState>,
        fallback_rates: Option<&TokenRates>,
    ) -> SessionMetrics {
        resolve_metrics_at(payload, state, fallback_rates, current_time_ms())
    }

    fn resolve_metrics_at(
        payload: &StatusPayload,
        state: Option<&mut SessionTrackerState>,
        fallback_rates: Option<&TokenRates>,
        now_ms: u64,
    ) -> SessionMetrics {
        let transcript = read_transcript_totals(payload);
        resolve_metrics_with_transcript(payload, transcript.as_ref(), state, fallback_rates, now_ms)
    }

    fn temp_test_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        std::env::temp_dir().join(format!("agy-status-test-{tag}-{nanos}"))
    }

    #[test]
    fn deduplicates_repeated_ticks_and_accumulates_new_turns() {
        let mut state = SessionTrackerState::default();
        let payload_turn_1: StatusPayload = serde_json::from_str(
            r#"{
              "session_id": "sess-1",
              "context_window": {
                "total_input_tokens": 1000,
                "total_output_tokens": 200,
                "used_percentage": 10.0,
                "current_usage": {
                  "input_tokens": 1000,
                  "output_tokens": 200,
                  "cache_read_input_tokens": 500,
                  "cache_creation_input_tokens": 100
                }
              },
              "cost": { "total_usd": 0.0123, "estimated": true }
            }"#,
        )
        .unwrap();

        let m1 = resolve_metrics(&payload_turn_1, Some(&mut state), None);
        assert_eq!(m1.input_tokens, 1000);
        assert_eq!(m1.cache_read_tokens, 500);
        assert_eq!(m1.cache_write_tokens, 100);
        assert_eq!(m1.cost_usd, Some(0.0123));
        assert!(!m1.cost_is_local_estimate);

        // Repeated 300ms tick with identical payload must not double-count.
        let m1_repeat = resolve_metrics(&payload_turn_1, Some(&mut state), None);
        assert_eq!(m1_repeat.input_tokens, 1000);
        assert_eq!(m1_repeat.cache_read_tokens, 500);
        assert_eq!(m1_repeat.cache_write_tokens, 100);

        // Turn 2 advances cumulative totals and current_usage.
        let payload_turn_2: StatusPayload = serde_json::from_str(
            r#"{
              "session_id": "sess-1",
              "context_window": {
                "total_input_tokens": 2200,
                "total_output_tokens": 450,
                "used_percentage": 15.0,
                "current_usage": {
                  "input_tokens": 1200,
                  "output_tokens": 250,
                  "cache_read_input_tokens": 800,
                  "cache_creation_input_tokens": 50
                }
              },
              "cost": { "estimated": 0.0456 }
            }"#,
        )
        .unwrap();

        let m2 = resolve_metrics(&payload_turn_2, Some(&mut state), None);
        assert_eq!(m2.input_tokens, 2200);
        assert_eq!(m2.output_tokens, 450);
        assert_eq!(m2.cache_read_tokens, 1300);
        assert_eq!(m2.cache_write_tokens, 150);
        assert_eq!(m2.cost_usd, Some(0.0456));
    }

    #[test]
    fn aggregates_transcript_tokens_and_estimates_missing_cost() {
        let mut state = SessionTrackerState::default();
        let dir = temp_test_dir("transcript");
        fs::create_dir_all(&dir).unwrap();
        let transcript = dir.join("transcript.jsonl");
        fs::write(
            &transcript,
            concat!(
                "{\"type\":\"USER_INPUT\"}\n",
                "{\"type\":\"PLANNER_RESPONSE\",\"input_tokens\":9653,\"cache_read_tokens\":0,\"output_tokens\":28}\n",
                "{\"type\":\"PLANNER_RESPONSE\",\"input_tokens\":2293,\"cache_read_tokens\":7599,\"output_tokens\":28}\n"
            ),
        )
        .unwrap();

        let payload = StatusPayload {
            transcript_path: Some(transcript.to_string_lossy().into_owned()),
            ..Default::default()
        };

        let rates = TokenRates {
            input_per_million: 2.0,
            cache_read_per_million: 0.2,
            cache_write_per_million: 2.5,
            output_per_million: 12.0,
        };

        let m = resolve_metrics(&payload, Some(&mut state), Some(&rates));
        assert_eq!(m.input_tokens, 11946);
        assert_eq!(m.cache_read_tokens, 7599);
        assert_eq!(m.output_tokens, 56);
        assert!(m.cost_is_local_estimate);
        let expected = (11946.0 * 2.0 + 7599.0 * 0.2 + 56.0 * 12.0) / 1_000_000.0;
        assert!((m.cost_usd.unwrap() - expected).abs() < 1e-9);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn estimates_session_tps_across_multiple_turns() {
        let mut state = SessionTrackerState::default();

        // Initial idle tick on an existing session must not fabricate TPS.
        let idle_initial: StatusPayload = serde_json::from_str(
            r#"{
              "session_id": "sess-tps",
              "agent_state": "idle",
              "context_window": {
                "total_input_tokens": 1000,
                "total_output_tokens": 500,
                "current_usage": { "input_tokens": 1000, "output_tokens": 100 }
              }
            }"#,
        )
        .unwrap();
        let m0 = resolve_metrics_at(&idle_initial, Some(&mut state), None, 10_000);
        assert_eq!(m0.session_estimated_tps, None);

        // Entering working starts the local turn timer; repeated working ticks keep t0.
        let working: StatusPayload = serde_json::from_str(
            r#"{
              "session_id": "sess-tps",
              "agent_state": "working",
              "context_window": {
                "total_input_tokens": 1000,
                "total_output_tokens": 500,
                "current_usage": { "input_tokens": 1000, "output_tokens": 100 }
              }
            }"#,
        )
        .unwrap();
        let m1 = resolve_metrics_at(&working, Some(&mut state), None, 11_000);
        assert_eq!(m1.session_estimated_tps, None);
        let m2 = resolve_metrics_at(&working, Some(&mut state), None, 12_500);
        assert_eq!(m2.session_estimated_tps, None);

        // Turn 1: +150 output tokens over 2.5s => 60.0 t/s.
        let tool_use: StatusPayload = serde_json::from_str(
            r#"{
              "session_id": "sess-tps",
              "agent_state": "tool_use",
              "context_window": {
                "total_input_tokens": 1200,
                "total_output_tokens": 650,
                "current_usage": { "input_tokens": 200, "output_tokens": 150 }
              }
            }"#,
        )
        .unwrap();
        let m3 = resolve_metrics_at(&tool_use, Some(&mut state), None, 13_500);
        assert!((m3.session_estimated_tps.unwrap() - 60.0).abs() < 1e-9);

        // Subsequent idle ticks retain the session TPS until the next turn completes.
        let idle_after: StatusPayload = serde_json::from_str(
            r#"{
              "session_id": "sess-tps",
              "agent_state": "idle",
              "context_window": {
                "total_input_tokens": 1200,
                "total_output_tokens": 650,
                "current_usage": { "input_tokens": 200, "output_tokens": 150 }
              }
            }"#,
        )
        .unwrap();
        let m4 = resolve_metrics_at(&idle_after, Some(&mut state), None, 15_000);
        assert!((m4.session_estimated_tps.unwrap() - 60.0).abs() < 1e-9);

        // Turn 2: +350 output tokens over 2.5s => cumulative (150 + 350) / (2.5 + 2.5) = 100.0 t/s.
        let working_2: StatusPayload = serde_json::from_str(
            r#"{
              "session_id": "sess-tps",
              "agent_state": "working",
              "context_window": {
                "total_input_tokens": 1200,
                "total_output_tokens": 650,
                "current_usage": { "input_tokens": 200, "output_tokens": 150 }
              }
            }"#,
        )
        .unwrap();
        let _ = resolve_metrics_at(&working_2, Some(&mut state), None, 20_000);

        let idle_2: StatusPayload = serde_json::from_str(
            r#"{
              "session_id": "sess-tps",
              "agent_state": "idle",
              "context_window": {
                "total_input_tokens": 1500,
                "total_output_tokens": 1000,
                "current_usage": { "input_tokens": 300, "output_tokens": 350 }
              }
            }"#,
        )
        .unwrap();
        let m5 = resolve_metrics_at(&idle_2, Some(&mut state), None, 22_500);
        assert!((m5.session_estimated_tps.unwrap() - 100.0).abs() < 1e-9);

        // Aborted working turn (working -> idle with no output gain) preserves prior session TPS
        // and clears the stale timer so the next real turn starts from its own working timestamp.
        let working_3: StatusPayload = serde_json::from_str(
            r#"{
              "session_id": "sess-tps",
              "agent_state": "working",
              "context_window": {
                "total_input_tokens": 1500,
                "total_output_tokens": 1000,
                "current_usage": { "input_tokens": 300, "output_tokens": 350 }
              }
            }"#,
        )
        .unwrap();
        let _ = resolve_metrics_at(&working_3, Some(&mut state), None, 30_000);
        let m_aborted = resolve_metrics_at(&idle_2, Some(&mut state), None, 35_000);
        assert!((m_aborted.session_estimated_tps.unwrap() - 100.0).abs() < 1e-9);

        // Counter rewind resets accumulated TPS state.
        let rewound: StatusPayload = serde_json::from_str(
            r#"{
              "session_id": "sess-tps",
              "agent_state": "idle",
              "context_window": {
                "total_input_tokens": 100,
                "total_output_tokens": 50,
                "current_usage": { "input_tokens": 100, "output_tokens": 50 }
              }
            }"#,
        )
        .unwrap();
        let m_rewound = resolve_metrics_at(&rewound, Some(&mut state), None, 40_000);
        assert_eq!(m_rewound.session_estimated_tps, None);
    }

    #[test]
    fn strips_intermediate_tool_wait_using_planner_created_at() {
        let mut state = SessionTrackerState::default();
        let dir = temp_test_dir("tps-planner-start");
        fs::create_dir_all(&dir).unwrap();
        let transcript = dir.join("transcript.jsonl");
        let base_secs = crate::db::unix_secs_from_iso8601("2026-11-14T19:00:00Z").unwrap();
        let base_ms = base_secs * 1_000;

        fs::write(
            &transcript,
            "{\"type\":\"PLANNER_RESPONSE\",\"created_at\":\"2026-11-14T19:00:00Z\",\"input_tokens\":1000,\"output_tokens\":200}\n",
        )
        .unwrap();

        let p0 = StatusPayload {
            session_id: Some("sess-tool-wait".to_string()),
            transcript_path: Some(transcript.to_string_lossy().into_owned()),
            agent_state: Some("working".to_string()),
            context_window: Some(crate::payload::ContextWindow {
                total_input_tokens: 1000,
                total_output_tokens: 0,
                ..Default::default()
            }),
            ..Default::default()
        };
        let _ = resolve_metrics_at(&p0, Some(&mut state), None, base_ms);

        // Step 1 finishes at +4s (200 tokens / 4s = 50 t/s), stays "working" while tool runs.
        let p1 = StatusPayload {
            session_id: Some("sess-tool-wait".to_string()),
            transcript_path: Some(transcript.to_string_lossy().into_owned()),
            agent_state: Some("working".to_string()),
            context_window: Some(crate::payload::ContextWindow {
                total_input_tokens: 1000,
                total_output_tokens: 200,
                current_usage: Some(CurrentUsage {
                    input_tokens: 1000,
                    output_tokens: 200,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let m1 = resolve_metrics_at(&p1, Some(&mut state), None, base_ms + 4_000);
        assert_eq!(
            m1.last_turn_tps_sample,
            Some(TpsTurnSample {
                output_tokens: 200,
                elapsed_ms: 4_000,
            })
        );

        // Tool + approval takes 36s (from +4s to +40s). Step 2 PLANNER_RESPONSE starts at +40s
        // and finishes at +45s (+300 output tokens in 5s = 60 t/s, NOT 300 / 41s).
        fs::write(
            &transcript,
            concat!(
                "{\"type\":\"PLANNER_RESPONSE\",\"created_at\":\"2026-11-14T19:00:00Z\",\"input_tokens\":1000,\"output_tokens\":200}\n",
                "{\"type\":\"GENERIC\",\"created_at\":\"2026-11-14T19:00:04Z\"}\n",
                "{\"type\":\"PLANNER_RESPONSE\",\"created_at\":\"2026-11-14T19:00:40Z\",\"input_tokens\":1200,\"output_tokens\":300}\n"
            ),
        )
        .unwrap();

        let p2 = StatusPayload {
            session_id: Some("sess-tool-wait".to_string()),
            transcript_path: Some(transcript.to_string_lossy().into_owned()),
            agent_state: Some("idle".to_string()),
            context_window: Some(crate::payload::ContextWindow {
                total_input_tokens: 2200,
                total_output_tokens: 500,
                current_usage: Some(CurrentUsage {
                    input_tokens: 1200,
                    output_tokens: 300,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let m2 = resolve_metrics_at(&p2, Some(&mut state), None, base_ms + 45_000);
        assert_eq!(
            m2.last_turn_tps_sample,
            Some(TpsTurnSample {
                output_tokens: 300,
                elapsed_ms: 5_000,
            })
        );

        // Step 3 starts at +50.950s and completes at +51.550s (actual 600ms generation).
        // Sub-second precision ensures elapsed_ms is 600ms, not truncated to +50.000s (1550ms).
        fs::write(
            &transcript,
            concat!(
                "{\"type\":\"PLANNER_RESPONSE\",\"created_at\":\"2026-11-14T19:00:00Z\",\"input_tokens\":1000,\"output_tokens\":200}\n",
                "{\"type\":\"GENERIC\",\"created_at\":\"2026-11-14T19:00:04Z\"}\n",
                "{\"type\":\"PLANNER_RESPONSE\",\"created_at\":\"2026-11-14T19:00:40Z\",\"input_tokens\":1200,\"output_tokens\":300}\n",
                "{\"type\":\"PLANNER_RESPONSE\",\"created_at\":\"2026-11-14T19:00:50.950Z\",\"input_tokens\":800,\"output_tokens\":150}\n"
            ),
        )
        .unwrap();

        let p3_start = StatusPayload {
            session_id: Some("sess-tool-wait".to_string()),
            transcript_path: Some(transcript.to_string_lossy().into_owned()),
            agent_state: Some("working".to_string()),
            context_window: Some(crate::payload::ContextWindow {
                total_input_tokens: 3000,
                total_output_tokens: 500,
                ..Default::default()
            }),
            ..Default::default()
        };
        let _ = resolve_metrics_at(&p3_start, Some(&mut state), None, base_ms + 49_000);

        let p3_done = StatusPayload {
            session_id: Some("sess-tool-wait".to_string()),
            transcript_path: Some(transcript.to_string_lossy().into_owned()),
            agent_state: Some("idle".to_string()),
            context_window: Some(crate::payload::ContextWindow {
                total_input_tokens: 3000,
                total_output_tokens: 650,
                current_usage: Some(CurrentUsage {
                    input_tokens: 800,
                    output_tokens: 150,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let m3 = resolve_metrics_at(&p3_done, Some(&mut state), None, base_ms + 51_550);
        assert_eq!(
            m3.last_turn_tps_sample,
            Some(TpsTurnSample {
                output_tokens: 150,
                elapsed_ms: 600,
            })
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn non_output_usage_change_does_not_synthesize_tps_and_multi_step_gap_preserves_total_window() {
        let mut state = SessionTrackerState::default();
        let dir = temp_test_dir("tps-guards");
        fs::create_dir_all(&dir).unwrap();
        let transcript = dir.join("transcript.jsonl");
        let base_ms = 1_763_146_800_000_u64;

        // Turn 1 completes normally.
        let p1 = StatusPayload {
            session_id: Some("sess-tps-guards".to_string()),
            agent_state: Some("working".to_string()),
            context_window: Some(crate::payload::ContextWindow {
                total_input_tokens: 500,
                total_output_tokens: 0,
                ..Default::default()
            }),
            ..Default::default()
        };
        let _ = resolve_metrics_at(&p1, Some(&mut state), None, base_ms);

        let p2 = StatusPayload {
            session_id: Some("sess-tps-guards".to_string()),
            agent_state: Some("working".to_string()),
            context_window: Some(crate::payload::ContextWindow {
                total_input_tokens: 500,
                total_output_tokens: 200,
                current_usage: Some(CurrentUsage {
                    input_tokens: 500,
                    output_tokens: 200,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let m2 = resolve_metrics_at(&p2, Some(&mut state), None, base_ms + 4_000);
        assert_eq!(
            m2.last_turn_tps_sample,
            Some(TpsTurnSample {
                output_tokens: 200,
                elapsed_ms: 4_000,
            })
        );

        // Input-only usage change without output advancing must not synthesize another TPS sample.
        let p_input_only = StatusPayload {
            session_id: Some("sess-tps-guards".to_string()),
            agent_state: Some("idle".to_string()),
            context_window: Some(crate::payload::ContextWindow {
                total_input_tokens: 900,
                total_output_tokens: 200,
                current_usage: Some(CurrentUsage {
                    input_tokens: 400,
                    output_tokens: 200,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let m_input_only =
            resolve_metrics_at(&p_input_only, Some(&mut state), None, base_ms + 5_000);
        assert_eq!(m_input_only.last_turn_tps_sample, None);

        // Now start a new working window where TWO planner responses finish before the next poll.
        let p_work = StatusPayload {
            session_id: Some("sess-tps-guards".to_string()),
            agent_state: Some("working".to_string()),
            context_window: Some(crate::payload::ContextWindow {
                total_input_tokens: 900,
                total_output_tokens: 200,
                ..Default::default()
            }),
            ..Default::default()
        };
        let _ = resolve_metrics_at(&p_work, Some(&mut state), None, base_ms + 10_000);

        fs::write(
            &transcript,
            concat!(
                "{\"type\":\"PLANNER_RESPONSE\",\"created_at\":\"2026-11-14T19:00:11Z\",\"output_tokens\":250}\n",
                "{\"type\":\"PLANNER_RESPONSE\",\"created_at\":\"2026-11-14T19:00:18Z\",\"output_tokens\":250}\n"
            ),
        )
        .unwrap();
        let p_multi_done = StatusPayload {
            session_id: Some("sess-tps-guards".to_string()),
            transcript_path: Some(transcript.to_string_lossy().into_owned()),
            agent_state: Some("idle".to_string()),
            context_window: Some(crate::payload::ContextWindow {
                total_input_tokens: 1_500,
                total_output_tokens: 700,
                current_usage: Some(CurrentUsage {
                    input_tokens: 600,
                    output_tokens: 250,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let m_multi = resolve_metrics_at(&p_multi_done, Some(&mut state), None, base_ms + 20_000);
        assert_eq!(
            m_multi.last_turn_tps_sample,
            Some(TpsTurnSample {
                output_tokens: 500,
                elapsed_ms: 10_000,
            })
        );

        let _ = fs::remove_dir_all(&dir);
    }
}
