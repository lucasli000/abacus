//! ConfigManager — 多源配置合并
//!
//! ## 依赖
//! - `serde`: 配置反序列化
//! - `serde_json`: JSON 配置源
//! - 环境变量: `ABACUS_*` 前缀
//!
//! ## 引用关系
//! - 被 `CoreLoop` 初始化时调用
//! - 被 `SafetyGuard` 读取安全限制
//! - 被 `PromptAssembly` 读取行为规则
//!
//! ## 配置源优先级 (从高到低)
//! 1. CLI 参数 (runtime overrides)
//! 2. 环境变量 (ABACUS_*)
//! 3. YAML 文件 (~/.abacus/config.yaml)
//! 4. 内置默认值

use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;
use serde_json::Value;

/// 配置值，支持多种类型
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ConfigValue {
    String(String),
    Number(f64),
    Bool(bool),
    List(Vec<ConfigValue>),
    Map(HashMap<String, ConfigValue>),
    Null,
}

impl ConfigValue {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            ConfigValue::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_number(&self) -> Option<f64> {
        match self {
            ConfigValue::Number(n) => Some(*n),
            _ => None,
        }
    }

    /// 转换为 serde_json::Value（用于 get_typed 反序列化复杂结构）
    /// 引用关系：被 ConfigManager::get_typed 调用，无副作用。
    pub fn to_json(&self) -> Value {
        match self {
            ConfigValue::String(s) => Value::String(s.clone()),
            ConfigValue::Number(n) => serde_json::Number::from_f64(*n)
                .map(Value::Number)
                .unwrap_or(Value::Null),
            ConfigValue::Bool(b) => Value::Bool(*b),
            ConfigValue::Null => Value::Null,
            ConfigValue::List(items) => {
                Value::Array(items.iter().map(|i| i.to_json()).collect())
            }
            ConfigValue::Map(map) => {
                let mut obj = serde_json::Map::new();
                for (k, v) in map {
                    obj.insert(k.clone(), v.to_json());
                }
                Value::Object(obj)
            }
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            ConfigValue::Bool(b) => Some(*b),
            ConfigValue::String(s) => match s.as_str() {
                "true" | "1" | "yes" => Some(true),
                "false" | "0" | "no" => Some(false),
                _ => None,
            },
            _ => None,
        }
    }
}

/// 配置源
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigSource {
    /// 内置默认值 (最低优先级)
    Default,
    /// YAML 文件
    File(String),
    /// 环境变量
    Env(String),
    /// CLI 参数 (最高优先级)
    Cli(String),
}

/// 带来源的配置值
#[derive(Debug, Clone)]
pub struct TaggedValue {
    pub value: ConfigValue,
    pub source: ConfigSource,
    pub key: String,
}

/// 配置管理器
pub struct ConfigManager {
    /// 合并后的配置 (已按优先级排序)
    merged: HashMap<String, TaggedValue>,
}

impl ConfigManager {
    /// 创建新的配置管理器，从默认值开始
    pub fn new(defaults: HashMap<String, ConfigValue>) -> Self {
        let merged = defaults.iter()
            .map(|(k, v)| (k.clone(), TaggedValue {
                value: v.clone(),
                source: ConfigSource::Default,
                key: k.clone(),
            }))
            .collect();

        Self { merged }
    }

    /// 从配置文件加载 (自动检测 JSON/YAML 由扩展名决定)
    pub fn load_file(&mut self, path: impl AsRef<Path>) -> Result<(), String> {
        let content = std::fs::read_to_string(&path)
            .map_err(|e| format!("failed to read config file: {e}"))?;

        let flat = match path.as_ref().extension().and_then(|e| e.to_str()) {
            Some("json") => {
                let json_value: serde_json::Value = serde_json::from_str(&content)
                    .map_err(|e| format!("failed to parse JSON: {e}"))?;
                flatten_json(&json_value, "")
            }
            Some("yaml") | Some("yml") => {
                let yaml_value: serde_yaml::Value = serde_yaml::from_str(&content)
                    .map_err(|e| format!("failed to parse YAML: {e}"))?;
                flatten_yaml(&yaml_value, "")
            }
            _ => {
                // Try JSON first, fall back to YAML
                if let Ok(json_value) = serde_json::from_str::<serde_json::Value>(&content) {
                    flatten_json(&json_value, "")
                } else if let Ok(yaml_value) = serde_yaml::from_str::<serde_yaml::Value>(&content) {
                    flatten_yaml(&yaml_value, "")
                } else {
                    return Err("config file is neither valid JSON nor YAML".into());
                }
            }
        };

        for (key, value) in flat {
            self.merged.insert(key.clone(), TaggedValue {
                value,
                source: ConfigSource::File(path.as_ref().to_string_lossy().to_string()),
                key,
            });
        }

        Ok(())
    }

