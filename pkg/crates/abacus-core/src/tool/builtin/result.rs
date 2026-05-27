//! result — Built-in result store tool (Phase γ-E)
//!
//! ## 场景
//! 大工具输出（>8KB）被 pipeline 自动截断后存入 `CoreLoop.result_store`，
//! LLM 通过 `result.expand` 按 id 取回完整内容。延迟读取避免每轮都把巨型结果塞进 messages。
//!
//! ## 依赖
//! - `CoreLoop.result_store: Arc<RwLock<HashMap<String, Value>>>`
//! - `compute_result_id` 用 session_id+tool_id+content 生成隔离的 id（同 session 内幂等去重）
//!
//! ## 引用关系
//! - 注册：`builtin::mod.rs::register_all()` 注册 schema；`register_executor()` 由 CoreLoop::new 注入 store
//! - 执行：CoreLoop::process_turn → ToolRegistry → ResultExpandExecutor
//!
//! ## 工具
//! | Tool | Confirm | Risk | Description |
//! |------|---------|------|-------------|
//! | result.expand | no | low | 按 result_id 取回截断前完整结果 |

use std::collections::HashMap;
use std::sync::Arc;

use abacus_types::{
    KernelError, ToolCost, ToolEffectiveness, ToolHandle, ToolId, ToolProvider,
    ToolSchema, ToolSecurity, ToolState,
};
use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::RwLock;

use crate::memory_palace::DualPalaceMemory;
use crate::tool::{ExecutionContext, ToolExecutor, ToolRegistry};

/// Phase γ-Palace-D：result_store value 类型
///
/// 第二字段 source_tool_id 让 result.expand 反查源工具 → 反馈到 palace 统计
/// "expanded" 比率，让 pipeline 决定该 tool 下次截断阈值是否翻倍。
pub type ResultStoreEntry = (Value, String);

/// result_store 单 session 最大条目数。
/// 每条存储完整大工具输出（≥64KB），200 条约 ≤ 12MB 峰值。
const MAX_RESULT_STORE_ENTRIES: usize = 200;

/// 容量有界的 result store：HashMap 存数据 + VecDeque 追踪插入顺序实现 FIFO eviction。
///
/// ## 生命周期
/// - 创建：CoreLoop::new() 初始化空 store
/// - 写入：pipeline 在工具输出 > RESULT_TRUNCATE_THRESHOLD 时存入
/// - 读取：result.expand 按 result_id 取回
/// - 销毁：随 CoreLoop drop（不持久化）
///
/// ## 容量策略
/// 超过 MAX_RESULT_STORE_ENTRIES 时 FIFO 淘汰最旧条目。
/// 被淘汰 id 调用 result.expand 返回 "not found" 错误（错误文案已说明可能被 evicted）。
pub struct BoundedResultStore {
    map: HashMap<String, ResultStoreEntry>,
    /// 插入顺序追踪（FIFO eviction 依赖此队列）
    order: std::collections::VecDeque<String>,
}

impl BoundedResultStore {
    pub fn new() -> Self {
        Self {
            map: HashMap::new(),
            order: std::collections::VecDeque::new(),
        }
    }

    /// 插入条目。同 id 重复写入幂等更新值但不改顺序；新 id 超容则 FIFO 淘汰最旧。
    pub fn insert(&mut self, key: String, value: ResultStoreEntry) {
        if self.map.contains_key(&key) {
            self.map.insert(key, value);
            return;
        }
        if self.map.len() >= MAX_RESULT_STORE_ENTRIES {
            if let Some(old_key) = self.order.pop_front() {
                self.map.remove(&old_key);
            }
        }
        self.order.push_back(key.clone());
        self.map.insert(key, value);
    }

    pub fn get(&self, key: &str) -> Option<&ResultStoreEntry> {
        self.map.get(key)
    }

