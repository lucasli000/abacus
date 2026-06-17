//! # MeetingPromptAssembler — Specialist 提示词组装
//!
//! ## 场景
//! AgentMeeting 中为每个 Specialist 组装独立推理 prompt，包含:
//! - 共享会议上下文（主题、参与专家、近期讨论）
//! - Specialist 专属信息（身份、指导策略、约束）
//! - 路由模式上下文（Fresh / FollowUp / Broadcast）
//!
//! ## 依赖链
//! ```text
//! crate::specialist (SpecialistInstance)
//! crate::meeting::context (ContextPool)
//! crate::meeting::router (RoutingMode)
//!   └── crate::meeting::assembler ← 本文件
//! ```
//!
//! ## 边界
//! - FollowUp 模式需要 private context 已通过 snapshot_private 写入
//! - agent_role 中文标签 hardcoded，同步 AgentRole 枚举变更

use std::collections::BTreeMap;
use crate::specialist::SpecialistInstance;
use crate::meeting::context::ContextPool;
use crate::meeting::router::RoutingMode;

pub struct MeetingPromptAssembler;

impl MeetingPromptAssembler {
    pub fn assemble_meeting_context(
        topic: &str,
        participants: &BTreeMap<String, SpecialistInstance>,
        pool: &ContextPool,
    ) -> String {
        let mut ctx = String::new();
        ctx.push_str(&format!("## 会议主题\n{}\n\n", topic));

        ctx.push_str("## 参与专家\n");
        for sp in participants.values() {
            ctx.push_str(&format!("- {} ({}) - {}\n", sp.name, sp.id.0, sp.specialty.domain));
        }

        ctx.push('\n');
        ctx.push_str("## 近期讨论\n");
        let recent = pool.recent(5);
        if recent.is_empty() {
            ctx.push_str("（尚无对话）\n");
        } else {
            for entry in recent {
                ctx.push_str(&format!("- 轮次{} [{}]: {} (置信度: {:.1})\n",
                    entry.turn, entry.speaker.0, entry.conclusion, entry.confidence));
            }
        }

        ctx
    }

    pub fn assemble_specialist_prompt(
        topic: &str,
        participants: &BTreeMap<String, SpecialistInstance>,
        pool: &ContextPool,
        specialist: &SpecialistInstance,
        routing_mode: &RoutingMode,
    ) -> String {
        let mut prompt = String::new();

        prompt.push_str("你是一名领域专家，正在参加一个多专家会议。\n\n");
        prompt.push_str(&Self::assemble_meeting_context(topic, participants, pool));

        let role_str = match specialist.role {
            crate::team::AgentRole::Leader => "主持人",
            crate::team::AgentRole::PM => "项目经理",
            crate::team::AgentRole::Advisor => "顾问",
            crate::team::AgentRole::Member => "成员",
            crate::team::AgentRole::ExternalAgent { .. } => "外部专家",
        };
        prompt.push_str(&format!("\n## 你的身份\n- 名称: {}\n- 领域: {}\n- 角色: {}\n\n",
            specialist.name, specialist.specialty.domain, role_str));

        prompt.push_str(&format!("## 你的指导策略\n{}\n\n", specialist.specialty.guide_strategy));

        match routing_mode {
            RoutingMode::Fresh => {
                prompt.push_str("这是你的首次参与。请基于你的领域知识进行分析。\n\n");
            }
            RoutingMode::FollowUp => {
                if let Some(ctx) = pool.get_private(&specialist.id) {
                    prompt.push_str("## 你的历史推理记录\n");
                    for msg in &ctx.messages {
                        prompt.push_str(&format!("- {}\n", msg));
                    }
                    prompt.push('\n');
                }
                prompt.push_str("请基于之前的推理和新的输入继续分析。\n\n");
            }
            RoutingMode::Broadcast => {
                prompt.push_str("所有专家将同时分析此问题。请从你的领域视角给出独立判断。\n\n");
            }
        }

        prompt.push_str("## 约束\n");
        prompt.push_str(&format!("{}\n", specialist.specialty.anti_pattern));
        prompt.push_str(&format!("- 最低置信度: {:.1}\n", specialist.specialty.engagement.min_confidence));
        prompt.push_str(&format!("- 每轮最多发言: {} 次\n", specialist.specialty.engagement.max_speeches_per_round));
        prompt.push_str("- 输出格式: 先推理后结论\n");

        prompt
    }
}