    /// 加载目录下所有 *.yaml / *.json 文件（按文件名字母序合并）
    ///
    /// ## 场景
    /// `~/.abacus/conf.d/` 中的分域配置文件。
    /// 文件名加数字前缀可控合并顺序（如 `10-security.yaml`）。
    /// 目录不存在时静默跳过。
    pub fn load_dir(&mut self, dir: impl AsRef<Path>) {
        let dir = dir.as_ref();
        if !dir.exists() { return; }
        let mut entries: Vec<_> = match std::fs::read_dir(dir) {
            Ok(rd) => rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| matches!(p.extension().and_then(|e| e.to_str()), Some("yaml") | Some("yml") | Some("json")))
                .collect(),
            Err(_) => return,
        };
        entries.sort(); // 字母顺序保证确定性
        for path in entries {
            if let Err(e) = self.load_file(&path) {
                tracing::warn!("conf.d load error {:?}: {e}", path.file_name());
            }
        }
    }

    /// 读取字符串列表配置项（如 mcip.exempt_prefixes）
    pub fn get_list(&self, key: &str) -> Option<Vec<String>> {
        match self.merged.get(key).map(|t| &t.value) {
            Some(ConfigValue::List(items)) => {
                let strings: Vec<String> = items.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect();
                if strings.is_empty() { None } else { Some(strings) }
            }
            _ => None,
        }
    }

    /// 从环境变量加载配置 (ABACUS_* 前缀)
    pub fn load_env(&mut self, prefix: &str) {
        for (key, value) in std::env::vars() {
            if let Some(suffix) = key.strip_prefix(prefix) {
                let config_key = suffix.to_lowercase().replace("__", ".");
                let config_value = parse_env_value(&value);
                self.merged.insert(config_key.clone(), TaggedValue {
                    value: config_value,
                    source: ConfigSource::Env(key),
                    key: config_key,
                });
            }
        }
    }

    /// 从 CLI 参数加载配置
    pub fn load_cli(&mut self, args: &[String]) {
        let mut i = 0;
        while i < args.len() {
            let arg = &args[i];
            if let Some(key_part) = arg.strip_prefix("--") {
                let key = key_part.to_string();
                let value = if i + 1 < args.len() && !args[i + 1].starts_with("--") {
                    i += 1;
                    parse_env_value(&args[i])
                } else {
                    ConfigValue::Bool(true)
                };
                self.merged.insert(key.clone(), TaggedValue {
                    value,
                    source: ConfigSource::Cli(arg.clone()),
                    key,
                });
            }
            i += 1;
        }
    }

    /// 从 JSON Value 加载配置
    pub fn load_json(&mut self, json: &Value, source: ConfigSource) {
        let flat = flatten_json(json, "");
        for (key, value) in flat {
            self.merged.insert(key.clone(), TaggedValue {
                value,
                source: source.clone(),
                key,
            });
        }
    }

    /// Validate loaded configuration against known constraints.
    /// Returns a list of warnings for invalid values (auto-corrected to defaults).
    pub fn validate(&mut self) -> Vec<String> {
        let mut warnings = Vec::new();
        // Numeric range validations
        let range_rules: &[(&str, f64, f64, f64)] = &[
            ("core.max_tokens", 100.0, 1_000_000.0, 8192.0),
            ("core.temperature", 0.0, 2.0, 0.7),
            ("core.max_turns", 1.0, 100.0, 25.0),  // V29.13: default 同步 default_config()
            ("server.max_sessions", 1.0, 100_000.0, 1000.0),
            ("server.rate_limit_per_sec", 1.0, 10_000.0, 60.0),
            ("pressure.soft_threshold", 0.1, 1.0, 0.70),
            ("pressure.hard_threshold", 0.1, 1.0, 0.85),
        ];
        for &(key, min, max, default) in range_rules {
            if let Some(val) = self.get_number(key) {
                if val < min || val > max {
                    warnings.push(format!(
                        "config '{}' = {} is out of range [{}, {}], reset to {}",
                        key, val, min, max, default
                    ));
                    self.merged.insert(key.to_string(), TaggedValue {
                        value: ConfigValue::Number(default),
                        source: ConfigSource::Default,
                        key: key.to_string(),
                    });
                }
            }
        }
        warnings
    }

    /// 获取配置值
    pub fn get(&self, key: &str) -> Option<&TaggedValue> {
        self.merged.get(key)
    }

    /// 获取字符串值
    pub fn get_str(&self, key: &str) -> Option<&str> {
        self.merged.get(key).and_then(|v| match &v.value {
            ConfigValue::String(s) => {
                let trimmed = s.trim();
                if trimmed.is_empty() { None } else { Some(trimmed) }
            }
            _ => None,
        })
    }

    /// 获取数字值
    pub fn get_number(&self, key: &str) -> Option<f64> {
        self.merged.get(key).and_then(|v| v.value.as_number())
    }

    /// 获取布尔值
    pub fn get_bool(&self, key: &str) -> Option<bool> {
        self.merged.get(key).and_then(|v| v.value.as_bool())
    }

    /// 反序列化为任意 serde 兼容类型（用于 Vec<McpConfig> 等复杂结构）。
    /// 引用关系：被 abacus-server / abacus-cli 启动路径调用读取 mcp.servers / plugins.* 等
    /// 嵌套配置；失败返回 None（不抛错），调用方自行决定 fallback 行为。
    /// 生命周期：纯计算无状态，每次调用独立。
    pub fn get_typed<T: serde::de::DeserializeOwned>(&self, key: &str) -> Option<T> {
        let json = self.merged.get(key)?.value.to_json();
        serde_json::from_value(json).ok()
    }

    /// Phase 3：统一 thinking 配置入口。
    ///
    /// ## 优先级（由高到低）
    /// 1. `core.thinking`（新 key，PRIMARY）—— 字符串：off/adaptive/low/medium/high/max/xhigh/minimal/<整数>
    /// 2. `core.thinking_enabled` + `core.thinking_effort`（旧 key，兼容期）
    /// 3. None（调用方走自己的默认值）
    ///
    /// ## Deprecation warning
    /// 仅当用户实际命中旧 key 路径（即新 key 不存在或为默认 "off"，但旧 key 显式开启）时打。
    /// 守护：`OnceLock`，每进程仅一次，避免日志洪水。
    pub fn get_thinking_intent(&self) -> Option<abacus_types::ThinkingIntent> {
        use abacus_types::ThinkingIntent;
        use std::sync::OnceLock;
        static DEPRECATION_WARNED: OnceLock<()> = OnceLock::new();

        // 辅助：从 ConfigValue 提取「字符串形式」用于解析（支持数字 → "n"）
        let read_thinking_as_str = |key: &str| -> Option<String> {
            self.merged.get(key).and_then(|v| match &v.value {
                ConfigValue::String(s) => Some(s.clone()),
                ConfigValue::Number(n) => Some((*n as i64).to_string()),
                ConfigValue::Bool(b) => Some(b.to_string()),
                _ => None,
            })
        };

        // 1. 优先新 key
        if let Some(s) = read_thinking_as_str("core.thinking") {
            let is_explicit = self.merged.get("core.thinking")
                .map(|v| !matches!(v.source, ConfigSource::Default))
                .unwrap_or(false);

            if is_explicit {
                return ThinkingIntent::from_str_loose(&s);
            }
        }

        // 2. 兼容旧 key（仅当用户显式设置时才生效，并打 warning）
        let old_enabled_explicit = self.merged.get("core.thinking_enabled")
            .map(|v| !matches!(v.source, ConfigSource::Default))
            .unwrap_or(false);
        let old_effort_explicit = self.merged.get("core.thinking_effort")
            .map(|v| !matches!(v.source, ConfigSource::Default))
            .unwrap_or(false);

        if old_enabled_explicit || old_effort_explicit {
            if DEPRECATION_WARNED.set(()).is_ok() {
                tracing::warn!(
                    "[DEPRECATED] `core.thinking_enabled` / `core.thinking_effort` 已弃用，\
                     请改用统一字段 `core.thinking`（值：off/adaptive/low/medium/high/max/xhigh/minimal/<整数>）。\
                     兼容期至 Phase 5。"
                );
            }

            let enabled = self.get_bool("core.thinking_enabled").unwrap_or(false);
            if !enabled {
                return Some(ThinkingIntent::Off);
            }
            let effort_str = self.get_str("core.thinking_effort").unwrap_or("high");
            return ThinkingIntent::from_str_loose(effort_str)
                .or(Some(ThinkingIntent::Effort(abacus_types::EffortLevel::High)));
        }

        // 3. 新 key 为默认值 "off" + 旧 key 也都是默认 → 走默认 off
        if let Some(s) = read_thinking_as_str("core.thinking") {
            return ThinkingIntent::from_str_loose(&s);
        }

        None
    }

    /// Phase 3：thinking display 偏好（仅 Anthropic adaptive 生效）。
    /// 返回 "summarized" / "omitted"，None 走 provider 默认。
    pub fn get_thinking_display(&self) -> Option<String> {
        self.get_str("core.thinking_display").map(|s| s.to_string())
    }

    /// 获取所有配置键
    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.merged.keys()
    }

    /// 获取配置摘要
    pub fn summary(&self) -> String {
        let mut lines = vec!["## Configuration".to_string()];
        let mut keys: Vec<&String> = self.merged.keys().collect();
        keys.sort();
        for key in keys {
            if let Some(tagged) = self.merged.get(key) {
                let source = match &tagged.source {
                    ConfigSource::Default => "default",
                    ConfigSource::File(p) => p.as_str(),
                    ConfigSource::Env(e) => e.as_str(),
                    ConfigSource::Cli(c) => c.as_str(),
                };
                let val_str = match &tagged.value {
                    ConfigValue::String(s) => {
                        if key == "api_key" || key == "server_token" {
                            if s.is_empty() { "(not set)".into() } else { "[REDACTED]".into() }
                        } else {
                            s.clone()
                        }
                    }
                    ConfigValue::Number(n) => n.to_string(),
                    ConfigValue::Bool(b) => b.to_string(),
                    ConfigValue::Null => "null".into(),
                    ConfigValue::List(_) => "[...]".into(),
                    ConfigValue::Map(_) => "{...}".into(),
                };
                lines.push(format!("  {key} = {val_str} ({source})"));
            }
        }
        lines.join("\n")
    }
}

