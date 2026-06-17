//! Tool Cluster Registry —— 工具协议同构感知层（段 J1）
//!
//! ## 设计动机
//! LLM 看到平铺工具列表会在"语义近邻工具"间犹豫。本模块把"工具间的横向关系"显式化：
//! 每个工具属于一个 cluster；同 cluster 的工具共享一个 `purpose`；
//! 每个 member 有 `differentiator`（与同簇其他工具的关键区分点）。
//!
//! ## Cluster vs ToolAgent 边界
//! - **Cluster**: LLM 工具选择辅助。静态配置，注入到 tool description。
//! - **ToolAgent**: 批量执行优化。运行时匹配，委托给隔离 sub-session。
//! - ToolAgent 通过 `cluster_refs` 引用 Cluster 组 ID，不重复维护工具列表。
//!
//! ## Token 优化
//! 每个 tool description 追加 cluster hint：
//! ```text
//! [g:fs_read | vs: fs_info/fs_search/fs_grep/fs_ls/fs_tree | this: read file contents]
//! ```
//! 约 30-50 tokens/tool（旧格式 ~100+）。
//!
//! ## 引用关系
//! - 上游：CoreLoop::build_tool_definitions_for 拿 cluster info 注入 description
//! - 下游：硬编码 builtin clusters；MCP/Plugin 工具默认无 cluster
//! - 配套：tool_compass 工具（段 J2）通过 ClusterRegistry::recommend_by_intent

use std::collections::HashMap;
use serde::Serialize;

/// 单个工具在所属 cluster 内的角色描述
#[derive(Debug, Clone)]
pub struct ToolMember {
    pub tool_id: &'static str,
    pub differentiator: &'static str,
}

/// 工具协议同构簇
#[derive(Debug, Clone)]
pub struct ToolCluster {
    /// 簇的唯一标识（小写下划线）
    pub id: &'static str,
    /// 簇的总体目的（一句话）
    pub purpose: &'static str,
    /// 成员——顺序无关
    pub members: Vec<ToolMember>,
}

impl ToolCluster {
    /// 列出除自己外的兄弟工具
    pub fn siblings_of(&self, tool_id: &str) -> Vec<&ToolMember> {
        self.members
            .iter()
            .filter(|m| m.tool_id != tool_id)
            .collect()
    }
}

/// 工具推荐结果（段 J2 tool_compass 用）
#[derive(Debug, Clone, Serialize)]
pub struct RecommendItem {
    pub cluster_id: String,
    pub tool_id: String,
    pub differentiator: String,
    pub relevance_score: usize,
}

/// Cluster 注册表
#[derive(Debug, Clone)]
pub struct ClusterRegistry {
    clusters: Vec<ToolCluster>,
    tool_index: HashMap<String, usize>,
}

impl ClusterRegistry {
    pub fn empty() -> Self {
        Self {
            clusters: Vec::new(),
            tool_index: HashMap::new(),
        }
    }

