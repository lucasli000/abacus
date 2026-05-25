//! Tool Cluster Registry —— 工具协议同构感知层（段 J1）
//!
//! ## 设计动机
//! LLM 看到平铺工具列表会在"语义近邻工具"间犹豫：cross_session_query /
//! session_resume_query / messages_recover 都涉及"过去内容"，description
//! 关键词撞车 → LLM 凭经验赌一个，常误选。本模块把"工具间的横向关系"显式化：
//! 每个工具属于一个 cluster；同 cluster 的工具共享一个 `purpose`（业务目的）；
//! 每个 member 有 `differentiator`（它与同簇其他工具的关键区分点）。
//! LLM 从 description 中可见这层结构，选工具时按"先认 cluster 再选 differentiator"
//! 的二级决策，命中率显著提升。
//!
//! ## 协议同构原则
//! 对应 CLAUDE.md 的 `feedback_protocol_homomorphism.md`——多协议路径能力对等
//! 则不分叉。这里把这条原则从代码层提到 LLM 感知层：能力近邻的工具不应让 LLM
//! 在无信息状态下盲选。
//!
//! ## 引用关系
//! - 上游：CoreLoop::build_tool_definitions_for 拿 cluster info 注入 description
//! - 下游：硬编码 builtin clusters；MCP/Plugin 工具默认无 cluster
//! - 配套：tool_compass 工具（段 J2）通过 ClusterRegistry::recommend_by_intent
//!   给 LLM 主动咨询入口
//!
//! ## 生命周期
//! - 创建：进程启动时 `ClusterRegistry::builtin()` 一次性构造（纯静态数据）
//! - 销毁：进程退出时随 CoreLoop drop
//! - 不持有锁——所有数据在 builtin() 时已 frozen
//!
//! ## 失败语义
//! - 工具不在任何 cluster → render_hint_for 返 None（不破坏现有行为）
//! - 同一 tool_id 注册到多 cluster → builtin() 配置阶段 panic（防误配）

use std::collections::HashMap;
use serde::Serialize;

/// 单个工具在所属 cluster 内的角色描述
///
/// `differentiator` 是关键——告诉 LLM 这个工具与同簇其他工具的**对比维度**，
/// 不是 description 的复述。
#[derive(Debug, Clone)]
pub struct ToolMember {
    pub tool_id: &'static str,
    pub differentiator: &'static str,
}

/// 工具协议同构簇——一组语义近邻工具
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
    /// 关键词命中数——越大越相关；调用方可按此再排
    pub relevance_score: usize,
}

/// Cluster 注册表
#[derive(Debug, Clone)]
pub struct ClusterRegistry {
    clusters: Vec<ToolCluster>,
    /// tool_id → cluster index（fast lookup）
    tool_index: HashMap<String, usize>,
}

impl ClusterRegistry {
    /// 空 registry——单元测试 / 自定义场景用
    pub fn empty() -> Self {
        Self {
            clusters: Vec::new(),
            tool_index: HashMap::new(),
        }
    }

