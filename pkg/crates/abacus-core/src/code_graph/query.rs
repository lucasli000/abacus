//! CodeGraph Query Engine — FTS5 搜索 + 图遍历 + 符号查找
//!
//! ## 职责
//! 提供 CodeGraph 的只读查询能力：
//! 1. **FTS5 符号搜索** — trigram tokenizer 模糊匹配符号名/签名/文档
//! 2. **图遍历** — BFS 遍历 call_graph/dep_graph（支持正向/反向）
//! 3. **符号查找** — 按 symbol_id 精确获取符号详情
//!
//! ## 依赖 (external)
//! - `rusqlite`: 共享 knowledge.db 连接（读路径）
//! - `tokio::sync::Mutex`: 异步互斥锁（跨 await point 安全）
//! - `serde_json`: 结构化结果序列化
//!
//! ## 依赖 (internal)
//! - `super::{Symbol, SymbolKind, CgDegradation, Visibility}`: 公共类型定义
//!
//! ## 引用关系
//! - 被 `CodeGraphManager::query()` 暴露给外部
//! - 被 `tool/builtin/cg.rs` 的 `cg_search` / `cg_graph` / `cg_symbol` 命令调用
//! - 被 `AnalyzeEngine` 内部调用（impact 分析时查找符号）
//!
//! ## 生命周期
//! - 创建：`CodeGraphManager::new()` 时构造
//! - 存活：与 `CodeGraphManager` 同生命周期（Arc 共享）
//! - 销毁：最后一个 Arc 引用 drop 时销毁（无需显式清理）
//!
//! ## 设计决策
//! - FTS5 content-sync 模式：查询 cg_symbols_fts 后 JOIN cg_symbols 获取完整字段
//! - BFS 用 VecDeque 实现，visited set 防止环路无限遍历
//! - 所有查询方法返回 `CgDegradation` 信号，让调用方感知数据新鲜度

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;

use rusqlite::Connection;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use super::{CgDegradation, Symbol, SymbolKind, Visibility};

// ─── 公共类型 ──────────────────────────────────────────────────────────────

/// 图遍历方向
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphDirection {
    /// 查找调用目标符号的所有调用者（call_graph 反向）
    Callers,
    /// 查找目标符号调用的所有被调用者（call_graph 正向）
    Callees,
    /// 查找目标文件依赖的所有文件（dep_graph 正向）
    Deps,
    /// 查找依赖目标文件的所有文件（dep_graph 反向）
    ReverseDeps,
}

impl GraphDirection {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Callers => "callers",
            Self::Callees => "callees",
            Self::Deps => "deps",
            Self::ReverseDeps => "rdeps",
        }
    }

    /// 从字符串解析方向（兼容 CLI 输入）
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "callers" | "caller" => Some(Self::Callers),
            "callees" | "callee" => Some(Self::Callees),
            "deps" | "dependencies" => Some(Self::Deps),
            "rdeps" | "reverse_deps" | "reverse-deps" => Some(Self::ReverseDeps),
            _ => None,
        }
    }
}

/// FTS5 搜索结果条目
#[derive(Debug, Clone, serde::Serialize)]
pub struct SymbolResult {
    /// 符号 ID（sha1 hash）
    pub symbol_id: String,
    /// 符号名
    pub name: String,
    /// 符号类型
    pub kind: String,
    /// 所在文件（相对路径）
    pub file: String,
    /// 起始行号
    pub line: u32,
    /// 签名（如有）
    pub signature: Option<String>,
    /// FTS5 匹配分数（rank，负数越小越相关）
    pub score: f64,
}

/// 图遍历结果
#[derive(Debug, Clone, serde::Serialize)]
pub struct GraphResult {
    /// 遍历起点
    pub root: String,
    /// 遍历方向
    pub direction: String,
    /// 请求深度
    pub max_depth: u32,
    /// 遍历到的节点列表（含深度信息）
    pub nodes: Vec<GraphNode>,
    /// 遍历到的边列表
    pub edges: Vec<GraphEdge>,
    /// 数据质量信号
    pub degradation: CgDegradation,
}

/// 图中的节点
#[derive(Debug, Clone, serde::Serialize)]
pub struct GraphNode {
    /// 节点标识（symbol_id 或 file path）
    pub id: String,
    /// 显示标签（符号名或文件名）
    pub label: String,
    /// 距离起点的深度
    pub depth: u32,
    /// 节点类型（"symbol" 或 "file"）
    pub node_type: String,
}

