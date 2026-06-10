//! CodeGraph Analysis Engine — 影响分析 + 循环检测 + 耦合度量
//!
//! ## 职责
//! 提供代码结构的深度分析能力：
//! 1. **Impact Analysis** — 给定变更目标，BFS 上游遍历 call_graph 计算影响范围
//! 2. **Cycle Detection** — Tarjan SCC 算法检测 dep_graph 中的循环依赖
//! 3. **Coupling Metrics** — 计算文件/模块的 Ca/Ce/Instability/Abstractness/Distance
//!
//! ## 依赖 (external)
//! - `rusqlite`: 共享 knowledge.db 连接（读路径）
//! - `tokio::sync::Mutex`: 异步互斥锁
//! - `std::collections`: HashMap/HashSet/VecDeque 用于图算法
//!
//! ## 依赖 (internal)
//! - `super::CgDegradation`: 数据质量信号
//!
//! ## 引用关系
//! - 被 `CodeGraphManager::analyze()` 暴露给外部
//! - 被 `tool/builtin/cg.rs` 的 `cg_impact` / `cg_cycles` / `cg_coupling` 命令调用
//!
//! ## 生命周期
//! - 创建：`CodeGraphManager::new()` 时构造
//! - 存活：与 `CodeGraphManager` 同生命周期（Arc 共享）
//! - 销毁：最后一个 Arc 引用 drop 时销毁（无需显式清理）
//!
//! ## 设计决策
//! - Impact BFS 反向遍历 call_graph（找所有调用者）以计算变更扩散
//! - Tarjan SCC 使用**迭代式**实现（避免大图递归栈溢出）
//! - Coupling 指标遵循 Robert C. Martin 的包度量理论

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use rusqlite::Connection;
use tokio::sync::Mutex;

use super::CgDegradation;

// ─── 公共类型 ──────────────────────────────────────────────────────────────

/// 影响分析报告
///
/// 描述一组变更符号可能影响的上游调用链。
#[derive(Debug, Clone, serde::Serialize)]
pub struct ImpactReport {
    /// 被分析的变更符号列表
    pub changed_symbols: Vec<String>,
    /// 影响链（从变更点到上游调用者）
    pub impact_chains: Vec<ImpactChain>,
    /// 受影响的文件总数（去重）
    pub total_affected_files: usize,
    /// 分析置信度（0.0-1.0）
    /// - 1.0: 索引完整，调用关系明确
    /// - < 1.0: 部分文件未索引或有解析错误
    pub confidence: f64,
    /// 数据质量信号
    pub degradation: CgDegradation,
}

/// 单条影响链
///
/// 从一个变更根节点出发，列出所有受影响的上游符号。
#[derive(Debug, Clone, serde::Serialize)]
pub struct ImpactChain {
    /// 变更根符号（ID 或名称）
    pub root_symbol: String,
    /// 受影响的上游符号列表（按 BFS 深度排序）
    pub affected: Vec<AffectedSymbol>,
}

/// 受影响的符号（含深度信息）
#[derive(Debug, Clone, serde::Serialize)]
pub struct AffectedSymbol {
    /// 符号 ID
    pub symbol_id: String,
    /// 符号名
    pub name: String,
    /// 所在文件
    pub file: String,
    /// 距离变更点的调用深度
    pub depth: u32,
}

/// 依赖循环
///
/// 描述 dep_graph 中的一个强连通分量（SCC，节点数 >= 2）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct DependencyCycle {
    /// 构成循环的文件列表
    pub files: Vec<String>,
    /// 严重程度
    /// - "high": 涉及 >= 5 个文件
    /// - "medium": 涉及 3-4 个文件
    /// - "low": 涉及 2 个文件
    pub severity: String,
}

/// 耦合度量
///
/// 基于 Robert C. Martin 的包度量理论：
/// - Ca (Afferent Coupling): 依赖该目标的文件数
/// - Ce (Efferent Coupling): 该目标依赖的文件数
/// - I (Instability): Ce / (Ca + Ce)，0=稳定，1=不稳定
/// - A (Abstractness): 抽象符号数 / 总符号数
/// - D (Distance from Main Sequence): |A + I - 1|，0=理想
#[derive(Debug, Clone, serde::Serialize)]
pub struct CouplingMetrics {
    /// 目标文件/模块
    pub target: String,
    /// Afferent Coupling — 依赖此目标的外部文件数
    pub ca: u32,
    /// Efferent Coupling — 此目标依赖的外部文件数
    pub ce: u32,
    /// Instability — Ce / (Ca + Ce)，范围 [0.0, 1.0]
    pub instability: f64,
    /// Abstractness — 抽象符号占比，范围 [0.0, 1.0]
    pub abstractness: f64,
    /// Distance from Main Sequence — |A + I - 1|，范围 [0.0, 1.0]
    pub distance: f64,
}

// ─── AnalyzeEngine ────────────────────────────────────────────────────────

