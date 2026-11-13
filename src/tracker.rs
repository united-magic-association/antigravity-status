use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

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
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct SessionCacheState {
    last_total_input: u64,
    last_total_output: u64,
    last_usage: Option<UsageSnapshot>,
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
        return Some(PathBuf::from(home).join(".cache").join("antigravity-status"));
    }
    Some(std::env::temp_dir().join("antigravity-status"))
}

/// Resolves session-level metrics from the `statusLine` payload, transcript file, and local state.
pub fn resolve_metrics(payload: &StatusPayload, cache_dir: Option<&Path>) -> SessionMetrics {
    let cw = payload.context_window.as_ref();
    let total_input = cw.map_or(0, |c| c.total_input_tokens);
    let total_output = cw.map_or(0, |c| c.total_output_tokens);
    let current_usage = cw.and_then(|c| c.current_usage);

    let (tracked_read, tracked_write) =
        update_session_cache(payload.effective_session_id(), total_input, total_output, current_usage, cache_dir);

    let transcript_totals = payload
        .transcript_path
        .as_deref()
        .filter(|p| !p.is_empty())
        .and_then(|p| sum_transcript_cache(Path::new(p)));

    let (cache_read_tokens, cache_write_tokens) = match transcript_totals {
        Some((tr_read, tr_write)) => (tr_read.max(tracked_read), tr_write.max(tracked_write)),
        None => (tracked_read, tracked_write),
    };

    let (cost_usd, subagent_cost_usd) = match payload.cost.as_ref() {
        Some(cost) => (cost.effective_total_usd(), cost.subagent_usd),
        None => (None, None),
    };

    SessionMetrics {
        used_percentage: cw.and_then(|c| c.used_percentage),
        context_window_size: cw.map_or(0, |c| c.context_window_size),
        input_tokens: total_input,
        cache_read_tokens,
        cache_write_tokens,
        output_tokens: total_output,
        cost_usd,
        subagent_cost_usd,
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct TranscriptStep {
    cache_read_tokens: Option<u64>,
    cache_creation_tokens: Option<u64>,
    cache_write_tokens: Option<u64>,
}

fn sum_transcript_cache(path: &Path) -> Option<(u64, u64)> {
    let file = fs::File::open(path).ok()?;
    let reader = BufReader::new(file);
    let mut read_sum = 0_u64;
    let mut write_sum = 0_u64;

    for line in reader.lines().map_while(Result::ok) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(step) = serde_json::from_str::<TranscriptStep>(trimmed) {
            read_sum = read_sum.saturating_add(step.cache_read_tokens.unwrap_or(0));
            let write = step
                .cache_creation_tokens
                .or(step.cache_write_tokens)
                .unwrap_or(0);
            write_sum = write_sum.saturating_add(write);
        }
    }

    Some((read_sum, write_sum))
}

fn update_session_cache(
    session_id: Option<&str>,
    total_input: u64,
    total_output: u64,
    current_usage: Option<CurrentUsage>,
    cache_dir: Option<&Path>,
) -> (u64, u64) {
    let fallback = current_usage.map_or((0, 0), |u| {
        (u.cache_read_input_tokens, u.cache_creation_input_tokens)
    });

    let (Some(sid), Some(dir)) = (session_id, cache_dir) else {
        return fallback;
    };

    let state_path = dir.join(format!("{}.json", sanitize_session_id(sid)));
    let mut state = fs::read_to_string(&state_path)
        .ok()
        .and_then(|raw| serde_json::from_str::<SessionCacheState>(&raw).ok())
        .unwrap_or_default();

    // Reset accumulated totals if counters moved backwards (e.g., rewound/new session with same ID).
    if total_input < state.last_total_input || total_output < state.last_total_output {
        state = SessionCacheState::default();
    }

    if let Some(curr) = current_usage {
        let snap = UsageSnapshot::from(curr);
        let totals_advanced =
            total_input > state.last_total_input || total_output > state.last_total_output;
        let usage_changed = state.last_usage.as_ref() != Some(&snap);

        if state.last_usage.is_none() || totals_advanced || usage_changed {
            state.sum_cache_read = state.sum_cache_read.saturating_add(curr.cache_read_input_tokens);
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

    (state.sum_cache_read, state.sum_cache_write)
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