    /// builtin 内置 clusters — 覆盖全部 ~70 个注册工具
    ///
    /// ## 划分原则
    /// 1. 同一 cluster 工具语义有交集（LLM 容易混选）
    /// 2. cluster 内 ≥2 个工具时提供 differentiator
    /// 3. differentiator 句式：简短区分点（不是 description 复述）
    /// 4. purpose 是业务目的不是技术细节
    /// 5. 单工具 cluster 保留结构（供 ToolAgent 引用）
    pub fn builtin() -> Self {
        let mut me = Self::empty();

        // ─── 文件系统读 ───────────────────────────────────────
        me.register(ToolCluster {
            id: "fs_read",
            purpose: "Read files or discover filesystem entries",
            members: vec![
                ToolMember { tool_id: "fs_read", differentiator: "full file contents" },
                ToolMember { tool_id: "fs_info", differentiator: "metadata only (size/mtime/perms)" },
                ToolMember { tool_id: "fs_search", differentiator: "glob pattern → file paths" },
                ToolMember { tool_id: "fs_grep", differentiator: "regex search inside files" },
                ToolMember { tool_id: "fs_ls", differentiator: "single directory listing" },
                ToolMember { tool_id: "fs_tree", differentiator: "recursive tree view" },
            ],
        });

        // ─── 文件系统写 ───────────────────────────────────────
        me.register(ToolCluster {
            id: "fs_write",
            purpose: "Modify files (DESTRUCTIVE)",
            members: vec![
                ToolMember { tool_id: "fs_write", differentiator: "create/overwrite entire file" },
                ToolMember { tool_id: "fs_edit", differentiator: "precise text replacement" },
                ToolMember { tool_id: "fs_move", differentiator: "rename/relocate" },
                ToolMember { tool_id: "fs_mkdir", differentiator: "create directories" },
            ],
        });

        // ─── Shell ────────────────────────────────────────────
        me.register(ToolCluster {
            id: "shell",
            purpose: "Execute shell commands",
            members: vec![
                ToolMember { tool_id: "bash_exec", differentiator: "shell command execution" },
            ],
        });

        // ─── Web I/O ──────────────────────────────────────────
        me.register(ToolCluster {
            id: "web_io",
            purpose: "Fetch from the live web",
            members: vec![
                ToolMember { tool_id: "web_fetch", differentiator: "fetch by exact URL" },
                ToolMember { tool_id: "web_search", differentiator: "search engine query" },
                ToolMember { tool_id: "http_request", differentiator: "full HTTP client (any method)" },
            ],
        });

        // ─── 数据处理 ─────────────────────────────────────────
        me.register(ToolCluster {
            id: "data_proc",
            purpose: "Transform or compare data",
            members: vec![
                ToolMember { tool_id: "json_process", differentiator: "JSON query/transform" },
                ToolMember { tool_id: "diff", differentiator: "file/text diff comparison" },
            ],
        });

        // ─── 数据库读 ─────────────────────────────────────────
        me.register(ToolCluster {
            id: "db_read",
            purpose: "Read from databases (no side effects)",
            members: vec![
                ToolMember { tool_id: "db_info", differentiator: "DB-level metadata" },
                ToolMember { tool_id: "db_list_tables", differentiator: "list table names" },
                ToolMember { tool_id: "db_table_schema", differentiator: "column schema of one table" },
                ToolMember { tool_id: "db_query", differentiator: "raw parameterized SQL" },
                ToolMember { tool_id: "db_read_records", differentiator: "structured filtered read" },
            ],
        });

        // ─── 数据库写 ─────────────────────────────────────────
        me.register(ToolCluster {
            id: "db_write",
            purpose: "Modify database records (DESTRUCTIVE)",
            members: vec![
                ToolMember { tool_id: "db_create_record", differentiator: "insert new record" },
                ToolMember { tool_id: "db_update_records", differentiator: "conditional update" },
                ToolMember { tool_id: "db_delete_records", differentiator: "conditional delete" },
            ],
        });

        // ─── 知识库 ───────────────────────────────────────────
        me.register(ToolCluster {
            id: "kb",
            purpose: "Knowledge base ingestion and retrieval",
            members: vec![
                ToolMember { tool_id: "kb_ingest", differentiator: "WRITE — index file into KB" },
                ToolMember { tool_id: "kb_query", differentiator: "KB chunks only (BM25)" },
                ToolMember { tool_id: "kb_search", differentiator: "multi-source: KB + palace" },
            ],
        });

        // ─── Git ──────────────────────────────────────────────
        me.register(ToolCluster {
            id: "git",
            purpose: "Git version control operations",
            members: vec![
                ToolMember { tool_id: "git_status", differentiator: "working tree status" },
                ToolMember { tool_id: "git_diff", differentiator: "diff analysis" },
                ToolMember { tool_id: "git_log", differentiator: "commit history" },
                ToolMember { tool_id: "git_blame", differentiator: "line attribution" },
                ToolMember { tool_id: "git_stash", differentiator: "stash management" },
                ToolMember { tool_id: "git_commit", differentiator: "commit changes" },
            ],
        });

        // ─── LSP ──────────────────────────────────────────────
        me.register(ToolCluster {
            id: "lsp",
            purpose: "Language server protocol operations",
            members: vec![
                ToolMember { tool_id: "lsp_goto_definition", differentiator: "jump to definition" },
                ToolMember { tool_id: "lsp_find_references", differentiator: "find all references" },
                ToolMember { tool_id: "lsp_hover", differentiator: "type info/docs at cursor" },
                ToolMember { tool_id: "lsp_document_symbol", differentiator: "symbols in one file" },
                ToolMember { tool_id: "lsp_workspace_symbol", differentiator: "search all symbols" },
                ToolMember { tool_id: "lsp_diagnostics", differentiator: "compiler errors/warnings" },
                ToolMember { tool_id: "lsp_goto_implementation", differentiator: "trait implementations" },
                ToolMember { tool_id: "lsp_call_hierarchy_incoming", differentiator: "upstream callers" },
                ToolMember { tool_id: "lsp_call_hierarchy_outgoing", differentiator: "downstream callees" },
            ],
        });

        // ─── Code Graph ───────────────────────────────────────
        me.register(ToolCluster {
            id: "cg",
            purpose: "Code graph indexing and querying",
            members: vec![
                ToolMember { tool_id: "cg_index", differentiator: "index code files" },
                ToolMember { tool_id: "cg_query", differentiator: "symbol search" },
                ToolMember { tool_id: "cg_graph", differentiator: "call graph traversal" },
                ToolMember { tool_id: "cg_analyze", differentiator: "structure analysis" },
            ],
        });

        // ─── Context 管理 ─────────────────────────────────────
        me.register(ToolCluster {
            id: "context",
            purpose: "Context window management and compression",
            members: vec![
                ToolMember { tool_id: "context_declare", differentiator: "declare intent to load" },
                ToolMember { tool_id: "context_keep", differentiator: "keep selected segments" },
                ToolMember { tool_id: "context_compress", differentiator: "compress context" },
                ToolMember { tool_id: "session_recall", differentiator: "search cold-tier archive" },
                ToolMember { tool_id: "context_pin", differentiator: "pin turn from compression" },
                ToolMember { tool_id: "context_unpin", differentiator: "unpin turn" },
                ToolMember { tool_id: "context_pinned", differentiator: "list pinned turns" },
                ToolMember { tool_id: "context_status", differentiator: "context window usage" },
            ],
        });

        // ─── Session 操作 ─────────────────────────────────────
        me.register(ToolCluster {
            id: "session",
            purpose: "Current session navigation and control",
            members: vec![
                ToolMember { tool_id: "interaction_status", differentiator: "current position" },
                ToolMember { tool_id: "interaction_path", differentiator: "full interaction path" },
                ToolMember { tool_id: "interaction_recall", differentiator: "recall checkpoint by id" },
                ToolMember { tool_id: "interaction_mark", differentiator: "create checkpoint" },
                ToolMember { tool_id: "session_set_focus", differentiator: "anchor goal/constraints" },
                ToolMember { tool_id: "session_request_permission", differentiator: "request user authz" },
                ToolMember { tool_id: "session_extend_timeout", differentiator: "extend turn timeout" },
            ],
        });

        // ─── 跨 Session 历史 ──────────────────────────────────
        me.register(ToolCluster {
            id: "session_history",
            purpose: "Access content from past sessions",
            members: vec![
                ToolMember { tool_id: "cross_session_query", differentiator: "semantic search across sessions" },
                ToolMember { tool_id: "session_resume_query", differentiator: "metadata stats of prior sessions" },
                ToolMember { tool_id: "messages_recover", differentiator: "verbatim compressed messages" },
            ],
        });

        // ─── 配置 ─────────────────────────────────────────────
        me.register(ToolCluster {
            id: "config",
            purpose: "Runtime configuration management",
            members: vec![
                ToolMember { tool_id: "config_get", differentiator: "read config value" },
                ToolMember { tool_id: "config_set", differentiator: "modify config value" },
            ],
        });

        // ─── 代码执行 ─────────────────────────────────────────
        me.register(ToolCluster {
            id: "code_exec",
            purpose: "Execute code in sandboxed environment",
            members: vec![
                ToolMember { tool_id: "code_execute", differentiator: "Rhai script execution" },
            ],
        });

        // ─── 编排 ─────────────────────────────────────────────
        me.register(ToolCluster {
            id: "orchestrate",
            purpose: "Task complexity assessment and escalation",
            members: vec![
                ToolMember { tool_id: "orchestrate_assess", differentiator: "evaluate task complexity" },
                ToolMember { tool_id: "orchestrate_upgrade", differentiator: "escalate execution level" },
            ],
        });

        // ─── 推理分析 ─────────────────────────────────────────
        me.register(ToolCluster {
            id: "reasoning",
            purpose: "Meta-analysis and deduction",
            members: vec![
                ToolMember { tool_id: "deduction_status", differentiator: "active deduction alerts" },
                ToolMember { tool_id: "deduction_analyze", differentiator: "run deep analysis" },
                ToolMember { tool_id: "magchain_status", differentiator: "hooks + epistemic state" },
            ],
        });

        // ─── 沙盒任务 ─────────────────────────────────────────
        me.register(ToolCluster {
            id: "sandbox",
            purpose: "Sandboxed task planning and execution",
            members: vec![
                ToolMember { tool_id: "task_plan", differentiator: "generate task plan" },
                ToolMember { tool_id: "task_run", differentiator: "execute confirmed plan" },
            ],
        });

        // ─── 元工具 ───────────────────────────────────────────
        me.register(ToolCluster {
            id: "meta",
            purpose: "System introspection and tool discovery",
            members: vec![
                ToolMember { tool_id: "env_status", differentiator: "local workspace snapshot" },
                ToolMember { tool_id: "tool_compass", differentiator: "tool recommendation by intent" },
                ToolMember { tool_id: "mode_switch", differentiator: "switch interaction mode" },
            ],
        });

        // ─── 结果展开 ─────────────────────────────────────────
        me.register(ToolCluster {
            id: "result",
            purpose: "Retrieve truncated results",
            members: vec![
                ToolMember { tool_id: "result_expand", differentiator: "expand truncated output" },
            ],
        });

        me
    }

