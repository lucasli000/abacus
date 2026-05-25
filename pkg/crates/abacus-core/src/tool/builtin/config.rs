//! config — Runtime configuration read/write tools
//!
//! ## 场景
//! LLM 在 session 中动态读取和修改 CoreConfig 参数。
//! 修改通过 runtime_overrides HashMap 中转，pipeline 各消费点按需读取。
//!
//! ## 依赖
//! - `CoreLoop.runtime_overrides: Arc<std::sync::RwLock<HashMap<String, String>>>`
//! - `CoreConfig`（base config 快照用于 config_get 展示默认值）
//!
//! ## 引用关系
//! - 注册：`builtin::mod.rs::register_all()` 注册 schema + executor
//! - 执行：CoreLoop::process_turn → ToolRegistry → ConfigToolExecutor
//! - 消费方：pipeline 各 config 读取点通过 `CoreLoop::get_effective_*` 读取覆盖值
//!
//! ## 工具
//! | Tool | Confirm | Risk | Description |
//! |------|---------|------|-------------|
//! | config_get | no | low | 读取当前运行时配置值 |
//! | config_set | no | low | 修改运行时配置参数 |

use std::collections::HashMap;
use std::sync::Arc;

use abacus_types::{
    KernelError, ThinkingIntent, ToolCost, ToolEffectiveness, ToolHandle, ToolId,
    ToolProvider, ToolSchema, ToolSecurity, ToolState,
};
use async_trait::async_trait;
use serde_json::{json, Value};

use crate::core::context::CompressLevel;
use crate::core::CoreConfig;
use crate::tool::{ExecutionContext, ToolExecutor, ToolRegistry};

/// 支持的配置键列表（用于 config_get 全量输出和 config_set 校验）
///
/// 引用关系：
/// - config_get: 遍历此列表输出所有键值
/// - config_set: 校验 key 是否在此列表中
const SUPPORTED_KEYS: &[&str] = &[
    "thinking",
    "max_turns",
    "max_tool_calls",
    "max_tokens",
    "temperature",
    "scene_tool_loading",
    "task_kind_routing",
    "tool_frequency_pruning_turns",
    "adaptive_d_tier_hide",
    "silent_router",
    "max_escalations",
    "compress_level",
];

/// Runtime overrides 共享类型别名
///
/// 生命周期：
/// - 创建：CoreLoop::new() 时 Arc::new(RwLock::new(HashMap::new()))
/// - 写入：ConfigToolExecutor::execute (config_set)
/// - 读取：ConfigToolExecutor::execute (config_get) + pipeline 各消费点
/// - 销毁：随 CoreLoop drop（Arc 引用计数归零）
pub type RuntimeOverrides = Arc<std::sync::RwLock<HashMap<String, String>>>;

/// Executor for config_get / config_set
///
/// 持有：
/// - `overrides`: 运行时覆盖 map（与 CoreLoop 共享同一 Arc）
/// - `base_config`: 启动时 CoreConfig 快照（只读，用于 config_get 展示默认值）
pub struct ConfigToolExecutor {
    overrides: RuntimeOverrides,
    base_config: CoreConfig,
}

impl ConfigToolExecutor {
    pub fn new(overrides: RuntimeOverrides, base_config: CoreConfig) -> Self {
        Self { overrides, base_config }
    }

    /// config_get 实现：返回所有支持的配置键及其当前有效值
    fn get(&self, params: Value) -> abacus_types::Result<Value> {
        let specific_key = params.get("key").and_then(|v| v.as_str());
        let overrides = self.overrides.read().unwrap_or_else(|p| p.into_inner());

        let mut result = serde_json::Map::new();

        for &key in SUPPORTED_KEYS {
            if let Some(specific) = specific_key {
                if key != specific {
                    continue;
                }
            }
            let (effective_value, is_overridden) = match overrides.get(key) {
                Some(v) => (v.clone(), true),
                None => (self.base_value(key), false),
            };
            result.insert(key.to_string(), json!({
                "value": effective_value,
                "overridden": is_overridden,
            }));
        }

        if let Some(specific) = specific_key {
            if result.is_empty() {
                return Err(KernelError::Other(format!(
                    "unknown config key: '{}'. Supported: {}",
                    specific,
                    SUPPORTED_KEYS.join(", ")
                )));
            }
        }

        Ok(Value::Object(result))
    }