    pub fn values(&self) -> impl Iterator<Item = &ResultStoreEntry> {
        self.map.values()
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

impl Default for BoundedResultStore {
    fn default() -> Self { Self::new() }
}

pub type ResultStore = Arc<RwLock<BoundedResultStore>>;

/// 大结果阈值（字节数）。超过则截断 + 存 store。
/// 参考 Claude Code：激进截断控制单条工具输出对 context 的冲击。
/// 16KB 覆盖 99% 文件读取场景；超大输出（grep 5000行）走 result.expand 延迟读取。
pub const RESULT_TRUNCATE_THRESHOLD: usize = 16384; // 16KB

/// 截断后 head/tail 各保留的字节数（合计 ~4KB）。
/// 增大 keep 保留更多上下文，弥补阈值降低的信息损失。
pub const RESULT_TRUNCATE_KEEP: usize = 2048; // 2KB head + 2KB tail

/// Compute a stable result_id from session_id + tool_id + content hash.
///
/// 同一 session 内同 tool 同输出 → 同 id → 自然去重；不同 session 间 id 不复用（隔离）。
pub fn compute_result_id(session_id: &str, tool_id: &str, content: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    session_id.hash(&mut h);
    tool_id.hash(&mut h);
    content.hash(&mut h);
    format!("rs_{:016x}", h.finish())
}

/// 把 large_output 截断为 head+tail+meta 摘要 Value。
///
/// 返回截断后的轻量 JSON（LLM 看到的内容）。
pub fn build_truncated_summary(
    result_id: &str,
    original: &Value,
    full_size_bytes: usize,
) -> Value {
    let serialized = serde_json::to_string(original).unwrap_or_default();
    let head: String = serialized.chars().take(RESULT_TRUNCATE_KEEP).collect();
    let tail: String = if serialized.len() > 2 * RESULT_TRUNCATE_KEEP {
        serialized.chars().skip(serialized.len().saturating_sub(RESULT_TRUNCATE_KEEP)).collect()
    } else {
        String::new()
    };
    json!({
        "_truncated": true,
        "result_id": result_id,
        "size_bytes": full_size_bytes,
        "head": head,
        "tail": tail,
        "hint": format!(
            "Result truncated ({} bytes). Call result.expand with id={} to retrieve the full output.",
            full_size_bytes, result_id
        ),
    })
}

/// Executor for result.expand — 持有 store Arc + 可选 palace 引用
///
/// expand 时除返回完整 value 外，还会向 palace 写入 "expanded:{source_tool}" pattern
/// 用于反馈"该工具的输出过大、需要二次 expand"的统计信号。
pub struct ResultExpandExecutor {
    store: ResultStore,
    palace: Option<Arc<RwLock<DualPalaceMemory>>>,
}

impl ResultExpandExecutor {
    pub fn new(store: ResultStore) -> Self {
        Self { store, palace: None }
    }

    /// Phase γ-Palace-D：注入 palace 让 expand 时反馈统计
    pub fn with_palace(mut self, palace: Arc<RwLock<DualPalaceMemory>>) -> Self {
        self.palace = Some(palace);
        self
    }
}

#[async_trait]
impl ToolExecutor for ResultExpandExecutor {
    async fn execute(&self, tool_id: &ToolId, params: Value, _ctx: &ExecutionContext) -> abacus_types::Result<Value> {
        if tool_id.0 != "result_expand" {
            return Err(KernelError::Other(format!("unknown tool: {}", tool_id.0)));
        }
        let result_id = params.get("result_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| KernelError::Other("missing required parameter: result_id".into()))?;

        let store = self.store.read().await;
        match store.get(result_id) {
            Some((full, source_tool)) => {
                // Phase γ-Palace-D：反馈到 palace —— 用 "expanded:{tool}" 模式累积
                if let Some(ref palace) = self.palace {
                    let pattern = format!("expanded:{source_tool}");
                    let p = palace.read().await;
                    p.behavior.record_interaction(
                        &pattern,
                        &["expand".to_string(), source_tool.clone()],
                    ).await;
                }
                Ok(json!({
                    "result_id": result_id,
                    "source_tool": source_tool,
                    "expanded": full.clone(),
                }))
            }
            None => Err(KernelError::Other(format!(
                "result_id not found: {} (may have been evicted or session restarted)",
                result_id
            ))),
        }
    }
}

pub fn schemas() -> Vec<ToolSchema> {
    vec![
        ToolSchema {
            name: "result_expand".into(),
            description: "Retrieve the full content of a previously truncated tool result by its result_id (returned in the _truncated payload).".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "result_id": {"type": "string", "description": "The result_id from a _truncated tool output."}
                },
                "required": ["result_id"],
            }),
            returns: None,
            security: Some(ToolSecurity {
                allowed_paths: None,
                max_size_mb: None,
                confirm_required: false,
                needs_sandbox: false,
            }),
            cost: Some(ToolCost { tokens: 16, latency: "1ms".into(), risk: "low".into() }),
            examples: Vec::new(),
            applicable_task_kinds: None,
            idempotent: true, // 同 id 永远返回相同 store 内容
        },
    ]
}

