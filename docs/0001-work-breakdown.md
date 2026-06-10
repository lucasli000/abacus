# Content Lifecycle Management — 任务级工作分解（含代码框架）

**总任务数：38 | 总预估：17 天**

---

## W1：基础优化

### W1-D1 (1/2): Mission Brief V2

---

#### 1.1 `SessionCheckpoint` 扩展字段

**文件：** `core/context.rs`，`SessionCheckpoint` struct 附近

```rust
// 新增字段（现有 struct 已含 accomplished/current_topic/pending，只需 getter）
impl SessionCheckpoint {
    /// mission 摘要格式，用于 Awareness Block
    pub fn mission_brief(&self) -> String {
        let done: Vec<&str> = self.accomplished.iter().map(|s| s.as_str()).collect();
        let todo: Vec<&str> = self.pending.iter().map(|p| p.task.as_str()).collect();
        let topic = if self.current_topic.is_empty() { "—" } else { &self.current_topic };
        format!("Mission: {} | ✓ {} | ↻ {}",
            topic,
            done.join(", "),
            todo.join(", "),
        )
    }
}
```

**验证：** ` cargo test -- context::session_checkpoint_mission_brief `（新增测试）

---

#### 1.2 渲染 Mission Brief + Context Bar

**文件：** `core/mod.rs` 中的 `build_system_output()`，Awareness 段

```rust
// 在 build_system_output 内部，替换旧的 Awareness 块渲染
async fn render_awareness_block(core: &CoreLoop) -> (String, bool) {
    // Mission Brief（从 checkpoint 取，总是有值）
    let cps = core.context_manager.get_checkpoints().await;
    let brief = cps.last().map(|cp| cp.mission_brief())
        .unwrap_or_else(|| "Mission: —".into());

    // Context Bar（从 context window 取）
    let window = core.context_manager.window.read().await;
    let usage = window.usage_pct();
    let bar = format!("[Context: {:.0}%]", usage);

    // Triage stats（先放固定值，后续接入后替换）
    let triage = "[Triage: Standby 0 | Cold 0]";

    let text = format!("---\n{}\n{}\n{}\n---", brief, bar, triage);
    let has_dynamic = true; // usage 每轮变
    (text, has_dynamic)
}
```

然后在 `PromptAssembly::assemble()` 中，将此块插入到 `Layer 255`（Kernel）之前，而不是现有的末尾追加。

```rust
// prompt_assembly.rs assemble() 方法
fn assemble(&self, layers: Vec<PromptLayer>) -> SystemPromptOutput {
    let mut segments = Vec::new();
    // ... 原有各层 ...
    // Layer 255: Kernel (原先第1个)
    // Layer 230: abacusbr_core
    // ...
    // 新 Awareness 块（插入这里，在全部 system 层之后但在 messages 之前）
    segments.push(SystemSegment {
        text: awareness_block,   // ← Mission Brief + Context Bar
        cacheable: false,        // 不 cache——usage 每轮变
    });
    // 原尾部 Awareness 块删除
    SystemPromptOutput { text: segments.join("\n\n"), segments }
}
```

**验证：** ` cargo check ` + 肉眼检查 segment 顺序 log

---

#### 1.3 Token 预计算（Stable 段 cache）

**文件：** `core/context.rs`，`estimate_total_tokens` 附近

```rust
// 新增：system prompt 稳定段 token cache
// 仅在 system prompt 文本变化时重算
pub struct SystemTokenCache {
    stable_hash: u64,      // hash of all static layers
    stable_tokens: usize,  // pre-computed token count
}

impl ContextManager {
    pub fn get_system_tokens(&self, system_text: &str) -> usize {
        let hash = /* hash of system_text */;
        if hash == self.system_cache.stable_hash {
            return self.system_cache.stable_tokens;
        }
        let tokens = estimate_tokens(system_text);
        self.system_cache = SystemTokenCache { stable_hash: hash, stable_tokens: tokens };
        tokens
    }
}
```

**验证：** ` cargo test -- context::system_token_cache `

---

### W1-D1 (2/2): ToolCache

#### 2.1 ToolSpecKey + Cache Map

**文件：** `llm/tool_view.rs`，文件顶部

```rust
use std::collections::HashMap;
use std::sync::RwLock;
use std::hash::{Hash, Hasher};

/// 工具 schema 缓存键——用 hash 替代全文比较
#[derive(Clone)]
struct ToolSpecKey {
    full_hash: u64,  // hash(name | description | parameters)
}

impl ToolSpecKey {
    fn from_spec(name: &str, desc: &Option<String>, params: &serde_json::Value) -> Self {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        name.hash(&mut h);
        desc.hash(&mut h);
        // JSON Value 的 Hash 实现基于内容而非指针
        params.hash(&mut h);
        Self { full_hash: h.finish() }
    }
}

/// 全局工具 schema 缓存（LRU，容量 128）
pub(crate) struct ToolSpecCache {
    inner: RwLock<HashMap<u64, ToolFunctionSpec>>,
    order: RwLock<Vec<u64>>,  // FIFO evict order
    cap: usize,
}

impl ToolSpecCache {
    pub fn new(cap: usize) -> Self { /* ... */ }
    pub fn get(&self, key: &ToolSpecKey) -> Option<ToolFunctionSpec> { /* ... */ }
    pub fn insert(&self, key: ToolSpecKey, spec: ToolFunctionSpec) { /* ... */ }
}

// 全局单例——所有 provider 共享
pub(crate) static TOOL_CACHE: once_cell::sync::Lazy<ToolSpecCache> =
    once_cell::sync::Lazy::new(|| ToolSpecCache::new(128));
```

