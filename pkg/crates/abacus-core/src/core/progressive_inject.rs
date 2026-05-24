//! 渐进输出 Prompt 注入
//!
//! ## 场景
//! 根据 ProgressiveController 状态，向 LLM system prompt 注入行为指令。
//! 优先级 175（Context Protocol 170 之上，Tool Operation 180 之下）。
//!
//! ## 依赖
//! - `abacus_types::progressive::*`: 状态和类型
//! - `crate::core::injector::PromptSegment`: 注入格式
//!
//! ## 引用关系
//! - 被 DynamicInjector 注册为 InjectionSource
//! - 读取 ProgressiveController 状态决定是否注入

use abacus_types::progressive::*;

/// Gated Phase 1 — 要求 LLM 输出 Checklist JSON
pub const GATED_PHASE1_PROMPT: &str = r#"
=== Progressive Output Protocol — Checklist Mode ===

Output a structured confirmation checklist in JSON. Do NOT write the full document yet.

Format:

```json
{
  "type": "checklist",
  "info_items": [
    {"category": "info_acquired|needs_verification|risk_notice", "label": "...", "source": "..."}
  ],
  "decisions": [
    {
      "id": 1,
      "question": "Decision question (one sentence)",
      "context": "Why this decision matters (≤30 chars)",
      "options": [
        {"id": "a", "label": "Option A", "summary": "one line", "pros": ["..."], "cons": ["..."], "confidence": 0.8}
      ],
      "recommended": "a",
      "recommendation_reason": "Reason (≤50 chars, conclusion first)"
    }
  ],
  "context_digest": "Task summary (≤100 chars)"
}
```

Rules:
- info_items: 2-5 items
- decisions: 1-3 blocks
- Each decision must include recommended + reason
- If confidence gap < 0.1, mark "均可" instead of recommending
- STOP immediately after outputting the checklist. Do not continue.
"#;

/// Staged 模式 — 分段输出指令
pub const STAGED_PROMPT: &str = r#"
=== Progressive Output Protocol — Staged Mode ===

Organize your output into clear sections. After each section, output the marker:
---section-break---

Continue with the next section without waiting.
"#;

/// 根据当前状态生成注入 Prompt
///
/// ## 返回
/// - None: 不需要注入（PassThrough 或无关状态）
/// - Some: 注入内容 + 优先级
pub fn build_progressive_prompt(state: &ProgressiveState, _strategy: Option<&OutputStrategy>) -> Option<(u16, String)> {
    match state {
        ProgressiveState::Analyzing => None,

        ProgressiveState::StrategyDecided { strategy: s } => {
            build_for_strategy(s)
        },

        ProgressiveState::Generating { confirmed_decisions, current_section, .. } => {
            Some((175, format_continuation_prompt(confirmed_decisions, *current_section)))
        },

        ProgressiveState::ReconfirmRequested { .. } => None,
        ProgressiveState::AwaitingConfirmation { .. } => None,
        ProgressiveState::Completed { .. } => None,
        ProgressiveState::Aborted { .. } => None,
    }
}

fn build_for_strategy(strategy: &OutputStrategy) -> Option<(u16, String)> {
    match strategy {
        OutputStrategy::PassThrough => None,
        OutputStrategy::Gated { .. } => Some((175, GATED_PHASE1_PROMPT.to_string())),
        OutputStrategy::Staged { sections } => {
            if sections.is_empty() {
                Some((175, STAGED_PROMPT.to_string()))
            } else {
                Some((175, format_staged_with_plan(sections)))
            }
        },
    }
}

/// 续写 Prompt：用户确认后注入
fn format_continuation_prompt(
    decisions: &[(u32, UserResponse)],
    current_section: Option<u32>,
) -> String {
    let mut prompt = String::from("=== Progressive Output Protocol — Continue ===\n\n");
    prompt.push_str("User has confirmed the following decisions. Write the full document based on these:\n\n");

    for (id, response) in decisions {
        match response {
            UserResponse::Confirmed => {
                prompt.push_str(&format!("- Item #{}: Confirmed as-is\n", id));
            }
            UserResponse::Corrected(c) => {
                prompt.push_str(&format!("- Item #{}: Corrected to「{}」\n", id, c));
            }
            UserResponse::Chosen(c) => {
                prompt.push_str(&format!("- Item #{}: User chose「{}」\n", id, c));
            }
            UserResponse::Supplemented(info) => {
                prompt.push_str(&format!("- Item #{}: User added「{}」\n", id, info));
            }
            UserResponse::Skipped => {
                prompt.push_str(&format!("- Item #{}: Skipped, use default\n", id));
            }
        }
    }

    if let Some(section) = current_section {
        prompt.push_str(&format!("\nCurrent section: #{}\n", section));
    }

    prompt.push_str("\nNow write the complete document based on confirmed decisions.");
    prompt
}