/// 将嵌套的 YAML 展平为点分键
fn flatten_yaml(value: &serde_yaml::Value, prefix: &str) -> Vec<(String, ConfigValue)> {
    match value {
        serde_yaml::Value::String(s) => vec![(prefix.to_string(), ConfigValue::String(s.clone()))],
        serde_yaml::Value::Number(n) => vec![(prefix.to_string(), ConfigValue::Number(n.as_f64().unwrap_or(0.0)))],
        serde_yaml::Value::Bool(b) => vec![(prefix.to_string(), ConfigValue::Bool(*b))],
        serde_yaml::Value::Null => vec![(prefix.to_string(), ConfigValue::Null)],
        serde_yaml::Value::Sequence(seq) => {
            let list: Vec<ConfigValue> = seq.iter().flat_map(|v| {
                let flattened = flatten_yaml(v, "");
                flattened.into_iter().map(|(_, val)| val).collect::<Vec<_>>()
            }).collect();
            vec![(prefix.to_string(), ConfigValue::List(list))]
        }
        serde_yaml::Value::Mapping(map) => {
            let mut result = Vec::new();
            for (k, v) in map {
                if let Some(key_str) = k.as_str() {
                    let new_prefix = if prefix.is_empty() {
                        key_str.to_string()
                    } else {
                        format!("{prefix}.{key_str}")
                    };
                    result.extend(flatten_yaml(v, &new_prefix));
                }
            }
            result
        }
        serde_yaml::Value::Tagged(tagged) => flatten_yaml(&tagged.value, prefix),
    }
}

