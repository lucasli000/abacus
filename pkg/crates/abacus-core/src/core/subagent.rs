//! # ToolAgent 自动委托框架
//!
//! ## 设计意图
//! 当 LLM 返回的一批 tool_calls 匹配某个 ToolAgent 的触发条件时，
//! 自动将这批工具调用委托给隔离的 sub-session 执行，主消息流仅展示汇总。
//!
//! ## 架构
//! ```text
//! Pipeline tool dispatch
//!   ↓ check: 本批 tool_calls 是否全部匹配某个 ToolAgentDef?
//!   ├── YES → 委托给 ToolAgentRunner (隔离 session, 只暴露 allowed_tools)
//!   │         └── 执行所有 tool_calls → 汇总结果 → 返回一条 StreamChunk::ToolAgentResult
//!   └── NO  → 走正常 pipeline tool dispatch (当前行为不变)
//! ```
//!
//! ## 可扩展性
//! 不止 Explorer——框架支持注册任意数量的 ToolAgentDef:
//! - Explorer: 只读查询（fs_read/grep/search/cg_query）
//! - Analyzer: 代码分析（AST/依赖/metrics）
//! - Researcher: 网络检索（web_search/web_fetch）
//! - 用户自定义: ~/.abacus/subagents.yaml
//!
//! ## 引用关系
//! - 创建: CoreLoop::new() 注册内置 + 加载用户自定义
//! - 消费: pipeline/mod.rs tool dispatch 分支
//! - 配置: ~/.abacus/subagents.yaml (可选)
//!
//! ## 生命周期
//! ToolAgentDef: 引擎生命周期（static config）
//! ToolAgentRunner: 单批 tool_calls 生命周期（执行完即销毁）

use serde::{Deserialize, Serialize};
use std::collections::HashSet;

use abacus_types::ToolOutput;

/// ToolAgent 定义 — 描述一种可自动委托的工具执行模式
///
/// ## 触发条件
/// 当一批 tool_calls 的所有 tool_id 都在 `tool_filter` 内时触发
///
/// ## 引用关系
/// - 注册: ToolAgentRegistry.register()
/// - 匹配: ToolAgentRegistry.match_batch()
/// - 执行: ToolAgentRunner.execute_batch()
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolAgentDef {
    /// 唯一标识（如 "explorer", "analyzer", "researcher"）
    pub id: String,
    /// 显示名（消息流中展示）
    pub name: String,
    /// 图标（消息流前缀）
    pub icon: String,
    /// 该 subagent 可以执行的工具集合
    /// 当一批 tool_calls 全部在此集合内时触发委托
    pub tool_filter: HashSet<String>,
    /// 匹配优先级（多个 ToolAgent 都匹配时取最高优先级）
    /// 0 = 最高
    pub priority: u32,
    /// 结果汇总模板
    /// {count} = 工具调用次数, {tools} = 工具名列表, {summary} = 结果摘要
    pub summary_template: String,
    /// 是否将多条 tool_result 压缩为一条摘要返回给 LLM
    /// true: LLM 只看到一条聚合摘要（省 token，适合大批量只读查询）
    /// false: LLM 看到每条 tool_result 的完整内容（推理精度最高）
    /// 默认 false（保守——不降级推理质量）
    #[serde(default)]
    pub summarize_results: bool,
    /// 是否启用
    pub enabled: bool,
}

/// ToolAgent 注册表 — 管理所有可用的 ToolAgent 定义
///
/// ## 引用关系
/// - 持有: CoreLoop.subagent_registry
/// - 查询: pipeline tool dispatch 每批 tool_calls 前调用 match_batch
pub struct ToolAgentRegistry {
    agents: Vec<ToolAgentDef>,
}

impl ToolAgentRegistry {
    pub fn new() -> Self {
        Self { agents: Vec::new() }
    }

    /// 注册一个 ToolAgent 定义
    pub fn register(&mut self, def: ToolAgentDef) {
        self.agents.push(def);
        self.agents.sort_by_key(|a| a.priority);
    }