/// Staged 模式带段落计划
fn format_staged_with_plan(sections: &[SectionPlan]) -> String {
    let mut prompt = String::from("=== Progressive Output Protocol — Staged Mode ===\n\n");
    prompt.push_str("Organize output into these sections:\n\n");

    for section in sections {
        let confirm_tag = if section.requires_confirmation { " [CONFIRM BEFORE WRITING]" } else { "" };
        prompt.push_str(&format!(
            "- Section {}: {} (~{} chars){}\n",
            section.id, section.title, section.estimated_chars, confirm_tag
        ));
    }

    prompt.push_str("\nAfter each section, output: ---section-break---\n");
    prompt.push_str("For sections marked [CONFIRM BEFORE WRITING], stop and wait for user confirmation.\n");
    prompt
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// Humanizer Prompt — 去 AI 味风格约束（优先级 85，条件注入）
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// 去 AI 味风格约束（中文版）
///
/// ## 注入条件
/// HumanizerMode != Off 时注入（prompt_only / detect_only / full 均注入此 prompt）
///
/// ## 优先级
/// 85 — 低于 Skill(90)，不干扰核心能力约束
pub const HUMANIZER_STYLE_PROMPT: &str = r#"
=== 输出风格约束 ===

你的输出将直接面向用户阅读。按以下规则生成内容。

── 写之前自检：这个词/句式能删掉吗？ ──

删除测试：把一个词或句子遮住，如果读者不会丢失任何信息 → 删掉它。
这条规则适用于下面所有禁止项。

── 禁止清单（命中任何一条就改写该句）──

| 检查项 | 触发词示例 | 改写方法 |
|--------|-----------|----------|
| 空洞修饰 | 显著/至关重要/不容忽视/深远影响/极其/非常 | 删除该词，或替换为具体量（+30%、耗时减半、3个团队受影响） |
| 万能开场 | 在当今.../随着...发展/众所周知 | 删除整句，从第一个事实开始 |
| 机械连接 | 因此/然而/此外/与此同时/值得注意的是 — 连续出现 2 次以上 | 保留 1 个最关键的，其余用句号断开或删除 |
| 序数模板 | 首先.../其次.../最后... 或 第一.../第二.../第三... | 按因果/重要性/时间组织，不用序数词 |
| 重复总结 | 综上所述/总之/归纳一下 + 重复正文已写的内容 | 删除，或只写正文没提到的行动建议 |
| 模糊归因 | 研究表明/专家指出/业界普遍认为（无来源） | 删除，或改为你的直接判断并加"我的判断是"前缀 |
| 否定排比 | 不是X，不是Y，而是Z | 直接写"是Z"，省略铺垫 |

── 用词选择规则 ──

当同一个意思有多种说法时，按此顺序选词：
1. 行业通用名词 — 读者不需要查就懂的词（如"吞吐量""审批流""成本"）
2. 专有名词 — 需要行业背景但更精确的词（如"QPS""BPMN""EBITDA"）
3. 技术术语 — 仅在必须严格定义时使用（如"amortized O(1)""idempotent"）

判断方法：如果用第1层的词就能让目标读者理解，不要用第2层或第3层。
如果上下文已经确立了专有名词，后续可以直接用缩写。

── 观点和判断 ──

不要写中立到没有信息量的"两边都有道理"。按以下规则表达立场：

- 有明确事实支撑 → 直接给结论（"X 方案更适合，因为 Y 数据"）
- 有经验判断但无硬数据 → 标注前提（"在 500 人以下规模，X 通常更合适"）
- 有不确定性 → 说出不确定的具体部分（"成本差异取决于 Z，目前无法量化"）
- 指出理论的局限 → "这个方法假设了 A，但现实中 A 不总成立"

禁止：无条件的"X很好""Y很重要"。每个判断后面必须跟可验证的依据或明确的前提条件。

── 句式 ──

- 一个句子只说一件事。超过 40 字的句子拆成两句。
- 能用主动语态就不用被动语态（"系统处理请求"而非"请求被系统处理"）。
- 段落首句是该段的结论或主张，后续句子是支撑。

── 输出自检 ──

输出前用这 5 个问题检查：
1. 第一句有没有信息量？（如果删掉第一句不影响理解 → 改）
2. 有没有连续 3 个以上的列表项？（考虑用叙述替代或分组）
3. 有没有不带数据/案例/前提的形容词？（删或补）
4. 读者需要回读才能理解吗？（拆长句、明确指代）
5. 最后一段是不是重复了前面的内容？（是 → 删）
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_passthrough_no_injection() {
        let state = ProgressiveState::StrategyDecided {
            strategy: OutputStrategy::PassThrough,
        };
        let result = build_progressive_prompt(&state, None);
        assert!(result.is_none());
    }

    #[test]
    fn test_gated_injects_phase1() {
        let state = ProgressiveState::StrategyDecided {
            strategy: OutputStrategy::Gated { checklist: Checklist::placeholder() },
        };
        let result = build_progressive_prompt(&state, None);
        assert!(result.is_some());
        let (priority, content) = result.unwrap();
        assert_eq!(priority, 175);
        assert!(content.contains("checklist"));
        assert!(content.contains("STOP immediately"));
    }

    #[test]
    fn test_continuation_prompt() {
        let decisions = vec![
            (1, UserResponse::Chosen("方案A".into())),
            (2, UserResponse::Corrected("500人".into())),
        ];
        let prompt = format_continuation_prompt(&decisions, Some(0));
        assert!(prompt.contains("方案A"));
        assert!(prompt.contains("500人"));
        assert!(prompt.contains("Corrected to"));
    }

    #[test]
    fn test_staged_with_plan() {
        let sections = vec![
            SectionPlan { id: 0, title: "需求分析".into(), estimated_chars: 1000, requires_confirmation: false, status: SectionStatus::Planned },
            SectionPlan { id: 1, title: "方案设计".into(), estimated_chars: 2000, requires_confirmation: true, status: SectionStatus::Planned },
        ];
        let prompt = format_staged_with_plan(&sections);
        assert!(prompt.contains("需求分析"));
        assert!(prompt.contains("[CONFIRM BEFORE WRITING]"));
        assert!(prompt.contains("section-break"));
    }
}
