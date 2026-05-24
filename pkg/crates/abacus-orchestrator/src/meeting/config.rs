//! # AbacusOrchestratorConfig — YAML 配置加载
//!
//! ## 场景
//! 系统初始化时从 YAML 文件加载配置，包含:
//! - `MeetingConfig`: 会议参数（容量/超时/并发）
//! - `Vec<SpecialistRegistration>`: Specialist 注册列表（可覆盖默认）
//!
//! ## 依赖链
//! ```text
//! crate::specialist (SpecialistRegistration)
//! crate::meeting::defaults (default_specialists)
//!   └── crate::meeting::config ← 本文件
//! ```
//!
//! ## 边界
//! - 未提供 YAML 时完全回退 `Default::default()`（含默认 Specialist）
//! - `from_file()` 返回 `Err(String)` 而非 `io::Error`（简化调用方）

use serde::{Deserialize, Serialize};
use crate::specialist::SpecialistRegistration;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeetingConfig {
    #[serde(default = "default_max_participants")]
    pub max_participants: u32,
    #[serde(default = "default_max_concurrent_llm")]
    pub max_concurrent_llm: u32,
    #[serde(default = "default_idle_timeout_secs")]
    pub idle_timeout_secs: u64,
    #[serde(default = "default_max_duration_minutes")]
    pub max_duration_minutes: u64,
    #[serde(default = "default_timeline_view_size")]
    pub timeline_view_size: usize,
}

fn default_max_participants() -> u32 { 8 }
fn default_max_concurrent_llm() -> u32 { 4 }
fn default_idle_timeout_secs() -> u64 { 300 }
fn default_max_duration_minutes() -> u64 { 60 }
fn default_timeline_view_size() -> usize { 5 }

impl Default for MeetingConfig {
    fn default() -> Self {
        Self {
            max_participants: 8,
            max_concurrent_llm: 4,
            idle_timeout_secs: 300,
            max_duration_minutes: 60,
            timeline_view_size: 5,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AbacusOrchestratorConfig {
    #[serde(default)]
    pub meeting: MeetingConfig,
    #[serde(default)]
    pub specialists: Vec<SpecialistRegistration>,
}

impl Default for AbacusOrchestratorConfig {
    fn default() -> Self {
        Self {
            meeting: MeetingConfig::default(),
            specialists: crate::meeting::defaults::default_specialists(),
        }
    }
}

impl AbacusOrchestratorConfig {
    pub fn from_yaml(yaml: &str) -> Result<Self, String> {
        serde_yaml::from_str(yaml).map_err(|e| e.to_string())
    }

    pub fn from_file(path: &str) -> Result<Self, String> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("读取配置文件失败: {}", e))?;
        Self::from_yaml(&content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config_uses_defaults() {
        let cfg = AbacusOrchestratorConfig::default();
        assert_eq!(cfg.meeting.max_participants, 8);
        assert_eq!(cfg.meeting.max_concurrent_llm, 4);
        assert!(cfg.specialists.is_empty());
    }

    #[test]
    fn test_from_yaml_minimal() {
        let yaml = r#"
meeting:
  max_participants: 4
  max_concurrent_llm: 2
"#;
        let cfg = AbacusOrchestratorConfig::from_yaml(yaml).unwrap();
        assert_eq!(cfg.meeting.max_participants, 4);
        assert_eq!(cfg.meeting.max_concurrent_llm, 2);
        assert_eq!(cfg.meeting.timeline_view_size, 5);
        assert!(cfg.specialists.is_empty());
    }

    #[test]
    fn test_from_yaml_with_specialists() {
        let yaml = r#"
meeting:
  max_participants: 8
specialists:
  - id: designer
    domain: ux_design
    name: Designer
    role: Member
    model: test
    guide_strategy: ""
    anti_pattern: ""
    capabilities: []
    tags: []
    allowed_tools: []
"#;
        let cfg = AbacusOrchestratorConfig::from_yaml(yaml).unwrap();
        assert_eq!(cfg.specialists.len(), 1);
        assert_eq!(cfg.specialists[0].id, "designer");
    }

    #[test]
    fn test_from_yaml_invalid_returns_err() {
        let result = AbacusOrchestratorConfig::from_yaml("not: valid: yaml: [[[");
        assert!(result.is_err());
    }

    #[test]
    fn test_finance_example_parses() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../examples/meeting-finance.yaml");
        let cfg = AbacusOrchestratorConfig::from_file(path).unwrap();
        assert_eq!(cfg.specialists.len(), 2);
        assert_eq!(cfg.specialists[0].id, "analyst");
        assert_eq!(cfg.specialists[0].domain, "financial_analysis");
        assert_eq!(cfg.specialists[1].id, "trader");
        assert_eq!(cfg.specialists[1].domain, "trading_strategy");
    }
}