    /// config_set 实现：解析 value 并写入 overrides map
    fn set(&self, params: Value) -> abacus_types::Result<Value> {
        let key = params.get("key")
            .and_then(|v| v.as_str())
            .ok_or_else(|| KernelError::Other("missing required parameter: key".into()))?;
        let value = params.get("value")
            .and_then(|v| v.as_str())
            .ok_or_else(|| KernelError::Other("missing required parameter: value".into()))?;

        // 校验 key 是否支持
        if !SUPPORTED_KEYS.contains(&key) {
            return Err(KernelError::Other(format!(
                "unknown config key: '{}'. Supported: {}",
                key,
                SUPPORTED_KEYS.join(", ")
            )));
        }

        // 校验 value 是否可解析为目标类型
        self.validate_value(key, value)?;

        // 写入 overrides
        self.overrides.write().unwrap_or_else(|p| p.into_inner()).insert(key.to_string(), value.to_string());

        Ok(json!({
            "status": "ok",
            "key": key,
            "value": value,
            "note": "Change takes effect on the next LLM turn.",
        }))
    }

    /// 从 base_config 读取指定 key 的默认值（字符串表示）
    fn base_value(&self, key: &str) -> String {
        match key {
            "thinking" => match &self.base_config.thinking_intent {
                Some(intent) => intent.to_str(),
                None => "off".into(),
            },
            "max_turns" => self.base_config.max_turns_per_request.to_string(),
            "max_tool_calls" => self.base_config.max_tool_calls_per_turn.to_string(),
            "max_tokens" => self.base_config.default_max_tokens.to_string(),
            "temperature" => self.base_config.default_temperature.to_string(),
            "scene_tool_loading" => self.base_config.scene_tool_loading_enabled.to_string(),
            "task_kind_routing" => self.base_config.task_kind_routing_enabled.to_string(),
            "tool_frequency_pruning_turns" => match self.base_config.tool_frequency_pruning_turns {
                Some(n) => n.to_string(),
                None => "off".into(),
            },
            "adaptive_d_tier_hide" => self.base_config.adaptive_d_tier_hide.to_string(),
            "silent_router" => self.base_config.silent_router_enabled.to_string(),
            "max_escalations" => self.base_config.max_escalations.to_string(),
            "compress_level" => match self.base_config.default_compress_level {
                CompressLevel::Brief => "brief".into(),
                CompressLevel::Detailed => "detailed".into(),
                CompressLevel::Minimal => "minimal".into(),
            },
            _ => "unknown".into(),
        }
    }

    /// 类型校验：确保 value 能被正确解析为目标类型
    fn validate_value(&self, key: &str, value: &str) -> abacus_types::Result<()> {
        match key {
            "thinking" => {
                if ThinkingIntent::from_str_loose(value).is_none() {
                    return Err(KernelError::Other(format!(
                        "invalid thinking value: '{}'. Expected: off/adaptive/low/medium/high/max/xhigh or integer budget",
                        value
                    )));
                }
            }
            "max_turns" | "max_tool_calls" | "max_tokens" | "max_escalations" => {
                value.parse::<u32>().map_err(|_| KernelError::Other(format!(
                    "invalid u32 value for '{}': '{}'", key, value
                )))?;
            }
            "temperature" => {
                value.parse::<f64>().map_err(|_| KernelError::Other(format!(
                    "invalid f64 value for 'temperature': '{}'", value
                )))?;
            }
            "scene_tool_loading" | "task_kind_routing" | "adaptive_d_tier_hide" | "silent_router" => {
                parse_bool(value).ok_or_else(|| KernelError::Other(format!(
                    "invalid bool value for '{}': '{}'. Expected: true/false/1/0/on/off",
                    key, value
                )))?;
            }
            "tool_frequency_pruning_turns" => {
                // "off" or u64
                let lower = value.to_lowercase();
                if lower != "off" && lower != "none" && lower != "disabled" {
                    value.parse::<u64>().map_err(|_| KernelError::Other(format!(
                        "invalid value for 'tool_frequency_pruning_turns': '{}'. Expected: number or 'off'",
                        value
                    )))?;
                }
            }
            "compress_level" => {
                let lower = value.to_lowercase();
                if !matches!(lower.as_str(), "brief" | "detailed" | "minimal") {
                    return Err(KernelError::Other(format!(
                        "invalid compress_level: '{}'. Expected: brief/detailed/minimal",
                        value
                    )));
                }
            }
            _ => {}
        }
        Ok(())
    }
}

