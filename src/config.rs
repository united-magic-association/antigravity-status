use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub general: GeneralConfig,
    pub format: FormatConfig,
    pub modules: ModulesConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct GeneralConfig {
    pub separator: String,
    pub order: Vec<String>,
    pub lines: Vec<Vec<String>>,
    pub hide_zero_cache_write: bool,
    pub hide_missing_cost: bool,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            separator: " │ ".to_string(),
            order: vec![
                "context_gauge".to_string(),
                "session_input_tokens".to_string(),
                "session_cache_read_tokens".to_string(),
                "session_cache_write_tokens".to_string(),
                "session_output_tokens".to_string(),
                "session_cost".to_string(),
            ],
            lines: Vec::new(),
            hide_zero_cache_write: false,
            hide_missing_cost: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct FormatConfig {
    pub compact_tokens: bool,
    pub cost_precision: usize,
    pub color: bool,
}

impl Default for FormatConfig {
    fn default() -> Self {
        Self {
            compact_tokens: true,
            cost_precision: 4,
            color: true,
        }
    }
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct ModulesConfig {
    pub context_gauge: ContextGaugeConfig,
    pub session_input_tokens: TokenModuleConfig<InputDefault>,
    pub session_cache_read_tokens: TokenModuleConfig<CacheReadDefault>,
    pub session_cache_write_tokens: TokenModuleConfig<CacheWriteDefault>,
    pub session_output_tokens: TokenModuleConfig<OutputDefault>,
    pub session_cost: CostModuleConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ContextGaugeConfig {
    pub enabled: bool,
    pub label: String,
    pub width: usize,
    pub warn_percent: f64,
    pub critical_percent: f64,
    pub filled_char: String,
    pub empty_char: String,
}

impl Default for ContextGaugeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            label: "Ctx".to_string(),
            width: 10,
            warn_percent: 70.0,
            critical_percent: 90.0,
            filled_char: "█".to_string(),
            empty_char: "░".to_string(),
        }
    }
}

pub trait DefaultLabel {
    fn default_label() -> &'static str;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct InputDefault;
impl DefaultLabel for InputDefault {
    fn default_label() -> &'static str {
        "In"
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct CacheReadDefault;
impl DefaultLabel for CacheReadDefault {
    fn default_label() -> &'static str {
        "CacheR"
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct CacheWriteDefault;
impl DefaultLabel for CacheWriteDefault {
    fn default_label() -> &'static str {
        "CacheW"
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct OutputDefault;
impl DefaultLabel for OutputDefault {
    fn default_label() -> &'static str {
        "Out"
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct TokenModuleConfig<L: DefaultLabel> {
    pub enabled: bool,
    pub label: String,
    #[serde(skip)]
    _marker: std::marker::PhantomData<L>,
}

impl<L: DefaultLabel> Default for TokenModuleConfig<L> {
    fn default() -> Self {
        Self {
            enabled: true,
            label: L::default_label().to_string(),
            _marker: std::marker::PhantomData,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CostModuleConfig {
    pub enabled: bool,
    pub label: String,
    pub show_subagent: bool,
}

impl Default for CostModuleConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            label: "Cost".to_string(),
            show_subagent: false,
        }
    }
}

/// Resolves and parses `config.toml` according to CLI, env, project, and user precedence.
pub fn load_config(explicit_path: Option<&Path>, payload_cwd: Option<&str>) -> Config {
    if let Some(path) = explicit_path {
        return parse_config_file(path).unwrap_or_default();
    }

    if let Ok(env_path) = std::env::var("AGY_STATUS_CONFIG")
        && !env_path.is_empty()
    {
        let path = PathBuf::from(env_path);
        if path.exists() {
            return parse_config_file(&path).unwrap_or_default();
        }
    }

    let project_dir = payload_cwd
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok());

    if let Some(dir) = project_dir {
        let local_cfg = dir.join(".antigravity-status.toml");
        if local_cfg.is_file() {
            return parse_config_file(&local_cfg).unwrap_or_default();
        }
    }

    if let Some(global_cfg) = default_global_config_path()
        && global_cfg.is_file()
    {
        return parse_config_file(&global_cfg).unwrap_or_default();
    }

    Config::default()
}

pub fn default_global_config_path() -> Option<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        return Some(
            PathBuf::from(xdg)
                .join("antigravity-status")
                .join("config.toml"),
        );
    }
    if let Ok(home) = std::env::var("HOME")
        && !home.is_empty()
    {
        return Some(
            PathBuf::from(home)
                .join(".config")
                .join("antigravity-status")
                .join("config.toml"),
        );
    }
    None
}

pub fn parse_config_str(raw: &str) -> Result<Config, toml::de::Error> {
    toml::from_str(raw)
}

fn parse_config_file(path: &Path) -> Option<Config> {
    let raw = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(err) => {
            eprintln!("antigravity-status: failed to read {}: {err}", path.display());
            return None;
        }
    };

    match parse_config_str(&raw) {
        Ok(cfg) => Some(cfg),
        Err(err) => {
            eprintln!("antigravity-status: invalid TOML in {}: {err}", path.display());
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_config_parses_cleanly() {
        let raw = include_str!("../config.example.toml");
        let cfg = parse_config_str(raw).expect("config.example.toml should parse");
        assert!(cfg.modules.context_gauge.enabled);
        assert!(cfg.modules.session_cost.enabled);
        assert_eq!(cfg.general.order.len(), 6);
    }

    #[test]
    fn partial_config_toggles_one_module_and_preserves_defaults() {
        let raw = r#"
[modules.session_cache_write_tokens]
enabled = false

[modules.session_cost]
enabled = false
"#;
        let cfg = parse_config_str(raw).expect("partial TOML should parse");
        assert!(cfg.modules.context_gauge.enabled);
        assert!(cfg.modules.session_input_tokens.enabled);
        assert!(cfg.modules.session_cache_read_tokens.enabled);
        assert!(!cfg.modules.session_cache_write_tokens.enabled);
        assert!(cfg.modules.session_output_tokens.enabled);
        assert!(!cfg.modules.session_cost.enabled);
        assert_eq!(cfg.modules.session_input_tokens.label, "In");
    }
}