/// 将嵌套的 JSON 展平为点分键
fn flatten_json(value: &Value, prefix: &str) -> Vec<(String, ConfigValue)> {
    match value {
        Value::String(s) => vec![(prefix.to_string(), ConfigValue::String(s.clone()))],
        Value::Number(n) => vec![(prefix.to_string(), ConfigValue::Number(n.as_f64().unwrap_or(0.0)))],
        Value::Bool(b) => vec![(prefix.to_string(), ConfigValue::Bool(*b))],
        Value::Null => vec![(prefix.to_string(), ConfigValue::Null)],
        Value::Array(arr) => {
            let list: Vec<ConfigValue> = arr.iter().flat_map(|v| {
                let flattened = flatten_json(v, "");
                flattened.into_iter().map(|(_, val)| val).collect::<Vec<_>>()
            }).collect();
            vec![(prefix.to_string(), ConfigValue::List(list))]
        }
        Value::Object(obj) => {
            let mut result = Vec::new();
            for (k, v) in obj {
                let new_prefix = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                result.extend(flatten_json(v, &new_prefix));
            }
            result
        }
    }
}

/// 解析环境变量字符串为 ConfigValue
fn parse_env_value(s: &str) -> ConfigValue {
    // Try bool
    match s {
        "true" | "1" | "yes" => return ConfigValue::Bool(true),
        "false" | "0" | "no" => return ConfigValue::Bool(false),
        _ => {}
    }
    // Try number
    if let Ok(n) = s.parse::<f64>() {
        return ConfigValue::Number(n);
    }
    // Default to string
    ConfigValue::String(s.to_string())
}