#[async_trait]
impl ToolExecutor for ConfigToolExecutor {
    async fn execute(&self, tool_id: &ToolId, params: Value, _ctx: &ExecutionContext) -> abacus_types::Result<Value> {
        match tool_id.0.as_str() {
            "config_get" => self.get(params),
            "config_set" => self.set(params),
            _ => Err(KernelError::Other(format!("unknown tool: {}", tool_id.0))),
        }
    }
}

// ─── Helpers ───────────────────────────────────────────────────────────────

/// 宽松 bool 解析
fn parse_bool(s: &str) -> Option<bool> {
    match s.to_lowercase().as_str() {
        "true" | "1" | "on" | "yes" | "enabled" => Some(true),
        "false" | "0" | "off" | "no" | "disabled" => Some(false),
        _ => None,
    }
}

// ─── Public helpers for pipeline consumption ───────────────────────────────

/// 从 runtime_overrides 读取并解析指定 key 的覆盖值。
/// 返回 None 表示未覆盖（调用方应使用 base config 值）。
///
/// 引用关系：
/// - 消费方：CoreLoop pipeline 各消费点（execute_loop, build_tool_definitions_for 等）
/// - 依赖：CoreLoop.runtime_overrides（同一 Arc）
pub fn read_override_u32(overrides: &RuntimeOverrides, key: &str) -> Option<u32> {
    overrides.read().unwrap_or_else(|p| p.into_inner()).get(key).and_then(|v| v.parse().ok())
}

pub fn read_override_u64(overrides: &RuntimeOverrides, key: &str) -> Option<u64> {
    overrides.read().unwrap_or_else(|p| p.into_inner()).get(key).and_then(|v| v.parse().ok())
}

pub fn read_override_f64(overrides: &RuntimeOverrides, key: &str) -> Option<f64> {
    overrides.read().unwrap_or_else(|p| p.into_inner()).get(key).and_then(|v| v.parse().ok())
}

pub fn read_override_bool(overrides: &RuntimeOverrides, key: &str) -> Option<bool> {
    overrides.read().unwrap_or_else(|p| p.into_inner()).get(key).and_then(|v| parse_bool(v))
}

pub fn read_override_thinking(overrides: &RuntimeOverrides) -> Option<ThinkingIntent> {
    overrides.read().unwrap_or_else(|p| p.into_inner()).get("thinking").and_then(|v| ThinkingIntent::from_str_loose(v))
}

pub fn read_override_compress_level(overrides: &RuntimeOverrides) -> Option<CompressLevel> {
    overrides.read().unwrap_or_else(|p| p.into_inner()).get("compress_level").map(|v| CompressLevel::from_str(v))
}

// ─── Schema ────────────────────────────────────────────────────────────────

pub fn schemas() -> Vec<ToolSchema> {
    vec![
        ToolSchema {
            name: "config_get".into(),
            description: "List current runtime config values. Returns all or a specific key.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "key": {
                        "type": "string",
                        "description": "Optional: specific config key to query. Omit for all."
                    }
                }
            }),
            returns: None,
            security: Some(ToolSecurity {
                allowed_paths: None,
                max_size_mb: None,
                confirm_required: false,
                needs_sandbox: false,
            }),
            cost: Some(ToolCost { tokens: 16, latency: "1ms".into(), risk: "low".into() }),
            examples: Vec::new(),
            applicable_task_kinds: None,
            idempotent: true,
        },
        ToolSchema {
            name: "config_set".into(),
            description: "Modify a runtime config parameter. Changes apply immediately.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "key": {
                        "type": "string",
                        "description": "Config key to set"
                    },
                    "value": {
                        "type": "string",
                        "description": "New value (auto-parsed to correct type)"
                    }
                },
                "required": ["key", "value"]
            }),
            returns: None,
            security: Some(ToolSecurity {
                allowed_paths: None,
                max_size_mb: None,
                confirm_required: false,
                needs_sandbox: false,
            }),
            cost: Some(ToolCost { tokens: 16, latency: "1ms".into(), risk: "low".into() }),
            examples: Vec::new(),
            applicable_task_kinds: None,
            idempotent: false,
        },
    ]
}