    /// 注册一个 cluster——同名 tool_id 重复时 warn + 覆盖
    fn register(&mut self, cluster: ToolCluster) {
        let cluster_idx = self.clusters.len();
        for m in &cluster.members {
            if let Some(prev_idx) = self.tool_index.get(m.tool_id) {
                tracing::warn!(
                    "Tool '{}' re-registered: cluster '{}' → '{}' (overwriting)",
                    m.tool_id, self.clusters[*prev_idx].id, cluster.id
                );
            }
            self.tool_index
                .insert(m.tool_id.to_string(), cluster_idx);
        }
        self.clusters.push(cluster);
    }

    /// 查工具所属 cluster
    pub fn cluster_for(&self, tool_id: &str) -> Option<&ToolCluster> {
        self.tool_index.get(tool_id).map(|&i| &self.clusters[i])
    }

    /// 渲染 cluster hint——追加到 tool description 末尾
    ///
    /// 格式: `[g:{cluster} | vs: {siblings} | this: {differentiator}]`
    /// 单成员 cluster 返回 None（无 sibling 不需要 differentiator）
    pub fn render_hint_for(&self, tool_id: &str) -> Option<String> {
        let cluster = self.cluster_for(tool_id)?;
        let me = cluster.members.iter().find(|m| m.tool_id == tool_id)?;
        let siblings = cluster.siblings_of(tool_id);
        if siblings.is_empty() {
            return None;
        }
        let names: Vec<&str> = siblings.iter().map(|m| m.tool_id).collect();
        Some(format!(
            " [g:{} | vs: {} | this: {}]",
            cluster.id,
            names.join("/"),
            me.differentiator
        ))
    }