**验证：** ` cargo test -- tool_view::tool_cache `

---

#### 2.2 缓存接入 tool_handle_to_llm_spec

**文件：** `llm/tool_view.rs:44`

```rust
pub fn tool_handle_to_llm_spec(handle: &ToolHandle) -> ToolFunctionSpec {
    let key = ToolSpecKey::from_spec(
        &handle.schema.name,
        &handle.schema.description,
        &handle.schema.parameters,
    );

    // cache hit → 克隆
    if let Some(cached) = TOOL_CACHE.get(&key) {
        return cached;
    }

    // cache miss → 旧构建逻辑
    let provenance_prefix = provenance_prefix_for(&handle.provider);
    let cost_suffix = cost_suffix_for(&handle.schema);
    let cooling_suffix = cooling_suffix_for(handle);
    let description = format!("{}{}{}", provenance_prefix, handle.schema.description, cost_suffix, cooling_suffix);
    let spec = ToolFunctionSpec {
        name: handle.schema.name.clone(),
        description: Some(description),
        parameters: handle.schema.parameters.clone(),
        strict: None,
    };

    // 写入 cache
    TOOL_CACHE.insert(key, spec.clone());
    spec
}
```

**验证：** ` cargo test `——确认两次调用同一 handle 只走一次完整逻辑

---

### W1-D2~3: TriageEngine 核心

#### 3.1 新建 `core/triage.rs`

```rust
//! Pre-injection content triage engine.
//!
//! Replaces auto_compress_messages when enabled.
//! Classifies messages into INJECT / COMPRESS / STANDBY / COLD.

use crate::llm::{Message, MessageRole, MessageContent};
use crate::core::context::{ContextManager, CompressLevel};
use crate::core::compress_math::{CompositeScorer, MinHashSig};
use std::sync::Arc;
use tokio::sync::RwLock;

// ─── Actions ───────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum TriageAction {
    /// 保留原文在 active context
    Inject,
    /// 合并为摘要 + 写 archive
    Compress { depth: u32 },
    /// 移入 StandbyCache
    Standby { recall_id: String },
    /// 移入 ColdTier
    Cold { recall_id: String, summary: String },
    /// 丢弃（compress_depth >= 3 的旧块、空消息等）
    Discard,
}

// ─── Triage Block ──────────────────────────────────────────────────

/// 一条待 triage 的消息或已压缩块
#[derive(Debug, Clone)]
pub struct TriageBlock {
    pub messages: Vec<Message>,
    pub block_id: String,
    pub original_tokens: usize,
    pub compress_depth: u32,
    pub turn_range: Option<(u32, u32)>,
    pub is_tool_protocol: bool,
    pub has_decision_marker: bool,
    /// 各项评分明细
    pub scores: ScoreBreakdown,
    /// triage 决策
    pub action: TriageAction,
}

#[derive(Debug, Clone, Default)]
pub struct ScoreBreakdown {
    pub relevance: f64,    // [0, 1] 与当前 query 的相关性
    pub importance: f64,   // [0, 1] 历史重要性（ref_count + recency）
    pub role_weight: f64,  // User=0.4, Assistant=0.3, Tool=0.2, System=0.1
    pub content_signal: f64, // decision=+0.3, error=+0.3, code=+0.1
    pub depth_penalty: f64,  // compress_depth * 0.1
    pub final_score: f64,    // 加权综合
}

// ─── Config ────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct TriageConfig {
    pub enabled: bool,
    pub audit_only: bool,     // 记录决策但不执行
    pub keep_count: usize,    // 保留最近 N 轮原文，默认 5
    pub early_keep: usize,    // 开头保留 N 条，默认 2
    pub inject_threshold: f64,    // 默认 0.65
    pub standby_threshold: f64,   // 默认 0.40
    pub cold_threshold: f64,      // 默认 0.20
    pub hysteresis_deadband: f64, // 默认 0.15
    pub sticky_turns: u32,        // 默认 3
    pub cooldown_turns: u32,      // 默认 10
    pub max_compress_depth: u32,  // 默认 3
    pub standby_capacity: usize,  // 默认 200
    pub cold_batch_cap: usize,    // 默认 20
}

impl Default for TriageConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            audit_only: true,  // 默认只记录不执行
            keep_count: 5,
            early_keep: 2,
            inject_threshold: 0.65,
            standby_threshold: 0.40,
            cold_threshold: 0.20,
            hysteresis_deadband: 0.15,
            sticky_turns: 3,
            cooldown_turns: 10,
            max_compress_depth: 3,
            standby_capacity: 200,
            cold_batch_cap: 20,
        }
    }
}

// ─── Audit ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct TriageAuditRecord {
    pub turn: u32,
    pub block_id: String,
    pub block_type: BlockType,
    pub score: f64,
    pub score_breakdown: ScoreBreakdown,
    pub action: TriageAction,
    pub token_saved: usize,
    pub compress_depth: u32,
    pub was_tool_protocol: bool,
}

#[derive(Debug, Clone, Serialize)]
pub enum BlockType { Original, Compressed, ToolProtocol, System }

// ─── Engine ────────────────────────────────────────────────────────

pub struct TriageEngine {
    pub config: TriageConfig,
    scorer: Arc<dyn TriageScorer>,
    ctx: Arc<ContextManager>,
    /// StandbyCache 可选引用——注入后启用 STANDBY 动作
    standby: Option<Arc<StandbyCache>>,
    /// ColdWriter 可选引用——注入后启用 COLD 动作
    cold_writer: Option<Arc<dyn ColdBlockWriter>>,
    audit: RwLock<Vec<TriageAuditRecord>>,
}
```

