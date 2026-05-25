//! 安全不变量守卫
//!
//! ## 依赖
//! - 无外部依赖，纯 Rust std
//!
//! ## 引用关系
//! - 被 `CoreLoop` 在每轮处理前后调用
//! - 被 `SessionManager` 在 session 创建时调用
//!
//! ## 安全检查项
//! - 输入长度限制
//! - 递归深度限制
//! - 工具调用频率限制
//! - 内存使用监控
//! - 敏感操作确认

use std::time::{Duration, Instant};

/// 安全不变量守卫
///
/// 在关键操作前后执行安全检查，确保系统不违反安全约束。
pub struct SafetyGuard {
    /// 最大输入长度 (字符数)
    pub max_input_length: usize,
    /// 最大工具调用次数（单轮内累计——一次用户消息到 LLM 回复的完整过程）
    /// Session 级不设限，长对话不受累积约束
    pub max_total_tool_calls: u32,
    /// 最大递归深度
    pub max_recursion_depth: u32,
    /// 最大 session 持续时间
    pub max_session_duration: Duration,
    /// 敏感操作列表 (需要用户确认)
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
            max_recursion_depth: 20,
            max_session_duration: Duration::from_secs(28800), // 8 小时
            // ToolId 命名空间：dispatch 链路传入的是 ToolId.0（带 filengine. 前缀），
            // is_sensitive_operation 当前用 contains 子串匹配做兼容兜底，但前缀化的
            // 名单消除子串匹配脆弱性（避免未来 mcp/server-fs.write 等命名误判敏感）。
            sensitive_operations: vec![
                "filengine_fs_write".into(),
                "filengine_fs_move".into(),
                "filengine_fs_mkdir".into(),
                "filengine_bash_exec".into(),
                "filengine_web_fetch".into(),
            ],
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

    /// 检查累计工具调用次数是否超限
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

    /// 检查递归深度
    pub fn check_recursion_depth(&self, current_depth: u32) -> Result<(), SafetyViolation> {
        if current_depth >= self.max_recursion_depth {
            Err(SafetyViolation::RecursionDepthExceeded {
                actual: current_depth,
                limit: self.max_recursion_depth,
            })
        } else {
            Ok(())
        }
    }

    /// 检查 session 持续时间
    pub fn check_session_duration(&self, start_time: Instant) -> Result<(), SafetyViolation> {
        let elapsed = start_time.elapsed();
        if elapsed > self.max_session_duration {
            Err(SafetyViolation::SessionTimeout {
                actual: elapsed,
                limit: self.max_session_duration,
            })
        } else {
            Ok(())
        }
    }

    /// 检查是否为敏感操作
    pub fn is_sensitive_operation(&self, tool_id: &str) -> bool {
        self.sensitive_operations.iter().any(|s| tool_id.contains(s))
    }

    /// 执行全部安全检查
    pub fn check_all(
        &self,
        input: &str,
        tool_call_count: u32,
        recursion_depth: u32,
        session_start: Instant,
    ) -> Result<(), Vec<SafetyViolation>> {
        let mut violations = Vec::new();

        if let Err(v) = self.check_input_length(input) {
            violations.push(v);
        }
        if let Err(v) = self.check_tool_call_count(tool_call_count) {
            violations.push(v);
        }
        if let Err(v) = self.check_recursion_depth(recursion_depth) {
            violations.push(v);
        }
        if let Err(v) = self.check_session_duration(session_start) {
            violations.push(v);
        }

        if violations.is_empty() {
            Ok(())
        } else {
            Err(violations)
        }
    }
}

/// 安全限制状态快照（供 CLI/TUI 查询展示）
#[derive(Debug, Clone)]
pub struct SafetyStatus {
    pub max_input_length: usize,
    pub max_total_tool_calls: u32,
    pub max_recursion_depth: u32,
    pub max_session_duration_secs: u64,
}

impl SafetyGuard {
    /// 返回当前安全限制状态
    pub fn status(&self) -> SafetyStatus {
        SafetyStatus {
            max_input_length: self.max_input_length,
            max_total_tool_calls: self.max_total_tool_calls,
            max_recursion_depth: self.max_recursion_depth,
            max_session_duration_secs: self.max_session_duration.as_secs(),
        }
    }
}

/// 安全违规
#[derive(Debug, Clone)]
pub enum SafetyViolation {
    /// 输入过长
    InputTooLong { actual: usize, limit: usize },
    /// 工具调用超限
    ToolCallLimitExceeded { actual: u32, limit: u32 },
    /// 递归深度超限
    RecursionDepthExceeded { actual: u32, limit: u32 },
    /// Session 超时
    SessionTimeout { actual: Duration, limit: Duration },
}

impl std::fmt::Display for SafetyViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SafetyViolation::InputTooLong { actual, limit } => {
                write!(f, "Input length {actual} exceeds limit {limit}")
            }
            SafetyViolation::ToolCallLimitExceeded { actual, limit } => {
                write!(f, "Tool call count {actual} exceeds limit {limit}")
            }
            SafetyViolation::RecursionDepthExceeded { actual, limit } => {
                write!(f, "Recursion depth {actual} exceeds limit {limit}")
            }
            SafetyViolation::SessionTimeout { actual, limit } => {
                write!(f, "Session duration {actual:?} exceeds limit {limit:?}")
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
    fn test_recursion_depth_check() {
        let guard = SafetyGuard::new();
        assert!(guard.check_recursion_depth(19).is_ok());
        assert!(guard.check_recursion_depth(20).is_err());
    }

    #[test]
    fn test_sensitive_operation() {
        let guard = SafetyGuard::new();
        assert!(guard.is_sensitive_operation("filengine_fs_write"));
        assert!(guard.is_sensitive_operation("filengine_bash_exec"));
        assert!(!guard.is_sensitive_operation("filengine_fs_read"));
    }

    #[test]
    fn test_check_all_pass() {
        let guard = SafetyGuard::new();
        let start = Instant::now();
        assert!(guard.check_all("hello", 5, 3, start).is_ok());
    }

    #[test]
    fn test_check_all_fail() {
        let guard = SafetyGuard::new();
        let start = Instant::now();
        let result = guard.check_all(&"x".repeat(100_001), 501, 21, start);
        assert!(result.is_err());
        let violations = result.unwrap_err();
        assert_eq!(violations.len(), 3); // input + tool + recursion
    }
}
