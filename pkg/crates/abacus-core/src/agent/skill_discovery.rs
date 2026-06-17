//! AgentSkillDiscovery — 从外部 Agent 发现并注册技能
//!
//! ## 设计
//! Agent 安装后，读取 manifest 中的 skills 定义，
//! 将每个技能注册为 ToolRegistry 中的可调用工具。
//!
//! ## 工具 ID 格式
//! `agent_{agent_id}_skill_{skill_id}`
//!
//! ## 引用关系
//! - 调用: AdaptationPipeline::register_tools() 完成后
//! - 下游: ToolRegistry, SkillEngine

use std::sync::Arc;
use abacus_types::agent::*;
use abacus_types::{ToolId, ToolHandle, ToolProvider, ToolState, ToolEffectiveness, ToolSchema};
use crate::mcp::McpClient;
use crate::agent::skill_executor::AgentSkillExecutor;
use crate::agent::executor::sanitize_name;

/// Agent 技能发现器
pub struct AgentSkillDiscovery;

impl AgentSkillDiscovery {
    /// 注册 Agent 的所有技能到 ToolRegistry
    ///
    /// 每个技能注册为一个复合工具（单工具调用，Agent 内部执行多步）
    pub async fn register_skills(
        manifest: &AgentManifest,
        client: &Arc<McpClient>,
        registry: &crate::tool::ToolRegistry,
    ) -> Vec<String> {
        let mut registered = Vec::new();

        for skill in &manifest.skills {
            let tool_id = format!(
                "agent_{}_skill_{}",
                sanitize_name(&manifest.id),
                sanitize_name(&skill.id)
            );

            // 构造 JSON Schema（从技能步骤推断参数）
            let params_schema = build_skill_schema(skill);

            let handle = ToolHandle {
                id: ToolId(tool_id.clone()),
                schema: ToolSchema {
                    name: format!("{}_{}", manifest.id, skill.id),
                    description: format!(
                        "Agent '{}' skill: {} ({} steps)",
                        manifest.name,
                        skill.id,
                        skill.steps.len()
                    ),
                    parameters: params_schema,
                    returns: None,
                    security: None,
                    cost: None,
                    examples: vec![],
                    applicable_task_kinds: None,
                    idempotent: false,
                    schema_stable: false,
                    short_description: Some(format!("{} skill", skill.id)),
                },
                provider: ToolProvider::ExternalAgent {
                    agent_id: manifest.id.clone(),
                    endpoint: manifest.transport.endpoint.clone(),
                },
                state: ToolState::Active,
                effectiveness: ToolEffectiveness::default(),
            };

            // 注册工具
            registry.register(handle).await;

            // 注册执行器
            let executor = Arc::new(AgentSkillExecutor::new(
                manifest.id.clone(),
                skill.id.clone(),
                client.clone(),
            ));
            registry.register_executor(ToolId(tool_id.clone()), executor).await;

            registered.push(tool_id);
        }

        registered
    }
}

/// 从技能步骤推断参数 JSON Schema
fn build_skill_schema(skill: &AgentSkillSpec) -> serde_json::Value {
    let mut properties = serde_json::Map::new();
    let mut required = Vec::new();

    // 从第一步的 params 推断输入参数
    if let Some(first_step) = skill.steps.first() {
        if let Some(obj) = first_step.params.as_object() {
            for (key, _value) in obj {
                if key == "type" || key == "properties" {
                    continue;
                }
                properties.insert(key.clone(), serde_json::json!({
                    "type": "string",
                    "description": format!("Parameter for {}", key)
                }));
                required.push(key.clone());
            }
        }
    }

    // 添加通用参数
    properties.insert("input".to_string(), serde_json::json!({
        "type": "string",
        "description": "Input text or query for the skill"
    }));

    serde_json::json!({
        "type": "object",
        "properties": properties,
        "required": required
    })
}
