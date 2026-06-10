# RFC-0001v2: Content Lifecycle Management · 认知适配版

---

## 1. Why：从 LLM 认知出发

当前每轮注入 10000-20000 tok，但 LLM 实际有效使用的部分：

| LLM 实际需要 | 占比 | 当前注入形式 | 问题 |
|-------------|------|-------------|------|
| 当前轮用户输入 | 5% | 原文在 messages 末尾 | OK |
| 任务使命+状态 | 5% | Awareness Block 末尾 | 位置不对，LLM 读到才发现 |
| 最近 3-5 轮原文 | 25% | keep_count=8 保留 | 多保留 3 轮原文，每轮多花 ~1000 tok |
| 工具 schema（首次学习后） | 5% | 每轮全量重建 30-60 个 | cache 可省 ~3000 tok/轮 |
| 决策历史索引 | 5% | 分散在压缩摘要中 | 要滚动查找，且含不需要的 snippet |
| 旧工具调用细节 | 0-5% | 完整 output（500-5000 tok） | 仅出错时有用，其余浪费 |
| 系统固定段（你是谁/规则） | 5% | Layer 255-188 共 ~610 tok | 每轮重估 token，可预计算 |
| **无用/低效** | **45%** | 旧压缩块叠加 + 冗余 schema + 不必要的历史 | 可消除 |

核心矛盾：**LLM 不需要的 45% token，却占着 context 预算，还让 prefix cache 经常失效。**

---

## 2. What：三层 + 三缓存 + 一简报

```
 每轮 pipeline setup()
         │
         ▼
┌─────────────────────────────────────────────────────┐
│  STEP 1: Mission Brief ← Awareness Block V2          │
│  从 checkpoint + triage 产物渲染 100 tok             │
│  注入 system prompt 最顶部                           │
└─────────────────────────────────────────────────────┘
         │
         ▼
┌─────────────────────────────────────────────────────┐
│  STEP 2: ToolCache ← schema 序列化缓存               │
│  hash(parameters,description) 未变 → 跳过 rebuild   │
│  内置在 build_tool_definitions_for 中                │
└─────────────────────────────────────────────────────┘
         │
         ▼
┌─────────────────────────────────────────────────────┐
│  STEP 3: TriageEngine ← 内容分流                     │
│                                                      │
│  ┌──────────┐ ┌──────────┐ ┌──────────┐ ┌────────┐ │
│  │ INJECT   │ │ COMPRESS │ │ STANDBY │ │  COLD  │ │
│  │ 最近5轮  │ │ 中间旧    │ │ 退缓存   │ │ 入SQL  │ │
│  │ +工具协议 │ │ 合并为    │ │ 1ms召回  │ │  recall │ │
│  │ +决策标记 │ │ 决策索引  │ │ TTL 50  │ │  FTS5  │ │
│  └──────────┘ └──────────┘ └──────────┘ └────────┘ │
└─────────────────────────────────────────────────────┘
         │
         ▼
┌─────────────────────────────────────────────────────┐
│  STEP 4: StandbyCache ← ICL + 中间件合并注入          │
│  · ICL 结果缓存（MinHash 近似匹配命中）                │
│  · MagChain/TriageHook 集成                          │
│  · warm-up 透明拉回 active                            │
└─────────────────────────────────────────────────────┘
         │
         ▼
   LlmRequest (messages ~60% baseline, tools ~30% rebuild)
```

---

## 3. How：逐个实现

### 3.1 Mission Brief（Awareness Block V2）

**位置**：`core/mod.rs` build_system_output 的 Awareness Block，改渲染格式 + 移到最顶层（在 Layer 255 之前）
**数据源**：全部已有——`checkpoint.accomplished`、`checkpoint.pending`、`checkpoint.current_topic`
**输出**（100 tok，LLM 第一眼看到）：

```
[Mission: 重构 API 网关 | ✓ Rust方案, ✗ 方案B | ↻ rate limit]
[Context: 42% | Standby: 3 | session.recall 可查历史]
```

**不新增数据、不新增工具、不新增 stream chunk。** 纯改渲染位置 + 文字格式。

### 3.2 ToolCache（schema 序列化缓存）

**位置**：`build_tool_definitions_for` 末尾 → 缓存 `(name, description_hash, params_hash) → serialized_value`
**命中条件**：工具数量相同 && 每个工具的 description 和 parameters 的 hash 都相同
**缓存替换**：LRU，64 条目（足够覆盖全工具集）
**集成方式**：`build_tool_definitions_for` 返回 `Vec<ToolDefinition>` 不变，内部直接 set cached 值