    /// 注册所有内置 ToolAgent
    ///
    /// ## 内置类型
    /// - Explorer: 只读文件/代码查询（idempotent 读操作聚合）
    /// - Researcher: 网络检索（web_search/fetch 聚合）
    /// - Coder: 代码编写/修改（fs_write/fs_edit/bash 聚合）
    /// - Mathematician: 数学计算/推导（compute/eval 聚合）
    ///
    /// ## 成长机制
    /// 用户可通过 ~/.abacus/subagents.yaml 添加自定义类型，
    /// 也可通过 `/subagent disable <id>` 禁用内置类型
    pub fn register_builtins(&mut self) {
        // Explorer — 只读查询聚合（消息流不刷屏 + 摘要返回省 token）
        self.register(ToolAgentDef {
            id: "explorer".into(),
            name: "Explorer".into(),
            icon: "▸".into(),
            tool_filter: [
                "fs_read", "fs_list", "fs_search", "fs_tree", "fs_info",
                "grep", "cg_query", "cg_search", "cg_list",
                "db_query", "db_read_records", "db_list_tables", "db_table_schema",
                "kb_query", "retrieval_search",
                "lsp_symbols", "lsp_definition", "lsp_references",
            ].iter().map(|s| s.to_string()).collect(),
            priority: 0,
            summary_template: "{icon} {name} · 查阅了 {count} 处 → {summary}".into(),
            summarize_results: true,  // 只读场景：摘要返回，省 ~2000 tok/batch
            enabled: true,
        });

        // Researcher — 网络检索聚合（摘要返回：网页内容通常很长）
        self.register(ToolAgentDef {
            id: "researcher".into(),
            name: "Researcher".into(),
            icon: "◆".into(),
            tool_filter: [
                "web_search", "web_fetch", "web_readable",
            ].iter().map(|s| s.to_string()).collect(),
            priority: 1,
            summary_template: "{icon} {name} · 检索了 {count} 条 → {summary}".into(),
            summarize_results: true,  // 网页内容长，摘要省大量 token
            enabled: true,
        });

        // Coder — 代码编写/修改聚合（文件写入 + 编辑 + bash 编译/测试）
        self.register(ToolAgentDef {
            id: "coder".into(),
            name: "Coder".into(),
            icon: "⚙".into(),
            tool_filter: [
                "fs_write", "fs_edit", "fs_read", "fs_search", "fs_list",
                "bash_exec", "grep",
                "lsp_symbols", "lsp_definition", "lsp_references",
            ].iter().map(|s| s.to_string()).collect(),
            priority: 10,
            summary_template: "{icon} {name} · 修改了 {count} 处 → {summary}".into(),
            summarize_results: false,  // 代码修改需要 LLM 看到完整结果（编译错误等）
            enabled: true,
        });

        // Mathematician — 数学计算/推导/数据分析
        self.register(ToolAgentDef {
            id: "mathematician".into(),
            name: "Math".into(),
            icon: "∑".into(),
            tool_filter: [
                "compute_eval", "compute_symbolic", "compute_matrix",
                "db_query",  // SQL 统计计算
                "bash_exec", // python/R 脚本执行
            ].iter().map(|s| s.to_string()).collect(),
            priority: 5,
            summary_template: "{icon} {name} · 计算了 {count} 步 → {summary}".into(),
            summarize_results: false,  // 计算结果需要精确值
            enabled: true,
        });
    }

    /// 加载用户自定义 ToolAgent (~/.abacus/subagents.yaml)
    ///
    /// ## 文件格式
    /// ```yaml
    /// - id: analyzer
    ///   name: Analyzer
    ///   icon: "📊"
    ///   tool_filter: [ast_parse, metrics_compute, dep_graph]
    ///   priority: 2
    ///   summary_template: "{icon} {name} · 分析了 {count} 项 → {summary}"
    /// ```
    pub fn load_user_definitions(&mut self) {
        let path = crate::paths::global_dir().join("config/subagents.yaml");
        if !path.exists() {
            return;
        }
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => return,
        };
        if let Ok(defs) = serde_yaml::from_str::<Vec<ToolAgentDef>>(&content) {
            for def in defs {
                if def.enabled {
                    self.register(def);
                }
            }
        }
    }

    /// 检查一批 tool_calls 是否全部匹配某个 ToolAgent
    ///
    /// ## 返回
    /// - `Some(def)` — 匹配的 ToolAgent（按 priority 取最高）
    /// - `None` — 无匹配，走正常 dispatch
    ///
    /// ## 匹配规则
    /// 批次中所有 tool_id 都在某个 ToolAgentDef.tool_filter 内 → 匹配
    /// 全量匹配：所有 tool_ids 都在同一 agent 的 filter 里
    pub fn match_batch(&self, tool_ids: &[&str]) -> Option<&ToolAgentDef> {
        if tool_ids.is_empty() {
            return None;
        }
        for agent in &self.agents {
            if !agent.enabled {
                continue;
            }
            let all_match = tool_ids.iter().all(|id| agent.tool_filter.contains(*id));
            if all_match {
                return Some(agent);
            }
        }
        None
    }

    /// V41: 部分匹配——将 tool_calls 拆分为 (agent匹配的, 不匹配的)
    ///
    /// 返回：Some((agent_def, matched_indices, unmatched_indices))
    /// 其中 matched_indices 对应可由 ToolAgent 批量处理的 tool_call index，
    /// unmatched_indices 需走普通逐个 dispatch 路径。
    ///
    /// 匹配条件：至少 2 个 tool_call 匹配同一 agent（否则 batch 无意义）
    pub fn match_partial(&self, tool_ids: &[&str]) -> Option<(&ToolAgentDef, Vec<usize>, Vec<usize>)> {
        if tool_ids.len() < 2 {
            return None;
        }
        // 按优先级找第一个能匹配 >=2 个 tool_call 的 agent
        for agent in &self.agents {
            if !agent.enabled {
                continue;
            }
            let mut matched: Vec<usize> = Vec::new();
            let mut unmatched: Vec<usize> = Vec::new();
            for (i, id) in tool_ids.iter().enumerate() {
                if agent.tool_filter.contains(*id) {
                    matched.push(i);
                } else {
                    unmatched.push(i);
                }
            }
            if matched.len() >= 2 {
                return Some((agent, matched, unmatched));
            }
        }
        None
    }

    /// 列出所有已注册的 ToolAgent
    pub fn list(&self) -> &[ToolAgentDef] {
        &self.agents
    }
}

