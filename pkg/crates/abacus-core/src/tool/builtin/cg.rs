//! cg — CodeGraph 内置工具（代码知识图谱）
//!
//! ## 场景
//! 提供代码符号索引、调用图遍历、依赖分析、结构分析能力。
//! 基于 tree-sitter 多语言解析 + SQLite FTS5 全文搜索。
//!
//! ## 依赖
//! - `crate::code_graph::CodeGraphManager`: 核心逻辑层
//!
//! ## 引用关系
//! - 被 `builtin::mod.rs::register_all()` 注册 schemas
//! - 被 `CoreLoop::enable_code_graph()` 注册 executors
//! - 被 `CoreLoop::process_turn()` 通过 ToolRegistry 执行
//!
//! ## 注册工具 (4)
//! | Tool | Confirm | Risk | Idempotent | Description |
//! |------|---------|------|------------|-------------|
//! | cg_index | no | low | no | 索引代码文件（tree-sitter 解析） |
//! | cg_query | no | low | yes | 符号搜索（FTS5 trigram） |
//! | cg_graph | no | low | yes | 图遍历（callers/callees/deps） |
//! | cg_analyze | no | low | yes | 结构分析（impact/cycles/coupling） |

use std::sync::Arc;

use abacus_types::{
    KernelError, ToolCost, ToolEffectiveness, ToolHandle, ToolId, ToolProvider,
    ToolSchema, ToolSecurity, ToolState,
};
use async_trait::async_trait;
use serde_json::{json, Value};

use crate::code_graph::CodeGraphManager;
use crate::code_graph::indexer::IndexStrategy;
use crate::code_graph::query::GraphDirection;
use crate::tool::{ExecutionContext, ToolExecutor, ToolRegistry};

// ─── Executor ───────────────────────────────────────────────────────────────

/// CodeGraph 工具执行器
///
/// ## 生命周期
/// - 创建：CoreLoop::enable_code_graph() → register_executors() 时
/// - 存活：与 ToolRegistry 同生命周期
pub struct CgToolExecutor {
    manager: Arc<CodeGraphManager>,
}

impl CgToolExecutor {
    pub fn new(manager: Arc<CodeGraphManager>) -> Self {
        Self { manager }
    }

    async fn cg_index(&self, params: Value) -> abacus_types::Result<Value> {
        let path = params.get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| KernelError::Other("missing required parameter: path".into()))?;
        let recursive = params.get("recursive").and_then(|v| v.as_bool()).unwrap_or(true);