#### 3.2 Scorer 模块

```rust
// ─── Scorer Trait ──────────────────────────────────────────────────

#[async_trait]
pub trait TriageScorer: Send + Sync {
    /// 对单条消息评分
    async fn score(&self, messages: &[Message], query: &str, turn: u32) -> Vec<ScoreBreakdown>;
}

// ─── DefaultScorer ─────────────────────────────────────────────────

pub struct DefaultScorer {
    // 复用 CompressMath 的 CompositeScorer
    composite: CompositeScorer,
}

impl DefaultScorer {
    /// 从 CompositeScorer 已有逻辑构建
    /// 引用：core/compress_math.rs CompositeScorer::score()
    pub fn new(decay_lambda: f64) -> Self { /* ... */ }
}

#[async_trait]
impl TriageScorer for DefaultScorer {
    async fn score(&self, messages: &[Message], query: &str, turn: u32) -> Vec<ScoreBreakdown> {
        messages.iter().map(|msg| {
            let text = extract_text(msg);
            let msg_type = MessageType::classify(&format!("{:?}", msg.role), &text);
            let token_count = estimate_tokens(&text);
            let unique_ratio = CompositeScorer::compute_unique_ratio(&text);

            // 复用 CompositeScorer 的核心公式
            let base = self.composite.score(msg_type, token_count, unique_ratio, 0.0, 0);

            // 扩展维度
            let relevance = compute_relevance(&text, query);     // MinHash Jaccard
            let importance = compute_importance(msg, turn);      // ref_count + recency
            let role_w = role_weight(&msg.role);
            let signal = content_signal(&text);
            let depth = /* extract from content if compressed */;

            ScoreBreakdown {
                relevance,
                importance,
                role_weight: role_w,
                content_signal: signal,
                depth_penalty: depth as f64 * 0.1,
                final_score: 0.30 * relevance
                    + 0.25 * importance
                    + 0.15 * role_w
                    + 0.10 * signal
                    - 0.10 * depth as f64 * 0.1,
            }
        }).collect()
    }
}
```

**验证：** ` cargo test -- triage::scorer `——确认评分在 [0, 1] 内

---

#### 3.3 Classifier 模块

```rust
impl TriageEngine {
    /// 按评分 + 当前状态把每条消息分类
    pub async fn classify(&self, blocks: &mut [TriageBlock], turn: u32) {
        // Phase 1: 基础分类
        for block in blocks.iter_mut() {
            block.action = match block.scores.final_score {
                s if s >= self.config.inject_threshold => TriageAction::Inject,
                s if s >= self.config.standby_threshold => TriageAction::Standby {
                    recall_id: format!("sb_{:016x}", hash(&block.messages)),
                },
                s if s >= self.config.cold_threshold => TriageAction::Cold {
                    recall_id: format!("cold_{:016x}", hash(&block.messages)),
                    summary: extract_summary(&block.messages),
                },
                _ => TriageAction::Discard,
            };
        }

        // Phase 2: Hysteresis（注入滞回区，防 thrashing）
        self.apply_hysteresis(blocks, turn).await;

        // Phase 3: Sticky bonus（当前 INJECT 的项目降级时保护 3 轮）
        self.apply_sticky_bonus(blocks, turn).await;

        // Phase 4: Cooldown check（COLD 后 10 轮内不 warm-up）
        self.apply_cooldown(blocks, turn).await;

        // Phase 5: enforce_invariants（最高优先级，覆盖前面所有）
        self.enforce_invariants(blocks).await;

        // Phase 6: Depth cutoff（compress_depth >= 3 的块强制 COLD 或 DISCARD）
        self.apply_depth_cutoff(blocks).await;
    }

    /// 不变量检查——工具协议消息永不降温
    async fn enforce_invariants(&self, blocks: &mut [TriageBlock]) {
        for block in blocks.iter_mut() {
            if block.is_tool_protocol {
                block.action = TriageAction::Inject;
            }
            if block.has_decision_marker {
                block.scores.final_score = block.scores.final_score.max(0.70);
                if matches!(block.action, TriageAction::Discard) {
                    block.action = TriageAction::Inject;
                }
            }
        }
    }
}
```

