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
                .filter(|s| !s.is_empty())
                .or_else(|| m.display_name.as_deref().filter(|s| !s.is_empty()))
        })
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
    /// Returns the estimated total session spend in USD when available.
    pub fn effective_total_usd(&self) -> Option<f64> {
        self.total_usd.or_else(|| match self.estimated {
            Some(EstimatedField::Amount(v)) => Some(v),
            _ => None,
        })
    }
}