/// 注册 schema（无 store；register_executors 才注入实际可用的 executor）
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

/// 注册 executor（与 schema 分离：store 在 CoreLoop::new() 才可用）
///
/// `palace` 可选；提供时 expand 行为反馈到行为宫殿，让后续 pipeline 据此决定是否调阈值。
pub async fn register_executors(
    registry: &ToolRegistry,
    store: ResultStore,
    palace: Option<Arc<RwLock<DualPalaceMemory>>>,
) {
    let mut exec = ResultExpandExecutor::new(store);
    if let Some(p) = palace {
        exec = exec.with_palace(p);
    }
    let executor = Arc::new(exec);
    for s in schemas() {
        registry.register_executor(ToolId(s.name.clone()), executor.clone()).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_result_id_stable() {
        let a = compute_result_id("s1", "filengine_fs_read", "hello");
        let b = compute_result_id("s1", "filengine_fs_read", "hello");
        assert_eq!(a, b, "same inputs → same id");
    }

    #[test]
    fn test_compute_result_id_distinguishes_sessions() {
        let a = compute_result_id("s1", "filengine_fs_read", "hello");
        let b = compute_result_id("s2", "filengine_fs_read", "hello");
        assert_ne!(a, b, "different session → different id");
    }

    #[test]
    fn test_build_truncated_summary_includes_meta() {
        let big = json!({"data": "x".repeat(10000)});
        let summary = build_truncated_summary("rs_abc", &big, 12345);
        assert_eq!(summary["_truncated"], true);
        assert_eq!(summary["result_id"], "rs_abc");
        assert_eq!(summary["size_bytes"], 12345);
        assert!(summary["head"].as_str().unwrap().len() <= RESULT_TRUNCATE_KEEP);
        assert!(summary["hint"].as_str().unwrap().contains("rs_abc"));
    }

    #[tokio::test]
    async fn test_executor_returns_stored_value() {
        let store: ResultStore = Arc::new(RwLock::new(BoundedResultStore::new()));
        store.write().await.insert("rs_test".into(), (json!({"big": "content"}), "filengine_fs_read".to_string()));
        let exec = ResultExpandExecutor::new(store);
        let ctx = crate::tool::ExecutionContext::noop("test");
        let res = exec.execute(
            &ToolId("result_expand".into()),
            json!({"result_id": "rs_test"}),
            &ctx,
        ).await.unwrap();
        assert_eq!(res["expanded"]["big"], "content");
        assert_eq!(res["source_tool"], "filengine_fs_read");
    }

    #[tokio::test]
    async fn test_executor_errors_on_missing_id() {
        let store: ResultStore = Arc::new(RwLock::new(BoundedResultStore::new()));
        let exec = ResultExpandExecutor::new(store);
        let ctx = crate::tool::ExecutionContext::noop("test");
        let res = exec.execute(
            &ToolId("result_expand".into()),
            json!({"result_id": "rs_missing"}),
            &ctx,
        ).await;
        assert!(res.is_err());
    }

    /// Phase γ-Palace-D：expand 时反馈到 palace
    #[tokio::test]
    async fn test_executor_records_to_palace_on_expand() {
        let store: ResultStore = Arc::new(RwLock::new(BoundedResultStore::new()));
        store.write().await.insert("rs_x".into(), (json!({}), "filengine_fs_read".to_string()));
        let palace = Arc::new(RwLock::new(DualPalaceMemory::new()));
        let exec = ResultExpandExecutor::new(store).with_palace(palace.clone());
        let ctx = crate::tool::ExecutionContext::noop("test");
        let _ = exec.execute(
            &ToolId("result_expand".into()),
            json!({"result_id": "rs_x"}),
            &ctx,
        ).await.unwrap();
        // palace 应该有 "expanded:filengine_fs_read" pattern
        let p = palace.read().await;
        let snapshot = p.behavior.snapshot().await;
        assert!(snapshot.contains_key("expanded:filengine_fs_read"),
            "expand 后 palace 应记录 expanded:{{tool}} pattern");
    }
}
