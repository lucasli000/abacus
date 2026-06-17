//! Agent 自适配管线
//!
//! ## 职责
//! Agent 安装后自动适配 Abacus 生态：
//! - 工具 → ToolRegistry 注册
//! - Cluster → 自动分配（延迟到 build_tool_definitions_for 时）
//! - Palace → 知识注册（延迟到运行时）
//!
//! ## 引用关系
//! - 调用: AgentRegistry::install() 完成后
//! - 下游: ToolRegistry, BehaviorPalace

use std::sync::Arc;
use abacus_types::agent::*;
use abacus_types::{ToolId, ToolHandle, ToolProvider, ToolState, ToolEffectiveness, ToolSchema};
use crate::mcp::McpClient;
use crate::agent::executor::{ExternalAgentToolExecutor, sanitize_name};

/// 自适配管线
pub struct AdaptationPipeline;

impl AdaptationPipeline {
    /// 注册 Agent 的所有工具到 ToolRegistry
    pub async fn register_tools(
        manifest: &AgentManifest,
        client: &Arc<McpClient>,
        registry: &crate::tool::ToolRegistry,
    ) -> Vec<String> {
        let mut registered = Vec::new();
        let prefix = format!("agent_{}", sanitize_name(&manifest.id));

        for tool_spec in &manifest.tools {
            let tool_id = format!("{}_{}", prefix, sanitize_name(&tool_spec.name));

            let handle = ToolHandle {
                id: ToolId(tool_id.clone()),
                schema: ToolSchema {
                    name: tool_spec.name.clone(),
                    description: tool_spec.description.clone(),
                    parameters: tool_spec.parameters.clone(),
                    returns: None,
                    security: None,
                    cost: None,
                    examples: vec![],
                    applicable_task_kinds: None,
                    idempotent: false,
                    schema_stable: false,
                    short_description: Some(tool_spec.description.chars().take(50).collect()),
                },
                provider: ToolProvider::ExternalAgent {
                    agent_id: manifest.id.clone(),
                    endpoint: manifest.transport.endpoint.clone(),
                },
                state: ToolState::Active,
                effectiveness: ToolEffectiveness::default(),
            };

            // 注册工具 schema
            registry.register(handle).await;

            // 注册执行器
            let executor = Arc::new(
                ExternalAgentToolExecutor::new(manifest.id.clone(), client.clone())
            );
            registry.register_executor(ToolId(tool_id.clone()), executor).await;

            registered.push(tool_id);
        }

        registered
    }
}