    /// builtin 内置 clusters
    ///
    /// ## 划分原则
    /// 1. 同一 cluster 工具语义有交集（LLM 容易混选）
    /// 2. cluster 内 ≥2 个工具（单工具不需要 differentiator）
    /// 3. differentiator 句式："vs them, this tool focuses on X"
    /// 4. purpose 是业务目的不是技术细节
    pub fn builtin() -> Self {
        let mut me = Self::empty();

        // session_history：跨/历史 session 内容查询——三件套最容易混
        me.register(ToolCluster {
            id: "session_history",
            purpose: "Access content/state from past or current sessions",
            members: vec![
                ToolMember {
                    tool_id: "cross_session_query",
                    differentiator: "semantic content retrieval (knowledge palace) across all past sessions",
                },
                ToolMember {
                    tool_id: "session_resume_query",
                    differentiator: "metadata stats (turn/tool counts, latency, compression) of prior sessions",
                },
                ToolMember {
                    tool_id: "messages_recover",
                    differentiator: "verbatim recall of compressed messages by recover_id",
                },
            ],
        });

        // interaction_checkpoint：当前 session 内导航
        me.register(ToolCluster {
            id: "interaction_checkpoint",
            purpose: "Navigate the current session's interaction map (checkpoints/path)",
            members: vec![
                ToolMember {
                    tool_id: "interaction_status",
                    differentiator: "current position only (lightest read)",
                },
                ToolMember {
                    tool_id: "interaction_path",
                    differentiator: "full path: completed + current + remaining",
                },
                ToolMember {
                    tool_id: "interaction_recall",
                    differentiator: "deep details of one specific past checkpoint by id",
                },
                ToolMember {
                    tool_id: "interaction_mark",
                    differentiator: "WRITE — manually create a new checkpoint",
                },
            ],
        });

        // session_meta：session 级元操作
        me.register(ToolCluster {
            id: "session_meta",
            purpose: "Session-level meta operations (focus/permission/introspect)",
            members: vec![
                ToolMember {
                    tool_id: "session_set_focus",
                    differentiator: "anchor goal/phase/constraints to prevent attention drift",
                },
                ToolMember {
                    tool_id: "session_request_permission",
                    differentiator: "request user authz for a permission-gated tool",
                },
                ToolMember {
                    tool_id: "magchain_status",
                    differentiator: "introspect MagChain hooks + epistemic + decay tier state",
                },
            ],
        });

        // knowledge_base：KB 检索——kb_query 与 kb_search 极易混淆
        me.register(ToolCluster {
            id: "knowledge_base",
            purpose: "Retrieve from the knowledge base (KB chunks + memory palace)",
            members: vec![
                ToolMember {
                    tool_id: "kb_ingest",
                    differentiator: "WRITE — index a file into KB; precondition for kb_query/kb_search",
                },
                ToolMember {
                    tool_id: "kb_query",
                    differentiator: "KB chunks ONLY (BM25 + trigram); narrowest scope",
                },
                ToolMember {
                    tool_id: "kb_search",
                    differentiator: "multi-source: KB chunks + memory palace + spatial atoms (broadest scope)",
                },
            ],
        });

        // fs_read_discover：文件系统读 / 发现类——8 个工具，差异点最容易模糊
        me.register(ToolCluster {
            id: "fs_read_discover",
            purpose: "Read files or discover filesystem entries",
            members: vec![
                ToolMember {
                    tool_id: "fs_read",
                    differentiator: "read full file contents (text)",
                },
                ToolMember {
                    tool_id: "fs_info",
                    differentiator: "metadata only (size/mtime/perms) — no content",
                },
                ToolMember {
                    tool_id: "fs_search",
                    differentiator: "find files by glob pattern (path-only)",
                },
                ToolMember {
                    tool_id: "fs_grep",
                    differentiator: "find content by regex INSIDE files",
                },
                ToolMember {
                    tool_id: "fs_ls",
                    differentiator: "list one directory's direct children (single-level)",
                },
                ToolMember {
                    tool_id: "fs_tree",
                    differentiator: "recursive tree view of a directory",
                },
            ],
        });

        // fs_write：文件系统写——破坏性，必须区分清楚
        me.register(ToolCluster {
            id: "fs_write",
            purpose: "Modify files (DESTRUCTIVE — verify intent before calling)",
            members: vec![
                ToolMember {
                    tool_id: "fs_write",
                    differentiator: "create new OR fully overwrite existing file",
                },
                ToolMember {
                    tool_id: "fs_edit",
                    differentiator: "precise text replacement (preferred for existing files)",
                },
                ToolMember {
                    tool_id: "fs_move",
                    differentiator: "rename or relocate; fails if dest exists",
                },
                ToolMember {
                    tool_id: "fs_mkdir",
                    differentiator: "create directories (incl nested); not file content",
                },
            ],
        });

        // db_read：数据库读类
        me.register(ToolCluster {
            id: "db_read",
            purpose: "Read from SQLite databases (no side effects)",
            members: vec![
                ToolMember {
                    tool_id: "db_info",
                    differentiator: "DB-level meta (path/size/table count)",
                },
                ToolMember {
                    tool_id: "db_list_tables",
                    differentiator: "list user table names",
                },
                ToolMember {
                    tool_id: "db_table_schema",
                    differentiator: "column schema of one specific table",
                },
                ToolMember {
                    tool_id: "db_query",
                    differentiator: "raw parameterized SQL (max flexibility, no safety rails)",
                },
                ToolMember {
                    tool_id: "db_read_records",
                    differentiator: "structured equality-filter row read (safer than db_query)",
                },
            ],
        });

        // web_io：网络
        me.register(ToolCluster {
            id: "web_io",
            purpose: "Fetch from the live web",
            members: vec![
                ToolMember {
                    tool_id: "web_fetch",
                    differentiator: "fetch a specific URL by exact address",
                },
                ToolMember {
                    tool_id: "web_search",
                    differentiator: "search engine query when URL is unknown",
                },
            ],
        });

        me
    }

