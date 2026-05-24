//! AbacusMode — 顶层会话模式（V33: 重命名自 SessionMode + 变体重组）
//!
//! 设计意图：
//! - 用户工作流被建模为 4 个有向无环图（DAG）阶段：
//!     Clarify → {Meeting | Plan} → Team
//! - 每个模式承载不同协作模式 + 工具集 + 输出形态
//! - mode 间通过 ModeArtifact 携带产出，下阶段消费
//!
//! 引用关系：
//! - 写：abacus-cli/src/tui/state/mod.rs (AppState.mode 字段)；slash_commands 转移
//! - 读：abacus-cli/tui/components （顶栏 stepper / panel layout 分支）
//!       abacus-cli/tui/run.rs （根据 mode 路由 send_xxx_message API）
//! - 流转 SSoT：本文件 transitions() / can_transit_to()
//!
//! 生命周期：'static const，整个进程不变
//!
//! ## 4 模式语义
//! - **Clarify**: 单 agent，目标导向 Q&A，澄清需求；产出 brief 摘要
//! - **Meeting**: 多专家会诊讨论；产出会议结论（自由文本）
//! - **Plan**: Planner agent 独立角色，规划任务；产出 TaskSpec[]
//! - **Team**: 多 agent 并行执行；消费上阶段产出（Meeting 结论 / Plan 任务）
//!
//! ## 流转 DAG
//! ```text
//!                    Clarify
//!                   /        \
//!              Meeting      Plan
//!                   \        /
//!                    \      /
//!                     Team
//!                       │
//!                       ▼
//!                 Clarify (重新开始)
//! ```

use serde::{Deserialize, Serialize};

use crate::sandbox::TaskSpec;

/// 顶层会话模式 — V33 替代历史 SessionMode
///
/// ## 不变量
/// - 严格按 transitions() 返回的 DAG 流转，非法转移在 cli 层拒绝
/// - 每模式独立 system prompt + tool whitelist + 协作形态
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum AbacusMode {
    /// 澄清需求 — 用户输入需求，agent 通过提问澄清歧义
    /// V33 续：作为 default 入口，新 session 一律从 Clarify 起步进 DAG
    #[default]
    Clarify,
    /// 专家会诊 — 多专家并行发言，综合得出讨论结论
    Meeting,
    /// 规划任务 — Planner agent 拆解为 TaskSpec[]
    Plan,
    /// 执行任务 — 多 agent 并行执行，消费上阶段产出
    Team,
}

impl AbacusMode {
    /// 短标签（小写英文，用于 slash 命令 / log / 序列化）
    pub fn label(self) -> &'static str {
        match self {
            AbacusMode::Clarify => "clarify",
            AbacusMode::Meeting => "meeting",
            AbacusMode::Plan => "plan",
            AbacusMode::Team => "team",
        }
    }

    /// 中文显示名（UI 主显）
    pub fn display_zh(self) -> &'static str {
        match self {
            AbacusMode::Clarify => "澄清",
            AbacusMode::Meeting => "会诊",
            AbacusMode::Plan => "规划",
            AbacusMode::Team => "执行",
        }
    }

    /// 字符串解析（slash 命令路由用）
    pub fn from_label(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "clarify" => Some(AbacusMode::Clarify),
            "meeting" => Some(AbacusMode::Meeting),
            "plan" => Some(AbacusMode::Plan),
            "team" => Some(AbacusMode::Team),
            _ => None,
        }
    }

    /// 当前模式的合法下一态列表
    ///
    /// DAG 规则：
    /// - Clarify → Meeting 或 Plan（用户决定走会诊还是规划路径）
    /// - Meeting → Team（讨论后转执行）
    /// - Plan → Team（规划后转执行）
    /// - Team → Clarify（执行完成回 Clarify 开新循环）
    /// - 任何状态都允许回 Clarify（用户重置）
    pub fn transitions(self) -> &'static [AbacusMode] {
        match self {
            AbacusMode::Clarify => &[AbacusMode::Meeting, AbacusMode::Plan],
            AbacusMode::Meeting => &[AbacusMode::Team, AbacusMode::Clarify],
            AbacusMode::Plan => &[AbacusMode::Team, AbacusMode::Clarify],
            AbacusMode::Team => &[AbacusMode::Clarify],
        }
    }

    /// 判断是否能从 self 直接转到 target
    pub fn can_transit_to(self, target: AbacusMode) -> bool {
        if self == target {
            return false; // 不允许同态自转（无意义）
        }
        self.transitions().contains(&target)
    }

    /// 是否为 DAG 终态（无后继/Team）
    pub fn is_terminal(self) -> bool {
        matches!(self, AbacusMode::Team)
    }

    /// 列出所有模式（UI picker / 测试用）
    pub fn all() -> &'static [AbacusMode] {
        &[
            AbacusMode::Clarify,
            AbacusMode::Meeting,
            AbacusMode::Plan,
            AbacusMode::Team,
        ]
    }

    /// stepper 在 DAG 中的"路径序号"
    /// Clarify=0；Meeting=1（左分支）；Plan=1（右分支）；Team=2（汇聚）
    /// UI 顶栏画 stepper 时用此值布局
    pub fn stepper_depth(self) -> u8 {
        match self {
            AbacusMode::Clarify => 0,
            AbacusMode::Meeting | AbacusMode::Plan => 1,
            AbacusMode::Team => 2,
        }
    }
}

