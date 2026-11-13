use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::TokenRates;
use crate::payload::{CurrentUsage, StatusPayload};

/// Resolved session metrics ready for rendering.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct SessionMetrics {
    pub used_percentage: Option<f64>,
    pub context_window_size: u64,
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: Option<f64>,
    pub subagent_cost_usd: Option<f64>,
    pub cost_is_local_estimate: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct SessionCacheState {
    last_total_input: u64,
    last_total_output: u64,
    last_usage: Option<UsageSnapshot>,
    sum_input: u64,
    sum_output: u64,
    sum_cache_read: u64,
    sum_cache_write: u64,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct UsageSnapshot {
    input_tokens: u64,
    output_tokens: u64,
    cache_read_input_tokens: u64,
    cache_creation_input_tokens: u64,
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
struct TranscriptTotals {
    input: u64,
    cache_read: u64,
    cache_write: u64,
    output: u64,
}

/// Returns the default cache directory for session accumulation state.
pub fn default_cache_dir() -> Option<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CACHE_HOME")
        && !xdg.is_empty()
    {
        return Some(PathBuf::from(xdg).join("antigravity-status"));
    }
    if let Ok(home) = std::env::var("HOME")
        && !home.is_empty()
    {
        return Some(
            PathBuf::from(home)
                .join(".cache")
                .join("antigravity-status"),
        );
    }
    Some(std::env::temp_dir().join("antigravity-status"))
}

/// Resolves session-level metrics from the `statusLine` payload, transcript file, and local state.
pub fn resolve_metrics(
    payload: &StatusPayload,
    cache_dir: Option<&Path>,
    fallback_rates: Option<&TokenRates>,
) -> SessionMetrics {
    let cw = payload.context_window.as_ref();
    let cw_total_input = cw.map_or(0, |c| c.total_input_tokens);
    let cw_total_output = cw.map_or(0, |c| c.total_output_tokens);
    let current_usage = cw.and_then(|c| c.current_usage);

    let tracked = update_session_cache(
        payload.effective_session_id(),
        cw_total_input,
        cw_total_output,
        current_usage,
        cache_dir,
    );

    let transcript = payload
        .transcript_path
        .as_deref()
        .filter(|p| !p.is_empty())
        .and_then(resolve_transcript_path)
        .and_then(|p| sum_transcript_tokens(&p));

    let (input_tokens, cache_read_tokens, cache_write_tokens, output_tokens) = match transcript {
        Some(tr) => (
            if tr.input > 0 {
                tr.input
            } else {
                tracked.sum_input.max(cw_total_input)
            },
            tr.cache_read.max(tracked.sum_cache_read),
            tr.cache_write.max(tracked.sum_cache_write),
            tr.output.max(cw_total_output).max(tracked.sum_output),
        ),
        None => (
            if tracked.sum_input > 0 {
                tracked.sum_input
            } else {
                cw_total_input
            },
            tracked.sum_cache_read,
            tracked.sum_cache_write,
            cw_total_output.max(tracked.sum_output),
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
                let est = estimate_usd(
                    input_tokens,
                    cache_read_tokens,
                    cache_write_tokens,
                    output_tokens,
                    rates,
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
        cost_usd,
        subagent_cost_usd,
        cost_is_local_estimate,
    }
}

fn estimate_usd(
    input_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
    output_tokens: u64,
    rates: &TokenRates,
) -> f64 {
    const PER_M: f64 = 1_000_000.0;
    (input_tokens as f64 * rates.input_per_million
        + cache_read_tokens as f64 * rates.cache_read_per_million
        + cache_write_tokens as f64 * rates.cache_write_per_million
        + output_tokens as f64 * rates.output_per_million)
        / PER_M
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
        }
    }

    Some(totals)
}

fn update_session_cache(
    session_id: Option<&str>,
    total_input: u64,
    total_output: u64,
    current_usage: Option<CurrentUsage>,
    cache_dir: Option<&Path>,
) -> SessionCacheState {
    let fallback = match current_usage {
        Some(u) => SessionCacheState {
            sum_input: u.input_tokens,
            sum_output: u.output_tokens,
            sum_cache_read: u.cache_read_input_tokens,
            sum_cache_write: u.cache_creation_input_tokens,
            ..Default::default()
        },
        None => SessionCacheState::default(),
    };

    let (Some(sid), Some(dir)) = (session_id, cache_dir) else {
        return fallback;
    };

    let state_path = dir.join(format!("{}.json", sanitize_session_id(sid)));
    let mut state = fs::read_to_string(&state_path)
        .ok()
        .and_then(|raw| serde_json::from_str::<SessionCacheState>(&raw).ok())
        .unwrap_or_default();

    // Reset accumulated totals if counters moved backwards (e.g., rewound/new session with same ID).
    if total_output < state.last_total_output {
        state = SessionCacheState::default();
    }

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

            if fs::create_dir_all(dir).is_ok()
                && let Ok(serialized) = serde_json::to_string(&state)
            {
                let _ = fs::write(&state_path, serialized);
            }
        }
    }

    state
}

fn sanitize_session_id(id: &str) -> String {
    let clean: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if clean.is_empty() {
        "default".to_string()
    } else {
        clean
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_test_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        std::env::temp_dir().join(format!("agy-status-test-{tag}-{nanos}"))
    }

    #[test]
    fn deduplicates_repeated_ticks_and_accumulates_new_turns() {
        let dir = temp_test_dir("dedup");
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

        let m1 = resolve_metrics(&payload_turn_1, Some(&dir), None);
        assert_eq!(m1.input_tokens, 1000);
        assert_eq!(m1.cache_read_tokens, 500);
        assert_eq!(m1.cache_write_tokens, 100);
        assert_eq!(m1.cost_usd, Some(0.0123));
        assert!(!m1.cost_is_local_estimate);

        // Repeated 300ms tick with identical payload must not double-count.
        let m1_repeat = resolve_metrics(&payload_turn_1, Some(&dir), None);
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

        let m2 = resolve_metrics(&payload_turn_2, Some(&dir), None);
        assert_eq!(m2.input_tokens, 2200);
        assert_eq!(m2.output_tokens, 450);
        assert_eq!(m2.cache_read_tokens, 1300);
        assert_eq!(m2.cache_write_tokens, 150);
        assert_eq!(m2.cost_usd, Some(0.0456));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn aggregates_transcript_tokens_and_estimates_missing_cost() {
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

        let m = resolve_metrics(&payload, Some(&dir), Some(&rates));
        assert_eq!(m.input_tokens, 11946);
        assert_eq!(m.cache_read_tokens, 7599);
        assert_eq!(m.output_tokens, 56);
        assert!(m.cost_is_local_estimate);
        let expected = (11946.0 * 2.0 + 7599.0 * 0.2 + 56.0 * 12.0) / 1_000_000.0;
        assert!((m.cost_usd.unwrap() - expected).abs() < 1e-9);

        let _ = fs::remove_dir_all(&dir);
    }
}
