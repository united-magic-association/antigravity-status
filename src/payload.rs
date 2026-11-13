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
    pub model: Option<ModelInfo>,
    pub workspace: Option<WorkspaceInfo>,
    pub context_window: Option<ContextWindow>,
    pub cost: Option<CostInfo>,
}

impl StatusPayload {
    /// Returns the active session identifier, preferring `session_id` over `conversation_id`.
    pub fn effective_session_id(&self) -> Option<&str> {
        self.session_id
            .as_deref()
            .filter(|s| !s.is_empty())
            .or_else(|| self.conversation_id.as_deref().filter(|s| !s.is_empty()))
    }
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
    Bool(bool),
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
