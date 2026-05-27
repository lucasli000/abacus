//! 任务分析 — 根据输入自动推荐交互模式

use crate::tui::state::AbacusMode;

/// 根据用户输入文本推荐最适合的交互模式。
/// 返回 None 表示保持当前模式（不建议切换）。
///
/// V33.1: 关闭关键词自动匹配——关键词命中率过低、
/// 误报率过高（任何含"实现"/"fix"/"帮我写"的问句都会误切 Team）。
/// 模式切换现在只走两条路径：
///   1. 用户显式命令：/clarify /plan /team /meeting + Ctrl+1/2/3
///   2. LLM 自主决策：引擎返回 AbacusMode 建议时由 switch_mode 执行
/// 本函数保留为 API 兼容占位，未来可接入 LLM 语义 Router。
pub fn suggest_mode(_input: &str) -> Option<AbacusMode> {
    None
}

// is_greeting 随 V33.1 关键词机制一起移除——模式切换不再需要文本分析

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_suggest_mode_always_none() {
        // V33.1: suggest_mode 已关闭关键词匹配，始终返回 None。
        // 模式切换仅走显式命令或 LLM 自主决策。
        assert_eq!(suggest_mode("帮我做 code review 检查这段代码的安全性"), None);
        assert_eq!(suggest_mode("帮我用 full stack 实现一个博客系统"), None);
        assert_eq!(suggest_mode("Rust 里 String 和 &str 有什么区别"), None);
        assert_eq!(suggest_mode("你好"), None);
    }
}