        let strategy = if std::path::Path::new(path).is_file() {
            IndexStrategy::Files { files: vec![path.into()] }
        } else if recursive {
            IndexStrategy::Full { workspace: path.into() }
        } else {
            // 非递归目录：只索引直接子文件
            let mut files = Vec::new();
            if let Ok(entries) = std::fs::read_dir(path) {
                for entry in entries.flatten() {
                    if entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                        files.push(entry.path());
                    }
                }
            }
            IndexStrategy::Files { files }
        };

        let report = self.manager.index(strategy).await
            .map_err(KernelError::Other)?;

        Ok(json!({
            "status": "indexed",
            "report": {
                "totalFiles": report.total_files,
                "indexed": report.indexed,
                "skippedUnchanged": report.skipped_unchanged,
                "failed": report.failed,
                "parseErrors": report.parse_errors,
                "durationMs": report.duration_ms,
                "symbolsAdded": report.symbols_added,
                "callsAdded": report.calls_added,
                "depsAdded": report.deps_added,
            }
        }))
    }

    async fn cg_query(&self, params: Value) -> abacus_types::Result<Value> {
        let query = params.get("query")
            .and_then(|v| v.as_str())
            .ok_or_else(|| KernelError::Other("missing required parameter: query".into()))?;
        let kind_filter = params.get("kind").and_then(|v| v.as_str());
        let file_filter = params.get("file").and_then(|v| v.as_str());
        let limit = params.get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(10)
            .min(50) as u32;

        // search_symbols returns (Vec<SymbolResult>, CgDegradation) directly
        let (symbols, degradation) = self.manager.query()
            .search_symbols(query, kind_filter, file_filter, limit).await;

        let degradation_str = if symbols.is_empty() {
            "NoIndex"
        } else {
            degradation.as_str()
        };

        Ok(json!({
            "results": symbols.iter().map(|s| json!({
                "id": s.symbol_id,
                "name": s.name,
                "kind": s.kind,
                "file": s.file,
                "line": s.line,
                "signature": s.signature,
                "score": s.score,
            })).collect::<Vec<_>>(),
            "degradation": degradation_str,
            "resultCount": symbols.len(),
        }))
    }

    async fn cg_graph(&self, params: Value) -> abacus_types::Result<Value> {
        let target = params.get("target")
            .and_then(|v| v.as_str())
            .ok_or_else(|| KernelError::Other("missing required parameter: target".into()))?;
        let direction_str = params.get("direction")
            .and_then(|v| v.as_str())
            .unwrap_or("callers");
        let depth = params.get("depth")
            .and_then(|v| v.as_u64())
            .unwrap_or(1)
            .min(5) as u32;
        let limit = params.get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(20)
            .min(100) as u32;

        let direction = match direction_str {
            "callers" => GraphDirection::Callers,
            "callees" => GraphDirection::Callees,
            "deps" => GraphDirection::Deps,
            "rdeps" => GraphDirection::ReverseDeps,
            _ => return Err(KernelError::Other(
                format!("invalid direction: {direction_str}. Must be callers|callees|deps|rdeps")
            )),
        };

        // graph_traverse returns GraphResult directly (not Result)
        let result = self.manager.query()
            .graph_traverse(target, direction, depth, limit).await;

        Ok(json!({
            "target": target,
            "direction": direction_str,
            "depth": depth,
            "nodes": result.nodes,
            "edges": result.edges,
            "degradation": result.degradation.as_str(),
        }))
    }

    async fn cg_analyze(&self, params: Value) -> abacus_types::Result<Value> {
        let mode = params.get("mode")
            .and_then(|v| v.as_str())
            .ok_or_else(|| KernelError::Other("missing required parameter: mode".into()))?;
        let target = params.get("target").and_then(|v| v.as_str()).unwrap_or(".");
        let depth = params.get("depth")
            .and_then(|v| v.as_u64())
            .unwrap_or(3)
            .min(10) as u32;

        match mode {
            "impact" => {
                // analyze_impact takes &[&str] slice and returns ImpactReport directly
                let report = self.manager.analyze()
                    .analyze_impact(&[target], depth).await;
                Ok(json!({
                    "mode": "impact",
                    "changedSymbols": report.changed_symbols.len(),
                    "impactChains": report.impact_chains,
                    "totalAffectedFiles": report.total_affected_files,
                    "confidence": report.confidence,
                    "degradation": report.degradation.as_str(),
                }))
            }
            "cycles" => {
                // detect_cycles takes Option<&str> and returns Vec<DependencyCycle> directly
                let scope = if target == "." { None } else { Some(target) };
                let cycles = self.manager.analyze()
                    .detect_cycles(scope).await;
                Ok(json!({
                    "mode": "cycles",
                    "cyclesFound": cycles.len(),
                    "cycles": cycles,
                }))
            }
            "coupling" => {
                // compute_coupling returns CouplingMetrics directly
                let metrics = self.manager.analyze()
                    .compute_coupling(target).await;
                Ok(json!({
                    "mode": "coupling",
                    "metrics": metrics,
                }))
            }
            _ => Err(KernelError::Other(
                format!("invalid mode: {mode}. Must be impact|cycles|coupling")
            )),
        }
    }
}

#[async_trait]
impl ToolExecutor for CgToolExecutor {
    async fn execute(&self, tool_id: &ToolId, params: Value, _ctx: &ExecutionContext) -> abacus_types::Result<Value> {
        match tool_id.0.as_str() {
            "cg_index" => self.cg_index(params).await,
            "cg_query" => self.cg_query(params).await,
            "cg_graph" => self.cg_graph(params).await,
            "cg_analyze" => self.cg_analyze(params).await,
            _ => Err(KernelError::Other(format!("unknown cg tool: {}", tool_id.0))),
        }
    }
}