/// CodeGraph 分析引擎
///
/// 提供影响分析、循环检测、耦合度量三大分析能力。
/// 所有方法都是只读操作。
///
/// ## 线程安全
/// 通过 `Arc<Mutex<Connection>>` 保证并发安全。
///
/// ## 引用关系
/// - 持有 `Arc<Mutex<Connection>>` — 共享 knowledge.db 连接
/// - 被 `CodeGraphManager` 持有（Arc 包装）
pub struct AnalyzeEngine {
    /// 共享 knowledge.db 连接
    ///
    /// 生命周期：与 CodeGraphManager 共享同一连接
    /// 销毁：最后一个 Arc drop 时关闭连接
    db: Arc<Mutex<Connection>>,
}

impl AnalyzeEngine {
    /// 创建 AnalyzeEngine 实例
    ///
    /// ## 参数
    /// - `db`: 共享 knowledge.db 连接（已完成 schema migration）
    ///
    /// ## 前置条件
    /// - cg_* 表已通过 `schema::ensure_tables()` 创建
    pub fn new(db: Arc<Mutex<Connection>>) -> Self {
        Self { db }
    }

    /// 影响分析：计算变更符号的上游影响范围
    ///
    /// 从目标符号出发，BFS 反向遍历 call_graph（找调用者），
    /// 逐层展开直到达到 depth 上限或无更多调用者。
    ///
    /// ## 参数
    /// - `targets`: 变更符号列表（symbol_id 或符号名）
    /// - `depth`: 最大追踪深度（1-10）
    ///
    /// ## 算法
    /// 1. 解析每个 target 为 symbol_id
    /// 2. 对每个 target 执行 BFS（反向 call_graph）
    /// 3. 收集所有受影响符号及其文件
    /// 4. 计算置信度（基于文件元数据完整性）
    ///
    /// ## 复杂度
    /// - 时间: O(V + E) per target，V=符号数，E=调用边数
    /// - 空间: O(V) visited set
    pub async fn analyze_impact(
        &self,
        targets: &[&str],
        depth: u32,
    ) -> ImpactReport {
        let effective_depth = depth.clamp(1, 10);
        let conn = self.db.lock().await;

        let mut impact_chains = Vec::new();
        let mut all_affected_files: HashSet<String> = HashSet::new();

        let resolved_targets: Vec<String> = targets
            .iter()
            .map(|t| self.resolve_symbol_id(&conn, t))
            .collect();

        for target_id in &resolved_targets {
            let chain = self.bfs_impact(&conn, target_id, effective_depth);

            // 收集受影响文件
            for affected in &chain.affected {
                all_affected_files.insert(affected.file.clone());
            }

            impact_chains.push(chain);
        }

        let confidence = self.compute_confidence(&conn);
        let degradation = self.check_degradation(&conn);

        ImpactReport {
            changed_symbols: resolved_targets,
            impact_chains,
            total_affected_files: all_affected_files.len(),
            confidence,
            degradation,
        }
    }

    /// 循环依赖检测：Tarjan SCC 算法（迭代式）
    ///
    /// 在 dep_graph 上执行 Tarjan 强连通分量算法，
    /// 返回所有包含 2+ 节点的 SCC（即循环依赖）。
    ///
    /// ## 参数
    /// - `scope`: 可选的文件路径前缀过滤（None = 全图扫描）
    ///
    /// ## 算法
    /// 迭代式 Tarjan SCC（避免大图递归栈溢出）：
    /// 1. 维护显式调用栈（Vec<StackFrame>）
    /// 2. 每个节点有 index + lowlink
    /// 3. SCC 完成时出栈检查大小
    ///
    /// ## 返回
    /// 按严重程度降序排列的循环列表
    pub async fn detect_cycles(
        &self,
        scope: Option<&str>,
    ) -> Vec<DependencyCycle> {
        let conn = self.db.lock().await;

        // 构建邻接表（file → [dependent files]）
        let adjacency = self.build_dep_adjacency(&conn, scope);

        // 迭代式 Tarjan SCC
        let sccs = iterative_tarjan(&adjacency);

        // 过滤单节点 SCC（非循环），转换为 DependencyCycle
        let mut cycles: Vec<DependencyCycle> = sccs
            .into_iter()
            .filter(|scc| scc.len() >= 2)
            .map(|files| {
                let severity = match files.len() {
                    2 => "low".to_string(),
                    3 | 4 => "medium".to_string(),
                    _ => "high".to_string(),
                };
                DependencyCycle { files, severity }
            })
            .collect();

        // 按严重程度降序排列（high > medium > low）
        cycles.sort_by(|a, b| {
            let ord_a = severity_order(&a.severity);
            let ord_b = severity_order(&b.severity);
            ord_b.cmp(&ord_a)
        });

        cycles
    }