**验证：** ` cargo test -- triage::classifier `——工具协议消息永不 COLD

---

#### 3.4 COMPRESS Action

```rust
impl TriageEngine {
    /// 合并一组 COMPRESS 块为一条摘要消息
    pub async fn execute_compress(
        &self,
        blocks: &[TriageBlock],
        archive: &mut MessageArchive,
    ) -> Message {
        // Step 1: 提取所有子块的 key decisions
        let all_decisions: Vec<String> = blocks.iter()
            .filter_map(|b| extract_key_points(&b.messages))
            .collect();

        // Step 2: 计算 recover_id（hash 所有子块）
        let hasher = /* hash all block messages */;
        let recover_id = format!("merged_{:016x}", hasher.finish());

        // Step 3: 写 archive（原始消息可追溯）
        let originals: Vec<Message> = blocks.iter()
            .flat_map(|b| b.messages.clone())
            .collect();
        archive.push((recover_id.clone(), originals, ArchiveMeta {
            created_turn: current_turn,
            original_count: originals.len(),
            turn_range: None,
        }));

        // Step 4: 渲染摘要消息（紧凑格式，不含 snippet）
        let total_tok: usize = blocks.iter().map(|b| b.original_tokens).sum();
        let decision_section = if all_decisions.is_empty() {
            String::new()
        } else {
            format!("\n[Decisions: {}]", all_decisions.join("; "))
        };
        let summary = format!(
            "[Merged: {} blocks, ~{} tok, recover_id={}]{}",
            blocks.len(), total_tok, recover_id, decision_section
        );

        // 用 System 角色——不破 user/assistant 交替序列
        Message {
            role: MessageRole::System,
            content: Some(MessageContent::Text(summary)),
            name: None, tool_calls: None, tool_call_id: None,
            reasoning_content: None, prefix: false,
        }
    }
}
```

**验证：** ` cargo test -- triage::execute_compress `

---

#### 3.5 Triage Summary → Mission Brief 回写

```rust
impl TriageEngine {
    /// 从 triage 结果渲染一条摘要行，注入 Mission Brief
    pub fn triage_summary(&self, stats: &TriageStats) -> String {
        format!(
            "[Triage: {} standby | {} cold | {} compressed | ~{} tok saved]",
            stats.standby_count,
            stats.cold_count,
            stats.compress_count,
            stats.tokens_saved,
        )
    }
}

/// 每轮 triage 产出的聚合统计
pub struct TriageStats {
    pub inject_count: usize,
    pub compress_count: usize,
    pub standby_count: usize,
    pub cold_count: usize,
    pub discard_count: usize,
    pub tokens_saved: usize,
}
```

在 `pipeline/mod.rs setup()` 中接入：

```rust
// pipeline/mod.rs setup()
let triage_stats = if self.core.triage_config().enabled {
    let mut msgs = session.messages.write().await;
    TriageEngine::run(&mut msgs, query, &self.core).await
} else {
    TriageStats::default()  // 无 triage 时 stats 为空
};

// 把 triage stats 传给 build_system_output 供 Awareness Block 渲染
```

**验证：** ` cargo check `

---

### W1-D4: Pipeline 集成

#### 4.1 setup() 集成点

**文件：** `core/pipeline/mod.rs` 的 `setup()` 方法，约 line 550

```rust
// 在 Phase Ctx-A pressure shed 检查之后、setup 构建 messages clone 之前
// 替代 auto_compress_messages 调用

async fn setup(&self) -> Result<TurnContext, KernelError> {
    // ... 现有 safety check ...

    // Triage 阶段（替代 auto_compress）
    if self.core.triage_config().enabled {
        let s = self.session.read().await;
        let mut msgs = s.messages.write().await;
        let query = self.input;
        let (new_msgs, stats) = TriageEngine::run_with_audit(
            &mut msgs,
            query,
            &self.core.triage_config(),
            &self.core.context_manager,
            self.core.standby_cache.as_deref(),
            self.core.cold_writer.as_deref(),
        ).await;
        *msgs = new_msgs;
        // stats 存入 TurnContext 供后续 awareness 块用
        ctx.triage_stats = Some(stats);
    } else {
        // 原始 auto_compress 路径（不变）
        if self.core.context_manager.take_shed_pending() {
            // ... 现有压缩逻辑 ...
        }
    }

    // ... 后续现有逻辑 ...
}
```

