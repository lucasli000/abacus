//! AbacusMode — 顶层会话模式（V34: Plan/Team 降级为执行策略，枚举仅保留 Clarify/Meeting）
//!
//! 设计意图：
//! - 用户工作流被建模为 2 个有向无环图（DAG）模式：
//!     Clarify ⇄ Meeting
//! - Plan / Team 不再是模式，而是执行策略（由 SlashCommand::ExecuteWithPlan/Team 触发）
//! - mode 间通过 ModeArtifact 携带产出，下阶段消费
//!
//! 引用关系：
//! - 写：abacus-cli/src/tui/state/mod.rs (AppState.mode 字段)；slash_commands 转移
//! - 读：abacus-cli/tui/components （顶栏显示 / panel layout 分支）
//!       abacus-cli/tui/run.rs （根据 mode 路由 send_xxx_message API）
//! - 流转 SSoT：本文件 transitions() / can_transit_to()
//!
//! 生命周期：'static const，整个进程不变
//!
//! ## 2 模式语义
//! - **Clarify**: 单 agent，目标导向 Q&A，澄清需求；产出 brief 摘要
//! - **Meeting**: 多专家会诊讨论；产出会议结论（自由文本）
//!
//! ## 流转 DAG
//! ```text
//!   Clarify ⇄ Meeting
//! ```

use serde::{Deserialize, Serialize};

/// 顶层会话模式 — V34 精简为 2 模式（Plan/Team 降级为执行策略）
///
/// ## 不变量
/// - 严格按 transitions() 返回的 DAG 流转，非法转移在 cli 层拒绝
/// - 每模式独立 system prompt + tool whitelist + 协作形态
/// - Plan/Team 执行策略通过 SlashCommand::ExecuteWithPlan/ExecuteWithTeam 触发，不占用 mode 位
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum AbacusMode {
    /// 澄清需求 — 用户输入需求，agent 通过提问澄清歧义
    /// V34：作为 default 入口，新 session 一律从 Clarify 起步
    #[default]
    Clarify,
    /// 专家会诊 — 多专家并行发言，综合得出讨论结论
    Meeting,
}

impl AbacusMode {
    /// 短标签（小写英文，用于 slash 命令 / log / 序列化）
    pub fn label(self) -> &'static str {
        match self {
            AbacusMode::Clarify => "clarify",
            AbacusMode::Meeting => "meeting",
        }
    }

    /// 中文显示名（UI 主显）
    pub fn display_zh(self) -> &'static str {
        match self {
            AbacusMode::Clarify => "澄清",
            AbacusMode::Meeting => "会诊",
        }
    }

    /// 字符串解析（slash 命令路由用）
    pub fn from_label(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "clarify" => Some(AbacusMode::Clarify),
            "meeting" => Some(AbacusMode::Meeting),
            _ => None,
        }
    }

    /// 当前模式的合法下一态列表
    ///
    /// DAG 规则（V34 简化为 2 模式互转）：
    /// - Clarify → Meeting（切换到会诊）
    /// - Meeting → Clarify（回到澄清）
    pub fn transitions(self) -> &'static [AbacusMode] {
        match self {
            AbacusMode::Clarify => &[AbacusMode::Meeting],
            AbacusMode::Meeting => &[AbacusMode::Clarify],
        }
    }

    /// 判断是否能从 self 直接转到 target
    pub fn can_transit_to(self, target: AbacusMode) -> bool {
        if self == target {
            return false; // 不允许同态自转（无意义）
        }
        self.transitions().contains(&target)
    }

    /// 是否为 DAG 终态（V34: 2 模式均可互转，无终态）
    pub fn is_terminal(self) -> bool {
        false
    }

    /// 列出所有模式（UI picker / 测试用）
    pub fn all() -> &'static [AbacusMode] {
        &[
            AbacusMode::Clarify,
            AbacusMode::Meeting,
        ]
    }

    /// stepper 在 DAG 中的"路径序号"
    /// V34: Clarify=0, Meeting=1（Plan/Team 已降级为策略，不占 stepper 位）
    pub fn stepper_depth(self) -> u8 {
        match self {
            AbacusMode::Clarify => 0,
            AbacusMode::Meeting => 1,
        }
    }
}

