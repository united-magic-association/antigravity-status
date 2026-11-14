use std::collections::BTreeMap;

use serde::Deserialize;

/// Top-level JSON payload piped to a custom `statusLine` command via `stdin`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct StatusPayload {
    pub cwd: Option<String>,
    pub session_id: Option<String>,
    pub conversation_id: Option<String>,
    pub conversation_title: Option<String>,
    pub transcript_path: Option<String>,
    pub version: Option<String>,
    pub plan_tier: Option<String>,
    pub product: Option<String>,
    pub agent_state: Option<String>,
    pub cycle_mode: Option<String>,
    pub terminal_width: Option<u16>,
    pub exceeds_200k_tokens: Option<bool>,
    pub model: Option<ModelInfo>,
    pub workspace: Option<WorkspaceInfo>,
    pub context_window: Option<ContextWindow>,
    pub cost: Option<CostInfo>,
    pub quota: BTreeMap<String, QuotaBucket>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaGroup {
    Gemini,
    ThirdParty,
}

impl QuotaGroup {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Gemini => "gemini",
            Self::ThirdParty => "3p",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaWindowKind {
    FiveHour,
    Weekly,
}

impl StatusPayload {
    /// Returns the active session identifier, preferring `session_id` over `conversation_id`.
    pub fn effective_session_id(&self) -> Option<&str> {
        self.session_id
            .as_deref()
            .filter(|s| !s.is_empty())
            .or_else(|| self.conversation_id.as_deref().filter(|s| !s.is_empty()))
    }

    /// Returns the active model identifier or display name for pricing and quota group lookup.
    pub fn effective_model_name(&self) -> Option<&str> {
        self.model.as_ref().and_then(|m| {
            m.id.as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .or_else(|| {
                    m.display_name
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                })
        })
    }

    /// Returns normalized `(model, effort)` with any trailing `(Effort)` suffix stripped
    /// from `model` and `effort` lowercased (falling back to `"default"` when absent).
    pub fn normalized_model_and_effort(&self) -> (String, String) {
        let Some(raw_name) = self.effective_model_name() else {
            return ("unknown".to_string(), "default".to_string());
        };
        let explicit_effort = self
            .model
            .as_ref()
            .and_then(|m| m.effort.as_deref())
            .map(str::trim)
            .filter(|s| !s.is_empty());

        if let Some(eff) = explicit_effort {
            let base = strip_trailing_effort(raw_name, eff).trim();
            let base = if base.is_empty() { "unknown" } else { base };
            return (base.to_string(), eff.to_ascii_lowercase());
        }

        if let Some((base, inferred_eff)) = split_known_trailing_effort(raw_name) {
            let base = base.trim();
            let base = if base.is_empty() { "unknown" } else { base };
            return (base.to_string(), inferred_eff.to_ascii_lowercase());
        }

        let trimmed = raw_name.trim();
        let base = if trimmed.is_empty() {
            "unknown"
        } else {
            trimmed
        };
        (base.to_string(), "default".to_string())
    }

    /// Returns normalized subscription plan tier (`"unknown"` when omitted).
    pub fn normalized_plan_tier(&self) -> &str {
        self.plan_tier
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("unknown")
    }

    /// Selects `gemini` or `3p` quota bucket family based on the active model.
    pub fn active_quota_group(&self) -> QuotaGroup {
        if let Some(name) = self.effective_model_name() {
            if name.to_ascii_lowercase().contains("gemini") {
                return QuotaGroup::Gemini;
            }
            return QuotaGroup::ThirdParty;
        }
        if self.quota.contains_key("gemini-5h") || self.quota.contains_key("gemini-weekly") {
            QuotaGroup::Gemini
        } else {
            QuotaGroup::ThirdParty
        }
    }

