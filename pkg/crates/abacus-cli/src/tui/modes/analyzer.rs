//! 模式建议分析器 — 2026-05-27 启发式实现（零 LLM 开销）
//!
//! ## 设计意图
//! Clarify 模式中，当 response 显示多领域复杂度时，非侵入式建议切换到 Meeting。
//! 不自动切换，仅 toast 提示用户。
//!
//! ## 引用关系
//! - 消费方: run.rs response 处理末尾，Clarify 模式下调用
//! - 控制: state.meeting_suggested_this_session 防止重复建议
//!
//! ## 触发阈值
//! - response 字符数 > 2000
//! - tool_count >= 3 且 domain_hits >= 2，或 domain_hits >= 3
//! - 本 session 尚未建议过

use crate::tui::state::AbacusMode;

/// 原始 API 兼容接口（V33.1 关闭，保留占位）
pub fn suggest_mode(_input: &str) -> Option<AbacusMode> {
    None
}

/// 2026-05-27: 基于 response 启发式的模式建议（零 LLM 开销）
///
/// 分析 Clarify 模式下的 engine response，检测多领域复杂度信号。
/// 返回 Some(Meeting) 表示建议切换，None 表示不建议。
///
/// ## 设计原则
/// - **零成本**: 纯文本扫描，不发 LLM 请求
/// - **高阈值**: 宁可不建议也不误触发（>2000字 + 多领域命中）
/// - **单次**: `already_suggested=true` 时直接返回 None
pub fn suggest_mode_from_response(
    response_text: &str,
    tool_count: usize,
    already_suggested: bool,
) -> Option<AbacusMode> {
    if already_suggested {
        return None;
    }

    let char_count = response_text.chars().count();
    if char_count < 2000 {
        return None;
    }

    // 领域关键词（双语覆盖，每对算一个 domain）
    let domain_keywords: &[(&str, &str)] = &[
        ("security", "安全"),
        ("performance", "性能"),
        ("architecture", "架构"),
        ("database", "数据库"),
        ("frontend", "前端"),
        ("backend", "后端"),
        ("testing", "测试"),
        ("deployment", "部署"),
        ("network", "网络"),
        ("concurrency", "并发"),
    ];

    let lower = response_text.to_lowercase();
    let domains_hit = domain_keywords
        .iter()
        .filter(|(en, zh)| lower.contains(en) || lower.contains(zh))
        .count();

    // 触发条件：高工具使用 + 多领域，或纯多领域
    if tool_count >= 3 && domains_hit >= 2 {
        return Some(AbacusMode::Meeting);
    }
    if domains_hit >= 3 {
        return Some(AbacusMode::Meeting);
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_suggest_mode_always_none() {
        // suggest_mode 保持 API 兼容，始终返回 None
        assert_eq!(suggest_mode("帮我做 code review"), None);
    }

    #[test]
    fn test_short_response_no_suggestion() {
        // 短回复不触发
        let short = "这是一段很短的回答。";
        assert_eq!(suggest_mode_from_response(short, 5, false), None);
    }

    #[test]
    fn test_long_multi_domain_suggests_meeting() {
        // 长回复 + 多领域 → 建议 Meeting
        let mut long = String::new();
        for _ in 0..300 {
            long.push_str("这段代码涉及前端架构和数据库性能优化的问题需要考虑。");
        }
        // 字符数 > 2000, 包含 前端/架构/数据库/性能 (4 domains)
        assert_eq!(
            suggest_mode_from_response(&long, 2, false),
            Some(AbacusMode::Meeting)
        );
    }

    #[test]
    fn test_high_tools_plus_two_domains() {
        // 3+ tools + 2 domains → 建议
        let text = "a".repeat(2100) + " security concerns and performance bottleneck";
        assert_eq!(
            suggest_mode_from_response(&text, 4, false),
            Some(AbacusMode::Meeting)
        );
    }

    #[test]
    fn test_already_suggested_blocks() {
        // 已建议过 → 不再建议
        let text = "a".repeat(2100) + " security performance architecture 安全 性能 架构";
        assert_eq!(suggest_mode_from_response(&text, 5, true), None);
    }

    #[test]
    fn test_long_but_single_domain_no_suggestion() {
        // 长但只有一个领域 → 不触发
        let text = "security ".repeat(500);
        assert_eq!(suggest_mode_from_response(&text, 1, false), None);
    }
}