// ─── Schema ─────────────────────────────────────────────────────────────────

fn cg_schema(
    name: &str,
    desc: &str,
    props: Value,
    required: &[&str],
    tokens: u32,
    latency: &str,
    idempotent: bool,
) -> ToolSchema {
    ToolSchema {
        short_description: None,
        name: name.into(),
        description: desc.into(),
        parameters: json!({
            "type": "object",
            "properties": props,
            "required": required,
        }),
        returns: None,
        security: Some(ToolSecurity {
            allowed_paths: None,
            max_size_mb: Some(10),
            confirm_required: false,
            needs_sandbox: false,
        }),
        cost: Some(ToolCost { tokens, latency: latency.into(), risk: "low".into() }),
        examples: Vec::new(),
        applicable_task_kinds: Some(vec![
            "code_reading".into(),
            "debugging".into(),
            "architecture".into(),
            "refactoring".into(),
            "code_review".into(),
        ]),
        idempotent,
        schema_stable: true,
    }
}

pub fn schemas() -> Vec<ToolSchema> {
    let mut v = vec![
        cg_schema(
            "cg_index",
            "Index code files into CodeGraph (tree-sitter parse + symbol extraction)",
            json!({
                "path": {"type": "string", "description": "File or directory path to index"},
                "recursive": {"type": "boolean", "description": "Recurse into subdirs (default true)"}
            }),
            &["path"],
            64, "500ms", false,
        ),
        cg_schema(
            "cg_query",
            "Search code symbols (FTS5 trigram + kind/file filter)",
            json!({
                "query": {"type": "string", "description": "Search query (supports prefix/fuzzy)"},
                "kind": {"type": "string", "description": "Filter: function|struct|trait|class|interface|..."},
                "file": {"type": "string", "description": "Filter: file path substring"},
                "limit": {"type": "integer", "description": "Max results (default 10, max 50)"}
            }),
            &["query"],
            48, "30ms", true,
        ),
        cg_schema(
            "cg_graph",
            "Traverse code graph (call chains / dependency chains)",
            json!({
                "target": {"type": "string", "description": "Symbol name, symbol_id, or file path"},
                "direction": {"type": "string", "description": "callers|callees|deps|rdeps"},
                "depth": {"type": "integer", "description": "Traversal depth (default 1, max 5)"},
                "limit": {"type": "integer", "description": "Max nodes per level (default 20)"}
            }),
            &["target"],
            96, "100ms", true,
        ),
        cg_schema(
            "cg_analyze",
            "Structural analysis (diff-impact / cycle detection / coupling metrics)",
            json!({
                "mode": {"type": "string", "description": "impact|cycles|coupling"},
                "target": {"type": "string", "description": "Target path or diff ref"},
                "depth": {"type": "integer", "description": "Impact traversal depth (default 3)"}
            }),
            &["mode"],
            128, "200ms", true,
        ),
    ];
    // Short-Mode 短描述注入
    for s in v.iter_mut() {
        s.short_description = Some(match s.name.as_str() {
            "cg_index"   => "Index code into symbol graph",
            "cg_query"   => "Search code symbols",
            "cg_graph"   => "Traverse call/dep chains",
            "cg_analyze" => "Impact/cycles/coupling analysis",
            _ => continue,
        }.into());
    }
    v
}

// ─── Registration ───────────────────────────────────────────────────────────

/// 注册 CodeGraph 工具 schemas（不含 executors）
///
/// 在 register_all() 中调用。即使 CodeGraphManager 未启用，
/// schemas 也会注册（LLM 可见但调用会返回 "no executor" 错误）。
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

/// 注册 CodeGraph executors（需要 CodeGraphManager 实例）
///
/// 由 CoreLoop::enable_code_graph() 在 Manager 初始化后调用。
pub async fn register_executors(
    registry: &ToolRegistry,
    manager: Arc<CodeGraphManager>,
) {
    let executor = Arc::new(CgToolExecutor::new(manager));
    for s in schemas() {
        registry.register_executor(ToolId(s.name.clone()), executor.clone()).await;
    }
}
