//! # ToolAgent 自动委托框架
//!
//! ## 设计意图
//! 当 LLM 返回的一批 tool_calls 匹配某个 ToolAgent 的触发条件时，
//! 自动将这批工具调用委托给隔离的 sub-session 执行，主消息流仅展示汇总。
//!
//! ## 流程
//! ```text
//! pipeline/dispatch: LLM 返回 N 个 tool_calls
//!   ↓ check: 本批 tool_calls 是否全部匹配某个 ToolAgentDef?
//!   ↓ YES → ToolBatchAgentRunner 隔离执行，主消息流只推 1 条 ToolAgentResult
//!   ↓ NO  → 正常逐个 dispatch（旧行为）
//! ```
//!
//! ## 设计权衡
//! - 全量匹配才走 batch（cheap path: simple match_key search）
//! - batch 内错误不中断整批——逐工具报告成功/失败状态
//! - 主消息流不展示 batch 内细节（TUI 通过 ToolAgentResult 展示汇总）
//!
//! ## 扩展性
//! 不止 Explorer——框架支持注册任意数量的 ToolAgentDef:
//! ```yaml
//! # ~/.abacus/subagents.yaml
//! - id: analyzer
//!   tool_filter: [ast_parse, metrics_compute, dep_graph]
//!   priority: 2
//!   summarize_results: true
//! ```
//!
//! ## 生命周期
//! ToolAgentDef: 引擎生命周期（static config）
//! ToolAgentBatchResult: 单次 batch 执行时创建，TUI 消费后 drop

use std::collections::{HashSet, BTreeMap, BTreeSet};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use abacus_types::ToolOutput;

/// ToolAgent 定义 — 描述一种可自动委托的工具执行模式
///
/// ## 关键字段
/// - `tool_filter`: 本 agent 可处理的工具白名单（全量匹配触发 batch）
/// - `priority`: 多个 agent 同时匹配时取最高优先级
/// - `summarize_results`: true → 批内工具结果只返回摘要（省 token）
///
/// ## 生命周期
/// 引擎启动时通过 register() 注册，运行时只读——不热修改
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolAgentDef {
    /// Agent 唯一 ID
    pub id: String,
    /// 显示名
    pub name: String,
    /// 图标（1-2 字符，TUI 展示）
    pub icon: String,
    /// 工具白名单（匹配集合）
    pub tool_filter: HashSet<String>,
    /// 匹配优先级（数字越大越优先）
    pub priority: i32,
    /// 汇总模板（LLM 可见的 tool result 摘要格式）
    pub summary_template: String,
    /// 是否对结果生成摘要（true → 省 token；false → 保留完整结果）
    pub summarize_results: bool,
    /// 启用状态
    pub enabled: bool,
}

/// ToolAgent 注册表 — 管理所有可用的 ToolAgent 定义
///
/// 内部用 RwLock 支持运行时 &self 只读调用 + 异步热添加
pub struct ToolAgentRegistry {
    agents: RwLock<Vec<ToolAgentDef>>,
}

impl ToolAgentRegistry {
    pub fn new() -> Self {
        Self { agents: RwLock::new(Vec::new()) }
    }

    /// 注册一个 ToolAgent 定义（幂等）
    pub async fn register(&self, def: ToolAgentDef) {
        let mut agents = self.agents.write().await;
        // 幂等：同名已存在则替换
        if let Some(pos) = agents.iter().position(|a| a.id == def.id) {
            agents[pos] = def;
        } else {
            agents.push(def);
        }
        agents.sort_by_key(|a| a.priority);
        // 不 invalidate cache——调用方按需决定
    }

    /// 注册所有内置 ToolAgent
    pub async fn register_builtins(&self) {
        // Explorer
        self.register(ToolAgentDef {
            id: "explorer".into(), name: "Explorer".into(), icon: "▸".into(),
            tool_filter: ["fs_read","fs_list","fs_search","fs_tree","fs_info",
                "grep","cg_query","cg_search","cg_list",
                "db_query","db_read_records","db_list_tables","db_table_schema",
                "kb_query","retrieval_search",
                "lsp_symbols","lsp_definition","lsp_references",
            ].iter().map(|s| s.to_string()).collect(),
            priority: 0, summary_template: "{icon} {name} · 查阅了 {count} 处 → {summary}".into(),
            summarize_results: true, enabled: true,
        }).await;
        // Researcher
        self.register(ToolAgentDef {
            id: "researcher".into(), name: "Researcher".into(), icon: "◆".into(),
            tool_filter: ["web_search","web_fetch","web_readable"].iter().map(|s| s.to_string()).collect(),
            priority: 1, summary_template: "{icon} {name} · 检索了 {count} 条 → {summary}".into(),
            summarize_results: true, enabled: true,
        }).await;
        // Coder
        self.register(ToolAgentDef {
            id: "coder".into(), name: "Coder".into(), icon: ">".into(),
            tool_filter: ["fs_write","fs_edit","fs_read","fs_search","fs_list",
                "bash_exec","grep","lsp_symbols","lsp_definition","lsp_references",
            ].iter().map(|s| s.to_string()).collect(),
            priority: 10, summary_template: "{icon} {name} · 修改了 {count} 处 → {summary}".into(),
            summarize_results: false, enabled: true,
        }).await;
        // Mathematician
        self.register(ToolAgentDef {
            id: "mathematician".into(), name: "Math".into(), icon: "∑".into(),
            tool_filter: ["compute_eval","compute_symbolic","compute_matrix",
                "db_query","bash_exec",
            ].iter().map(|s| s.to_string()).collect(),
            priority: 5, summary_template: "{icon} {name} · 计算了 {count} 步 → {summary}".into(),
            summarize_results: false, enabled: true,
        }).await;
    }