```rust
// tool_view.rs: 在 tool_handle_to_llm_spec 处加入
fn tool_handle_to_llm_spec(handle: &ToolHandle) -> ToolFunctionSpec {
    let cache_key = ToolSpecKey { 
        name: handle.schema.name.clone(),
        desc_hash: hash(&handle.schema.description),
        params_hash: hash(&handle.schema.parameters),
    };
    if let Some(cached) = TOOL_SPEC_CACHE.get(&cache_key) {
        return cached.clone();  // O(1) clone Arc<String>
    }
    // ... 原有构建逻辑 ...
    let spec = ToolFunctionSpec { ... };
    TOOL_SPEC_CACHE.insert(cache_key, spec.clone());
    spec
}
```

**为什么放 tool_view.rs 不是 provider 层？** 因为所有 provider 共享 `tool_handle_to_llm_spec` 的产出，缓存一次惠及全部。

### 3.3 TriageEngine（内容分流）

**核心逻辑**：不再是"满了才压缩"，而是**每轮对所有消息做一次分类**。

```
输入: session.messages (Vec<Message>)

分类规则:
  1. 最近 5 轮 (keep_count=5) → INJECT (保留原文)
  2. 工具协议消息 (tool_calls / tool_call_id) → INJECT (保留原文)
  3. 高重要性 (决策/错误标记, score > 0.70) → INJECT (保留原文，可能截断)
  4. 中间段 → COMPRESS → 合并为决策索引 + 写入 archive
  5. compress_depth >= 3 的旧块 → COLD → 写入 ColdTier
  6. 其余 → STANDBY → 写入 StandbyCache

输出: new_messages (Vec<Message>, 可能比输入短)
      + injected_summary (String, 挂在 Mission Brief 下)
```

**和现有 auto_compress 的关系**：triage 是 INJECT/COMPRESS/STANDBY/COLD 四路，现有 auto_compress 只有 KEEP/COMPRESS 两路。triage 开启时替代 auto_compress，关闭时回退原路径。

**状态：** 单 `RwLock<Message>` 修改，不额外引入异步锁。

### 3.4 StandbyCache

**位置**：`standby_cache.rs` 新文件
**结构**：`DashMap<String, StandbyEntry>` + TTL
**容量**：200 条目，FIFO evict

```rust
struct StandbyEntry {
    recall_id: String,           // 唯一 ID
    content: Arc<Vec<Message>>,  // 原始消息，Arc 避免 clone
    summary: String,             // 注入时的摘要文本
    content_hash: u64,           // 校验过时
    created_at: Instant,         // TTL 起点
    warm_count: u32,             // 被 warm-up 次数
}
```

**warm-up 机制**：Triage 评分时，发现 query 与某 standby 条目的 MinHash 相似度 > 0.60 → 拉回 INJECT，TTL 重置。

**不作为独立 StreamChunk 暴露**——warm-up 是静默操作，用户无感知。

### 3.5 ColdTier 扩展

**现有**：`SqliteSessionStore` 存 `SessionSnapshot`（session_id, turn_count, summary, key_decisions）
**扩**：新增表 `message_blocks (recall_id, session_id, turn_range, summary, content_json, key_decisions, created_at)`
**FTS5**：对 summary + key_decisions + content_json 建索引

```sql
CREATE TABLE IF NOT EXISTS message_blocks (
    recall_id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL,
    turn_start INTEGER NOT NULL,
    turn_end INTEGER NOT NULL,
    summary TEXT NOT NULL DEFAULT '',
    content_json TEXT NOT NULL DEFAULT '[]',
    key_decisions TEXT NOT NULL DEFAULT '[]',
    original_tokens INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL
);

CREATE VIRTUAL TABLE IF NOT EXISTS block_search USING fts5(
    recall_id, summary, key_decisions, content='message_blocks', content_rowid='rowid'
);
```

**写入**：batch buffer 模式——先写到内存 Vec（cap 20），达阈值或每 3 turn flush 一次。
**读取**：复用 `session.recall` 工具——LLM 调 `session.recall("query")` → 查 `block_search` + `session_search` 返回合并结果。LLM 接口不变。

---

## 4. Pluggable Embedding / Reranker 升级路径

系统已有完整的 `MemoryEmbedder` trait（`memory_palace.rs:547`）+ `VllmEmbedder` 实现（`vllm_embedder.rs`）+ KnowledgeStore 的 semantic search + FTS5 降级。

Triage 的每个环节按 embedding 有无做两级实现：

| 环节 | 无 embedding（零依赖） | 有 embedding | 最优 |
|------|----------------------|-------------|------|
| **Triage relevance 评分** | MinHash Jaccard + token overlap | `embed(query) × embed(block)` → cosine | 先 MinHash 粗筛，top-10 走 embedding 打分 |
| **Standby warm-up** | MinHash 阈值 0.60 | cosine 阈值 0.75 | FTS5 召回 → embedding 精排 |
| **Cold recall** | FTS5 BM25 | `semantic_search()` 已有 | **rerank**: FTS5 取 top-20 → embedding 精排取 top-5 |
| **Decision 节点识别** | 正则 + ref_count | `embed(decision_patterns) × embed(block)` | 同左，embedding 提升 recall |
| **ICL 缓存** | exact MinHash key | MinHash 近似匹配 | 已有 MinHash = 够用 |

