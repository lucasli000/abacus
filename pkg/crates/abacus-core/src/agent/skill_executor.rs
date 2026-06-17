//! AgentSkillExecutor — LLM 直接调用外部 Agent 技能
//!
//! ## 设计
//! Agent 技能注册为 ToolRegistry 中的复合工具。
//! LLM 调用时，通过 MCP/HTTP 协议将请求发送给外部 Agent，
//! Agent 内部执行多步工作流，返回聚合结果。
//!
//! ## 与 ExternalAgentToolExecutor 的区别
//! - ExternalAgentToolExecutor: 调用 Agent 的单个工具
//! - AgentSkillExecutor: 调用 Agent 的技能（可能包含多步工作流）
//!
//! ## 工具 ID 格式
//! `agent_{agent_id}_skill_{skill_id}`

use std::sync::Arc;
use async_trait::async_trait;
use serde_json::Value;
use abacus_types::{ToolId, KernelError};
use crate::tool::{ToolExecutor, ExecutionContext};
use crate::mcp::McpClient;
use crate::agent::executor::sanitize_name;

/// Agent 技能执行器
pub struct AgentSkillExecutor {
    agent_id: String,
    skill_id: String,
    client: Arc<McpClient>,
}

impl AgentSkillExecutor {
    pub fn new(agent_id: String, skill_id: String, client: Arc<McpClient>) -> Self {
        Self { agent_id, skill_id, client }
    }

    /// 工具 ID 前缀
    pub fn prefix(agent_id: &str, skill_id: &str) -> String {
        format!("agent_{}_skill_{}", sanitize_name(agent_id), sanitize_name(skill_id))
    }
}

#[async_trait]
impl ToolExecutor for AgentSkillExecutor {
    async fn execute(
        &self,
        tool_id: &ToolId,
        params: Value,
        ctx: &ExecutionContext,
    ) -> Result<Value, KernelError> {
        let prefix = Self::prefix(&self.agent_id, &self.skill_id);
        if !tool_id.0.starts_with(&prefix) {
            return Err(KernelError::Other(
                format!("Tool ID '{}' does not match skill prefix '{}'", tool_id.0, prefix)
            ));
        }

        // 构造技能调用请求
        let invoke_params = serde_json::json!({
            "skill_id": self.skill_id,
            "params": params,
            "context": {
                "session_id": ctx.session_id,
                "turn_number": ctx.turn_number,
            }
        });

        // 通过 MCP 协议调用 Agent 的 skill_invoke 工具
        let skill_tool_id = ToolId(format!("agent_{}_skill_invoke", sanitize_name(&self.agent_id)));
        let output = self.client.execute(&skill_tool_id, invoke_params).await
            .map_err(|e| KernelError::Provider(
                format!("Agent '{}' skill '{}' error: {}", self.agent_id, self.skill_id, e)
            ))?;

        Ok(serde_json::to_value(&output).unwrap_or(Value::Null))
    }
}
