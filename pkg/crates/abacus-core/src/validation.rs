//! ValidationGate — Schema-driven config validation before engine startup.
//!
//! ## 场景
//! 在 engine_init 加载 env/yaml 后、创建 CoreLoop 前，
//! 运行所有校验规则。Error 级阻断启动，Warning 级继续但通知用户。
//!
//! ## 引用关系
//! - 被 `engine_init::create_engine()` 调用（CLI + TUI 共享）
//! - 被 `AbacusServer::new()` 调用（HTTP 服务器启动时）
//! - 规则可扩展：实现 `ConfigRule` trait 注册自定义校验

use crate::config::{ConfigManager, ConfigValue};

/// 校验严重级别
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// 阻断启动
    Error,
    /// 继续运行但警告
    Warning,
}

/// 单条校验错误
#[derive(Debug, Clone)]
pub struct ValidationError {
    pub key: String,
    pub message: String,
    pub severity: Severity,
    pub suggestion: Option<String>,
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let level = match self.severity {
            Severity::Error => "ERROR",
            Severity::Warning => "WARN",
        };
        write!(f, "[{}] {}: {}", level, self.key, self.message)?;
        if let Some(ref s) = self.suggestion {
            write!(f, " → {}", s)?;
        }
        Ok(())
    }
}

/// 校验规则 trait
pub trait ConfigRule: Send + Sync {
    fn key(&self) -> &str;
    fn validate(&self, value: Option<&ConfigValue>) -> Result<(), ValidationError>;
    fn severity(&self) -> Severity;
}

/// 校验报告
#[derive(Debug, Clone)]
pub struct ValidationReport {
    pub errors: Vec<ValidationError>,
    pub warnings: Vec<ValidationError>,
}

impl ValidationReport {
    pub fn is_ok(&self) -> bool {
        self.errors.is_empty()
    }

    /// 有 Error 级问题时返回 Err（阻断启动），否则返回 warnings
    pub fn into_result(self) -> Result<Vec<ValidationError>, abacus_types::KernelError> {
        if self.errors.is_empty() {
            Ok(self.warnings)
        } else {
            let msg = self.errors.iter()
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
                .join("\n");
            Err(abacus_types::KernelError::Config(msg))
        }
    }
}

/// 校验入口
pub struct ValidationGate {
    rules: Vec<Box<dyn ConfigRule>>,
}

impl Default for ValidationGate {
    fn default() -> Self {
        Self::new()
    }
}

impl ValidationGate {
    pub fn new() -> Self {
        Self { rules: Vec::new() }
    }

    /// 加载 Abacus 默认规则集
    pub fn with_defaults(mut self) -> Self {
        self.rules.push(Box::new(NonEmptyString { key: "llm.api_key", severity: Severity::Warning, label: "API Key" }));
        self.rules.push(Box::new(NonEmptyString { key: "llm.anthropic_api_key", severity: Severity::Warning, label: "Anthropic Key" }));
        self.rules.push(Box::new(NonEmptyString { key: "llm.openai_api_key", severity: Severity::Warning, label: "OpenAI Key" }));
        self.rules.push(Box::new(NumericRange { key: "core.max_turns", min: 1.0, max: 200.0 }));
        self.rules.push(Box::new(NumericRange { key: "core.temperature", min: 0.0, max: 2.0 }));
        self.rules.push(Box::new(NumericRange { key: "core.max_tokens", min: 1.0, max: 1_000_000.0 }));
        self.rules.push(Box::new(ConfigDirCheck));
        self
    }

    /// 运行所有规则
    pub fn validate(&self, config: &ConfigManager) -> ValidationReport {
        let mut errors = Vec::new();
        let mut warnings = Vec::new();

        for rule in &self.rules {
            let value = config.get(rule.key()).map(|tv| &tv.value);
            if let Err(e) = rule.validate(value) {
                match e.severity {
                    Severity::Error => errors.push(e),
                    Severity::Warning => warnings.push(e),
                }
            }
        }

        ValidationReport { errors, warnings }
    }
}

// ─── 内置规则实现 ────────────────────────────────────────────────────

/// 非空字符串校验（API keys）
struct NonEmptyString {
    key: &'static str,
    severity: Severity,
    label: &'static str,
}