#### 4.2 Config 门控

**文件：** `core/mod.rs`, `CoreConfig` struct

```rust
// 在 CoreConfig 中新增
pub struct CoreConfig {
    // ... existing fields ...
    pub triage: TriageConfig,  // = TriageConfig::default() = disabled
}
```

然后 `CoreLoop` 暴露 getter：

```rust
impl CoreLoop {
    pub fn triage_config(&self) -> &TriageConfig {
        &self.config.triage
    }

    pub fn standby_cache(&self) -> Option<&StandbyCache> {
        self.standby_cache.as_ref()
    }

    pub fn cold_writer(&self) -> Option<&dyn ColdBlockWriter> {
        self.cold_writer.as_deref()
    }
}
```

**验证：** ` cargo check `

---

## W2：缓存 + 冷端

### W2-D1: StandbyCache

**文件：** 新建 `core/standby_cache.rs`

```rust
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use crate::core::compress_math::MinHashSig;
use crate::llm::Message;

/// Standby 缓存条目
#[derive(Debug, Clone)]
pub struct StandbyEntry {
    pub recall_id: String,
    pub content: Arc<[Message]>,  // Arc 避免 clone
    pub summary: String,
    pub content_hash: u64,
    pub source_turn: u32,
    pub last_active_turn: u32,
    pub created_at: Instant,
    pub warm_count: u32,
    pub minhash_sig: Arc<MinHashSig>,
}

/// 内存 Standby Cache——LRU + TTL 50 turn
pub struct StandbyCache {
    entries: RwLock<HashMap<String, StandbyEntry>>,
    order: RwLock<Vec<String>>,     // FIFO evict 顺序
    cap: usize,
    default_ttl_turns: u32,
    warm_threshold: f64,              // MinHash 相似度阈值 0.60
}

impl StandbyCache {
    pub fn new(cap: usize) -> Self { /* ... */ }

    /// 写入缓存
    pub fn set(&self, entry: StandbyEntry) {
        // LRU evict if full
        // insert entry
    }

    /// 按 recall_id 取回
    pub fn get(&self, recall_id: &str) -> Option<StandbyEntry> { /* ... */ }

    /// 按 query 查找相关条目（warm-up 用）
    /// 对所有条目扫描 MinHash 相似度，返回 > threshold 的条目
    pub fn warm_up(&self, query: &str, query_sig: &MinHashSig, current_turn: u32) -> Vec<StandbyEntry> {
        let entries = self.entries.read().unwrap();
        let mut matches: Vec<(f64, &StandbyEntry)> = entries.values()
            .filter_map(|e| {
                let sim = query_sig.jaccard(&e.minhash_sig);
                if sim >= self.warm_threshold {
                    // content_hash 校验
                    let current_hash = /* hash of query content */;
                    if current_hash == e.content_hash {
                        Some((sim, e))
                    } else {
                        None  // 内容已过时，丢弃
                    }
                } else { None }
            })
            .collect();
        matches.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
        matches.into_iter().map(|(_, e)| e.clone()).collect()
    }

    /// 检查是否含某 recall_id
    pub fn contains(&self, recall_id: &str) -> bool { /* ... */ }

    /// 统计
    pub fn stats(&self) -> (usize, usize) { /* (count, total_tokens) */ }
}
```

**验证：** ` cargo test -- standby_cache `

---

### W2-D2: ColdTier 扩展

**文件：** `core/session_store.rs`