/// ToolAgent 批次执行结果
///
/// 引用关系: pipeline 委托执行后接收此结构，转为 StreamChunk 推给 TUI
#[derive(Debug, Clone)]
pub struct ToolAgentBatchResult {
    /// 触发的 ToolAgent ID
    pub agent_id: String,
    /// 图标
    pub icon: String,
    /// 显示名
    pub name: String,
    /// 执行的工具调用数量
    pub call_count: usize,
    /// 各工具的原始输出（完整，供折叠展开用）
    pub outputs: Vec<ToolOutput>,
    /// 汇总摘要（一句话描述结果）
    pub summary: String,
}

impl ToolAgentBatchResult {
    /// 生成消息流展示文本
    pub fn display_text(&self) -> String {
        format!("{} {} · 查阅了 {} 处", self.icon, self.name, self.call_count)
    }

    /// 生成折叠详情（展开后可见）
    pub fn detail_text(&self) -> String {
        self.outputs.iter()
            .map(|o| {
                let status = if o.success { "✓" } else { "✗" };
                let preview: String = o.output.to_string().chars().take(100).collect();
                format!("  {} {} → {}", status, o.tool_id.0, preview)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builtin_explorer_matches_read_batch() {
        let mut reg = ToolAgentRegistry::new();
        reg.register_builtins();

        // 全读操作 → 匹配 Explorer
        let tools = vec!["fs_read", "grep", "fs_search"];
        let matched = reg.match_batch(&tools);
        assert!(matched.is_some());
        assert_eq!(matched.unwrap().id, "explorer");
    }

    #[test]
    fn test_mixed_batch_matches_coder() {
        let mut reg = ToolAgentRegistry::new();
        reg.register_builtins();

        // 读写混合 → 匹配 Coder（其 tool_filter 含 fs_read + fs_write）
        let tools = vec!["fs_read", "fs_write"];
        let matched = reg.match_batch(&tools);
        assert!(matched.is_some());
        assert_eq!(matched.unwrap().id, "coder");
    }

    #[test]
    fn test_unknown_tool_no_match() {
        let mut reg = ToolAgentRegistry::new();
        reg.register_builtins();

        // 含未知工具 → 无 subagent 匹配
        let tools = vec!["fs_read", "unknown_tool"];
        let matched = reg.match_batch(&tools);
        assert!(matched.is_none());
    }

    #[test]
    fn test_web_batch_matches_researcher() {
        let mut reg = ToolAgentRegistry::new();
        reg.register_builtins();

        let tools = vec!["web_search", "web_fetch"];
        let matched = reg.match_batch(&tools);
        assert!(matched.is_some());
        assert_eq!(matched.unwrap().id, "researcher");
    }

    #[test]
    fn test_empty_batch_no_match() {
        let mut reg = ToolAgentRegistry::new();
        reg.register_builtins();

        let matched = reg.match_batch(&[]);
        assert!(matched.is_none());
    }

    #[test]
    fn test_priority_explorer_over_researcher() {
        let mut reg = ToolAgentRegistry::new();
        reg.register_builtins();

        // Explorer priority=0 比 Researcher priority=1 高
        // 但 web_search 不在 explorer filter 里所以不会冲突
        let tools = vec!["fs_read"];
        let matched = reg.match_batch(&tools);
        assert_eq!(matched.unwrap().id, "explorer");
    }

    #[test]
    fn test_user_defined_subagent() {
        let mut reg = ToolAgentRegistry::new();
        reg.register_builtins();
        reg.register(ToolAgentDef {
            id: "analyzer".into(),
            name: "Analyzer".into(),
            icon: "◇".into(),
            tool_filter: ["ast_parse", "metrics"].iter().map(|s| s.to_string()).collect(),
            priority: 2,
            summarize_results: true,
            summary_template: "{icon} analyzed {count} items".into(),
            enabled: true,
        });

        let tools = vec!["ast_parse", "metrics"];
        let matched = reg.match_batch(&tools);
        assert!(matched.is_some());
        assert_eq!(matched.unwrap().id, "analyzer");
    }
}
