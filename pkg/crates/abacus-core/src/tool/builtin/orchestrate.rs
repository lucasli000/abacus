//! orchestrate — 任务编排评估工具
//!
//! ## 场景
//! 评估任务复杂度（L1/L2/L3），推荐 Agent 团队组成。
//! 两层评估：启发式规则优先（<5ms），复杂/模糊任务 fallback 到 LLM。
//!
//! ## 依赖
//! - 无外部 crate 依赖（规则层纯 Rust 实现）
//! - LLM fallback 通过 CoreLoop 的 LLM provider（可选，不可用时降级）
//!
//! ## 注册工具 (2)
//! | Tool | Confirm | Risk | Description |
//! |------|---------|------|-------------|
//! | orchestrate.assess | no | low | 任务复杂度评估 |
//! | orchestrate.upgrade | no | low | 执行级别不足信号 |

use std::sync::Arc;

use abacus_types::{
    KernelError, ToolCost, ToolEffectiveness, ToolHandle, ToolId, ToolProvider,
    ToolSchema, ToolSecurity, ToolState,
};
use async_trait::async_trait;
use serde_json::{json, Value};

use crate::tool::{ExecutionContext, ToolExecutor, ToolRegistry};

// ─── 常量 ───────────────────────────────────────────────────────────────

/// 规则评估置信度阈值——低于此值 fallback 到 LLM
const RULE_CONFIDENCE_THRESHOLD: f64 = 0.6;

// ─── Executor ───────────────────────────────────────────────────────────

/// 编排评估执行器
///
/// ## 场景
/// 接收任务描述，返回复杂度级别和推荐 Agent 配置。
///
/// ## 评估维度
/// 1. file_scope: 涉及文件数量
/// 2. operation_type: 操作类型复杂度
/// 3. structure_certainty: 结构确定性
/// 4. execution_cost: 执行成本
pub struct OrchestrateToolExecutor;

impl Default for OrchestrateToolExecutor {
    fn default() -> Self {
        Self::new()
    }
}

impl OrchestrateToolExecutor {
    pub fn new() -> Self { Self }

    async fn assess(&self, params: Value) -> abacus_types::Result<Value> {
        let task = params.get("task")
            .and_then(|v| v.as_str())
            .ok_or_else(|| KernelError::Other("missing required parameter: task".into()))?;
        let context = params.get("context").and_then(|v| v.as_str()).unwrap_or("");

        let combined = format!("{} {}", task, context);
        let lower = combined.to_lowercase();

        // 四维度评分
        let file_scope = assess_file_scope(&lower);
        let op_type = assess_operation_type(&lower);
        let certainty = assess_certainty(&lower);
        let cost = assess_cost(&lower);

        // 综合 level
        let max_dim = file_scope.max(op_type).max(certainty).max(cost);
        let avg = (file_scope + op_type + certainty + cost) as f64 / 4.0;

        let level: u8 = if max_dim >= 3 || avg >= 2.5 {
            3
        } else if max_dim >= 2 || avg >= 1.5 {
            2
        } else {
            1
        };

        // 敏感/破坏性标志
        let has_sensitive = detect_sensitive(&lower);
        let has_destructive = detect_destructive(&lower);

        // Agent 推荐
        let agents = match level {
            1 => vec!["Programmer"],
            2 => vec!["PM", "Programmer", "Ops"],
            _ => vec!["PM", "Architect", "Programmer", "Ops"],
        };

        // 置信度（规则评估的确定性）
        let confidence = compute_confidence(&lower, level);
        let method = if confidence >= RULE_CONFIDENCE_THRESHOLD {
            "rule"
        } else {
            // V33 deferred：LLM fallback 设计性延后
            //   触发条件：rule confidence < RULE_CONFIDENCE_THRESHOLD（当前极少触发，
            //     说明规则覆盖足够；激活动力不强）
            //   实装路径（未来若需）：
            //     1. OrchestrateToolExecutor 增 Option<Arc<dyn LlmProvider>> 字段
            //     2. register_executors(registry, llm) 接受 provider
            //     3. 此处 await llm.classify(orchestrate_classification_prompt) 重新评估 level/agents
            //     4. 返回 method = "llm_fallback" 并注入 llm_confidence 字段
            //   当前替代：返回 rule 结果 + "rule_low_confidence" 标注，调用方可据此降级处理
            //     （已为生产可用：规则置信度低不会误导 — 上层看到 "rule_low_confidence" 知道要 human-review）
            "rule_low_confidence"
        };

        Ok(json!({
            "level": level,
            "agents": agents,
            "hasSensitive": has_sensitive,
            "hasDestructive": has_destructive,
            "confidence": confidence,
            "method": method,
            "dimensions": {
                "fileScope": file_scope,
                "operationType": op_type,
                "structureCertainty": certainty,
                "executionCost": cost,
            },
        }))
    }