    /// 加载用户自定义 ToolAgent（~/.abacus/config/subagents.yaml）
    pub async fn load_user_definitions(&self) {
        let path = crate::paths::global_dir().join("config/subagents.yaml");
        if !path.exists() { return; }
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c, Err(_) => return,
        };
        if let Ok(defs) = serde_yaml::from_str::<Vec<ToolAgentDef>>(&content) {
            for def in defs {
                if def.enabled { self.register(def).await; }
            }
        }
    }

    /// 全量匹配：所有 tool_ids 在同一 agent 的 filter 内
    pub async fn match_batch(&self, tool_ids: &[&str]) -> Option<(usize, ToolAgentDef)> {
        if tool_ids.is_empty() { return None; }
        let agents = self.agents.read().await;
        let mut result: Option<(usize, &ToolAgentDef)> = None;
        for agent in agents.iter() {
            if !agent.enabled { continue; }
            if tool_ids.iter().all(|id| agent.tool_filter.contains(*id)) {
                // 找最高优先级
                match &result {
                    Some((_, cur)) if cur.priority >= agent.priority => {},
                    _ => result = Some((0, agent)),
                }
            }
        }
        result.map(|(_, a)| (a.priority as usize, (*a).clone()))
    }

    /// 部分匹配：至少 2 个 tool_id 在同 agent 内 → 拆分 batch+normal
    pub async fn match_partial(&self, tool_ids: &[&str]) -> Option<(ToolAgentDef, Vec<usize>, Vec<usize>)> {
        if tool_ids.len() < 2 { return None; }
        let agents = self.agents.read().await;
        for agent in agents.iter() {
            if !agent.enabled { continue; }
            let mut matched = Vec::new();
            let mut unmatched = Vec::new();
            for (i, id) in tool_ids.iter().enumerate() {
                if agent.tool_filter.contains(*id) { matched.push(i); } else { unmatched.push(i); }
            }
            if matched.len() >= 2 {
                return Some((agent.clone(), matched, unmatched));
            }
        }
        None
    }

    /// 列出所有已注册的 ToolAgent
    pub async fn list(&self) -> Vec<ToolAgentDef> {
        self.agents.read().await.clone()
    }

    /// 从 Palace 行为桥接自动发现高频工具组合
    pub async fn auto_discover_from_palace(&self, palace: &crate::memory_palace::DualPalaceMemory) {
        let snapshot = palace.behavior.snapshot().await;

        let mut strengths: BTreeMap<String, BTreeMap<String, f64>> = BTreeMap::new();
        for (pattern, _memory) in &snapshot {
            if !pattern.starts_with("tool_call:") { continue; }
            let tool = pattern.strip_prefix("tool_call:").unwrap();
            let relations = palace.bridge.get_related(pattern).await;
            for rel in &relations {
                if rel.relation_type != crate::memory_palace::RelationType::Similar
                    && rel.relation_type != crate::memory_palace::RelationType::RelatedBehavior { continue; }
                let target = if rel.from_id == *pattern { &rel.to_id } else { &rel.from_id };
                if !target.starts_with("tool_call:") { continue; }
                let other = target.strip_prefix("tool_call:").unwrap();
                if other == tool { continue; }
                *strengths.entry(tool.to_string()).or_default().entry(other.to_string()).or_insert(0.0) += rel.strength;
            }
        }

        let registered = self.list().await;

        for (tool, related) in &strengths {
            let mut group: BTreeSet<String> = BTreeSet::new();
            group.insert(tool.clone());
            for (other, strength) in related {
                if *strength >= 0.15 && group.len() < 6 { group.insert(other.clone()); }
            }
            if group.len() < 2 { continue; }
            // 已全部注册 → 跳过
            let all_registered: HashSet<&str> = registered.iter().filter(|a| a.enabled)
                .flat_map(|a| &a.tool_filter).map(|s| s.as_str()).collect();
            if group.iter().all(|t| all_registered.contains(t.as_str())) { continue; }
            let id = format!("auto_{}", group.iter().cloned().collect::<Vec<_>>().join("_"));
            if registered.iter().any(|a| a.id == id) { continue; }
            self.register(ToolAgentDef {
                id, name: format!("Auto-{}", group.iter().cloned().collect::<Vec<_>>().join(",")),
                icon: ">".into(),
                tool_filter: group.iter().cloned().collect(),
                priority: 20,
                summary_template: "{icon} {name} · 处理了 {count} 项 → {summary}".into(),
                summarize_results: true,
                enabled: true,
            }).await;
        }
    }
}

/// ToolAgent 批次执行结果
#[derive(Debug, Clone)]
pub struct ToolAgentBatchResult {
    pub agent_id: String,
    pub icon: String,
    pub count: usize,
    pub summary: String,
    pub details: Vec<String>,
    pub outputs: Vec<abacus_types::ToolOutput>,
}

impl ToolAgentBatchResult {
    pub fn new(agent: &ToolAgentDef, details: Vec<String>, outputs: Vec<ToolOutput>) -> Self {
        let count = details.len();
        let summary = details.first().cloned().unwrap_or_default();
        Self {
            agent_id: agent.id.clone(),
            icon: agent.icon.clone(),
            count,
            summary,
            details: details.clone(),
            outputs,
        }
    }
}