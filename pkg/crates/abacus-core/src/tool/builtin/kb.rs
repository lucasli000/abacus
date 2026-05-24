//! kb — Built-in knowledge base tools
//!
//! ## 场景
//! 提供文件摄入、语义搜索、多源检索能力。
//! 依赖 `knowledge_store::KnowledgeStore` 作为持久化后端。
//!
//! ## 依赖
//! - `crate::knowledge_store::KnowledgeStore`: KB 持久化层
//! - `crate::memory_palace::DualPalaceMemory`: Memory Palace（kb.search 多源合并）
//!
//! ## 引用关系
//! - 被 `builtin::mod.rs::register_all()` 调用注册
//! - 被 `CoreLoop::process_turn()` 通过 ToolRegistry 执行
//!
//! ## 注册工具 (3)
//! | Tool | Confirm | Risk | Description |
//! |------|---------|------|-------------|
//! | kb.ingest | no | low | 文件摄入（chunk + FTS5 索引） |
//! | kb.query | no | low | BM25 语义搜索 |
//! | kb.search | no | low | 多源检索（KB + Memory Palace） |

use std::sync::Arc;

use abacus_types::{
    KernelError, ToolCost, ToolEffectiveness, ToolHandle, ToolId, ToolProvider,
    ToolSchema, ToolSecurity, ToolState,
};
use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::RwLock;

use crate::knowledge_store::KnowledgeStore;
use crate::memory_palace::DualPalaceMemory;
use crate::tool::{ExecutionContext, ToolExecutor, ToolRegistry};

// ─── Degradation 信号 ───────────────────────────────────────────────────

/// 检索结果质量信号（§11.5.6 设计规格）
///
/// 消费者（CoreLoop）必须据此决定如何使用检索结果：
/// - Normal: 正常使用
/// - WeakSignal: 标记 [未验证] 后使用
/// - ZeroHit: 禁止作为答案依据
/// - Unavailable: 纯 BM25 结果，未经语义验证
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DegradationLevel {
    Normal,
    WeakSignal,
    ZeroHit,
    Unavailable,
}

impl DegradationLevel {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Normal => "Normal",
            Self::WeakSignal => "WeakSignal",
            Self::ZeroHit => "ZeroHit",
            Self::Unavailable => "Unavailable",
        }
    }
}

// ─── Executor ───────────────────────────────────────────────────────────

/// KB 工具执行器
///
/// ## 场景
/// 接收 kb.ingest / kb.query / kb.search 调用。
///
/// ## 生命周期
/// - 创建：register_executors() 时
/// - 存活：与 ToolRegistry 同生命周期
pub struct KbToolExecutor {
    store: Arc<KnowledgeStore>,
    palace: Arc<RwLock<DualPalaceMemory>>,
}

impl KbToolExecutor {
    pub fn new(store: Arc<KnowledgeStore>, palace: Arc<RwLock<DualPalaceMemory>>) -> Self {
        Self { store, palace }
    }

    async fn kb_ingest(&self, params: Value) -> abacus_types::Result<Value> {
        let path = params.get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| KernelError::Other("missing required parameter: path".into()))?;
        let force = params.get("force").and_then(|v| v.as_bool()).unwrap_or(false);

