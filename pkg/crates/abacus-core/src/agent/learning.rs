//! Agent 学习模块 — BehaviorPalace 集成
//!
//! ## 设计
//! Agent 执行完成后，将执行结果记录到 BehaviorPalace，
//! 跨 session 积累 Agent 效能数据，影响后续路由优先级。
//!
//! ## 记录内容
//! - Agent 工具调用成功/失败 → BehaviorPalace
//! - Agent 技能执行结果 → BehaviorPalace
//! - 执行延迟 → 效能统计
//!
//! ## 引用关系
//! - 调用: ExternalAgentToolExecutor / AgentSkillExecutor 执行后
//! - 下游: BehaviorPalace → EffectivenessTracker → 工具可见性

use crate::memory_palace::DualPalaceMemory;
use std::sync::Weak;

/// Agent 执行结果记录器
pub struct AgentLearner {
    palace: Weak<DualPalaceMemory>,
}

impl AgentLearner {
    pub fn new(palace: Weak<DualPalaceMemory>) -> Self {
        Self { palace }
    }

    /// 记录 Agent 工具调用结果到 BehaviorPalace
    pub fn record_tool_execution(
        &self,
        agent_id: &str,
        tool_name: &str,
        success: bool,
        _latency_ms: u64,
    ) {
        let Some(palace) = self.palace.upgrade() else { return };

        let pattern = format!("agent:{}:tool:{}", agent_id, tool_name);
        let tags = vec![
            "agent".to_string(),
            format!("agent:{}", agent_id),
            if success { "success".to_string() } else { "failure".to_string() },
        ];

        // 异步记录到 BehaviorPalace（不阻塞主路径）
        let palace = palace.clone();
        tokio::spawn(async move {
            palace.record_interaction(&pattern, &tags).await;
            palace.record_tool_behavior(&pattern, success).await;
        });
    }

    /// 记录 Agent 技能执行结果到 BehaviorPalace
    pub fn record_skill_execution(
        &self,
        agent_id: &str,
        skill_id: &str,
        success: bool,
        _latency_ms: u64,
    ) {
        let Some(palace) = self.palace.upgrade() else { return };

        let pattern = format!("agent:{}:skill:{}", agent_id, skill_id);
        let tags = vec![
            "agent".to_string(),
            "skill".to_string(),
            format!("agent:{}", agent_id),
            format!("skill:{}", skill_id),
            if success { "success".to_string() } else { "failure".to_string() },
        ];

        let palace = palace.clone();
        tokio::spawn(async move {
            palace.record_interaction(&pattern, &tags).await;
            palace.record_tool_behavior(&pattern, success).await;
        });
    }

    /// 记录 Agent 健康状态变化
    pub fn record_health_change(&self, agent_id: &str, reachable: bool) {
        let Some(palace) = self.palace.upgrade() else { return };

        let pattern = format!("agent:{}:health", agent_id);
        let tags = vec![
            "agent".to_string(),
            "health".to_string(),
            format!("agent:{}", agent_id),
            if reachable { "reachable".to_string() } else { "unreachable".to_string() },
        ];

        let palace = palace.clone();
        tokio::spawn(async move {
            palace.record_interaction(&pattern, &tags).await;
        });
    }
}
