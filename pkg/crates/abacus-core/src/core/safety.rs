//! 安全不变量守卫（Turn 级防呆层）
//!
//! ## 设计原则
//! - Session 级不设限：长对话完全自由
//! - Turn 级防呆：防止单轮死循环，到达时引导 LLM 输出结论
//! - 只保留两个检查：输入长度 + 单轮工具调用总量
//!
//! ## 引用关系
//! - 被 `CoreLoop` pipeline 在单轮处理中调用
//! - 被 `SessionManager` 在输入验证时调用
//!
//! ## 已移除的 session 级限制
//! - max_recursion_depth（跨轮无意义）
//! - max_session_duration（不限制长对话）

use abacus_types::UserProfile;

/// 安全不变量守卫（纯 Turn 级）
///
/// 只保护单轮内的资源消耗，不对 session 做任何累积限制。
pub struct SafetyGuard {
    /// 用户单条输入最大字符数（超出截断，不拒绝）
    pub max_input_length: usize,
    /// 单轮工具调用次数上限（一次用户消息 → LLM 回复的完整过程）
    pub max_total_tool_calls: u32,
    /// 敏感操作列表（从 UserProfile 加载，可覆盖默认）
    pub sensitive_operations: Vec<String>,
}

impl Default for SafetyGuard {
    fn default() -> Self { Self::new() }
}

impl SafetyGuard {
    /// 创建默认安全守卫
    pub fn new() -> Self {
        Self {
            max_input_length: 100_000,
            max_total_tool_calls: 500,
            sensitive_operations: vec![
                "filengine_fs_write".into(),
                "filengine_fs_move".into(),
                "filengine_fs_mkdir".into(),
                "filengine_bash_exec".into(),
                "web_fetch".into(),
            ],
        }
    }

    /// 从 UserProfile 创建安全守卫（白名单覆盖）
    pub fn from_profile(profile: &UserProfile) -> Self {
        let mut guard = Self::new();
        // 从 UserProfile 覆盖敏感操作白名单
        if !profile.safe_operations.is_empty() {
            guard.sensitive_operations.retain(|op| !profile.safe_operations.contains(op));
        }
        guard.max_total_tool_calls = profile.max_tool_calls_per_turn;
        guard
    }

    /// 判断是否需要用户确认
    pub fn requires_confirmation(&self, tool_id: &str, profile: Option<&UserProfile>) -> bool {
        match profile {
            Some(p) => p.requires_confirmation(tool_id),
            None => self.is_sensitive_operation(tool_id),
        }
    }

    /// 检查输入长度
    pub fn check_input_length(&self, input: &str) -> Result<(), SafetyViolation> {
        if input.len() > self.max_input_length {
            Err(SafetyViolation::InputTooLong {
                actual: input.len(),
                limit: self.max_input_length,
            })
        } else {
            Ok(())
        }
    }

    /// 检查单轮累计工具调用次数是否超限
    pub fn check_tool_call_count(&self, current_count: u32) -> Result<(), SafetyViolation> {
        if current_count > self.max_total_tool_calls {
            Err(SafetyViolation::ToolCallLimitExceeded {
                actual: current_count,
                limit: self.max_total_tool_calls,
            })
        } else {
            Ok(())
        }
    }

    /// 检查是否为敏感操作
    ///
    /// 使用精确相等匹配而非 `contains()`，防止 `filengine_fs_write` 误匹配
    /// `filengine_fs_write_batch` 等名称前缀相似的未来工具。
    pub fn is_sensitive_operation(&self, tool_id: &str) -> bool {
        self.sensitive_operations.iter().any(|s| tool_id == s)
    }

    /// 返回当前安全限制状态（供 TUI/API 展示）
    pub fn status(&self) -> SafetyStatus {
        SafetyStatus {
            max_input_length: self.max_input_length,
            max_total_tool_calls: self.max_total_tool_calls,
        }
    }
}

/// 安全限制状态快照
#[derive(Debug, Clone)]
pub struct SafetyStatus {
    pub max_input_length: usize,
    pub max_total_tool_calls: u32,
}

/// 安全违规
#[derive(Debug, Clone)]
pub enum SafetyViolation {
    /// 输入过长
    InputTooLong { actual: usize, limit: usize },
    /// 单轮工具调用超限
    ToolCallLimitExceeded { actual: u32, limit: u32 },
}

impl std::fmt::Display for SafetyViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SafetyViolation::InputTooLong { actual, limit } => {
                write!(f, "Input length {actual} exceeds limit {limit}")
            }
            SafetyViolation::ToolCallLimitExceeded { actual, limit } => {
                write!(f, "Turn tool call count {actual} exceeds limit {limit}")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_input_length_check() {
        let guard = SafetyGuard::new();
        assert!(guard.check_input_length("hello").is_ok());
        assert!(guard.check_input_length(&"x".repeat(100_001)).is_err());
    }

    #[test]
    fn test_tool_call_count_check() {
        let guard = SafetyGuard::new();
        assert!(guard.check_tool_call_count(499).is_ok());
        assert!(guard.check_tool_call_count(500).is_ok());
        assert!(guard.check_tool_call_count(501).is_err());
    }

    #[test]
    fn test_sensitive_operation() {
        let guard = SafetyGuard::new();
        assert!(guard.is_sensitive_operation("filengine_fs_write"));
        assert!(guard.is_sensitive_operation("filengine_bash_exec"));
        assert!(!guard.is_sensitive_operation("filengine_fs_read"));
    }
}