        self.store.ingest(path, force).await
            .map_err(KernelError::Other)
    }

    async fn kb_query(&self, params: Value) -> abacus_types::Result<Value> {
        let query = params.get("query")
            .and_then(|v| v.as_str())
            .ok_or_else(|| KernelError::Other("missing required parameter: query".into()))?;
        let top_k = params.get("topK")
            .and_then(|v| v.as_u64())
            .unwrap_or(5)
            .min(20) as usize;
        let file_filter = params.get("fileFilter").and_then(|v| v.as_str());

        let results = self.store.query(query, top_k, file_filter).await
            .map_err(KernelError::Other)?;

        // 计算 degradation level
        let degradation = if results.is_empty() {
            DegradationLevel::ZeroHit
        } else if results[0].score < 0.2 {
            DegradationLevel::WeakSignal
        } else {
            DegradationLevel::Normal
        };

        let result_json: Vec<Value> = results.iter().map(|r| {
            json!({
                "chunkId": r.chunk_id,
                "file": r.file,
                "chunkIdx": r.chunk_idx,
                "score": r.score,
                "content": r.content,
                "headingPath": r.heading_path,
            })
        }).collect();

        Ok(json!({
            "results": result_json,
            "degradation": {
                "level": degradation.as_str(),
                "resultCount": results.len(),
                "topScore": results.first().map(|r| r.score).unwrap_or(0.0),
            },
        }))
    }

    async fn kb_search(&self, params: Value) -> abacus_types::Result<Value> {
        let query = params.get("query")
            .and_then(|v| v.as_str())
            .ok_or_else(|| KernelError::Other("missing required parameter: query".into()))?;
        let limit = params.get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(10)
            .min(20) as usize;
        let show_content = params.get("showContent")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        // Source 1: KB chunks (FTS5)
        let kb_results = self.store.query(query, limit, None).await
            .map_err(KernelError::Other)?;

        // Source 2: Memory Palace knowledge entries
        let palace = self.palace.read().await;
        let palace_results = palace.knowledge.search(query).await;

        // 合并结果，统一格式
        let mut unified: Vec<Value> = Vec::new();

        for r in &kb_results {
            unified.push(json!({
                "source": "kb",
                "id": r.chunk_id,
                "score": r.score,
                "content": if show_content { r.content.as_str() } else { "" },
                "origin": r.file,
                "headingPath": r.heading_path,
            }));
        }

        for entry in &palace_results {
            // Palace entries 没有 BM25 score，用固定 0.5 + SM-2 ease 加权
            let score = 0.5 * (entry.sm2_ease / 2.5).min(1.0);
            unified.push(json!({
                "source": "palace",
                "id": entry.id,
                "score": score,
                "content": if show_content { entry.content.as_str() } else { "" },
                "origin": entry.domain,
                "headingPath": entry.title,
            }));
        }

        // 按 score 降序排序
        unified.sort_by(|a, b| {
            let sa = a["score"].as_f64().unwrap_or(0.0);
            let sb = b["score"].as_f64().unwrap_or(0.0);
            sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
        });

        unified.truncate(limit);

        // Degradation
        let degradation = if unified.is_empty() {
            DegradationLevel::ZeroHit
        } else {
            let top = unified[0]["score"].as_f64().unwrap_or(0.0);
            if top < 0.2 {
                DegradationLevel::WeakSignal
            } else {
                DegradationLevel::Normal
            }
        };

        Ok(json!({
            "results": unified,
            "degradation": {
                "level": degradation.as_str(),
                "resultCount": unified.len(),
                "kbHits": kb_results.len(),
                "palaceHits": palace_results.len(),
            },
        }))
    }
}

#[async_trait]
impl ToolExecutor for KbToolExecutor {
    async fn execute(&self, tool_id: &ToolId, params: Value, _ctx: &ExecutionContext) -> abacus_types::Result<Value> {
        match tool_id.0.as_str() {
            "kb_ingest" => self.kb_ingest(params).await,
            "kb_query" => self.kb_query(params).await,
            "kb_search" => self.kb_search(params).await,
            _ => Err(KernelError::Other(format!("unknown kb tool: {}", tool_id.0))),
        }
    }
}

// ─── Schema ─────────────────────────────────────────────────────────────

fn kb_schema(name: &str, desc: &str, props: Value, required: &[&str],
             tokens: u32, latency: &str) -> ToolSchema {
    ToolSchema {
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
        applicable_task_kinds: None,
        // kb.query/search 是 idempotent；ingest 写入故 false
        idempotent: matches!(name, "kb_query" | "kb_search"),
    }
}