// ─── Registration ──────────────────────────────────────────────────────────

/// 注册 schema（executor 需在 CoreLoop::new 中单独注册——依赖 runtime_overrides Arc）
pub async fn register(registry: &ToolRegistry) {
    for s in schemas() {
        registry.register(ToolHandle {
            id: ToolId(s.name.clone()),
            schema: s,
            provider: ToolProvider::BuiltIn,
            state: ToolState::Loaded,
            effectiveness: ToolEffectiveness::default(),
        }).await;
    }
}

/// 注册 executor（与 schema 分离：runtime_overrides 在 CoreLoop::new() 才可用）
///
/// 引用关系：
/// - 调用方：CoreLoop::new()（在 register_all 之后调用）
/// - 依赖：runtime_overrides Arc（与 CoreLoop 共享）、base_config 快照
pub async fn register_executors(
    registry: &ToolRegistry,
    overrides: RuntimeOverrides,
    base_config: CoreConfig,
) {
    let executor = Arc::new(ConfigToolExecutor::new(overrides, base_config));
    for s in schemas() {
        registry.register_executor(ToolId(s.name.clone()), executor.clone()).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_overrides() -> RuntimeOverrides {
        Arc::new(std::sync::RwLock::new(HashMap::new()))
    }

    #[tokio::test]
    async fn test_config_get_all_returns_all_keys() {
        let overrides = make_overrides();
        let exec = ConfigToolExecutor::new(overrides, CoreConfig::default());
        let ctx = crate::tool::ExecutionContext::noop("test");
        let result = exec.execute(
            &ToolId("config_get".into()),
            json!({}),
            &ctx,
        ).await.unwrap();
        // Should have all supported keys
        for &key in SUPPORTED_KEYS {
            assert!(result.get(key).is_some(), "missing key: {}", key);
        }
    }

    #[tokio::test]
    async fn test_config_get_specific_key() {
        let overrides = make_overrides();
        let exec = ConfigToolExecutor::new(overrides, CoreConfig::default());
        let ctx = crate::tool::ExecutionContext::noop("test");
        let result = exec.execute(
            &ToolId("config_get".into()),
            json!({"key": "temperature"}),
            &ctx,
        ).await.unwrap();
        assert!(result.get("temperature").is_some());
        // Should NOT have other keys
        assert!(result.get("max_turns").is_none());
    }

    #[tokio::test]
    async fn test_config_get_unknown_key_errors() {
        let overrides = make_overrides();
        let exec = ConfigToolExecutor::new(overrides, CoreConfig::default());
        let ctx = crate::tool::ExecutionContext::noop("test");
        let result = exec.execute(
            &ToolId("config_get".into()),
            json!({"key": "nonexistent"}),
            &ctx,
        ).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_config_set_valid_temperature() {
        let overrides = make_overrides();
        let exec = ConfigToolExecutor::new(overrides.clone(), CoreConfig::default());
        let ctx = crate::tool::ExecutionContext::noop("test");
        let result = exec.execute(
            &ToolId("config_set".into()),
            json!({"key": "temperature", "value": "0.8"}),
            &ctx,
        ).await.unwrap();
        assert_eq!(result["status"], "ok");
        // Verify override was written
        assert_eq!(overrides.read().unwrap().get("temperature").unwrap(), "0.8");
    }

    #[tokio::test]
    async fn test_config_set_invalid_temperature() {
        let overrides = make_overrides();
        let exec = ConfigToolExecutor::new(overrides, CoreConfig::default());
        let ctx = crate::tool::ExecutionContext::noop("test");
        let result = exec.execute(
            &ToolId("config_set".into()),
            json!({"key": "temperature", "value": "not_a_number"}),
            &ctx,
        ).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_config_set_thinking_intent() {
        let overrides = make_overrides();
        let exec = ConfigToolExecutor::new(overrides.clone(), CoreConfig::default());
        let ctx = crate::tool::ExecutionContext::noop("test");
        let result = exec.execute(
            &ToolId("config_set".into()),
            json!({"key": "thinking", "value": "high"}),
            &ctx,
        ).await.unwrap();
        assert_eq!(result["status"], "ok");
        assert_eq!(overrides.read().unwrap().get("thinking").unwrap(), "high");
    }

    #[tokio::test]
    async fn test_config_set_bool_field() {
        let overrides = make_overrides();
        let exec = ConfigToolExecutor::new(overrides.clone(), CoreConfig::default());
        let ctx = crate::tool::ExecutionContext::noop("test");
        let result = exec.execute(
            &ToolId("config_set".into()),
            json!({"key": "silent_router", "value": "true"}),
            &ctx,
        ).await.unwrap();
        assert_eq!(result["status"], "ok");
        assert_eq!(read_override_bool(&overrides, "silent_router"), Some(true));
    }

    #[tokio::test]
    async fn test_config_set_pruning_turns_off() {
        let overrides = make_overrides();
        let exec = ConfigToolExecutor::new(overrides.clone(), CoreConfig::default());
        let ctx = crate::tool::ExecutionContext::noop("test");
        let result = exec.execute(
            &ToolId("config_set".into()),
            json!({"key": "tool_frequency_pruning_turns", "value": "off"}),
            &ctx,
        ).await.unwrap();
        assert_eq!(result["status"], "ok");
    }

    #[tokio::test]
    async fn test_config_set_unknown_key_errors() {
        let overrides = make_overrides();
        let exec = ConfigToolExecutor::new(overrides, CoreConfig::default());
        let ctx = crate::tool::ExecutionContext::noop("test");
        let result = exec.execute(
            &ToolId("config_set".into()),
            json!({"key": "nonexistent", "value": "foo"}),
            &ctx,
        ).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_config_get_shows_override() {
        let overrides = make_overrides();
        overrides.write().unwrap().insert("temperature".into(), "0.9".into());
        let exec = ConfigToolExecutor::new(overrides, CoreConfig::default());
        let ctx = crate::tool::ExecutionContext::noop("test");
        let result = exec.execute(
            &ToolId("config_get".into()),
            json!({"key": "temperature"}),
            &ctx,
        ).await.unwrap();
        assert_eq!(result["temperature"]["value"], "0.9");
        assert_eq!(result["temperature"]["overridden"], true);
    }

    #[test]
    fn test_read_override_helpers() {
        let overrides = make_overrides();
        overrides.write().unwrap().insert("max_turns".into(), "50".into());
        overrides.write().unwrap().insert("temperature".into(), "0.7".into());
        overrides.write().unwrap().insert("silent_router".into(), "on".into());

        assert_eq!(read_override_u32(&overrides, "max_turns"), Some(50));
        assert_eq!(read_override_f64(&overrides, "temperature"), Some(0.7));
        assert_eq!(read_override_bool(&overrides, "silent_router"), Some(true));
        assert_eq!(read_override_u32(&overrides, "nonexistent"), None);
    }

    #[test]
    fn test_read_override_thinking() {
        let overrides = make_overrides();
        overrides.write().unwrap().insert("thinking".into(), "high".into());
        let intent = read_override_thinking(&overrides).unwrap();
        assert!(matches!(intent, ThinkingIntent::Effort(abacus_types::EffortLevel::High)));
    }

    #[test]
    fn test_schema_descriptions_under_150_bytes() {
        for s in schemas() {
            assert!(
                s.description.len() <= 150,
                "config tool '{}' description too long: {} bytes",
                s.name, s.description.len()
            );
        }
    }
}