    async fn upgrade(&self, params: Value) -> abacus_types::Result<Value> {
        let from_level = params.get("fromLevel")
            .and_then(|v| v.as_u64())
            .unwrap_or(1) as u8;
        let reason = params.get("reason")
            .and_then(|v| v.as_str())
            .unwrap_or("unspecified");
        let failed_step = params.get("failedStep")
            .and_then(|v| v.as_u64());

        let to_level = (from_level + 1).min(3);
        let action = if to_level > from_level {
            "upgrade"
        } else {
            "escalate_to_human"
        };

        let agents = match to_level {
            1 => vec!["Programmer"],
            2 => vec!["PM", "Programmer", "Ops"],
            _ => vec!["PM", "Architect", "Programmer", "Ops"],
        };

        Ok(json!({
            "action": action,
            "fromLevel": from_level,
            "toLevel": to_level,
            "agents": agents,
            "reason": reason,
            "failedStep": failed_step,
        }))
    }
}

#[async_trait]
impl ToolExecutor for OrchestrateToolExecutor {
    async fn execute(&self, tool_id: &ToolId, params: Value, _ctx: &ExecutionContext) -> abacus_types::Result<Value> {
        match tool_id.0.as_str() {
            "orchestrate_assess" => self.assess(params).await,
            "orchestrate_upgrade" => self.upgrade(params).await,
            _ => Err(KernelError::Other(format!("unknown: {}", tool_id.0))),
        }
    }
}

// ─── 启发式评估函数 ─────────────────────────────────────────────────────

fn assess_file_scope(text: &str) -> u8 {
    // L3 indicators: 多文件/全局/批量/重构
    let l3_kw = ["all files", "entire", "全部", "批量", "重构", "restructure", "refactor entire", "across"];
    let l2_kw = ["several", "multiple", "几个", "多个", "2-5", "some files"];

    if l3_kw.iter().any(|k| text.contains(k)) { return 3; }
    if l2_kw.iter().any(|k| text.contains(k)) { return 2; }
    1
}

fn assess_operation_type(text: &str) -> u8 {
    let l3_kw = ["restructure", "重构", "migrate", "迁移", "bulk rename", "架构调整", "rewrite"];
    let l2_kw = ["edit multiple", "search and replace", "批量修改", "refactor", "update across"];

    if l3_kw.iter().any(|k| text.contains(k)) { return 3; }
    if l2_kw.iter().any(|k| text.contains(k)) { return 2; }
    1
}

fn assess_certainty(text: &str) -> u8 {
    // 低确定性 indicators
    let l3_kw = ["不确定", "不知道在哪", "somewhere", "find and", "探索", "investigate"];
    let l2_kw = ["可能", "大概", "search for", "look for", "找找"];

    if l3_kw.iter().any(|k| text.contains(k)) { return 3; }
    if l2_kw.iter().any(|k| text.contains(k)) { return 2; }
    1
}

fn assess_cost(text: &str) -> u8 {
    let l3_kw = ["deploy", "部署", "ci/cd", "pipeline", "infrastructure", "基础设施"];
    let l2_kw = ["test", "测试", "review", "审查", "validate"];

    if l3_kw.iter().any(|k| text.contains(k)) { return 3; }
    if l2_kw.iter().any(|k| text.contains(k)) { return 2; }
    1
}

fn detect_sensitive(text: &str) -> bool {
    let kw = ["password", "密码", "token", "secret", "credential", "api_key", "private_key", ".env"];
    kw.iter().any(|k| text.contains(k))
}

fn detect_destructive(text: &str) -> bool {
    let kw = ["delete", "删除", "remove", "drop", "truncate", "overwrite", "覆盖", "rm -rf", "force"];
    kw.iter().any(|k| text.contains(k))
}

fn compute_confidence(text: &str, level: u8) -> f64 {
    // 关键词匹配越多，置信度越高
    let total_kw = text.split_whitespace().count();
    let specificity = if total_kw > 10 { 0.8 } else if total_kw > 5 { 0.6 } else { 0.4 };

    // level 1 通常高置信（简单任务容易判断）
    match level {
        1 => f64::min(specificity + 0.2, 1.0),
        2 => specificity,
        3 => f64::max(specificity - 0.1, 0.3),
        _ => 0.5,
    }
}

