//! 任务分析 — 根据输入自动推荐交互模式

use crate::tui::state::AbacusMode;

/// 根据用户输入文本推荐最适合的交互模式。
/// 返回 None 表示保持当前模式（不建议切换）。
///
/// V38: 增强关键词覆盖 + 补充 Plan 模式触发条件
pub fn suggest_mode(input: &str) -> Option<AbacusMode> {
    let trimmed = input.trim();
    let char_count = trimmed.chars().count();

    // 短输入（<15个字符）或纯问候 → 不建议切换
    if char_count < 15 || is_greeting(trimmed) {
        return None;
    }

    let lower = trimmed.to_lowercase();

    // Meeting 模式：需多视角评审 / 对比分析 / 讨论
    let meeting_keywords = [
        "code review", "review", "设计评审", "方案选择", "架构评审",
        "pros and cons", "利弊分析", "对比分析", "多方评估",
        "讨论", "评估", "权衡", "trade-off", "tradeoff",
        "会诊", "brainstorm", "头脑风暴", "方案对比",
    ];
    if meeting_keywords.iter().any(|k| lower.contains(k)) {
        return Some(AbacusMode::Meeting);
    }

    // Plan 模式：规划/拆解/分步骤任务
    let plan_keywords = [
        "规划", "计划", "拆解", "分步", "步骤", "plan",
        "roadmap", "路线图", "设计方案", "implementation plan",
        "怎么实现", "如何实现", "分阶段", "milestone",
        "任务拆分", "todo", "task list", "清单",
    ];
    if plan_keywords.iter().any(|k| lower.contains(k)) {
        return Some(AbacusMode::Plan);
    }

    // Team 模式：明确多步骤/多角色/执行类任务
    let team_keywords = [
        "full stack", "全栈", "多模块", "pipeline", "workflow",
        "端到端", "从零开始构建", "完整系统", "微服务",
        "实现", "开发", "构建", "重构", "refactor",
        "帮我写", "帮我做", "帮我改", "fix", "implement",
        "build", "create", "自动化", "部署", "deploy",
    ];
    if team_keywords.iter().any(|k| lower.contains(k)) {
        return Some(AbacusMode::Team);
    }

    // 超长输入（>200字符）含代码/技术内容 → Team
    if char_count > 200 && (trimmed.contains("```") || trimmed.contains("fn ") || trimmed.contains("def ")) {
        return Some(AbacusMode::Team);
    }

    None
}

fn is_greeting(s: &str) -> bool {
    let lower = s.trim().to_lowercase();
    matches!(
        lower.as_str(),
        "hi" | "hello" | "hey" | "你好" | "嗨" | "您好" | "早上好" | "下午好" | "晚上好"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_greeting_no_switch() {
        assert_eq!(suggest_mode("你好"), None);
        assert_eq!(suggest_mode("hi"), None);
        assert_eq!(suggest_mode("hello"), None);
    }

    #[test]
    fn test_short_input_no_switch() {
        assert_eq!(suggest_mode("帮我写个函数"), None);
        assert_eq!(suggest_mode("分析一下这个问题"), None);
    }

    #[test]
    fn test_code_review_triggers_meeting() {
        assert_eq!(suggest_mode("帮我做 code review 检查这段代码的安全性"), Some(AbacusMode::Meeting));
    }

    #[test]
    fn test_design_review_triggers_meeting() {
        assert_eq!(suggest_mode("我想做一次架构评审评估，讨论整体系统设计方案的安全性"), Some(AbacusMode::Meeting));
    }

    #[test]
    fn test_fullstack_triggers_team() {
        assert_eq!(suggest_mode("帮我用 full stack 实现一个博客系统"), Some(AbacusMode::Team));
    }

    #[test]
    fn test_long_code_block_triggers_team() {
        let long = std::iter::repeat_n("fn hello() { println!(\"world\"); }\n", 100).collect::<String>();
        let input = format!("```\n{}```\n请优化", long);
        assert_eq!(suggest_mode(&input), Some(AbacusMode::Team));
    }

    #[test]
    fn test_common_question_no_switch() {
        assert_eq!(suggest_mode("Rust 里 String 和 &str 有什么区别"), None);
        assert_eq!(suggest_mode("如何用 actix-web 做 JWT 认证"), None);
        assert_eq!(suggest_mode("帮我改下这个函数让它支持异步"), None);
    }
}