```rust
// 新增表操作

/// 消息块级存储——扩展 SessionStore trait
#[async_trait]
pub trait SessionStore: Send + Sync {
    // ... 现有方法 ...

    // 新增（有默认实现——向后兼容）
    async fn save_block(&self, block: BlockRecord) -> Result<(), KernelError> {
        Err(KernelError::Other("not implemented".into()))
    }

    async fn search_blocks(&self, query: &str, limit: usize) -> Result<Vec<BlockResult>, KernelError> {
        Ok(Vec::new())
    }

    async fn union_search(&self, query: &str, limit: usize) -> Result<Vec<UnionResult>, KernelError> {
        let blocks = self.search_blocks(query, limit).await?;
        let snaps = self.search(query).await?;
        // 合并、去重、排序
        Ok(merge_results(blocks, snaps, limit))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockRecord {
    pub recall_id: String,
    pub session_id: String,
    pub turn_start: u32,
    pub turn_end: u32,
    pub summary: String,
    pub content_json: String,        // serde_json::to_string(&messages)
    pub key_decisions: Vec<String>,
    pub original_tokens: usize,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockResult {
    pub recall_id: String,
    pub summary: String,
    pub key_decisions: Vec<String>,
    pub original_tokens: usize,
    pub turn_range: (u32, u32),
    pub score: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum UnionResult {
    Session(SessionSnapshot),
    Block(BlockResult),
}

// ─── SqliteSessionStore 实现 ──────────────────────────────────────

impl SessionStore for SqliteSessionStore {
    async fn save_block(&self, block: BlockRecord) -> Result<(), KernelError> {
        let decisions_json = serde_json::to_string(&block.key_decisions)?;
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO message_blocks (recall_id, session_id, turn_start, turn_end,
             summary, content_json, key_decisions, original_tokens, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(recall_id) DO UPDATE SET
                turn_start = excluded.turn_start,
                turn_end = excluded.turn_end,
                summary = excluded.summary",
            params![block.recall_id, block.session_id, block.turn_start, block.turn_end,
                    block.summary, block.content_json, decisions_json,
                    block.original_tokens, block.created_at],
        )?;
        Ok(())
    }

    async fn search_blocks(&self, query: &str, limit: usize) -> Result<Vec<BlockResult>, KernelError> {
        let escaped = query.replace('"', "\"\"");
        let fts_query = format!("\"{}\"", escaped);
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT b.recall_id, b.summary, b.key_decisions,
                    b.original_tokens, b.turn_start, b.turn_end
             FROM message_blocks b
             JOIN block_search fts ON b.rowid = fts.rowid
             WHERE block_search MATCH ?1
             ORDER BY rank
             LIMIT ?2"
        )?;
        let rows = stmt.query_map(params![fts_query, limit], |row| { /* ... */ })?;
        // collect into Vec<BlockResult>
    }
}
```

**初始化时自动迁移：**

```rust
impl SqliteSessionStore {
    fn migrate_schema(&self) -> Result<(), KernelError> {
        self.conn.execute(
            "CREATE TABLE IF NOT EXISTS message_blocks (
                recall_id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                turn_start INTEGER NOT NULL,
                turn_end INTEGER NOT NULL,
                summary TEXT NOT NULL DEFAULT '',
                content_json TEXT NOT NULL DEFAULT '[]',
                key_decisions TEXT NOT NULL DEFAULT '[]',
                original_tokens INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL
            )", []
        )?;
        self.conn.execute(
            "CREATE VIRTUAL TABLE IF NOT EXISTS block_search USING fts5(
                recall_id, summary, key_decisions,
                content='message_blocks', content_rowid='rowid'
            )", []
        )?;
        self.conn.execute_batch(
            "CREATE TRIGGER IF NOT EXISTS block_ai AFTER INSERT ON message_blocks BEGIN
                INSERT INTO block_search(rowid, recall_id, summary, key_decisions)
                VALUES (new.rowid, new.recall_id, new.summary, new.key_decisions);
            END;"
        )?;
        Ok(())
    }
}
```

**验证：** ` cargo test -- session_store::block_tests `

---

### W2-D3: Cold Batch Writer

**文件：** `core/context.rs` 或新建 `core/cold_buffer.rs`

```rust
/// Cold 数据批量写入器——先 buffer 再 flush，防止 per-message SQLite 延迟
pub struct ColdBufferWriter {
    buffer: RwLock<Vec<BlockRecord>>,
    cap: usize,
    store: Arc<dyn SessionStore>,
    flush_count: AtomicU32,  // 每 3 次 flush 强制写入
}

impl ColdBufferWriter {
    pub fn new(store: Arc<dyn SessionStore>, cap: usize) -> Self { /* ... */ }

    /// 推入缓冲区
    pub async fn push(&self, block: BlockRecord) {
        let mut buf = self.buffer.write().await;
        buf.push(block);
        if buf.len() >= self.cap {
            self.flush().await;
        }
    }

    /// 强制 flush
    pub async fn flush(&self) {
        let mut buf = self.buffer.write().await;
        if buf.is_empty() { return; }
        let batch = buf.drain(..).collect::<Vec<_>>();
        drop(buf);
        for block in batch {
            if let Err(e) = self.store.save_block(block).await {
                tracing::warn!("cold buffer flush: save_block failed: {e}");
            }
        }
    }
}

impl ColdBlockWriter for ColdBufferWriter {
    fn save(&self, block: BlockRecord) -> impl Future<Output = ()> {
        self.push(block)
    }
}

/// Triage COLD action 输出的 trait——抽象 batch writer
#[async_trait]
pub trait ColdBlockWriter: Send + Sync {
    async fn save_block(&self, block: BlockRecord);
}
```

**验证：** ` cargo test -- cold_buffer `

---

## W3：放大增益

### W3-D1: Palace × Triage

**文件：** `memory_palace.rs`