    /// 耦合度量计算
    ///
    /// 计算目标文件的 Ca/Ce/I/A/D 五项指标。
    ///
    /// ## 参数
    /// - `target`: 文件路径
    ///
    /// ## 指标定义
    /// - **Ca** (Afferent): `SELECT COUNT(DISTINCT source_file) FROM cg_dep_graph WHERE target_file = target`
    /// - **Ce** (Efferent): `SELECT COUNT(DISTINCT target_file) FROM cg_dep_graph WHERE source_file = target`
    /// - **I** (Instability): `Ce / (Ca + Ce)`，Ca+Ce=0 时 I=0
    /// - **A** (Abstractness): trait/interface 符号数 / 总符号数
    /// - **D** (Distance): `|A + I - 1|`
    ///
    /// ## 边界情况
    /// - 目标文件不存在于 dep_graph → Ca=0, Ce=0, I=0
    /// - 目标文件无符号 → A=0
    pub async fn compute_coupling(&self, target: &str) -> CouplingMetrics {
        let conn = self.db.lock().await;

        // Ca: 有多少文件依赖此目标
        let ca: u32 = conn
            .query_row(
                "SELECT COUNT(DISTINCT source_file) FROM cg_dep_graph WHERE target_file = ?1",
                [target],
                |row| row.get(0),
            )
            .unwrap_or(0);

        // Ce: 此目标依赖多少文件
        let ce: u32 = conn
            .query_row(
                "SELECT COUNT(DISTINCT target_file) FROM cg_dep_graph WHERE source_file = ?1",
                [target],
                |row| row.get(0),
            )
            .unwrap_or(0);

        // Instability: Ce / (Ca + Ce)
        let instability = if ca + ce > 0 {
            ce as f64 / (ca + ce) as f64
        } else {
            0.0
        };

        // Abstractness: abstract symbols / total symbols
        let total_symbols: u32 = conn
            .query_row(
                "SELECT COUNT(*) FROM cg_symbols WHERE file = ?1",
                [target],
                |row| row.get(0),
            )
            .unwrap_or(0);

        let abstract_symbols: u32 = conn
            .query_row(
                "SELECT COUNT(*) FROM cg_symbols WHERE file = ?1 AND kind IN ('trait', 'interface')",
                [target],
                |row| row.get(0),
            )
            .unwrap_or(0);

        let abstractness = if total_symbols > 0 {
            abstract_symbols as f64 / total_symbols as f64
        } else {
            0.0
        };

        // Distance from main sequence: |A + I - 1|
        let distance = (abstractness + instability - 1.0).abs();

        CouplingMetrics {
            target: target.to_string(),
            ca,
            ce,
            instability,
            abstractness,
            distance,
        }
    }

    // ─── 内部辅助方法 ──────────────────────────────────────────────────────

    /// 解析符号标识：名称 → symbol_id
    ///
    /// 如果是 40 字符 hex hash，直接返回。
    /// 否则按名称查找第一个匹配的 symbol_id。
    fn resolve_symbol_id(&self, conn: &Connection, target: &str) -> String {
        if target.len() == 40 && target.chars().all(|c| c.is_ascii_hexdigit()) {
            return target.to_string();
        }
        conn.query_row(
            "SELECT symbol_id FROM cg_symbols WHERE name = ?1 LIMIT 1",
            [target],
            |row| row.get::<_, String>(0),
        )
        .unwrap_or_else(|_| target.to_string())
    }

    /// BFS 反向遍历 call_graph，构建影响链
    ///
    /// 从 target 出发，反向找所有 caller，逐层展开。
    fn bfs_impact(&self, conn: &Connection, target_id: &str, max_depth: u32) -> ImpactChain {
        let mut visited: HashSet<String> = HashSet::new();
        let mut queue: VecDeque<(String, u32)> = VecDeque::new();
        let mut affected: Vec<AffectedSymbol> = Vec::new();

        visited.insert(target_id.to_string());
        queue.push_back((target_id.to_string(), 0));

        while let Some((current_id, current_depth)) = queue.pop_front() {
            if current_depth >= max_depth {
                continue;
            }

            // 查找当前符号的所有调用者
            let callers = self.get_callers(conn, &current_id);

            for (caller_id, caller_name, caller_file) in callers {
                if visited.contains(&caller_id) {
                    continue;
                }
                visited.insert(caller_id.clone());

                affected.push(AffectedSymbol {
                    symbol_id: caller_id.clone(),
                    name: caller_name,
                    file: caller_file,
                    depth: current_depth + 1,
                });

                queue.push_back((caller_id, current_depth + 1));
            }
        }

        // 获取 root 符号名
        let root_name = conn
            .query_row(
                "SELECT name FROM cg_symbols WHERE symbol_id = ?1",
                [target_id],
                |row| row.get::<_, String>(0),
            )
            .unwrap_or_else(|_| target_id.to_string());

        ImpactChain {
            root_symbol: root_name,
            affected,
        }
    }