### Reranker 接口（triage 特有）

现有 `MemoryEmbedder` 只有 embedding 能力。Triage 需要一个 optional reranker：

```rust
#[async_trait]
pub trait TriageReranker: Send + Sync {
    /// 对 query + candidates 重排序，返回按相关性降序的 indices
    async fn rerank(&self, query: &str, candidates: &[&str]) -> Result<Vec<usize>, String>;
}
```

可独立于 `MemoryEmbedder` 注入——用户有 vLLM rerank  endpoint 时启用，没有时 triage 用 FTS5 order by rank。

VllmEmbedder 可以扩展实现 `TriageReranker`（vLLM 的 `POST /v1/rerank` 接口）。

### 用户配置（`~/.abacus/config.toml`）

```toml
[triage]
enabled = false            # 全局开关
keep_count = 5             # 保留最近 N 轮原文

[triage.embedding]
enabled = false            # 有 embedder 时自动启用
provider = "vllm"          # vllm | mlx | ollama
minhash_fallback = true    # embedding 不可用时降级

[triage.reranker]
enabled = false
provider = "vllm"
```

没有 embedding/reranker 时，系统用 MinHash + FTS5，没有任何额外依赖。

---

## 5. 六联动放大

| 联动 | 接入点 | 效果 | 代码量 |
|------|--------|------|--------|
| **Mission Brief ← Checkpoint** | `checkpoint.accomplished/pending` 已在 post.rs | 0 新数据，纯改渲染 | 半天 |
| **ToolCache ← Registry** | `tool_view.rs` 加 static HashMap | 全 provider 受益 | 半天 |
| **Triage ← CompressMath** | 复用 MinHash + CompositeScorer | 不另写评分算法 | 0 |
| **StandbyCache ← ICL Primer** | pipeline/setup KB query 后塞 standby | 相邻 query 省 KB 调用 | 1 天 |
| **ColdTier ← session.recall** | 同一 FTS5 引擎，union 两个表 | recall 返回范围更广 | 1 天 |
| **Mission ← Palace** | Palace 学 triage 模式 → 预判 | 跨 session 越用越准 | 2 天 |

---

## 5. 版本计划

| 版本 | 改动 | 用户感知 | 风险 |
|------|------|---------|------|
| v2.1.0 | Awareness Block V2 + ToolCache | Token 账单下降 | 极低（纯渲染+纯 cache） |
| v2.2.0 | TriageEngine INJECT/COMPRESS 两路（替代 auto_compress） | 无（相同效果更快） | 中（需 canary） |
| v2.3.0 | StandbyCache + ICL 集成 | 无（更快更省） | 低-mid（需验证 warm-up) |
| v2.4.0 | ColdTier 扩展 + session.recall 合并 | recall 返回更精细结果 | 中（schema 迁移） |
| v3.0.0 | Palace triage 学习全量上线 | 越用越快 | 低（增量上线） |

**v2.1.0（2 天）即可拿到 70% 收益**——Mission Brief + ToolCache。

---

## 6. 零精度损失保障

| 防护 | 机制 | 违反后果 |
|------|------|---------|
| 工具协议消息永不 COLD | `enforce_invariants` 在 triage 分类器后执行 | 触发即 auto-disable triage |
| 决策/错误标记提升评分 | score = max(score, 0.70) | LLM 可能丢失上下文 |
| session.recall 覆盖全 cold 数据 | FTS5 双表 union | LLM 查不到 |
| compress_depth >= 3 → COLD 非 COMPRESS | 防递归 | 压缩块无限嵌套 |
| Standby warm-up content_hash 校验 | 不一致时丢弃 | 污染 active context |
| audit 离线回放 | `TriageAuditRecord` 每轮记录 | 假阴性不被发现 |

---

## 7. 实现顺序

```
Week 1:
  周一:  Mission Brief (awareness.rs)
  周二:  ToolCache (tool_view.rs)
  周三:  TriageEngine 评分 + 两路 (triage.rs)
  周四:  INJECT/COMPRESS 替代 auto_compress
  周五:  测试 + canary

Week 2:
  周一:  StandbyCache (standby_cache.rs)
  周二:  ICL 写入 standby
  周三:  ColdTier 扩展 (session_store.rs)
  周四:  session.recall 合并查询
  周五:  集成 -> pipeline setup()

Week 3:
  周一:  Palace triage 学习
  周二:  全链路 A/B + audit 分析
  周三:  fix + 调参
  周四:  GA 发布 v2.1.0
```