```rust
impl DualPalaceMemory {
    /// 记录 triage 决策作为行为模式
    pub async fn record_triage_action(
        &self,
        action: &TriageAction,
        score: f64,
        text_snippet: &str,
    ) {
        let intent = match action {
            TriageAction::Inject => "triage_inject",
            TriageAction::Compress { .. } => "triage_compress",
            TriageAction::Standby { .. } => "triage_standby",
            TriageAction::Cold { .. } => "triage_cold",
            TriageAction::Discard => "triage_discard",
        };
        // 用文本摘要提取领域
        let domain = extract_domain(text_snippet);
        self.record_interaction(intent, &[domain, "triage"]).await;
    }

    /// 对输入推测 triage hint
    pub async fn triage_hint(&self, input: &str) -> Option<(TriageAction, f64)> {
        let (domains, _) = self.classify_input(input).await;
        // 检查有无 triage 历史模式
        for (domain, score) in &domains {
            if *score > 0.70 {
                // 查询 behavior palace 中此 domain 的 triage 记录
                if let Some(action) = self.behavior.triage_for(domain).await {
                    return Some((action, *score));
                }
            }
        }
        None
    }
}
```

**验证：** ` cargo test -- memory_palace::triage_hint `

---

### W3-D2: KB × Triage（relevance 评分增强）

**文件：** `knowledge_store.rs` + `core/triage.rs`

```rust
impl KnowledgeStore {
    /// 计算文本与 query 的相关性分数 [0, 1]
    /// 降级：FTS5 BM25 → Normalize
    /// 增强：+embedding cosine（有 embedder 时）
    pub async fn relevance_score(&self, text: &str, query: &str) -> f64 {
        // 路径 1: FTS5 BM25（总是可用）
        let fts5_score = self.fts5_bm25(text, query).await;

        // 路径 2: Embedding cosine（可选）
        let embed_score = if let Some(embedder) = self.embedder.read().as_ref() {
            embedder.similarity(text, query).await.unwrap_or(0.0)
        } else {
            0.0
        };

        // 综合：取 max（BM25 召回广，cosine 精度高）
        fts5_score.max(embed_score)
    }

    /// FTS5 BM25 → [0,1] 归一化
    async fn fts5_bm25(&self, text: &str, query: &str) -> f64 {
        // 用简单的 FTS5 MATCH 得到 rank，转换为分数
        let conn = self.conn.lock().await;
        let result: Result<f64, _> = conn.query_row(
            "SELECT rank FROM knowledge_chunks WHERE content MATCH ?1 LIMIT 1",
            params![query],
            |row| row.get(0),
        );
        // 归一化：BM25 rank 通常为负值，clamp 到 [0,1]
        result.map(|r| (-r).clamp(0.0, 1.0)).unwrap_or(0.0)
    }
}

// TriageEngine scorer 接入
impl DefaultScorer {
    fn with_kb(kb: Option<Arc<KnowledgeStore>>) -> Self {
        Self { kb, composite: CompositeScorer::new(1.2) }
    }
}

#[async_trait]
impl TriageScorer for DefaultScorer {
    async fn score(&self, messages: &[Message], query: &str, turn: u32) -> Vec<ScoreBreakdown> {
        messages.iter().map(|msg| {
            let text = extract_text(msg);
            let base: f64 = 0.3; // 基线

            // KB relevance（有 KB 时）
            let relevance = if let Some(kb) = &self.kb {
                kb.relevance_score(&text, query).await
            } else {
                // 无 KB 时用 MinHash
                minhash_jaccard(&text, query)
            };

            ScoreBreakdown {
                relevance,
                final_score: 0.30 * relevance + 0.25 * base + ...,
                ..Default::default()
            }
        }).collect()
    }
}
```

**验证：** ` cargo test -- knowledge_store::relevance_score `

---

### W3-D3: MagChain × Triage

```rust
// mag_chain.rs —— 新增 PipelineEvent 变体

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PipelineEvent {
    // ... 现有变体 ...

    /// Triage 完成时广播
    TriageResult {
        turn: u32,
        score: f64,
        action: String,           // "inject" | "compress" | "standby" | "cold"
        blocks_processed: usize,
        tokens_saved: usize,
    },
}

// ─── TriageHook ───────────────────────────────────────────────────

pub struct TriageHook {
    prefix: String,
    triage_threshold: RwLock<f64>, // 分数低于此值触发 abort
}

#[async_trait]
impl PipelineHook for TriageHook {
    fn name(&self) -> &str { "ContentTriage" }

    async fn on_event(&self, event: &PipelineEvent) -> Result<HookAction, KernelError> {
        match event {
            PipelineEvent::TurnStart { input, .. } => {
                // 高风险输入 → 拦截
                // 具体实现由 triage engine scorer 处理
                Ok(HookAction::Continue)
            }
            PipelineEvent::TriageResult { score, .. } => {
                if *score < *self.triage_threshold.read().await {
                    // 安全系统介入
                    tracing::warn!("triage score {:.2} below threshold, tightening constraints");
                }
                Ok(HookAction::Continue)
            }
            _ => Ok(HookAction::Continue),
        }
    }
}
```

**验证：** ` cargo test -- mag_chain::triage_hook `

---

### W3-D4: TriageReranker（embedding 升级路径）

