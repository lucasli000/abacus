//! L0 types — error layer

use thiserror::Error;

#[derive(Error, Debug)]
pub enum KernelError {
    #[error("provider error: {0}")]
    Provider(String),

    #[error("API error: {status} {body}")]
    ApiError { status: u16, body: String },

    #[error("rate limited: retry after {retry_after}s")]
    RateLimited { retry_after: u64 },

    #[error("unauthorized: {0}")]
    Unauthorized(String),

    #[error("model not found: {0}")]
    ModelNotFound(String),

    #[error("context overflow: {current} > {limit}")]
    ContextOverflow { current: usize, limit: usize },

    #[error("needs human review: {0}")]
    NeedsHumanReview(String),

    #[error("output aborted: {0}")]
    OutputAborted(String),

    #[error("needs more context: {0:?}")]
    NeedsMoreContext(Vec<String>),

    #[error("agent not found")]
    AgentNotFound,

    #[error("checkpoint not found")]
    CheckpointNotFound,

    #[error("review timeout")]
    ReviewTimeout,

    #[error("validation error: {0}")]
    Validation(String),

    #[error("config error: {0}")]
    Config(String),

    #[error("io error ({context}): {detail}")]
    Io { context: String, detail: String },

    #[error("serialization error ({context}): {detail}")]
    Serialization { context: String, detail: String },

    #[error("subsystem degraded ({subsystem}): {reason}")]
    Degraded { subsystem: String, reason: String },

    #[error("{0}")]
    Other(String),
}

impl KernelError {
    /// Returns a user-safe message that won't leak internals.
    /// Use this for API responses and user-facing UI; use Display for logging.
    pub fn user_message(&self) -> String {
        match self {
            Self::Provider(_) => "服务提供商暂时不可用，请稍后重试".into(),
            Self::ApiError { status, .. } => match status {
                429 => "请求过于频繁，请稍后重试".into(),
                401 | 403 => "认证失败，请检查 API Key 配置".into(),
                500..=599 => "服务端错误，请稍后重试".into(),
                _ => format!("请求失败 ({})", status),
            },
            Self::RateLimited { retry_after } => format!("请求限流，请 {}s 后重试", retry_after),
            Self::Unauthorized(_) => "认证失败，请检查凭据配置".into(),
            Self::ModelNotFound(m) => format!("模型 '{}' 不可用", m),
            Self::ContextOverflow { .. } => "对话上下文已满，请压缩或新建会话".into(),
            Self::NeedsHumanReview(_) => "该操作需要人工确认".into(),
            Self::OutputAborted(_) => "输出已中止".into(),
            Self::NeedsMoreContext(_) => "需要更多上下文信息".into(),
            Self::AgentNotFound => "指定 Agent 不存在".into(),
            Self::CheckpointNotFound => "会话检查点未找到".into(),
            Self::ReviewTimeout => "审核超时".into(),
            Self::Validation(msg) => format!("配置验证失败: {}", msg),
            Self::Config(msg) => format!("配置错误: {}", msg),
            Self::Io { context, .. } => format!("IO 操作失败: {}", context),
            Self::Serialization { context, .. } => format!("数据解析失败: {}", context),
            Self::Degraded { subsystem, .. } => format!("子系统 '{}' 降级中", subsystem),
            Self::Other(_) => "内部错误，请重试".into(),
        }
    }
}

/// 自动转换：serde_json::Error → KernelError::Serialization
///
/// 启用 `?` 操作符在 L0+ crate 中自动把 JSON 解析失败转成内核错误。
/// 上下文用调用点文件名（通过 `.map_err(|e| e.context("..."))?` 模式补充）。
impl From<serde_json::Error> for KernelError {
    fn from(e: serde_json::Error) -> Self {
        Self::Serialization {
            context: "json".into(),
            detail: e.to_string(),
        }
    }
}

/// 自动转换：String → KernelError::Other
///
/// 简化调用点：函数返回 Result<_, String> 时可通过 `?` 自动升级为内核错误。
/// 鼓励统一错误类型，避免 String 错误在 crate 边界泄漏。
impl From<String> for KernelError {
    fn from(s: String) -> Self {
        Self::Other(s)
    }
}

impl From<&str> for KernelError {
    fn from(s: &str) -> Self {
        Self::Other(s.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_json_error_converts_to_serialization() {
        let bad_json = "not json{";
        let parse_err: serde_json::Error =
            serde_json::from_str::<serde_json::Value>(bad_json).unwrap_err();
        let kernel_err: KernelError = parse_err.into();
        match kernel_err {
            KernelError::Serialization { context, .. } => {
                assert_eq!(context, "json");
            }
            _ => panic!("expected Serialization variant"),
        }
    }

    #[test]
    fn question_mark_operator_works_with_serde_json() {
        fn parse_user() -> Result<serde_json::Value> {
            // 使用 ? 操作符自动转换（无需手动 map_err）
            let v: serde_json::Value = serde_json::from_str("{ invalid")?;
            Ok(v)
        }
        let err = parse_user().unwrap_err();
        assert!(matches!(err, KernelError::Serialization { .. }));
    }
}

pub type Result<T> = std::result::Result<T, KernelError>;