/// 创建默认配置
pub fn default_config() -> HashMap<String, ConfigValue> {
    let mut defaults = HashMap::new();
    // V29.13: max_turns 默认 5 → 25
    //   原 5 是 V0 时代值, 工具生态扩展后多文件 refactor / 调研类任务 5 轮极易撞线
    //   兜底文案 "(max turns reached)" 会覆盖 LLM 实际输出, 用户感知差
    //   25 在多数任务下足够收敛, 同时不至于让 runaway loop 烧太多 token
    //   范围参考: 简单问答 5-10, 工具任务 15-25, 复杂调研 40+
    //   引用: validation.rs NumericRange max=200 上限不变; engine_init.rs unwrap_or(20) 兜底不变
    defaults.insert("core.max_turns".into(), ConfigValue::Number(25.0));
    defaults.insert("core.max_tool_calls".into(), ConfigValue::Number(8.0));
    defaults.insert("core.default_model".into(), ConfigValue::String("deepseek-v4-flash".into()));
    defaults.insert("core.temperature".into(), ConfigValue::Number(0.6));
    defaults.insert("core.max_tokens".into(), ConfigValue::Number(8192.0));
    defaults.insert("core.thinking_enabled".into(), ConfigValue::Bool(false));   // [DEPRECATED] Phase 5 移除
    defaults.insert("core.thinking_effort".into(), ConfigValue::String("medium".into())); // [DEPRECATED] Phase 5 移除
    // Phase 3 新 key（PRIMARY）：单字段表达 thinking 意图
    //   off | adaptive | minimal | low | medium | high | max | xhigh | <整数 budget>
    defaults.insert("core.thinking".into(), ConfigValue::String("off".into()));
    //   summarized | omitted（仅 Anthropic adaptive 路径生效）
    defaults.insert("core.thinking_display".into(), ConfigValue::String("summarized".into()));
    defaults.insert("core.context_window".into(), ConfigValue::Number(128000.0));
    defaults.insert("safety.max_input_length".into(), ConfigValue::Number(100000.0));
    defaults.insert("safety.max_session_duration".into(), ConfigValue::Number(3600.0));
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| "/tmp".into());
    defaults.insert("safety.allowed_roots".into(), ConfigValue::List(vec![
        ConfigValue::String(home),
    ]));
    defaults.insert("llm.base_url".into(), ConfigValue::String("https://api.deepseek.com".into()));
    defaults.insert("llm.api_key".into(), ConfigValue::String("".into()));
    defaults.insert("llm.openai_base_url".into(), ConfigValue::String("".into()));
    defaults.insert("llm.openai_api_key".into(), ConfigValue::String("".into()));
    defaults.insert("llm.openai_auth_header".into(), ConfigValue::String("Authorization".into()));
    defaults.insert("llm.openai_auth_prefix".into(), ConfigValue::String("Bearer ".into()));
    defaults.insert("llm.anthropic_base_url".into(), ConfigValue::String("".into()));
    defaults.insert("llm.anthropic_api_key".into(), ConfigValue::String("".into()));
    defaults.insert("log.level".into(), ConfigValue::String("info".into()));

    // Progressive Output Protocol
    defaults.insert("progressive.enabled".into(), ConfigValue::Bool(true));
    defaults.insert("progressive.autonomy_level".into(), ConfigValue::String("medium".into()));
    defaults.insert("progressive.threshold_passthrough".into(), ConfigValue::Number(0.30));
    defaults.insert("progressive.threshold_gated".into(), ConfigValue::Number(0.70));
    defaults.insert("progressive.checklist_timeout_secs".into(), ConfigValue::Number(300.0));
    defaults.insert("progressive.max_checklist_items".into(), ConfigValue::Number(7.0));
    defaults.insert("progressive.forced_gated_types".into(), ConfigValue::List(vec![
        ConfigValue::String("prd".into()),
        ConfigValue::String("sop".into()),
        ConfigValue::String("architecture_design".into()),
        ConfigValue::String("financial_report".into()),
        ConfigValue::String("compliance_doc".into()),
    ]));
    defaults.insert("progressive.team_exempt_in_execution".into(), ConfigValue::Bool(true));
    defaults.insert("progressive.calibrator_alpha".into(), ConfigValue::Number(0.10));
    defaults.insert("progressive.calibrator_drift_limit".into(), ConfigValue::Number(0.10));
    defaults.insert("progressive.calibrator_min_gap".into(), ConfigValue::Number(0.15));

    // SubAgent limits
    defaults.insert("subagent.max_steps".into(), ConfigValue::Number(20.0));
    defaults.insert("subagent.max_tokens".into(), ConfigValue::Number(8192.0));
    defaults.insert("subagent.max_duration_secs".into(), ConfigValue::Number(120.0));

    // Specialist engagement limits
    defaults.insert("specialist.max_speeches".into(), ConfigValue::Number(3.0));
    defaults.insert("specialist.max_think_tokens".into(), ConfigValue::Number(4096.0));
    defaults.insert("specialist.max_tool_calls_per_think".into(), ConfigValue::Number(5.0));
    defaults.insert("specialist.min_confidence".into(), ConfigValue::Number(0.30));
    defaults.insert("specialist.timeout_secs".into(), ConfigValue::Number(120.0));

    // Inertia detection
    defaults.insert("inertia.max_retries".into(), ConfigValue::Number(2.0));
    defaults.insert("inertia.trigger_threshold".into(), ConfigValue::Number(0.60));

    // Sandbox
    defaults.insert("sandbox.max_retries_per_step".into(), ConfigValue::Number(2.0));
    defaults.insert("sandbox.default_timeout_secs".into(), ConfigValue::Number(120.0));
    defaults.insert("sandbox.verify_model".into(), ConfigValue::String("deepseek-v4-flash".into()));

    // MCIP — 工具访问权限配置（全部在 security.yaml `mcip:` 节配置）
    // mcip.exempt_prefixes: 前缀豆免列表（空 = 仅内置豆免生效）
    defaults.insert("mcip.exempt_prefixes".into(), ConfigValue::List(vec![]));
    // mcip.allow_tools: 精确允许名单（空 = 仅靠豆免前缀和策略）
    defaults.insert("mcip.allow_tools".into(), ConfigValue::List(vec![]));
    // mcip.deny_tools: 永久禁止名单（空 = 不禁用任何工具）
    defaults.insert("mcip.deny_tools".into(), ConfigValue::List(vec![]));

    // Server limits
    defaults.insert("server.max_sessions".into(), ConfigValue::Number(1000.0));
    defaults.insert("server.rate_limit_per_sec".into(), ConfigValue::Number(60.0));
    defaults.insert("server.silent_router_enabled".into(), ConfigValue::Bool(true));

    // Pipeline limits
    defaults.insert("pipeline.max_total_tool_calls".into(), ConfigValue::Number(40.0));
    defaults.insert("pipeline.escalation_target_model".into(), ConfigValue::String("deepseek-v4-pro".into()));

    defaults
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let defaults = default_config();
        let manager = ConfigManager::new(defaults);
        // V29.13: max_turns default 5 → 25 (工具生态扩展后 5 轮易撞线)
        assert_eq!(manager.get_number("core.max_turns"), Some(25.0));
        assert_eq!(manager.get_str("core.default_model"), Some("deepseek-v4-flash"));
    }

    #[test]
    fn test_env_loading() {
        let defaults = default_config();
        let mut manager = ConfigManager::new(defaults);
        // Set a test env var
        std::env::set_var("ABACUS_CORE__MAX_TURNS", "10");
        manager.load_env("ABACUS_");
        assert_eq!(manager.get_number("core.max_turns"), Some(10.0));
        std::env::remove_var("ABACUS_CORE__MAX_TURNS");
    }

    #[test]
    fn test_cli_loading() {
        let defaults = default_config();
        let mut manager = ConfigManager::new(defaults);
        manager.load_cli(&["--core.max_turns".into(), "15".into()]);
        assert_eq!(manager.get_number("core.max_turns"), Some(15.0));
    }

    #[test]
    fn test_priority_order() {
        let defaults = default_config();
        let mut manager = ConfigManager::new(defaults);

        // V29.13: Default: 25 (原 5 偏低)
        assert_eq!(manager.get_number("core.max_turns"), Some(25.0));

        // Env overrides default
        std::env::set_var("ABACUS_CORE__MAX_TURNS", "10");
        manager.load_env("ABACUS_");
        assert_eq!(manager.get_number("core.max_turns"), Some(10.0));

        // CLI overrides env
        manager.load_cli(&["--core.max_turns".into(), "20".into()]);
        assert_eq!(manager.get_number("core.max_turns"), Some(20.0));

        std::env::remove_var("ABACUS_CORE__MAX_TURNS");
    }

    #[test]
    fn test_summary() {
        let defaults = default_config();
        let manager = ConfigManager::new(defaults);
        let summary = manager.summary();
        assert!(summary.contains("core.max_turns"));
        assert!(summary.contains("default"));
    }

    // ── Phase 3：get_thinking_intent 测试 ──────────────────────────────

    #[test]
    fn test_get_thinking_intent_default_off() {
        let m = ConfigManager::new(default_config());
        // 全部走默认值 → "off" → ThinkingIntent::Off
        assert_eq!(m.get_thinking_intent(), Some(abacus_types::ThinkingIntent::Off));
    }

    #[test]
    fn test_get_thinking_intent_new_key_priority() {
        let mut m = ConfigManager::new(default_config());
        m.load_cli(&["--core.thinking".into(), "adaptive".into()]);
        assert_eq!(m.get_thinking_intent(), Some(abacus_types::ThinkingIntent::Adaptive));
    }

    #[test]
    fn test_get_thinking_intent_new_key_xhigh() {
        let mut m = ConfigManager::new(default_config());
        m.load_cli(&["--core.thinking".into(), "xhigh".into()]);
        assert_eq!(
            m.get_thinking_intent(),
            Some(abacus_types::ThinkingIntent::Effort(abacus_types::EffortLevel::XHigh))
        );
    }

    #[test]
    fn test_get_thinking_intent_new_key_budget() {
        let mut m = ConfigManager::new(default_config());
        m.load_cli(&["--core.thinking".into(), "8192".into()]);
        assert_eq!(m.get_thinking_intent(), Some(abacus_types::ThinkingIntent::Budget(8192)));
    }

    #[test]
    fn test_get_thinking_intent_legacy_keys_translated() {
        // 用户只设了旧 key（thinking_enabled=true, thinking_effort=high）
        let mut m = ConfigManager::new(default_config());
        m.load_cli(&[
            "--core.thinking_enabled".into(), "true".into(),
            "--core.thinking_effort".into(), "high".into(),
        ]);
        assert_eq!(
            m.get_thinking_intent(),
            Some(abacus_types::ThinkingIntent::Effort(abacus_types::EffortLevel::High))
        );
    }

    #[test]
    fn test_get_thinking_intent_legacy_disabled() {
        let mut m = ConfigManager::new(default_config());
        m.load_cli(&["--core.thinking_enabled".into(), "false".into()]);
        assert_eq!(m.get_thinking_intent(), Some(abacus_types::ThinkingIntent::Off));
    }

    #[test]
    fn test_get_thinking_intent_new_overrides_legacy() {
        // 同时设了新旧 key，新 key 必须优先
        let mut m = ConfigManager::new(default_config());
        m.load_cli(&[
            "--core.thinking".into(), "minimal".into(),
            "--core.thinking_enabled".into(), "true".into(),
            "--core.thinking_effort".into(), "high".into(),
        ]);
        assert_eq!(
            m.get_thinking_intent(),
            Some(abacus_types::ThinkingIntent::Effort(abacus_types::EffortLevel::Minimal)),
            "新 key 应优先"
        );
    }
}