/// 图中的边
#[derive(Debug, Clone, serde::Serialize)]
pub struct GraphEdge {
    /// 源节点 ID
    pub from: String,
    /// 目标节点 ID
    pub to: String,
    /// 边类型（"call", "dep" 等）
    pub edge_type: String,
}

// ─── QueryEngine ──────────────────────────────────────────────────────────

/// CodeGraph 查询引擎
///
/// 提供符号搜索、图遍历、符号精确查找三大能力。
/// 所有方法都是只读操作，不修改数据库。
///
/// ## 线程安全
/// 通过 `Arc<Mutex<Connection>>` 保证并发安全。
/// Mutex 持有时间极短（单次查询），不会造成显著争用。
///
/// ## 引用关系
/// - 持有 `Arc<Mutex<Connection>>` — 共享 knowledge.db 连接
/// - 被 `CodeGraphManager` 持有（Arc 包装）
/// - 被 `AnalyzeEngine` 间接使用（通过 DB 共享）
pub struct QueryEngine {
    /// 共享 knowledge.db 连接
    ///
    /// 生命周期：与 CodeGraphManager 共享同一连接
    /// 销毁：最后一个 Arc drop 时关闭连接
    db: Arc<Mutex<Connection>>,
}

impl QueryEngine {
    /// 创建 QueryEngine 实例
    ///
    /// ## 参数
    /// - `db`: 共享 knowledge.db 连接（已完成 schema migration）
    ///
    /// ## 前置条件
    /// - cg_* 表已通过 `schema::ensure_tables()` 创建
    pub fn new(db: Arc<Mutex<Connection>>) -> Self {
        Self { db }
    }