    /// 注册一个 cluster——同名 tool_id 重复时 warn + 覆盖（不再 panic 崩溃启动）
    ///
    /// ## 旧行为
    /// panic → 开发者配置错误直接崩溃进程，阻塞启动
    ///
    /// ## 新行为
    /// warn log + 覆盖到新 cluster（最后注册的赢）
    /// 生产安全：启动不被配置漂移阻塞；CI 通过 lint audit 捕获重复
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

    /// 渲染拼到 description 末尾的 cluster hint
    ///
    /// 形如：
    /// `\n[Cluster: session_history (Access content from past sessions). \
    ///  Siblings: session_resume_query (metadata stats); messages_recover (verbatim recall). \
    ///  This tool: semantic content retrieval (knowledge palace) across all past sessions]`
    ///
    /// ## byte-stable
    /// 同一组工具集 + 同一 ClusterRegistry 输出 byte-identical——不破 KV cache 前缀。
    pub fn render_hint_for(&self, tool_id: &str) -> Option<String> {
        let cluster = self.cluster_for(tool_id)?;
        let me = cluster.members.iter().find(|m| m.tool_id == tool_id)?;
        let siblings = cluster.siblings_of(tool_id);
        if siblings.is_empty() {
            // 单工具 cluster 不需要对比——LLM 不会混淆
            return None;
        }
        let sibling_str: Vec<String> = siblings
            .iter()
            .map(|m| format!("{} ({})", m.tool_id, m.differentiator))
            .collect();
        Some(format!(
            "\n[Cluster: {} ({}). Siblings: {}. This tool: {}]",
            cluster.id,
            cluster.purpose,
            sibling_str.join("; "),
            me.differentiator
        ))
    }

    /// 列所有 clusters（段 J2 tool_compass 用）
    pub fn all_clusters(&self) -> &[ToolCluster] {
        &self.clusters
    }

    /// 工具数（统计/audit 用）
    pub fn tool_count(&self) -> usize {
        self.tool_index.len()
    }

    /// cluster 数
    pub fn cluster_count(&self) -> usize {
        self.clusters.len()
    }