// V33 注：Default for AbacusMode 已迁移到 #[derive(Default)] + #[default] Clarify 标注
//   理由：clippy::derivable_impls 抓到的 idiomatic 替换；行为完全等价。

/// 模式间携带数据 — 上阶段产出，下阶段消费
///
/// ## 引用关系
/// - 生产者：mode 完成判定时 (Clarify /done / Plan 输出 TaskSpec / Meeting 结论)
/// - 消费者：下阶段进入时（Plan 入口加载 brief / Team 入口加载 tasks）
/// - 携带：放在 AppState.mode_artifact: Option<ModeArtifact>，mode 切换时取走
///
/// ## 生命周期
/// - 进入新 mode 时取走（take）→ 用完弃；不跨多次 mode 切换持留
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ModeArtifact {
    /// Clarify 产出：澄清后的需求摘要
    ClarifyBrief(String),
    /// Meeting 产出：会议结论（自由文本）
    MeetingConclusion(String),
    /// Plan 产出：规划好的任务列表（TaskSpec 序列）
    PlanTasks(Vec<TaskSpec>),
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
            ModeArtifact::PlanTasks(tasks) => {
                format!("📋 已规划 {} 个任务", tasks.len())
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
        assert_eq!(zh.len(), 4, "中文显示名应该唯一");
    }

    #[test]
    fn dag_clarify_to_meeting_or_plan() {
        assert!(AbacusMode::Clarify.can_transit_to(AbacusMode::Meeting));
        assert!(AbacusMode::Clarify.can_transit_to(AbacusMode::Plan));
        // 不能直接 Clarify → Team（必须经 Meeting 或 Plan）
        assert!(!AbacusMode::Clarify.can_transit_to(AbacusMode::Team));
    }

    #[test]
    fn dag_meeting_or_plan_to_team() {
        assert!(AbacusMode::Meeting.can_transit_to(AbacusMode::Team));
        assert!(AbacusMode::Plan.can_transit_to(AbacusMode::Team));
    }

    #[test]
    fn dag_team_back_to_clarify() {
        assert!(AbacusMode::Team.can_transit_to(AbacusMode::Clarify));
        // Team 不能直接到 Meeting/Plan（必须先回 Clarify 开新循环）
        assert!(!AbacusMode::Team.can_transit_to(AbacusMode::Meeting));
        assert!(!AbacusMode::Team.can_transit_to(AbacusMode::Plan));
    }

    #[test]
    fn dag_meeting_can_back_to_clarify() {
        // 用户对会议结果不满意，回 Clarify 重新开始
        assert!(AbacusMode::Meeting.can_transit_to(AbacusMode::Clarify));
        assert!(AbacusMode::Plan.can_transit_to(AbacusMode::Clarify));
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
        assert_eq!(AbacusMode::Plan.stepper_depth(), 1);
        assert_eq!(AbacusMode::Team.stepper_depth(), 2);
    }

    #[test]
    fn default_is_clarify() {
        assert_eq!(AbacusMode::default(), AbacusMode::Clarify);
    }

    #[test]
    fn artifact_summary_renders() {
        let s = ModeArtifact::ClarifyBrief("a".to_string()).summary();
        assert!(s.contains("需求"));
        let s = ModeArtifact::PlanTasks(vec![]).summary();
        assert!(s.contains("规划"));
    }
}