    /// 快速检查是否有任何已索引符号
    ///
    /// 用于判断 CodeGraph 是否可用（未索引时跳过查询路径）。
    /// 使用 `EXISTS` 子查询，O(1) 性能。
    ///
    /// ## 返回
    /// - `true`: 至少有一个符号已索引
    /// - `false`: 无符号（未索引或索引为空）
    pub async fn has_any_symbols(&self) -> bool {
        let conn = self.db.lock().await;
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM cg_symbols LIMIT 1)",
            [],
            |row| row.get::<_, bool>(0),
        )
        .unwrap_or(false)
    }

    /// FTS5 符号搜索
    ///
    /// 使用 trigram tokenizer 进行模糊匹配，支持子串搜索。
    /// 结果按 FTS5 rank 排序（相关性从高到低）。
    ///
    /// ## 参数
    /// - `query`: 搜索词（支持 FTS5 语法：AND/OR/NOT/前缀*）
    /// - `kind_filter`: 可选符号类型过滤（如 "function", "struct"）
    /// - `file_filter`: 可选文件路径前缀过滤
    /// - `limit`: 最大返回数量（默认 20，上限 100）
    ///
    /// ## 返回
    /// `(Vec<SymbolResult>, CgDegradation)` — 结果集 + 数据质量信号
    ///
    /// ## FTS5 查询语法
    /// - 普通词: `foo` → 匹配包含 "foo" 的符号
    /// - 前缀: `proc*` → 匹配以 "proc" 开头的
    /// - 短语: `"process turn"` → 匹配完整短语
    /// - 布尔: `foo AND bar`, `foo OR bar`, `foo NOT bar`
    pub async fn search_symbols(
        &self,
        query: &str,
        kind_filter: Option<&str>,
        file_filter: Option<&str>,
        limit: u32,
    ) -> (Vec<SymbolResult>, CgDegradation) {
        let effective_limit = limit.min(100).max(1);
        let conn = self.db.lock().await;

        // 检查是否有索引数据
        let has_data = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM cg_symbols LIMIT 1)",
                [],
                |row| row.get::<_, bool>(0),
            )
            .unwrap_or(false);

        if !has_data {
            return (Vec::new(), CgDegradation::NoIndex);
        }

        // 构建 FTS5 查询
        // 对于 trigram tokenizer，直接用原始查询即可（无需分词处理）
        let sanitized_query = sanitize_fts5_query(query);
        if sanitized_query.is_empty() {
            return (Vec::new(), CgDegradation::Normal);
        }

        // 基础 SQL：FTS5 搜索 + JOIN 主表获取完整信息
        // content-sync 模式下需要 JOIN cg_symbols 拿详细字段
        let mut sql = String::from(
            "SELECT s.symbol_id, s.name, s.kind, s.file, s.line, s.signature, \
             fts.rank \
             FROM cg_symbols_fts AS fts \
             JOIN cg_symbols AS s ON s.rowid = fts.rowid \
             WHERE cg_symbols_fts MATCH ?1"
        );

        // 动态追加过滤条件
        if kind_filter.is_some() {
            sql.push_str(" AND s.kind = ?2");
        }
        if file_filter.is_some() {
            let param_idx = if kind_filter.is_some() { "?3" } else { "?2" };
            sql.push_str(&format!(" AND s.file LIKE {param_idx}"));
        }

        sql.push_str(" ORDER BY fts.rank LIMIT ?");
        // 最后一个参数位置
        let limit_param_idx = match (kind_filter.is_some(), file_filter.is_some()) {
            (true, true) => 4,
            (true, false) | (false, true) => 3,
            (false, false) => 2,
        };
        // 重写 SQL 末尾的 ? 为正确的参数索引
        let sql = sql.replacen(
            "LIMIT ?",
            &format!("LIMIT ?{limit_param_idx}"),
            1,
        );

        let results = self.execute_fts_query(
            &conn,
            &sql,
            &sanitized_query,
            kind_filter,
            file_filter,
            effective_limit,
        );

        let degradation = self.check_degradation(&conn);
        (results, degradation)
    }

    /// 执行 FTS5 查询并解析结果
    ///
    /// 内部辅助方法，负责参数绑定和结果集映射。
    fn execute_fts_query(
        &self,
        conn: &Connection,
        sql: &str,
        query: &str,
        kind_filter: Option<&str>,
        file_filter: Option<&str>,
        limit: u32,
    ) -> Vec<SymbolResult> {
        // 使用动态参数绑定
        let file_prefix = file_filter.map(|f| format!("{f}%"));

        let result = match (kind_filter, file_filter) {
            (Some(kind), Some(_)) => {
                let mut stmt = match conn.prepare(sql) {
                    Ok(s) => s,
                    Err(_) => return Vec::new(),
                };
                stmt.query_map(
                    rusqlite::params![query, kind, file_prefix.as_deref().unwrap_or(""), limit],
                    Self::map_symbol_result,
                )
                .map(|rows| rows.filter_map(|r| r.ok()).collect())
                .unwrap_or_default()
            }
            (Some(kind), None) => {
                let mut stmt = match conn.prepare(sql) {
                    Ok(s) => s,
                    Err(_) => return Vec::new(),
                };
                stmt.query_map(
                    rusqlite::params![query, kind, limit],
                    Self::map_symbol_result,
                )
                .map(|rows| rows.filter_map(|r| r.ok()).collect())
                .unwrap_or_default()
            }
            (None, Some(_)) => {
                let mut stmt = match conn.prepare(sql) {
                    Ok(s) => s,
                    Err(_) => return Vec::new(),
                };
                stmt.query_map(
                    rusqlite::params![query, file_prefix.as_deref().unwrap_or(""), limit],
                    Self::map_symbol_result,
                )
                .map(|rows| rows.filter_map(|r| r.ok()).collect())
                .unwrap_or_default()
            }
            (None, None) => {
                let mut stmt = match conn.prepare(sql) {
                    Ok(s) => s,
                    Err(_) => return Vec::new(),
                };
                stmt.query_map(
                    rusqlite::params![query, limit],
                    Self::map_symbol_result,
                )
                .map(|rows| rows.filter_map(|r| r.ok()).collect())
                .unwrap_or_default()
            }
        };

        result
    }

    /// 行映射函数：将 SQL 行转换为 SymbolResult
    fn map_symbol_result(row: &rusqlite::Row) -> rusqlite::Result<SymbolResult> {
        Ok(SymbolResult {
            symbol_id: row.get(0)?,
            name: row.get(1)?,
            kind: row.get(2)?,
            file: row.get(3)?,
            line: row.get(4)?,
            signature: row.get(5)?,
            score: row.get(6)?,
        })
    }

    /// 图遍历（BFS）
    ///
    /// 从指定起点出发，按给定方向遍历 call_graph 或 dep_graph。
    /// 使用 BFS 保证按深度递增顺序返回节点。
    ///
    /// ## 参数
    /// - `target`: 起点标识
    ///   - Callers/Callees: symbol_id 或符号名
    ///   - Deps/ReverseDeps: 文件路径
    /// - `direction`: 遍历方向
    /// - `depth`: 最大遍历深度（1-10，防止大图爆炸）
    /// - `limit`: 最大返回节点数（防止结果集过大）
    ///
    /// ## 算法
    /// BFS + visited set:
    /// 1. 起点入队（depth=0）
    /// 2. 出队节点查 DB 获取邻居
    /// 3. 未访问的邻居入队（depth+1）
    /// 4. 深度 > max_depth 或节点数 > limit 时停止
    ///
    /// ## 边界条件
    /// - 环路：visited set 防止重复访问
    /// - 大扇出：limit 截断
    /// - 孤立节点：返回空 nodes（root 不计入结果）
    pub async fn graph_traverse(
        &self,
        target: &str,
        direction: GraphDirection,
        depth: u32,
        limit: u32,
    ) -> GraphResult {
        let effective_depth = depth.min(10).max(1);
        let effective_limit = limit.min(500).max(1);
        let conn = self.db.lock().await;

        // 解析起点：如果是符号名而非 ID，先查找 symbol_id
        let root_id = self.resolve_target(&conn, target, &direction);

        let mut visited: HashSet<String> = HashSet::new();
        let mut queue: VecDeque<(String, u32)> = VecDeque::new();
        let mut nodes: Vec<GraphNode> = Vec::new();
        let mut edges: Vec<GraphEdge> = Vec::new();

        // 起点标记为已访问但不加入结果
        visited.insert(root_id.clone());
        queue.push_back((root_id.clone(), 0));

        while let Some((current, current_depth)) = queue.pop_front() {
            if current_depth >= effective_depth {
                continue;
            }
            if nodes.len() >= effective_limit as usize {
                break;
            }

            // 根据方向查询邻居
            let neighbors = self.get_neighbors(&conn, &current, &direction);

            for (neighbor_id, neighbor_label, edge_type) in neighbors {
                if visited.contains(&neighbor_id) {
                    continue;
                }
                if nodes.len() >= effective_limit as usize {
                    break;
                }

                visited.insert(neighbor_id.clone());

                let node_type = match direction {
                    GraphDirection::Deps | GraphDirection::ReverseDeps => "file",
                    _ => "symbol",
                };

                nodes.push(GraphNode {
                    id: neighbor_id.clone(),
                    label: neighbor_label,
                    depth: current_depth + 1,
                    node_type: node_type.to_string(),
                });

                edges.push(GraphEdge {
                    from: current.clone(),
                    to: neighbor_id.clone(),
                    edge_type: edge_type.clone(),
                });

                queue.push_back((neighbor_id, current_depth + 1));
            }
        }

        let degradation = self.check_degradation(&conn);

        GraphResult {
            root: root_id,
            direction: direction.as_str().to_string(),
            max_depth: effective_depth,
            nodes,
            edges,
            degradation,
        }
    }

    /// 按 symbol_id 精确查找符号
    ///
    /// ## 参数
    /// - `symbol_id`: 符号的唯一标识（sha1 hash）
    ///
    /// ## 返回
    /// - `Some(Symbol)`: 找到符号
    /// - `None`: 符号不存在（已删除或 ID 错误）
    pub async fn get_symbol(&self, symbol_id: &str) -> Option<Symbol> {
        let conn = self.db.lock().await;
        self.fetch_symbol_by_id(&conn, symbol_id)
    }

    /// 将查询结果序列化为 JSON Value（供 tool 层直接输出）
    ///
    /// ## 参数
    /// - `results`: FTS5 搜索结果
    /// - `degradation`: 数据质量信号
    pub fn results_to_json(results: &[SymbolResult], degradation: CgDegradation) -> Value {
        json!({
            "symbols": results,
            "count": results.len(),
            "degradation": degradation.as_str(),
        })
    }

    /// 将图遍历结果序列化为 JSON Value
    pub fn graph_to_json(result: &GraphResult) -> Value {
        json!({
            "root": result.root,
            "direction": result.direction,
            "max_depth": result.max_depth,
            "node_count": result.nodes.len(),
            "edge_count": result.edges.len(),
            "nodes": result.nodes,
            "edges": result.edges,
            "degradation": result.degradation.as_str(),
        })
    }

    // ─── 内部辅助方法 ──────────────────────────────────────────────────────

    /// 解析目标标识：符号名 → symbol_id，文件路径原样返回
    ///
    /// 对于 Callers/Callees 方向：
    /// - 如果 target 看起来像 sha1 hash（40 hex chars），直接返回
    /// - 否则按符号名查找，取第一个匹配的 symbol_id
    ///
    /// 对于 Deps/ReverseDeps 方向：
    /// - target 就是文件路径，直接返回
    fn resolve_target(
        &self,
        conn: &Connection,
        target: &str,
        direction: &GraphDirection,
    ) -> String {
        match direction {
            GraphDirection::Deps | GraphDirection::ReverseDeps => target.to_string(),
            GraphDirection::Callers | GraphDirection::Callees => {
                // 如果是 40 字符 hex string，认为已经是 symbol_id
                if target.len() == 40 && target.chars().all(|c| c.is_ascii_hexdigit()) {
                    return target.to_string();
                }
                // 否则按名称查找
                conn.query_row(
                    "SELECT symbol_id FROM cg_symbols WHERE name = ?1 LIMIT 1",
                    [target],
                    |row| row.get::<_, String>(0),
                )
                .unwrap_or_else(|_| target.to_string())
            }
        }
    }

    /// 获取指定节点的邻居列表
    ///
    /// 返回 `(neighbor_id, neighbor_label, edge_type)` 三元组。
    ///
    /// ## 查询策略
    /// - Callers: SELECT caller_id FROM cg_call_graph WHERE callee_id = target
    /// - Callees: SELECT callee_id FROM cg_call_graph WHERE caller_id = target
    /// - Deps: SELECT target_file FROM cg_dep_graph WHERE source_file = target
    /// - ReverseDeps: SELECT source_file FROM cg_dep_graph WHERE target_file = target
    fn get_neighbors(
        &self,
        conn: &Connection,
        node_id: &str,
        direction: &GraphDirection,
    ) -> Vec<(String, String, String)> {
        match direction {
            GraphDirection::Callers => {
                let mut stmt = match conn.prepare(
                    "SELECT DISTINCT cg.caller_id, COALESCE(s.name, cg.caller_id) \
                     FROM cg_call_graph cg \
                     LEFT JOIN cg_symbols s ON s.symbol_id = cg.caller_id \
                     WHERE cg.callee_id = ?1"
                ) {
                    Ok(s) => s,
                    Err(_) => return Vec::new(),
                };
                stmt.query_map([node_id], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        "call".to_string(),
                    ))
                })
                .map(|rows| rows.filter_map(|r| r.ok()).collect())
                .unwrap_or_default()
            }
            GraphDirection::Callees => {
                let mut stmt = match conn.prepare(
                    "SELECT DISTINCT cg.callee_id, COALESCE(s.name, cg.callee_id) \
                     FROM cg_call_graph cg \
                     LEFT JOIN cg_symbols s ON s.symbol_id = cg.callee_id \
                     WHERE cg.caller_id = ?1"
                ) {
                    Ok(s) => s,
                    Err(_) => return Vec::new(),
                };
                stmt.query_map([node_id], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        "call".to_string(),
                    ))
                })
                .map(|rows| rows.filter_map(|r| r.ok()).collect())
                .unwrap_or_default()
            }
            GraphDirection::Deps => {
                let mut stmt = match conn.prepare(
                    "SELECT DISTINCT target_file, target_file, dep_kind \
                     FROM cg_dep_graph WHERE source_file = ?1"
                ) {
                    Ok(s) => s,
                    Err(_) => return Vec::new(),
                };
                stmt.query_map([node_id], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })
                .map(|rows| rows.filter_map(|r| r.ok()).collect())
                .unwrap_or_default()
            }
            GraphDirection::ReverseDeps => {
                let mut stmt = match conn.prepare(
                    "SELECT DISTINCT source_file, source_file, dep_kind \
                     FROM cg_dep_graph WHERE target_file = ?1"
                ) {
                    Ok(s) => s,
                    Err(_) => return Vec::new(),
                };
                stmt.query_map([node_id], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })
                .map(|rows| rows.filter_map(|r| r.ok()).collect())
                .unwrap_or_default()
            }
        }
    }

    /// 按 ID 获取完整符号信息
    fn fetch_symbol_by_id(&self, conn: &Connection, symbol_id: &str) -> Option<Symbol> {
        conn.query_row(
            "SELECT symbol_id, name, kind, file, line, col, end_line, \
             signature, doc_comment, visibility, parent_id, hash \
             FROM cg_symbols WHERE symbol_id = ?1",
            [symbol_id],
            |row| {
                let kind_str: String = row.get(2)?;
                let vis_str: String = row.get(9)?;
                Ok(Symbol {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    kind: SymbolKind::from_str(&kind_str).unwrap_or(SymbolKind::Function),
                    file: row.get(3)?,
                    line: row.get(4)?,
                    col: row.get(5)?,
                    end_line: row.get(6)?,
                    signature: row.get(7)?,
                    doc_comment: row.get(8)?,
                    visibility: match vis_str.as_str() {
                        "pub" => Visibility::Public,
                        "pub(crate)" => Visibility::PublicCrate,
                        "protected" => Visibility::Protected,
                        _ => Visibility::Private,
                    },
                    parent_id: row.get(10)?,
                    hash: row.get(11)?,
                })
            },
        )
        .ok()
    }

    /// 检查数据新鲜度（degradation 信号）
    ///
    /// 策略：
    /// - 有 parse_errors > 0 的文件 → PartialParse
    /// - 有文件 last_indexed_at 早于该文件 mtime → StaleIndex（此处简化为检查 error count）
    /// - 无任何文件元数据 → NoIndex
    /// - 否则 → Normal
    fn check_degradation(&self, conn: &Connection) -> CgDegradation {
        // 检查是否有任何文件元数据
        let has_meta = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM cg_file_meta LIMIT 1)",
                [],
                |row| row.get::<_, bool>(0),
            )
            .unwrap_or(false);

        if !has_meta {
            return CgDegradation::NoIndex;
        }

        // 检查是否有解析错误
        let has_errors = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM cg_file_meta WHERE parse_errors > 0 LIMIT 1)",
                [],
                |row| row.get::<_, bool>(0),
            )
            .unwrap_or(false);

        if has_errors {
            return CgDegradation::PartialParse;
        }

        CgDegradation::Normal
    }
}

