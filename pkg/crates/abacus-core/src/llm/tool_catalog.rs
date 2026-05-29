//! tool_catalog — 紧凑工具目录生成器
//!
//! ## 设计目标
//! 替代全量 tool schema 注入：
//! - 生成按类别分组的工具名列表（每工具 ~2-3 tokens）
//! - LLM 通过目录知道可用工具全集
//! - 场景相关工具另行通过 ToolDefinition 发送完整 schema
//!
//! ## Token 开销
//! 100 工具 × 2-3 tokens/name = ~200-300 tokens（vs 全量 5000-10000）
//!
//! ## 引用关系
//! - 被 `crate::core::prompt_assembly` 在 Layer 180 注入 system prompt
//! - 读取 `ToolHandle` 列表（来自 ToolRegistry）
//!
//! ## 生命周期
//! Session 内工具集不变时，catalog 文本 byte-stable（利于 KV cache）

use abacus_types::{ToolHandle, ToolProvider};

/// 按 provider 类别分组生成紧凑工具目录
///
/// ## 输出格式
/// ```text
/// [Available Tools — use by name; call any tool directly]
/// builtin: fs_read, fs_write, fs_edit, fs_search, code_exec, ...
/// mcp(server1): tool_a, tool_b
/// skill(workflow1): step_x, step_y
/// ```
///
/// ## 设计决策
/// - 按 provider 分组（builtin/mcp/plugin/skill）—— LLM 需要知道来源以判断延迟
/// - 工具名之间逗号分隔 —— 最紧凑可解析格式
/// - 标题行明确告知 LLM "call any tool directly" —— 即使未见完整 schema 也可调用
pub fn generate_catalog(tools: &[ToolHandle]) -> String {
    let mut builtin: Vec<&str> = Vec::new();
    let mut mcp: std::collections::BTreeMap<&str, Vec<&str>> = std::collections::BTreeMap::new();
    let mut plugins: std::collections::BTreeMap<&str, Vec<&str>> = std::collections::BTreeMap::new();
    let mut skills: std::collections::BTreeMap<&str, Vec<&str>> = std::collections::BTreeMap::new();

    for tool in tools {
        let name = tool.schema.name.as_str();
        match &tool.provider {
            ToolProvider::BuiltIn => builtin.push(name),
            ToolProvider::Mcp { server_id } => {
                mcp.entry(server_id.as_str()).or_default().push(name);
            }
            ToolProvider::Plugin { plugin_id } => {
                plugins.entry(plugin_id.as_str()).or_default().push(name);
            }
            ToolProvider::Skill { skill_id } => {
                skills.entry(skill_id.as_str()).or_default().push(name);
            }
        }
    }

    let mut out = String::with_capacity(512);
    out.push_str("[Available Tools — for multi-step tasks prefer skill() over individual tools]\n");

    if !builtin.is_empty() {
        builtin.sort_unstable();
        out.push_str("builtin: ");
        out.push_str(&builtin.join(", "));
        out.push('\n');
    }

    for (server, mut names) in mcp {
        names.sort_unstable();
        out.push_str(&format!("mcp({}): {}\n", server, names.join(", ")));
    }

    for (plugin, mut names) in plugins {
        names.sort_unstable();
        out.push_str(&format!("plugin({}): {}\n", plugin, names.join(", ")));
    }

    for (skill, mut names) in skills {
        names.sort_unstable();
        out.push_str(&format!("skill({}): {}\n", skill, names.join(", ")));
    }

    out
}

/// 估算 catalog 的 token 开销（CJK-aware）
pub fn estimate_catalog_tokens(catalog: &str) -> usize {
    // Tool names are ASCII, so ~0.25 tokens/byte + overhead
    let bytes = catalog.len();
    (bytes as f64 * 0.28) as usize + 10 // conservative estimate
}

#[cfg(test)]
mod tests {
    use super::*;
    use abacus_types::*;

    fn mk_tool(name: &str, provider: ToolProvider) -> ToolHandle {
        ToolHandle {
            id: ToolId(name.into()),
            schema: ToolSchema {
                name: name.into(),
                description: "desc".into(),
                parameters: serde_json::json!({"type": "object"}),
                returns: None, security: None, cost: None,
                examples: Vec::new(),
                applicable_task_kinds: None,
                idempotent: false,
                                schema_stable: false,            },
            provider,
            state: ToolState::Loaded,
            effectiveness: ToolEffectiveness {
                tool_id: ToolId(name.into()),
                composite_score: 0.5,
                tier: VisibilityTier::C,
                cooldown_remaining: 0,
                blocked_by_env: false,
                insufficient_data: false,
            },
        }
    }

    #[test]
    fn test_catalog_groups_by_provider() {
        let tools = vec![
            mk_tool("fs_read", ToolProvider::BuiltIn),
            mk_tool("fs_write", ToolProvider::BuiltIn),
            mk_tool("web_fetch", ToolProvider::Mcp { server_id: "fetch".into() }),
            mk_tool("db_query", ToolProvider::Plugin { plugin_id: "sqlite".into() }),
        ];
        let catalog = generate_catalog(&tools);
        assert!(catalog.contains("[Available Tools"));
        assert!(catalog.contains("builtin: fs_read, fs_write"));
        assert!(catalog.contains("mcp(fetch): web_fetch"));
        assert!(catalog.contains("plugin(sqlite): db_query"));
    }

    #[test]
    fn test_catalog_sorted_within_group() {
        let tools = vec![
            mk_tool("z_tool", ToolProvider::BuiltIn),
            mk_tool("a_tool", ToolProvider::BuiltIn),
            mk_tool("m_tool", ToolProvider::BuiltIn),
        ];
        let catalog = generate_catalog(&tools);
        assert!(catalog.contains("builtin: a_tool, m_tool, z_tool"));
    }

    #[test]
    fn test_catalog_empty() {
        let catalog = generate_catalog(&[]);
        assert!(catalog.contains("[Available Tools"));
        assert!(!catalog.contains("builtin:"));
    }

    #[test]
    fn test_token_estimate_reasonable() {
        let tools: Vec<_> = (0..50).map(|i| mk_tool(&format!("tool_{}", i), ToolProvider::BuiltIn)).collect();
        let catalog = generate_catalog(&tools);
        let tokens = estimate_catalog_tokens(&catalog);
        // 50 tools with ~7 char names → ~350 bytes → ~100-120 tokens
        assert!(tokens < 200);
        assert!(tokens > 50);
    }
}