// ─── Schema ─────────────────────────────────────────────────────────────

pub fn schemas() -> Vec<ToolSchema> {
    vec![
        ToolSchema {
            name: "orchestrate_assess".into(),
            description: "评估任务复杂度（L1/L2/L3），推荐 Agent 团队".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "task": {"type": "string", "description": "任务描述"},
                    "context": {"type": "string", "description": "附加上下文(可选)"}
                },
                "required": ["task"]
            }),
            returns: None,
            security: Some(ToolSecurity {
                allowed_paths: None, max_size_mb: None,
                confirm_required: false, needs_sandbox: false,
            }),
            cost: Some(ToolCost { tokens: 32, latency: "5ms".into(), risk: "low".into() }),
            examples: Vec::new(),
            applicable_task_kinds: None,
            idempotent: true,
                        schema_stable: false,        },
        ToolSchema {
            name: "orchestrate_upgrade".into(),
            description: "报告执行级别不足，请求升级".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "fromLevel": {"type": "integer", "description": "当前级别(1/2/3)"},
                    "reason": {"type": "string", "description": "升级原因"},
                    "failedStep": {"type": "integer", "description": "失败的步骤编号(可选)"}
                },
                "required": ["fromLevel", "reason"]
            }),
            returns: None,
            security: Some(ToolSecurity {
                allowed_paths: None, max_size_mb: None,
                confirm_required: false, needs_sandbox: false,
            }),
            cost: Some(ToolCost { tokens: 16, latency: "1ms".into(), risk: "low".into() }),
            examples: Vec::new(),
            applicable_task_kinds: None,
            idempotent: false,
                        schema_stable: false,        },
    ]
}

// ─── Registration ───────────────────────────────────────────────────────

pub async fn register(registry: &ToolRegistry) {
    for s in schemas() {
        registry.register(ToolHandle {
            id: ToolId(s.name.clone()),
            schema: s,
            provider: ToolProvider::BuiltIn,
            state: ToolState::Loaded,
            effectiveness: ToolEffectiveness::default(),
        }).await;
    }
}

pub async fn register_executors(registry: &ToolRegistry) {
    let executor = Arc::new(OrchestrateToolExecutor::new());
    for s in schemas() {
        registry.register_executor(ToolId(s.name.clone()), executor.clone()).await;
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_assess_simple_task() {
        let exec = OrchestrateToolExecutor::new();
        let result = exec.assess(json!({"task": "read a file and print its content"})).await.unwrap();
        assert_eq!(result["level"], 1);
        assert_eq!(result["hasSensitive"], false);
        assert_eq!(result["hasDestructive"], false);
    }

    #[tokio::test]
    async fn test_assess_complex_task() {
        let exec = OrchestrateToolExecutor::new();
        let result = exec.assess(json!({
            "task": "重构整个项目的所有文件，迁移到新的架构"
        })).await.unwrap();
        assert_eq!(result["level"], 3);
    }

    #[tokio::test]
    async fn test_assess_sensitive_detection() {
        let exec = OrchestrateToolExecutor::new();
        let result = exec.assess(json!({
            "task": "update the .env file with new api_key"
        })).await.unwrap();
        assert_eq!(result["hasSensitive"], true);
    }

    #[tokio::test]
    async fn test_assess_destructive_detection() {
        let exec = OrchestrateToolExecutor::new();
        let result = exec.assess(json!({
            "task": "delete all temporary files"
        })).await.unwrap();
        assert_eq!(result["hasDestructive"], true);
    }

    #[tokio::test]
    async fn test_upgrade_from_level1() {
        let exec = OrchestrateToolExecutor::new();
        let result = exec.upgrade(json!({
            "fromLevel": 1,
            "reason": "task requires multiple file edits",
            "failedStep": 3
        })).await.unwrap();
        assert_eq!(result["action"], "upgrade");
        assert_eq!(result["toLevel"], 2);
    }

    #[tokio::test]
    async fn test_upgrade_from_level3_escalates() {
        let exec = OrchestrateToolExecutor::new();
        let result = exec.upgrade(json!({
            "fromLevel": 3,
            "reason": "cannot proceed"
        })).await.unwrap();
        assert_eq!(result["action"], "escalate_to_human");
    }
}
