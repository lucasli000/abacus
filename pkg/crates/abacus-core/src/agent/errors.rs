//! Agent 错误处理 — 统一错误类型 + 回退策略
//!
//! ## 设计
//! - AgentError: 统一错误类型，包含链式错误信息
//! - 回退策略: Agent 不可用时的降级方案
//! - 速率限制: 每 Agent 并发控制
//!
//! ## 引用关系
//! - 消费: ExternalAgentToolExecutor, AgentSkillExecutor, MeetingManager
//! - 下游: ToolOutput.failure_kind, TUI toast

use std::time::Duration;

/// Agent 操作错误
#[derive(Debug)]
pub enum AgentError {
    /// 连接失败
    ConnectionFailed { agent_id: String, reason: String },
    /// 工具执行失败
    ExecutionFailed { agent_id: String, tool: String, reason: String },
    /// 技能执行失败
    SkillFailed { agent_id: String, skill: String, reason: String },
    /// 超时
    Timeout { agent_id: String, timeout: Duration },
    /// 速率限制
    RateLimited { agent_id: String, retry_after: Duration },
    /// 信任级别不足
    InsufficientTrust { agent_id: String, required: String, actual: String },
    /// Agent 不可达
    Unreachable { agent_id: String },
    /// 通用错误
    Other(String),
}

impl std::fmt::Display for AgentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ConnectionFailed { agent_id, reason } =>
                write!(f, "Agent '{}' connection failed: {}", agent_id, reason),
            Self::ExecutionFailed { agent_id, tool, reason } =>
                write!(f, "Agent '{}' tool '{}' failed: {}", agent_id, tool, reason),
            Self::SkillFailed { agent_id, skill, reason } =>
                write!(f, "Agent '{}' skill '{}' failed: {}", agent_id, skill, reason),
            Self::Timeout { agent_id, timeout } =>
                write!(f, "Agent '{}' timed out after {}s", agent_id, timeout.as_secs()),
            Self::RateLimited { agent_id, retry_after } =>
                write!(f, "Agent '{}' rate limited, retry after {}s", agent_id, retry_after.as_secs()),
            Self::InsufficientTrust { agent_id, required, actual } =>
                write!(f, "Agent '{}' requires '{}' trust, has '{}'", agent_id, required, actual),
            Self::Unreachable { agent_id } =>
                write!(f, "Agent '{}' is unreachable", agent_id),
            Self::Other(msg) => write!(f, "Agent error: {}", msg),
        }
    }
}

impl std::error::Error for AgentError {}

/// 回退策略
#[derive(Debug, Clone)]
pub enum FallbackStrategy {
    /// 跳过该工具调用
    Skip,
    /// 用本地工具替代
    UseLocal(String),
    /// 返回错误给 LLM
    ReturnError,
    /// 重试 N 次后失败
    RetryThenFail { max_retries: u32, backoff_ms: u64 },
}

impl Default for FallbackStrategy {
    fn default() -> Self {
        Self::ReturnError
    }
}

/// Agent 速率限制器
pub struct AgentRateLimiter {
    /// 每 Agent 的最大并发数
    max_concurrent: usize,
    /// 当前活跃调用计数
    active: std::sync::atomic::AtomicUsize,
}

impl AgentRateLimiter {
    pub fn new(max_concurrent: usize) -> Self {
        Self {
            max_concurrent: max_concurrent.max(1),
            active: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// 尝试获取许可
    pub fn try_acquire(&self) -> bool {
        let current = self.active.load(std::sync::atomic::Ordering::Relaxed);
        if current < self.max_concurrent {
            self.active.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            true
        } else {
            false
        }
    }

    /// 释放许可
    pub fn release(&self) {
        self.active.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// 当前活跃数
    pub fn active_count(&self) -> usize {
        self.active.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// 根据 Agent 错误类型决定回退策略
pub fn decide_fallback(error: &AgentError) -> FallbackStrategy {
    match error {
        AgentError::ConnectionFailed { .. } => FallbackStrategy::Skip,
        AgentError::Unreachable { .. } => FallbackStrategy::Skip,
        AgentError::Timeout { .. } => FallbackStrategy::RetryThenFail {
            max_retries: 1,
            backoff_ms: 2000,
        },
        AgentError::RateLimited { retry_after, .. } => FallbackStrategy::RetryThenFail {
            max_retries: 3,
            backoff_ms: retry_after.as_millis() as u64,
        },
        AgentError::InsufficientTrust { .. } => FallbackStrategy::ReturnError,
        AgentError::ExecutionFailed { .. } => FallbackStrategy::ReturnError,
        AgentError::SkillFailed { .. } => FallbackStrategy::ReturnError,
        AgentError::Other(_) => FallbackStrategy::ReturnError,
    }
}