    /// 获取指定符号的所有调用者
    ///
    /// 返回 `(caller_id, caller_name, caller_file)` 三元组。
    fn get_callers(
        &self,
        conn: &Connection,
        callee_id: &str,
    ) -> Vec<(String, String, String)> {
        let mut stmt = match conn.prepare(
            "SELECT DISTINCT cg.caller_id, COALESCE(s.name, cg.caller_id), COALESCE(s.file, '') \
             FROM cg_call_graph cg \
             LEFT JOIN cg_symbols s ON s.symbol_id = cg.caller_id \
             WHERE cg.callee_id = ?1"
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };

        stmt.query_map([callee_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    }

    /// 构建 dep_graph 邻接表
    ///
    /// ## 参数
    /// - `scope`: 可选路径前缀过滤
    ///
    /// ## 返回
    /// `HashMap<source_file, Vec<target_file>>` — 正向依赖邻接表
    fn build_dep_adjacency(
        &self,
        conn: &Connection,
        scope: Option<&str>,
    ) -> HashMap<String, Vec<String>> {
        let mut adjacency: HashMap<String, Vec<String>> = HashMap::new();

        let query = match scope {
            Some(_) => {
                "SELECT source_file, target_file FROM cg_dep_graph \
                 WHERE source_file LIKE ?1 OR target_file LIKE ?1"
            }
            None => "SELECT source_file, target_file FROM cg_dep_graph",
        };

        let result: Vec<(String, String)> = if let Some(prefix) = scope {
            let pattern = format!("{prefix}%");
            let mut stmt = match conn.prepare(query) {
                Ok(s) => s,
                Err(_) => return adjacency,
            };
            stmt.query_map([&pattern], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
        } else {
            let mut stmt = match conn.prepare(query) {
                Ok(s) => s,
                Err(_) => return adjacency,
            };
            stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
        };

        // 确保所有节点都在邻接表中（包括叶子节点）
        for (source, target) in result {
            adjacency.entry(target.clone()).or_default();
            adjacency.entry(source.clone()).or_default().push(target);
        }

        adjacency
    }

    /// 计算分析置信度
    ///
    /// 基于文件元数据的完整性：
    /// - 全部文件无解析错误 → 1.0
    /// - 有解析错误 → 按错误文件比例降低
    /// - 无文件元数据 → 0.0
    fn compute_confidence(&self, conn: &Connection) -> f64 {
        let total_files: u32 = conn
            .query_row("SELECT COUNT(*) FROM cg_file_meta", [], |row| row.get(0))
            .unwrap_or(0);

        if total_files == 0 {
            return 0.0;
        }

        let error_files: u32 = conn
            .query_row(
                "SELECT COUNT(*) FROM cg_file_meta WHERE parse_errors > 0",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);

        1.0 - (error_files as f64 / total_files as f64)
    }

    /// 检查数据新鲜度
    fn check_degradation(&self, conn: &Connection) -> CgDegradation {
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

// ─── Tarjan SCC (迭代式) ──────────────────────────────────────────────────

/// 迭代式 Tarjan 强连通分量算法
///
/// 避免递归实现在大图上的栈溢出风险。
/// 使用显式栈模拟 DFS 递归调用。
///
/// ## 算法概述
/// 1. 为每个未访问节点启动 DFS
/// 2. 每个节点分配 index 和 lowlink
/// 3. 节点入栈（on_stack 标记）
/// 4. DFS 回溯时更新 lowlink
/// 5. 当 lowlink == index 时，栈顶到当前节点构成一个 SCC
///
/// ## 参数
/// - `adjacency`: 邻接表（节点 → 后继列表）
///
/// ## 返回
/// 所有强连通分量（包括单节点的）
fn iterative_tarjan(adjacency: &HashMap<String, Vec<String>>) -> Vec<Vec<String>> {
    // Tarjan 状态
    let mut index_counter: u32 = 0;
    let mut node_index: HashMap<&str, u32> = HashMap::new();
    let mut node_lowlink: HashMap<&str, u32> = HashMap::new();
    let mut on_stack: HashSet<&str> = HashSet::new();
    let mut stack: Vec<&str> = Vec::new();
    let mut sccs: Vec<Vec<String>> = Vec::new();

    // 显式 DFS 栈帧
    struct StackFrame<'a> {
        node: &'a str,
        neighbor_idx: usize, // 当前正在处理哪个邻居
    }

    for start_node in adjacency.keys() {
        if node_index.contains_key(start_node.as_str()) {
            continue; // 已访问
        }

        // DFS 调用栈
        let mut dfs_stack: Vec<StackFrame> = Vec::new();

        // 初始化起始节点
        let start_str = start_node.as_str();
        node_index.insert(start_str, index_counter);
        node_lowlink.insert(start_str, index_counter);
        index_counter += 1;
        stack.push(start_str);
        on_stack.insert(start_str);

        dfs_stack.push(StackFrame {
            node: start_str,
            neighbor_idx: 0,
        });

        while let Some(frame) = dfs_stack.last_mut() {
            let current_node = frame.node;
            let neighbors = adjacency.get(current_node).map(|v| v.as_slice()).unwrap_or(&[]);

            if frame.neighbor_idx < neighbors.len() {
                let neighbor = neighbors[frame.neighbor_idx].as_str();
                frame.neighbor_idx += 1;

                // 邻居不在邻接表中（外部节点）则跳过
                if !adjacency.contains_key(neighbor) {
                    continue;
                }

                if !node_index.contains_key(neighbor) {
                    // 未访问：初始化并入栈（模拟递归调用）
                    node_index.insert(neighbor, index_counter);
                    node_lowlink.insert(neighbor, index_counter);
                    index_counter += 1;
                    stack.push(neighbor);
                    on_stack.insert(neighbor);

                    dfs_stack.push(StackFrame {
                        node: neighbor,
                        neighbor_idx: 0,
                    });
                } else if on_stack.contains(neighbor) {
                    // 在栈上：更新 lowlink（回边）
                    let neighbor_idx = node_index[neighbor];
                    let current_lowlink = node_lowlink[current_node];
                    if neighbor_idx < current_lowlink {
                        node_lowlink.insert(current_node, neighbor_idx);
                    }
                }
            } else {
                // 所有邻居已处理：回溯
                let finished_node = current_node;
                let finished_lowlink = node_lowlink[finished_node];
                let finished_index = node_index[finished_node];

                dfs_stack.pop();

                // 更新父节点的 lowlink
                if let Some(parent_frame) = dfs_stack.last() {
                    let parent = parent_frame.node;
                    let parent_lowlink = node_lowlink[parent];
                    if finished_lowlink < parent_lowlink {
                        node_lowlink.insert(parent, finished_lowlink);
                    }
                }

                // 如果是 SCC 根节点，弹出整个 SCC
                if finished_lowlink == finished_index {
                    let mut scc: Vec<String> = Vec::new();
                    while let Some(w) = stack.pop() {
                        on_stack.remove(w);
                        scc.push(w.to_string());
                        if w == finished_node {
                            break;
                        }
                    }
                    sccs.push(scc);
                }
            }
        }
    }

    sccs
}

/// 严重程度排序辅助
fn severity_order(severity: &str) -> u8 {
    match severity {
        "high" => 3,
        "medium" => 2,
        "low" => 1,
        _ => 0,
    }
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

    /// 插入包含循环依赖的测试数据
    async fn seed_cycle_data(db: &Arc<Mutex<Connection>>) {
        let conn = db.lock().await;
        conn.execute_batch(
            "INSERT INTO cg_dep_graph (source_file, target_file, dep_kind) VALUES
             ('a.rs', 'b.rs', 'use'),
             ('b.rs', 'a.rs', 'use'),
             ('c.rs', 'd.rs', 'use'),
             ('d.rs', 'e.rs', 'use'),
             ('e.rs', 'c.rs', 'use'),
             ('f.rs', 'a.rs', 'use');

             INSERT INTO cg_file_meta (file, hash, symbol_count, last_indexed_at, parse_errors, language, file_size) VALUES
             ('a.rs', 'h1', 2, 1700000000, 0, 'rust', 1000),
             ('b.rs', 'h2', 2, 1700000000, 0, 'rust', 1000),
             ('c.rs', 'h3', 1, 1700000000, 0, 'rust', 500),
             ('d.rs', 'h4', 1, 1700000000, 0, 'rust', 500),
             ('e.rs', 'h5', 1, 1700000000, 0, 'rust', 500),
             ('f.rs', 'h6', 1, 1700000000, 0, 'rust', 200);"
        ).unwrap();
    }

    /// 插入调用图测试数据（用于 impact 分析）
    async fn seed_call_graph_data(db: &Arc<Mutex<Connection>>) {
        let conn = db.lock().await;
        conn.execute_batch(
            "INSERT INTO cg_symbols (symbol_id, name, kind, file, line, col, visibility, hash, language) VALUES
             ('s1', 'db_query', 'function', 'db/query.rs', 10, 4, 'pub', 'h1', 'rust'),
             ('s2', 'service_get', 'function', 'service/get.rs', 20, 4, 'pub', 'h2', 'rust'),
             ('s3', 'handler_api', 'function', 'handler/api.rs', 30, 4, 'pub', 'h3', 'rust'),
             ('s4', 'middleware_auth', 'function', 'middleware/auth.rs', 40, 4, 'pub', 'h4', 'rust'),
             ('s5', 'unrelated_fn', 'function', 'other/misc.rs', 50, 4, 'pub', 'h5', 'rust');

             INSERT INTO cg_call_graph (caller_id, callee_id, call_site_line) VALUES
             ('s2', 's1', 25),
             ('s3', 's2', 35),
             ('s4', 's3', 45);

             INSERT INTO cg_file_meta (file, hash, symbol_count, last_indexed_at, parse_errors, language, file_size) VALUES
             ('db/query.rs', 'fh1', 1, 1700000000, 0, 'rust', 2000),
             ('service/get.rs', 'fh2', 1, 1700000000, 0, 'rust', 1500),
             ('handler/api.rs', 'fh3', 1, 1700000000, 0, 'rust', 1000),
             ('middleware/auth.rs', 'fh4', 1, 1700000000, 0, 'rust', 800),
             ('other/misc.rs', 'fh5', 1, 1700000000, 0, 'rust', 500);"
        ).unwrap();
    }

    /// 插入耦合度量测试数据
    async fn seed_coupling_data(db: &Arc<Mutex<Connection>>) {
        let conn = db.lock().await;
        conn.execute_batch(
            "INSERT INTO cg_symbols (symbol_id, name, kind, file, line, col, visibility, hash, language) VALUES
             ('t1', 'Serializer', 'trait', 'core/serde.rs', 1, 0, 'pub', 'h1', 'rust'),
             ('t2', 'Deserializer', 'trait', 'core/serde.rs', 20, 0, 'pub', 'h2', 'rust'),
             ('t3', 'JsonSerializer', 'struct', 'core/serde.rs', 40, 0, 'pub', 'h3', 'rust'),
             ('t4', 'serialize_value', 'function', 'core/serde.rs', 60, 0, 'pub', 'h4', 'rust');

             INSERT INTO cg_dep_graph (source_file, target_file, dep_kind) VALUES
             ('handler/api.rs', 'core/serde.rs', 'use'),
             ('service/get.rs', 'core/serde.rs', 'use'),
             ('service/post.rs', 'core/serde.rs', 'use'),
             ('core/serde.rs', 'core/types.rs', 'use'),
             ('core/serde.rs', 'core/error.rs', 'use');

             INSERT INTO cg_file_meta (file, hash, symbol_count, last_indexed_at, parse_errors, language, file_size) VALUES
             ('core/serde.rs', 'fh1', 4, 1700000000, 0, 'rust', 3000),
             ('core/types.rs', 'fh2', 2, 1700000000, 0, 'rust', 1000),
             ('core/error.rs', 'fh3', 1, 1700000000, 0, 'rust', 500);"
        ).unwrap();
    }

    // ─── Cycle Detection Tests ─────────────────────────────────────────────

    #[tokio::test]
    async fn test_detect_simple_cycle_a_b_a() {
        let db = setup_test_db();
        seed_cycle_data(&db).await;
        let engine = AnalyzeEngine::new(db);

        let cycles = engine.detect_cycles(None).await;

        // 应检测到两个循环：a↔b (size 2) 和 c→d→e→c (size 3)
        assert!(
            cycles.len() >= 2,
            "Should detect at least 2 cycles, got {}",
            cycles.len()
        );

        // 验证存在 {a.rs, b.rs} 循环
        let has_ab_cycle = cycles.iter().any(|c| {
            c.files.len() == 2
                && c.files.contains(&"a.rs".to_string())
                && c.files.contains(&"b.rs".to_string())
        });
        assert!(has_ab_cycle, "Should detect a.rs <-> b.rs cycle");

        // 验证存在 {c.rs, d.rs, e.rs} 循环
        let has_cde_cycle = cycles.iter().any(|c| {
            c.files.len() == 3
                && c.files.contains(&"c.rs".to_string())
                && c.files.contains(&"d.rs".to_string())
                && c.files.contains(&"e.rs".to_string())
        });
        assert!(has_cde_cycle, "Should detect c.rs -> d.rs -> e.rs -> c.rs cycle");
    }

    #[tokio::test]
    async fn test_detect_cycles_severity() {
        let db = setup_test_db();
        seed_cycle_data(&db).await;
        let engine = AnalyzeEngine::new(db);

        let cycles = engine.detect_cycles(None).await;

        // 验证严重程度标记
        for cycle in &cycles {
            match cycle.files.len() {
                2 => assert_eq!(cycle.severity, "low"),
                3 | 4 => assert_eq!(cycle.severity, "medium"),
                n if n >= 5 => assert_eq!(cycle.severity, "high"),
                _ => panic!("Unexpected cycle size: {}", cycle.files.len()),
            }
        }

        // 验证排序（高严重程度在前）
        if cycles.len() >= 2 {
            let first_order = severity_order(&cycles[0].severity);
            let last_order = severity_order(&cycles[cycles.len() - 1].severity);
            assert!(
                first_order >= last_order,
                "Cycles should be sorted by severity descending"
            );
        }
    }

    #[tokio::test]
    async fn test_detect_cycles_no_cycles() {
        let db = setup_test_db();
        {
            let conn = db.lock().await;
            // 线性依赖链，无循环
            conn.execute_batch(
                "INSERT INTO cg_dep_graph (source_file, target_file, dep_kind) VALUES
                 ('x.rs', 'y.rs', 'use'),
                 ('y.rs', 'z.rs', 'use');"
            ).unwrap();
        }
        let engine = AnalyzeEngine::new(db);
        let cycles = engine.detect_cycles(None).await;
        assert!(cycles.is_empty(), "Linear chain should have no cycles");
    }

    #[tokio::test]
    async fn test_detect_cycles_with_scope() {
        let db = setup_test_db();
        {
            let conn = db.lock().await;
            conn.execute_batch(
                "INSERT INTO cg_dep_graph (source_file, target_file, dep_kind) VALUES
                 ('src/a.rs', 'src/b.rs', 'use'),
                 ('src/b.rs', 'src/a.rs', 'use'),
                 ('lib/x.rs', 'lib/y.rs', 'use'),
                 ('lib/y.rs', 'lib/x.rs', 'use');"
            ).unwrap();
        }
        let engine = AnalyzeEngine::new(db);

        // 只扫描 src/ 前缀
        let cycles = engine.detect_cycles(Some("src/")).await;
        assert_eq!(cycles.len(), 1, "Should find 1 cycle in src/ scope");
        assert!(cycles[0].files.contains(&"src/a.rs".to_string()));
    }

    // ─── Impact Analysis Tests ─────────────────────────────────────────────

    #[tokio::test]
    async fn test_impact_bfs_depth_limiting() {
        let db = setup_test_db();
        seed_call_graph_data(&db).await;
        let engine = AnalyzeEngine::new(db);

        // depth=1 从 db_query 出发：只应找到 service_get（直接调用者）
        let report = engine.analyze_impact(&["s1"], 1).await;
        assert_eq!(report.impact_chains.len(), 1);
        let chain = &report.impact_chains[0];
        assert_eq!(chain.affected.len(), 1, "Depth=1 should find 1 direct caller");
        assert_eq!(chain.affected[0].symbol_id, "s2");
        assert_eq!(chain.affected[0].depth, 1);
    }

    #[tokio::test]
    async fn test_impact_bfs_full_chain() {
        let db = setup_test_db();
        seed_call_graph_data(&db).await;
        let engine = AnalyzeEngine::new(db);

        // depth=10 从 db_query 出发：应找到完整调用链 s2→s3→s4
        let report = engine.analyze_impact(&["s1"], 10).await;
        let chain = &report.impact_chains[0];
        assert_eq!(chain.affected.len(), 3, "Full chain: s2, s3, s4");

        // 验证深度递增
        assert_eq!(chain.affected[0].depth, 1); // service_get
        assert_eq!(chain.affected[1].depth, 2); // handler_api
        assert_eq!(chain.affected[2].depth, 3); // middleware_auth
    }

    #[tokio::test]
    async fn test_impact_multiple_targets() {
        let db = setup_test_db();
        seed_call_graph_data(&db).await;
        let engine = AnalyzeEngine::new(db);

        // 两个目标
        let report = engine.analyze_impact(&["s1", "s5"], 5).await;
        assert_eq!(report.impact_chains.len(), 2);

        // s5 (unrelated_fn) 没有调用者
        let chain_s5 = &report.impact_chains[1];
        assert!(chain_s5.affected.is_empty(), "unrelated_fn has no callers");
    }

    #[tokio::test]
    async fn test_impact_affected_files_dedup() {
        let db = setup_test_db();
        seed_call_graph_data(&db).await;
        let engine = AnalyzeEngine::new(db);

        let report = engine.analyze_impact(&["s1"], 10).await;
        // 受影响文件：service/get.rs, handler/api.rs, middleware/auth.rs = 3 files
        assert_eq!(report.total_affected_files, 3);
    }

    #[tokio::test]
    async fn test_impact_confidence_full() {
        let db = setup_test_db();
        seed_call_graph_data(&db).await;
        let engine = AnalyzeEngine::new(db);

        let report = engine.analyze_impact(&["s1"], 1).await;
        assert_eq!(report.confidence, 1.0, "No parse errors means full confidence");
        assert_eq!(report.degradation, CgDegradation::Normal);
    }

    #[tokio::test]
    async fn test_impact_confidence_degraded() {
        let db = setup_test_db();
        seed_call_graph_data(&db).await;
        // 添加一个有解析错误的文件
        {
            let conn = db.lock().await;
            conn.execute(
                "INSERT INTO cg_file_meta (file, hash, symbol_count, last_indexed_at, parse_errors, language, file_size) \
                 VALUES ('broken.rs', 'bh', 0, 1700000000, 3, 'rust', 100)",
                [],
            ).unwrap();
        }
        let engine = AnalyzeEngine::new(db);

        let report = engine.analyze_impact(&["s1"], 1).await;
        assert!(report.confidence < 1.0, "Parse errors should reduce confidence");
        assert_eq!(report.degradation, CgDegradation::PartialParse);
    }

    #[tokio::test]
    async fn test_impact_resolve_by_name() {
        let db = setup_test_db();
        seed_call_graph_data(&db).await;
        let engine = AnalyzeEngine::new(db);

        // 使用符号名而非 ID
        let report = engine.analyze_impact(&["db_query"], 1).await;
        assert_eq!(report.changed_symbols[0], "s1", "Should resolve name to ID");
        assert_eq!(report.impact_chains[0].affected.len(), 1);
    }

    // ─── Coupling Metrics Tests ────────────────────────────────────────────

    #[tokio::test]
    async fn test_coupling_metrics_calculation() {
        let db = setup_test_db();
        seed_coupling_data(&db).await;
        let engine = AnalyzeEngine::new(db);

        let metrics = engine.compute_coupling("core/serde.rs").await;

        // Ca: 3 files depend on core/serde.rs (handler/api.rs, service/get.rs, service/post.rs)
        assert_eq!(metrics.ca, 3, "Ca should be 3");

        // Ce: core/serde.rs depends on 2 files (core/types.rs, core/error.rs)
        assert_eq!(metrics.ce, 2, "Ce should be 2");

        // Instability: 2 / (3 + 2) = 0.4
        let expected_instability = 2.0 / 5.0;
        assert!(
            (metrics.instability - expected_instability).abs() < 1e-10,
            "Instability should be 0.4, got {}",
            metrics.instability
        );

        // Abstractness: 2 traits / 4 total = 0.5
        let expected_abstractness = 2.0 / 4.0;
        assert!(
            (metrics.abstractness - expected_abstractness).abs() < 1e-10,
            "Abstractness should be 0.5, got {}",
            metrics.abstractness
        );

        // Distance: |0.5 + 0.4 - 1.0| = 0.1
        let expected_distance = (expected_abstractness + expected_instability - 1.0).abs();
        assert!(
            (metrics.distance - expected_distance).abs() < 1e-10,
            "Distance should be ~0.1, got {}",
            metrics.distance
        );
    }

    #[tokio::test]
    async fn test_coupling_metrics_leaf_file() {
        let db = setup_test_db();
        seed_coupling_data(&db).await;
        let engine = AnalyzeEngine::new(db);

        // core/types.rs: depended on by serde.rs, depends on nothing
        let metrics = engine.compute_coupling("core/types.rs").await;

        assert_eq!(metrics.ca, 1, "One file depends on core/types.rs");
        assert_eq!(metrics.ce, 0, "core/types.rs depends on nothing");
        assert_eq!(metrics.instability, 0.0, "Leaf files have I=0 (maximally stable)");
    }

    #[tokio::test]
    async fn test_coupling_metrics_nonexistent_file() {
        let db = setup_test_db();
        let engine = AnalyzeEngine::new(db);

        let metrics = engine.compute_coupling("nonexistent.rs").await;

        assert_eq!(metrics.ca, 0);
        assert_eq!(metrics.ce, 0);
        assert_eq!(metrics.instability, 0.0);
        assert_eq!(metrics.abstractness, 0.0);
        assert_eq!(metrics.distance, 1.0, "|0 + 0 - 1| = 1.0 (zone of pain)");
    }

    // ─── Tarjan Unit Tests ─────────────────────────────────────────────────

    #[test]
    fn test_tarjan_simple_cycle() {
        let mut adj: HashMap<String, Vec<String>> = HashMap::new();
        adj.insert("A".into(), vec!["B".into()]);
        adj.insert("B".into(), vec!["A".into()]);

        let sccs = iterative_tarjan(&adj);
        let multi_node_sccs: Vec<_> = sccs.iter().filter(|s| s.len() >= 2).collect();
        assert_eq!(multi_node_sccs.len(), 1);
        assert!(multi_node_sccs[0].contains(&"A".to_string()));
        assert!(multi_node_sccs[0].contains(&"B".to_string()));
    }

    #[test]
    fn test_tarjan_triangle_cycle() {
        let mut adj: HashMap<String, Vec<String>> = HashMap::new();
        adj.insert("A".into(), vec!["B".into()]);
        adj.insert("B".into(), vec!["C".into()]);
        adj.insert("C".into(), vec!["A".into()]);

        let sccs = iterative_tarjan(&adj);
        let multi_node_sccs: Vec<_> = sccs.iter().filter(|s| s.len() >= 2).collect();
        assert_eq!(multi_node_sccs.len(), 1);
        assert_eq!(multi_node_sccs[0].len(), 3);
    }

    #[test]
    fn test_tarjan_no_cycle() {
        let mut adj: HashMap<String, Vec<String>> = HashMap::new();
        adj.insert("A".into(), vec!["B".into()]);
        adj.insert("B".into(), vec!["C".into()]);
        adj.insert("C".into(), vec![]);

        let sccs = iterative_tarjan(&adj);
        let multi_node_sccs: Vec<_> = sccs.iter().filter(|s| s.len() >= 2).collect();
        assert!(multi_node_sccs.is_empty(), "DAG should have no multi-node SCCs");
    }

    #[test]
    fn test_tarjan_two_separate_cycles() {
        let mut adj: HashMap<String, Vec<String>> = HashMap::new();
        adj.insert("A".into(), vec!["B".into()]);
        adj.insert("B".into(), vec!["A".into()]);
        adj.insert("X".into(), vec!["Y".into()]);
        adj.insert("Y".into(), vec!["Z".into()]);
        adj.insert("Z".into(), vec!["X".into()]);

        let sccs = iterative_tarjan(&adj);
        let multi_node_sccs: Vec<_> = sccs.iter().filter(|s| s.len() >= 2).collect();
        assert_eq!(multi_node_sccs.len(), 2, "Should find 2 separate cycles");
    }

    #[test]
    fn test_tarjan_self_loop() {
        let mut adj: HashMap<String, Vec<String>> = HashMap::new();
        adj.insert("A".into(), vec!["A".into()]);
        adj.insert("B".into(), vec![]);

        let sccs = iterative_tarjan(&adj);
        // Self-loop: A→A forms a single-node SCC, but since A is its own successor,
        // Tarjan will mark lowlink == index and produce SCC = {A}
        // Whether this counts depends on interpretation; we return it as size-1 SCC.
        // The filter in detect_cycles() will exclude size-1.
        let total_sccs: usize = sccs.iter().map(|s| s.len()).sum();
        assert!(total_sccs >= 1, "Should process all nodes");
    }
}