// ─── 辅助函数 ──────────────────────────────────────────────────────────────

/// 清洗 FTS5 查询字符串
///
/// - 移除可能导致语法错误的特殊字符
/// - 保留 FTS5 支持的运算符（AND/OR/NOT）和通配符（*）
/// - 对于 trigram tokenizer，大多数字符都是合法的
fn sanitize_fts5_query(query: &str) -> String {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    // 移除导致 FTS5 语法错误的字符：未闭合引号、孤立括号
    let mut result = String::with_capacity(trimmed.len());
    let mut in_quotes = false;

    for ch in trimmed.chars() {
        match ch {
            '"' => {
                in_quotes = !in_quotes;
                result.push(ch);
            }
            // 移除可能破坏查询的特殊字符（非引号内）
            '^' | '$' | '{' | '}' | '[' | ']' if !in_quotes => {}
            // 保留其他所有字符
            _ => result.push(ch),
        }
    }

    // 如果引号未闭合，移除最后的开引号
    if in_quotes {
        if let Some(pos) = result.rfind('"') {
            result.remove(pos);
        }
    }

    result
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::code_graph::schema;

    /// 创建测试用内存数据库（已初始化 schema）
    fn setup_test_db() -> Arc<Mutex<Connection>> {
        let conn = Connection::open_in_memory().unwrap();
        schema::ensure_tables(&conn).unwrap();
        Arc::new(Mutex::new(conn))
    }

    /// 向测试库插入示例符号和关系数据
    async fn seed_test_data(db: &Arc<Mutex<Connection>>) {
        let conn = db.lock().await;
        conn.execute_batch(
            "INSERT INTO cg_symbols (symbol_id, name, kind, file, line, col, signature, visibility, hash, language)
             VALUES
             ('aaa111', 'process_turn', 'function', 'core/loop.rs', 50, 4, 'fn process_turn(&self) -> Result<()>', 'pub', 'h1', 'rust'),
             ('bbb222', 'handle_input', 'function', 'core/input.rs', 10, 4, 'fn handle_input(msg: &str) -> Action', 'pub', 'h2', 'rust'),
             ('ccc333', 'render_output', 'function', 'ui/render.rs', 20, 4, 'fn render_output(action: &Action)', 'pub(crate)', 'h3', 'rust'),
             ('ddd444', 'CoreState', 'struct', 'core/state.rs', 1, 0, 'struct CoreState', 'pub', 'h4', 'rust');

             INSERT INTO cg_call_graph (caller_id, callee_id, call_site_line) VALUES
             ('aaa111', 'bbb222', 55),
             ('aaa111', 'ccc333', 60),
             ('bbb222', 'ccc333', 15);

             INSERT INTO cg_dep_graph (source_file, target_file, dep_kind) VALUES
             ('core/loop.rs', 'core/input.rs', 'use'),
             ('core/loop.rs', 'ui/render.rs', 'use'),
             ('core/input.rs', 'core/state.rs', 'use'),
             ('ui/render.rs', 'core/state.rs', 'use');

             INSERT INTO cg_file_meta (file, hash, symbol_count, last_indexed_at, parse_errors, language, file_size) VALUES
             ('core/loop.rs', 'fh1', 1, 1700000000, 0, 'rust', 5000),
             ('core/input.rs', 'fh2', 1, 1700000000, 0, 'rust', 3000),
             ('ui/render.rs', 'fh3', 1, 1700000000, 0, 'rust', 2000),
             ('core/state.rs', 'fh4', 1, 1700000000, 0, 'rust', 1500);"
        ).unwrap();
    }

    #[tokio::test]
    async fn test_has_any_symbols_empty_db() {
        let db = setup_test_db();
        let engine = QueryEngine::new(db);
        assert!(!engine.has_any_symbols().await);
    }

    #[tokio::test]
    async fn test_has_any_symbols_with_data() {
        let db = setup_test_db();
        seed_test_data(&db).await;
        let engine = QueryEngine::new(db);
        assert!(engine.has_any_symbols().await);
    }

    #[tokio::test]
    async fn test_search_symbols_fts5() {
        let db = setup_test_db();
        seed_test_data(&db).await;
        let engine = QueryEngine::new(db);

        // 搜索 "process" — 应匹配 process_turn
        let (results, degradation) = engine.search_symbols("process", None, None, 10).await;
        assert!(!results.is_empty(), "FTS5 search should find 'process_turn'");
        assert_eq!(results[0].name, "process_turn");
        assert_eq!(degradation, CgDegradation::Normal);
    }

    #[tokio::test]
    async fn test_search_symbols_with_kind_filter() {
        let db = setup_test_db();
        seed_test_data(&db).await;
        let engine = QueryEngine::new(db);

        // 搜索 "Core" 限定 struct 类型
        let (results, _) = engine.search_symbols("Core", Some("struct"), None, 10).await;
        assert!(
            results.iter().all(|r| r.kind == "struct"),
            "Kind filter should only return structs"
        );
    }

    #[tokio::test]
    async fn test_search_symbols_no_index() {
        let db = setup_test_db();
        let engine = QueryEngine::new(db);

        let (results, degradation) = engine.search_symbols("anything", None, None, 10).await;
        assert!(results.is_empty());
        assert_eq!(degradation, CgDegradation::NoIndex);
    }

    #[tokio::test]
    async fn test_graph_traverse_callees() {
        let db = setup_test_db();
        seed_test_data(&db).await;
        let engine = QueryEngine::new(db);

        // process_turn 调用了 handle_input 和 render_output
        let result = engine
            .graph_traverse("aaa111", GraphDirection::Callees, 1, 50)
            .await;

        assert_eq!(result.root, "aaa111");
        assert_eq!(result.direction, "callees");
        assert_eq!(result.nodes.len(), 2, "process_turn has 2 direct callees");

        let callee_ids: HashSet<&str> = result.nodes.iter().map(|n| n.id.as_str()).collect();
        assert!(callee_ids.contains("bbb222"));
        assert!(callee_ids.contains("ccc333"));
    }

    #[tokio::test]
    async fn test_graph_traverse_callers() {
        let db = setup_test_db();
        seed_test_data(&db).await;
        let engine = QueryEngine::new(db);

        // render_output 被 process_turn 和 handle_input 调用
        let result = engine
            .graph_traverse("ccc333", GraphDirection::Callers, 1, 50)
            .await;

        assert_eq!(result.nodes.len(), 2, "render_output has 2 callers");
    }

    #[tokio::test]
    async fn test_graph_traverse_deps() {
        let db = setup_test_db();
        seed_test_data(&db).await;
        let engine = QueryEngine::new(db);

        // core/loop.rs 依赖 core/input.rs 和 ui/render.rs
        let result = engine
            .graph_traverse("core/loop.rs", GraphDirection::Deps, 1, 50)
            .await;

        assert_eq!(result.nodes.len(), 2);
        let dep_files: HashSet<&str> = result.nodes.iter().map(|n| n.id.as_str()).collect();
        assert!(dep_files.contains("core/input.rs"));
        assert!(dep_files.contains("ui/render.rs"));
    }

    #[tokio::test]
    async fn test_graph_traverse_depth_limit() {
        let db = setup_test_db();
        seed_test_data(&db).await;
        let engine = QueryEngine::new(db);

        // Depth=2 从 core/loop.rs 开始：
        // depth 1: core/input.rs, ui/render.rs
        // depth 2: core/state.rs (via input.rs and render.rs, deduplicated)
        let result = engine
            .graph_traverse("core/loop.rs", GraphDirection::Deps, 2, 50)
            .await;

        assert!(
            result.nodes.len() == 3,
            "Depth-2 BFS should find 3 unique files (input, render, state)"
        );

        // 验证 core/state.rs 在 depth=2
        let state_node = result.nodes.iter().find(|n| n.id == "core/state.rs");
        assert!(state_node.is_some());
        assert_eq!(state_node.unwrap().depth, 2);
    }

    #[tokio::test]
    async fn test_graph_traverse_by_symbol_name() {
        let db = setup_test_db();
        seed_test_data(&db).await;
        let engine = QueryEngine::new(db);

        // 通过符号名而非 ID 遍历
        let result = engine
            .graph_traverse("process_turn", GraphDirection::Callees, 1, 50)
            .await;

        // resolve_target 应该把 "process_turn" 解析为 "aaa111"
        assert_eq!(result.root, "aaa111");
        assert_eq!(result.nodes.len(), 2);
    }

    #[tokio::test]
    async fn test_get_symbol_exists() {
        let db = setup_test_db();
        seed_test_data(&db).await;
        let engine = QueryEngine::new(db);

        let sym = engine.get_symbol("aaa111").await;
        assert!(sym.is_some());
        let sym = sym.unwrap();
        assert_eq!(sym.name, "process_turn");
        assert_eq!(sym.kind, SymbolKind::Function);
        assert_eq!(sym.file, "core/loop.rs");
        assert_eq!(sym.line, 50);
        assert_eq!(sym.visibility, Visibility::Public);
    }

    #[tokio::test]
    async fn test_get_symbol_not_found() {
        let db = setup_test_db();
        let engine = QueryEngine::new(db);
        assert!(engine.get_symbol("nonexistent_id").await.is_none());
    }

    #[test]
    fn test_sanitize_fts5_query() {
        assert_eq!(sanitize_fts5_query("  hello  "), "hello");
        assert_eq!(sanitize_fts5_query("foo[bar]"), "foobar");
        assert_eq!(sanitize_fts5_query("\"quoted phrase\""), "\"quoted phrase\"");
        // 未闭合引号被修复
        assert_eq!(sanitize_fts5_query("broken\"quote"), "brokenquote");
        assert_eq!(sanitize_fts5_query(""), "");
    }

    #[test]
    fn test_graph_direction_roundtrip() {
        let dirs = [
            GraphDirection::Callers,
            GraphDirection::Callees,
            GraphDirection::Deps,
            GraphDirection::ReverseDeps,
        ];
        for dir in dirs {
            assert_eq!(GraphDirection::from_str(dir.as_str()), Some(dir));
        }
    }

    #[test]
    fn test_graph_direction_aliases() {
        assert_eq!(GraphDirection::from_str("caller"), Some(GraphDirection::Callers));
        assert_eq!(GraphDirection::from_str("callee"), Some(GraphDirection::Callees));
        assert_eq!(GraphDirection::from_str("dependencies"), Some(GraphDirection::Deps));
        assert_eq!(GraphDirection::from_str("reverse-deps"), Some(GraphDirection::ReverseDeps));
        assert_eq!(GraphDirection::from_str("invalid"), None);
    }
}
