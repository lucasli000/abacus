//! ExternalAgentToolExecutor — 通过 MCP 协议调用外部 Agent 的工具

use std::sync::Arc;
use async_trait::async_trait;
use serde_json::Value;
use abacus_types::{ToolId, KernelError};
use crate::tool::{ToolExecutor, ExecutionContext};
use crate::mcp::McpClient;

/// 外部 Agent 工具执行器
pub struct ExternalAgentToolExecutor {
    agent_id: String,
    client: Arc<McpClient>,
}

impl ExternalAgentToolExecutor {
    pub fn new(agent_id: String, client: Arc<McpClient>) -> Self {
        Self { agent_id, client }
    }

    pub fn prefix(agent_id: &str) -> String {
        format!("agent_{}", sanitize_name(agent_id))
    }
}

#[async_trait]
impl ToolExecutor for ExternalAgentToolExecutor {
    async fn execute(
        &self,
        tool_id: &ToolId,
        params: Value,
        _ctx: &ExecutionContext,
    ) -> Result<Value, KernelError> {
        let prefix = Self::prefix(&self.agent_id);
        if !tool_id.0.starts_with(&prefix) {
            return Err(KernelError::Other(
                format!("Tool ID '{}' does not match agent prefix '{}'", tool_id.0, prefix)
            ));
        }

        // McpClient::execute 接受 &ToolId，返回 ToolOutput
        // 由于 agent 工具 ID 格式为 agent_{id}_{tool}，需要传给 MCP 时用原始 tool 名
        // 但 McpClient 内部有 name_map 做反查，所以直接传完整 tool_id 即可
        let output = self.client.execute(tool_id, params).await
            .map_err(|e| KernelError::Provider(
                format!("Agent '{}' tool error: {}", self.agent_id, e)
            ))?;

        Ok(serde_json::to_value(&output).unwrap_or(Value::Null))
    }
}

pub fn sanitize_name(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_alphanumeric() || c == '_' { c } else { '_' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_name_basic() {
        assert_eq!(sanitize_name("code-reviewer"), "code_reviewer");
        assert_eq!(sanitize_name("my_agent"), "my_agent");
        assert_eq!(sanitize_name("Agent@123"), "Agent_123");
    }

    #[test]
    fn prefix_generation() {
        assert_eq!(ExternalAgentToolExecutor::prefix("code-reviewer"), "agent_code_reviewer");
        assert_eq!(ExternalAgentToolExecutor::prefix("my_agent"), "agent_my_agent");
    }

    #[test]
    fn tool_id_matches_prefix() {
        let prefix = ExternalAgentToolExecutor::prefix("code-reviewer");
        let tool_id = format!("{}_review_code", prefix);
        assert!(tool_id.starts_with(&prefix));
    }
}