impl ConfigRule for NonEmptyString {
    fn key(&self) -> &str { self.key }
    fn severity(&self) -> Severity { self.severity }
    fn validate(&self, value: Option<&ConfigValue>) -> Result<(), ValidationError> {
        match value {
            Some(ConfigValue::String(s)) if !s.trim().is_empty() => {
                if s.trim().len() < 8 {
                    return Err(ValidationError {
                        key: self.key.to_string(),
                        message: format!("{} 过短 ({} 字符)，可能不完整", self.label, s.trim().len()),
                        severity: Severity::Warning,
                        suggestion: Some(format!("检查 {} 配置是否完整", self.key)),
                    });
                }
                Ok(())
            }
            Some(ConfigValue::String(_)) => {
                // 空字符串 — 视为未配置，不报错（允许只配置部分 provider）
                Ok(())
            }
            None => Ok(()), // 未配置 = 使用默认，不报错
            _ => Ok(()),
        }
    }
}

/// 数值范围校验
struct NumericRange {
    key: &'static str,
    min: f64,
    max: f64,
}

impl ConfigRule for NumericRange {
    fn key(&self) -> &str { self.key }
    fn severity(&self) -> Severity { Severity::Error }
    fn validate(&self, value: Option<&ConfigValue>) -> Result<(), ValidationError> {
        match value {
            Some(ConfigValue::Number(n)) => {
                if *n < self.min || *n > self.max {
                    return Err(ValidationError {
                        key: self.key.to_string(),
                        message: format!("值 {} 超出有效范围 [{}, {}]", n, self.min, self.max),
                        severity: Severity::Error,
                        suggestion: None,
                    });
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

/// 配置目录检查（存在性 + 权限）
struct ConfigDirCheck;

impl ConfigRule for ConfigDirCheck {
    fn key(&self) -> &str { "system.config_dir" }
    fn severity(&self) -> Severity { Severity::Warning }
    fn validate(&self, _value: Option<&ConfigValue>) -> Result<(), ValidationError> {
        let home = std::env::var("HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from("/tmp"));
        let config_dir = home.join(".abacus");
        if !config_dir.exists() {
            return Err(ValidationError {
                key: "system.config_dir".into(),
                message: "~/.abacus/ 目录不存在".into(),
                severity: Severity::Warning,
                suggestion: Some("运行 mkdir -p ~/.abacus && chmod 700 ~/.abacus".into()),
            });
        }
        // Unix: 检查权限
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = std::fs::metadata(&config_dir) {
                let mode = meta.permissions().mode();
                if mode & 0o077 != 0 {
                    return Err(ValidationError {
                        key: "system.config_dir".into(),
                        message: format!("~/.abacus/ 权限过宽 (mode {:o})，可能泄露 API Key", mode & 0o777),
                        severity: Severity::Warning,
                        suggestion: Some("运行 chmod 700 ~/.abacus".into()),
                    });
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_numeric_range_valid() {
        let rule = NumericRange { key: "core.temperature", min: 0.0, max: 2.0 };
        assert!(rule.validate(Some(&ConfigValue::Number(0.6))).is_ok());
    }

    #[test]
    fn test_numeric_range_invalid() {
        let rule = NumericRange { key: "core.temperature", min: 0.0, max: 2.0 };
        assert!(rule.validate(Some(&ConfigValue::Number(3.0))).is_err());
    }

    #[test]
    fn test_empty_key_not_error() {
        let rule = NonEmptyString { key: "llm.api_key", severity: Severity::Warning, label: "API Key" };
        // 空字符串不报错（未配置视为可选）
        assert!(rule.validate(Some(&ConfigValue::String("".into()))).is_ok());
    }

    #[test]
    fn test_short_key_warns() {
        let rule = NonEmptyString { key: "llm.api_key", severity: Severity::Warning, label: "API Key" };
        let result = rule.validate(Some(&ConfigValue::String("abc".into())));
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().severity, Severity::Warning);
    }

    #[test]
    fn test_config_dir_missing() {
        // 不运行实际文件检查（CI 环境可能不同）
        // 仅验证规则结构正确
        let rule = ConfigDirCheck;
        assert_eq!(rule.key(), "system.config_dir");
        assert_eq!(rule.severity(), Severity::Warning);
    }
}