pub fn schemas() -> Vec<ToolSchema> {
    vec![
        kb_schema("kb_ingest", "摄入文件到知识库（chunk + FTS5 索引）",
            json!({
                "path": {"type": "string", "description": "文件绝对路径"},
                "force": {"type": "boolean", "description": "强制重新摄入(忽略hash校验)"}
            }),
            &["path"], 48, "200ms"),
        kb_schema("kb_query", "知识库语义搜索（BM25 + trigram）",
            json!({
                "query": {"type": "string", "description": "搜索查询"},
                "topK": {"type": "integer", "description": "返回最大条数(默认5，最大20)"},
                "fileFilter": {"type": "string", "description": "文件路径子串过滤(可选)"}
            }),
            &["query"], 64, "50ms"),
        kb_schema("kb_search", "多源检索（KB chunks + Memory Palace，统一排序）",
            json!({
                "query": {"type": "string", "description": "搜索查询"},
                "limit": {"type": "integer", "description": "返回最大条数(默认10，最大20)"},
                "showContent": {"type": "boolean", "description": "是否返回内容片段(默认false)"}
            }),
            &["query"], 96, "100ms"),
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

pub async fn register_executors(
    registry: &ToolRegistry,
    store: Arc<KnowledgeStore>,
    palace: Arc<RwLock<DualPalaceMemory>>,
) {
    let executor = Arc::new(KbToolExecutor::new(store, palace));
    for s in schemas() {
        registry.register_executor(ToolId(s.name.clone()), executor.clone()).await;
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory_palace::DualPalaceMemory;

    #[tokio::test]
    async fn test_kb_ingest_via_executor() {
        let store = Arc::new(KnowledgeStore::in_memory().unwrap());
        let palace = Arc::new(RwLock::new(DualPalaceMemory::new()));
        let executor = KbToolExecutor::new(store, palace);

        let tmp = "/tmp/abacus_kb_tool_test.md";
        std::fs::write(tmp, "# Test\n\nSome content about testing knowledge base tools.\n").unwrap();

        let noop_ctx = crate::tool::ExecutionContext::noop("test");
        let result = executor.execute(
            &ToolId("kb_ingest".into()),
            json!({"path": tmp}),
            &noop_ctx,
        ).await.unwrap();

        assert_eq!(result["status"], "ingested");
        let _ = std::fs::remove_file(tmp);
    }

    #[tokio::test]
    async fn test_kb_query_degradation() {
        let store = Arc::new(KnowledgeStore::in_memory().unwrap());
        let palace = Arc::new(RwLock::new(DualPalaceMemory::new()));
        let executor = KbToolExecutor::new(store, palace);

        // Query empty KB → ZeroHit
        let noop_ctx = crate::tool::ExecutionContext::noop("test");
        let result = executor.execute(
            &ToolId("kb_query".into()),
            json!({"query": "nonexistent topic"}),
            &noop_ctx,
        ).await.unwrap();

        assert_eq!(result["degradation"]["level"], "ZeroHit");
        assert_eq!(result["degradation"]["resultCount"], 0);
    }

    #[tokio::test]
    async fn test_kb_search_merges_sources() {
        let store = Arc::new(KnowledgeStore::in_memory().unwrap());
        let palace = Arc::new(RwLock::new(DualPalaceMemory::new()));

        // 写入 palace 知识
        {
            let p = palace.write().await;
            use crate::memory_palace::KnowledgeEntry;
            let entry = KnowledgeEntry::new("k1", "Rust patterns", "Pattern matching in Rust", "programming");
            p.knowledge.store(entry).await;
        }

        let executor = KbToolExecutor::new(store, palace);
        let noop_ctx = crate::tool::ExecutionContext::noop("test");
        let result = executor.execute(
            &ToolId("kb_search".into()),
            json!({"query": "Rust", "showContent": true}),
            &noop_ctx,
        ).await.unwrap();

        // Palace 结果应该出现
        let results = result["results"].as_array().unwrap();
        assert!(results.iter().any(|r| r["source"] == "palace"));
    }
}