    /// Returns the active 5h or weekly quota bucket matching the current model family.
    pub fn active_quota_bucket(&self, kind: QuotaWindowKind) -> Option<&QuotaBucket> {
        let key = match (self.active_quota_group(), kind) {
            (QuotaGroup::Gemini, QuotaWindowKind::FiveHour) => "gemini-5h",
            (QuotaGroup::Gemini, QuotaWindowKind::Weekly) => "gemini-weekly",
            (QuotaGroup::ThirdParty, QuotaWindowKind::FiveHour) => "3p-5h",
            (QuotaGroup::ThirdParty, QuotaWindowKind::Weekly) => "3p-weekly",
        };
        self.quota.get(key)
    }
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct QuotaBucket {
    pub remaining_fraction: Option<f64>,
    pub reset_time: Option<String>,
    pub reset_in_seconds: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ModelInfo {
    pub id: Option<String>,
    pub display_name: Option<String>,
    pub effort: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct WorkspaceInfo {
    pub current_dir: Option<String>,
    pub project_dir: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ContextWindow {
    /// Current context window token count in `agy` (matches `used_percentage`).
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub context_window_size: u64,
    pub used_percentage: Option<f64>,
    pub remaining_percentage: Option<f64>,
    pub current_usage: Option<CurrentUsage>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct CurrentUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct CostInfo {
    pub total_usd: Option<f64>,
    pub subagent_usd: Option<f64>,
    pub estimated: Option<EstimatedField>,
}

/// Supports both boolean (`estimated: true`) and numeric (`estimated: 0.0412`) variants.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(untagged)]
pub enum EstimatedField {
    Bool(#[allow(dead_code)] bool),
    Amount(f64),
}

impl CostInfo {
    /// Returns the estimated total session spend in USD when available and valid.
    pub fn effective_total_usd(&self) -> Option<f64> {
        self.total_usd
            .filter(|v| v.is_finite() && *v >= 0.0)
            .or(match self.estimated {
                Some(EstimatedField::Amount(v)) if v.is_finite() && v >= 0.0 => Some(v),
                _ => None,
            })
    }
}

/// Normalizes a telemetry/storage label by trimming whitespace, bounding to `max_utf16` UTF-16
/// code units, and substituting `fallback` when the result is empty.
pub fn normalize_label(raw: &str, fallback: &str, max_utf16: usize) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return fallback.to_string();
    }
    let mut utf16_len = 0usize;
    let mut end_byte = trimmed.len();
    for (idx, ch) in trimmed.char_indices() {
        let ch_len = ch.len_utf16();
        if utf16_len + ch_len > max_utf16 {
            end_byte = idx;
            break;
        }
        utf16_len += ch_len;
    }
    let truncated = trimmed[..end_byte].trim();
    if truncated.is_empty() {
        fallback.to_string()
    } else {
        truncated.to_string()
    }
}

/// Strips a matching trailing `(effort)` suffix from a model name when present.
pub fn strip_trailing_effort<'a>(name: &'a str, effort: &str) -> &'a str {
    let trimmed = name.trim_end();
    if let Some(without_paren) = trimmed.strip_suffix(')')
        && let Some(open_idx) = without_paren.rfind('(')
    {
        let inner = without_paren[open_idx + 1..].trim();
        if inner.eq_ignore_ascii_case(effort.trim()) {
            return without_paren[..open_idx].trim_end();
        }
    }
    name
}

/// Splits trailing `(Low|Medium|High|XHigh|Minimal|Max|None)` when `model.effort` is omitted.
pub fn split_known_trailing_effort(name: &str) -> Option<(&str, &str)> {
    let trimmed = name.trim_end();
    let without_paren = trimmed.strip_suffix(')')?;
    let open_idx = without_paren.rfind('(')?;
    let inner = without_paren[open_idx + 1..].trim();
    if is_known_effort_tag(inner) {
        Some((without_paren[..open_idx].trim_end(), inner))
    } else {
        None
    }
}

fn is_known_effort_tag(tag: &str) -> bool {
    matches!(
        tag.to_ascii_lowercase().as_str(),
        "low" | "medium" | "high" | "xhigh" | "minimal" | "max" | "none"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_whitespace_and_bare_effort_models() {
        let payload = StatusPayload {
            model: Some(ModelInfo {
                id: Some("  (High)  ".to_string()),
                display_name: Some("Ignored".to_string()),
                effort: None,
            }),
            ..Default::default()
        };
        assert_eq!(
            payload.normalized_model_and_effort(),
            ("unknown".to_string(), "high".to_string())
        );

        let ws = StatusPayload {
            model: Some(ModelInfo {
                id: Some("   ".to_string()),
                display_name: Some("  Gemini 3  ".to_string()),
                effort: None,
            }),
            ..Default::default()
        };
        assert_eq!(ws.effective_model_name(), Some("Gemini 3"));
        assert_eq!(
            ws.normalized_model_and_effort(),
            ("Gemini 3".to_string(), "default".to_string())
        );
    }

    #[test]
    fn normalize_label_respects_utf16_limit_and_cost_rejects_non_finite() {
        let emoji = "😀".repeat(70); // 140 UTF-16 code units
        let clamped = normalize_label(&emoji, "unknown", 128);
        assert_eq!(clamped.encode_utf16().count(), 128);
        assert_eq!(normalize_label("   ", "default", 128), "default");

        let cost = CostInfo {
            total_usd: Some(f64::NAN),
            subagent_usd: None,
            estimated: Some(EstimatedField::Amount(0.25)),
        };
        assert_eq!(cost.effective_total_usd(), Some(0.25));

        let neg = CostInfo {
            total_usd: Some(-1.0),
            subagent_usd: None,
            estimated: None,
        };
        assert_eq!(neg.effective_total_usd(), None);
    }
}