// V33 注：Default for AbacusMode 已迁移到 #[derive(Default)] + #[default] Clarify 标注
//   理由：clippy::derivable_impls 抓到的 idiomatic 替换；行为完全等价。

/// 模式间携带数据 — 上阶段产出，下阶段消费
///
/// ## 引用关系
/// - 生产者：mode 完成判定时 (Clarify /done 携带 ClarifyBrief / Meeting 结论)
/// - 消费者：下阶段进入时（Meeting 入口加载 brief）
/// - 携带：放在 AppState.mode_artifact: Option<ModeArtifact>，mode 切换时取走
///
/// ## 生命周期
/// - 进入新 mode 时取走（take）→ 用完弃；不跨多次 mode 切换持留
///
/// ## V34 变更
/// - 删除 PlanTasks 变体（Plan 已降级为执行策略，不再携带 TaskSpec 到 mode）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ModeArtifact {
    /// Clarify 产出：澄清后的需求摘要
    ClarifyBrief(String),
    /// Meeting 产出：会议结论（自由文本）
    MeetingConclusion(String),
}

impl ModeArtifact {
    /// 简短描述（UI toast 用）
    pub fn summary(&self) -> String {
        match self {
            ModeArtifact::ClarifyBrief(s) => {
                let preview: String = s.chars().take(40).collect();
                let count = s.chars().count();
                if count > 40 {
                    format!("📋 需求摘要 ({} 字): {}…", count, preview)
                } else {
                    format!("📋 需求摘要 ({} 字)", count)
                }
            }
            ModeArtifact::MeetingConclusion(s) => {
                format!("🎙 会议结论 ({} 字)", s.chars().count())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_round_trip() {
        for m in AbacusMode::all() {
            assert_eq!(AbacusMode::from_label(m.label()), Some(*m));
        }
    }

    #[test]
    fn display_zh_unique() {
        let mut zh: Vec<&str> = AbacusMode::all().iter().map(|m| m.display_zh()).collect();
        zh.sort();
        zh.dedup();
        assert_eq!(zh.len(), 2, "中文显示名应该唯一");
    }

    #[test]
    fn dag_clarify_to_meeting() {
        // V34: Clarify 只能转 Meeting
        assert!(AbacusMode::Clarify.can_transit_to(AbacusMode::Meeting));
        // 自转不允许
        assert!(!AbacusMode::Clarify.can_transit_to(AbacusMode::Clarify));
    }

    #[test]
    fn dag_meeting_back_to_clarify() {
        // 用户对会议结果不满意，回 Clarify 重新开始
        assert!(AbacusMode::Meeting.can_transit_to(AbacusMode::Clarify));
        // 自转不允许
        assert!(!AbacusMode::Meeting.can_transit_to(AbacusMode::Meeting));
    }

    #[test]
    fn no_self_transition() {
        for m in AbacusMode::all() {
            assert!(!m.can_transit_to(*m), "{:?} 不应允许自转", m);
        }
    }

    #[test]
    fn stepper_depth_correct() {
        assert_eq!(AbacusMode::Clarify.stepper_depth(), 0);
        assert_eq!(AbacusMode::Meeting.stepper_depth(), 1);
    }

    #[test]
    fn default_is_clarify() {
        assert_eq!(AbacusMode::default(), AbacusMode::Clarify);
    }

    #[test]
    fn is_terminal_always_false() {
        // V34: 无终态，两个模式都可互转
        for m in AbacusMode::all() {
            assert!(!m.is_terminal());
        }
    }

    #[test]
    fn artifact_summary_renders() {
        let s = ModeArtifact::ClarifyBrief("a".to_string()).summary();
        assert!(s.contains("需求"));
        let s = ModeArtifact::MeetingConclusion("会议".to_string()).summary();
        assert!(s.contains("会议"));
    }
}