    /// 列所有 clusters
    pub fn all_clusters(&self) -> &[ToolCluster] {
        &self.clusters
    }

    /// 工具数
    pub fn tool_count(&self) -> usize {
        self.tool_index.len()
    }

    /// cluster 数
    pub fn cluster_count(&self) -> usize {
        self.clusters.len()
    }

    /// 按 intent 关键词命中推荐工具（段 J2 tool_compass 后端）
    pub fn recommend_by_intent(&self, intent: &str, top_k: usize) -> Vec<RecommendItem> {
        let intent_lower = intent.to_lowercase();
        let intent_words: Vec<String> = intent_lower
            .split(|c: char| c.is_whitespace() || ",.;:!?，。；：！？\"'()[]{}".contains(c))
            .filter(|w| w.chars().count() > 2)
            .map(|w| w.to_string())
            .collect();

        if intent_words.is_empty() {
            return Vec::new();
        }

        let mut scored: Vec<(usize, &ToolCluster, &ToolMember)> = Vec::new();
        for cluster in &self.clusters {
            for member in &cluster.members {
                let haystack = format!(
                    "{} {} {}",
                    cluster.purpose.to_lowercase(),
                    member.differentiator.to_lowercase(),
                    member.tool_id.to_lowercase()
                );
                let hits: usize = intent_words
                    .iter()
                    .filter(|w| haystack.contains(w.as_str()))
                    .count();
                if hits > 0 {
                    scored.push((hits, cluster, member));
                }
            }
        }
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.2.tool_id.cmp(b.2.tool_id)));
        scored
            .into_iter()
            .take(top_k)
            .map(|(score, cluster, member)| RecommendItem {
                cluster_id: cluster.id.to_string(),
                tool_id: member.tool_id.to_string(),
                differentiator: member.differentiator.to_string(),
                relevance_score: score,
            })
            .collect()
    }
}