```rust
// core/triage.rs —— 新增 reranker trait

/// 可选 reranker——对 FTS5 结果做二次精排
#[async_trait]
pub trait TriageReranker: Send + Sync {
    async fn rerank(&self, query: &str, candidates: &[(String, f64)]) -> Result<Vec<usize>, String>;
}

// vllm_embedder.rs —— 扩展实现

impl TriageReranker for VllmEmbedder {
    async fn rerank(&self, query: &str, candidates: &[(String, f64)]) -> Result<Vec<usize>, String> {
        // POST /v1/rerank
        // body: { query, documents: [...], top_n: candidates.len() }
        // 返回排序后的 indices
        let url = format!("{}/v1/rerank", self.base_url);
        let docs: Vec<&str> = candidates.iter().map(|(text, _)| text.as_str()).collect();
        let resp = self.client.post(&url)
            .json(&serde_json::json!({
                "query": query,
                "documents": docs,
                "top_n": docs.len(),
            }))
            .send().await
            .map_err(|e| format!("rerank request: {e}"))?;
        let body: serde_json::Value = resp.json().await
            .map_err(|e| format!("rerank response: {e}"))?;
        let results = body["results"].as_array().ok_or("missing results")?;
        let mut indices: Vec<(usize, f64)> = results.iter()
            .filter_map(|r| {
                let idx = r["index"].as_u64()? as usize;
                let score = r["relevance_score"].as_f64()?;
                Some((idx, score))
            })
            .collect();
        indices.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
        Ok(indices.into_iter().map(|(i, _)| i).collect())
    }
}

// TriageEngine 可选接入

pub struct TriageEngine {
    // ... 现有字段 ...
    pub reranker: Option<Arc<dyn TriageReranker>>,
}

impl TriageEngine {
    /// 重排序 cold recall 结果
    pub async fn rerank_recall(
        &self,
        query: &str,
        candidates: &[BlockResult],
    ) -> Vec<BlockResult> {
        let reranker = match &self.reranker {
            Some(r) => r,
            None => return candidates.to_vec(),  // 没有 reranker，保持 FTS5 顺序
        };
        let texts: Vec<String> = candidates.iter().map(|c| c.summary.clone()).collect();
        let pairs: Vec<(String, f64)> = candidates.iter()
            .map(|c| (c.summary.clone(), c.score)).collect();
        let indices = reranker.rerank(query, &pairs).await.unwrap_or_default();
        indices.into_iter().filter_map(|i| candidates.get(i).cloned()).collect()
    }
}
```

**验证：** ` cargo check ` + 集成测试（mock reranker 返回固定顺序）

---

### W3-D5: Config 全量

**文件：** `~/.abacus/config.toml` 文档 + `core/config.rs`

```toml
[triage]
enabled = false              # 全局开关，默认关
audit_only = true            # 默认只记录不执行
keep_count = 5               # 保留最近 N 轮原文

[triage.thresholds]
inject = 0.65
standby = 0.40
cold = 0.20
hysteresis = 0.15
sticky_turns = 3
cooldown_turns = 10
max_compress_depth = 3

[triage.cache]
standby_capacity = 200
standby_ttl_turns = 50
cold_batch_cap = 20
tool_cache_cap = 128

[triage.embedding]
enabled = false
provider = "vllm"            # vllm | mlx | ollama
minhash_fallback = true

[triage.reranker]
enabled = false
provider = "vllm"
```

**验证：** 配置加载 + 解析测试

---

## 附录：文件变更总索引

| 文件 | 变更类型 | 涉及任务 |
|------|---------|---------|
| `core/triage.rs` | **新建** | 3.1-3.7, 13.1-13.2 |
| `core/standby_cache.rs` | **新建** | 5.1-5.8 |
| `core/cold_buffer.rs` | **新建** | 7.5 |
| `llm/tool_view.rs` | 修改 | 2.1-2.5 |
| `core/mod.rs` | 修改 | 1.2, 4.1-4.2, 10.4 |
| `core/context.rs` | 修改 | 1.1, 1.3, 8.1-8.3 |
| `core/pipeline/mod.rs` | 修改 | 4.1, 6.1-6.2 |
| `core/pipeline/post.rs` | 修改 | 1.1(触发写 checkpoint) |
| `core/session_store.rs` | 修改 | 7.1-7.7 |
| `memory_palace.rs` | 修改 | 10.1-10.3 |
| `knowledge_store.rs` | 修改 | 11.1-11.3 |
| `mag_chain.rs` | 修改 | 12.1-12.4 |
| `vllm_embedder.rs` | 修改 | 13.3 |
| `deduction/mod.rs` | 修改 | 12.3-12.4 |
| `core/compress_math.rs` | 修改 | 3.2, 6.3 |
| `core/event_sink.rs` | 修改 | 12.1 |
| `prompt_assembly.rs` | 修改 | 1.2(segment 顺序) |

---

**总计：13 个文件修改 + 3 个新文件 = 38 项任务，17 天。**