    /// 按 intent 关键词命中推荐工具（段 J2 tool_compass 后端）
    ///
    /// ## 算法
    /// 1. intent 切词（>2 字符）；中英文混合下用 unicode_segmentation 略过——简化为
    ///    `split_whitespace` + 显式逗号/标点分隔
    /// 2. 对每个 cluster.purpose + member.differentiator + tool_id 拼接为 haystack
    /// 3. 计算每词命中次数；按命中数降序排，取 top_k
    /// 4. 同分的按 tool_id 字典序稳定排（保证 deterministic 输出）
    ///
    /// ## 失败语义
    /// 无命中 → 返回空 vec；调用方可降级为"列所有 clusters 让 LLM 自选"
    pub fn recommend_by_intent(&self, intent: &str, top_k: usize) -> Vec<RecommendItem> {
        let intent_lower = intent.to_lowercase();
        // 简单切词：空白 + 中文标点 + 半角标点
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
        // 主键 hits 降序，副键 tool_id 升序——deterministic
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
    fn builtin_registry_loads_all_clusters() {
        let r = ClusterRegistry::builtin();
        // 至少 8 个 cluster（与 builtin 列表对齐）
        assert!(r.cluster_count() >= 8, "应注册至少 8 个 builtin cluster");
        assert!(r.tool_count() >= 25, "应注册至少 25 个工具到 cluster");
    }

    #[test]
    fn cluster_for_session_history_tools() {
        let r = ClusterRegistry::builtin();
        for t in ["cross_session_query", "session_resume_query", "messages_recover"] {
            let c = r.cluster_for(t).unwrap_or_else(|| panic!("{t} 应在 cluster"));
            assert_eq!(c.id, "session_history");
        }
    }

    #[test]
    fn render_hint_includes_siblings_and_self_diff() {
        let r = ClusterRegistry::builtin();
        let hint = r
            .render_hint_for("cross_session_query")
            .expect("应有 hint");
        assert!(hint.contains("session_history"));
        assert!(hint.contains("session_resume_query"));
        assert!(hint.contains("messages_recover"));
        assert!(hint.contains("This tool:"));
        // 不应包含自己 tool_id 在 siblings 里
        let siblings_part = hint.split("This tool:").next().unwrap();
        assert!(
            !siblings_part.contains("cross_session_query ("),
            "siblings 不应含自己: {hint}"
        );
    }

    #[test]
    fn render_hint_none_for_unknown_tool() {
        let r = ClusterRegistry::builtin();
        assert!(r.render_hint_for("unknown_xyz_tool").is_none());
    }

    #[test]
    fn recommend_matches_intent_keywords() {
        let r = ClusterRegistry::builtin();
        // "search past sessions" → session_history cluster
        let recs = r.recommend_by_intent("retrieve content from past sessions", 3);
        assert!(!recs.is_empty(), "应至少 1 个推荐");
        let top = &recs[0];
        assert!(
            top.cluster_id == "session_history" || top.tool_id.contains("session"),
            "top 应命中 session 相关, got: {:?}",
            top
        );
    }

    #[test]
    fn recommend_empty_when_no_keywords() {
        let r = ClusterRegistry::builtin();
        let recs = r.recommend_by_intent("a b c", 5); // 全是单字符词，被切词过滤
        assert!(recs.is_empty(), "短词应被过滤导致无命中");
    }

    #[test]
    fn recommend_top_k_respected() {
        let r = ClusterRegistry::builtin();
        let recs = r.recommend_by_intent("read file content from disk", 2);
        assert!(recs.len() <= 2, "top_k=2 应最多返 2 项");
    }

    #[test]
    fn duplicate_tool_in_clusters_overwrites() {
        // 重复 tool_id 不再 panic，改为 warn + 覆盖到新 cluster
        let mut r = ClusterRegistry::empty();
        r.register(ToolCluster {
            id: "c1",
            purpose: "x",
            members: vec![ToolMember {
                tool_id: "shared_tool",
                differentiator: "first",
            }],
        });
        r.register(ToolCluster {
            id: "c2",
            purpose: "y",
            members: vec![ToolMember {
                tool_id: "shared_tool",
                differentiator: "second",
            }],
        });
        // 最后注册的 cluster 赢
        let c = r.cluster_for("shared_tool").unwrap();
        assert_eq!(c.id, "c2");
    }

    #[test]
    fn single_member_cluster_renders_no_hint() {
        // 单成员 cluster 没有 sibling，render_hint_for 应返 None
        let mut r = ClusterRegistry::empty();
        r.register(ToolCluster {
            id: "lonely",
            purpose: "single tool",
            members: vec![ToolMember {
                tool_id: "alone",
                differentiator: "the only one",
            }],
        });
        assert!(r.render_hint_for("alone").is_none());
    }

    #[test]
    fn siblings_of_excludes_self() {
        let r = ClusterRegistry::builtin();
        let cluster = r.cluster_for("kb_query").unwrap();
        let sibs = cluster.siblings_of("kb_query");
        assert!(sibs.iter().all(|m| m.tool_id != "kb_query"));
        assert!(sibs.len() >= 2); // kb_ingest + kb_search
    }
}