impl Default for ClusterRegistry {
    fn default() -> Self {
        Self::builtin()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_covers_all_tools() {
        let r = ClusterRegistry::builtin();
        assert!(r.cluster_count() >= 21, "应有 21 个 cluster, 实际 {}", r.cluster_count());
        assert!(r.tool_count() >= 70, "应覆盖 70+ 工具, 实际 {}", r.tool_count());
    }

    #[test]
    fn every_cluster_has_members() {
        let r = ClusterRegistry::builtin();
        for c in r.all_clusters() {
            assert!(!c.members.is_empty(), "cluster '{}' 无成员", c.id);
        }
    }

    #[test]
    fn no_duplicate_tool_ids() {
        let r = ClusterRegistry::builtin();
        let mut seen = std::collections::HashSet::new();
        for c in r.all_clusters() {
            for m in &c.members {
                assert!(seen.insert(m.tool_id), "工具 '{}' 重复注册", m.tool_id);
            }
        }
    }

    #[test]
    fn render_hint_format() {
        let r = ClusterRegistry::builtin();
        let hint = r.render_hint_for("fs_read").expect("应有 hint");
        assert!(hint.starts_with(" [g:fs_read | vs:"));
        assert!(hint.contains("this: full file contents"));
        // 不含自己在 siblings 中
        assert!(!hint.contains("vs: fs_read/"));
    }

    #[test]
    fn single_member_no_hint() {
        let r = ClusterRegistry::builtin();
        // bash_exec 是 shell cluster 唯一成员
        assert!(r.render_hint_for("bash_exec").is_none());
    }

    #[test]
    fn recommend_matches_intent() {
        let r = ClusterRegistry::builtin();
        let recs = r.recommend_by_intent("read file contents from disk", 3);
        assert!(!recs.is_empty());
        assert!(recs.iter().any(|r| r.tool_id == "fs_read"));
    }

    #[test]
    fn recommend_git_tools() {
        let r = ClusterRegistry::builtin();
        let recs = r.recommend_by_intent("show git commit history", 3);
        assert!(recs.iter().any(|r| r.tool_id == "git_log"));
    }

    #[test]
    fn recommend_lsp_tools() {
        let r = ClusterRegistry::builtin();
        let recs = r.recommend_by_intent("find all references to this function", 3);
        assert!(recs.iter().any(|r| r.tool_id == "lsp_find_references"));
    }
}
