# RFC-0001: Pre-Injection Content Triage & Lifecycle Management

- **Status**: Pre-Research
- **Author**: AI Architect
- **Date**: 2026-06-09

---

## 1. Executive Summary

当前 Abacus 的 token 管理是**事后补救型**：context 达到阈值（85%-95%）后才触发 `auto_compress_messages`，将中间段 non-tool 合并为摘要。messages Vec **只增不减**，摘要块**层层叠加**，Tool definitions 和 System prompt **每轮全量重建**。

本提案改为**事前审校型**：每轮构建 LlmRequest 之前，对所有候选内容做一次三路分流（INJECT / STANDBY / COLD），以**零认知精度损失**为硬约束，系统性解决 messages 增长、摘要叠加、工具定义重建、system prompt 重算四个瓶颈。

---

## 2. Design Goals & Non-goals

### Goals
- 消除 messages Vec 单调增长（允许真正收缩）
- 消除旧摘要块重复叠加
- 稳定 tool definitions 跨轮复用
- 稳定 system prompt 段 token 预计算
- **零 LLM 认知精度损失** — triage 不导致 LLM 做出错误决策或丢失必要上下文

### Non-goals
- 不改变现有 compress 路径的 correctness（triage 可选启用，backup compress 仍可用）
- 不做语义 embedding / LLM-driven 评分（全 deterministic + 已有信号）
- 不做跨 session 的 global tier（仅 session 级）

---

## 3. Architecture Overview

### 3.1 Components

```
┌─────────────────────────────────────────────────────────────────────┐
│                        TriageEngine                                │
│  ┌─────────────┐  ┌──────────────┐  ┌───────────────────────────┐ │
│  │ Scorer      │  │ Classifier   │  │ Action Executor           │ │
│  │ · relevance │→ │ · threshold  │→ │ · splice messages          │ │
│  │ · importance│  │ · hysteresis │  │ · write standby            │ │
│  │ · cost      │  │ · sticky     │  │ · buffer cold write        │ │
│  │ · metadata  │  │ · cooldown   │  │ · inject summary block     │ │
│  └─────────────┘  └──────────────┘  └───────────────────────────┘ │
└─────────────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────────────┐
│                        StandbyCache                                │
│  In-memory, DashMap<String, StandbyEntry>                          │
│  · TTL = 50 turn / 300s (whichever first)                         │
│  · Max entries = 200 (FIFO evict)                                  │
│  · Warm-up: triage 检测到 query 匹配 → 拉回 ACTIVE                │
└─────────────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────────────┐
│                        ColdTier (enhanced)                         │
│  SQLite-backed, existing SessionStore extended:                    │
│  · Stores block-level content + summary + recall_id                │
│  · FTS5 full-text search (existing)                                │
│  · Batch write buffer (cap=20, flush every 3 turns)                │
└─────────────────────────────────────────────────────────────────────┘
```

### 3.2 Data Flow

```
Session.messages
  │
  ▼
TriageEngine.review(&messages, query, context_budget)
  │
  ├─ ACTIVE ─────→ build_llm_request.messages (subset, may shrink)
  ├─ STANDBY ────→ StandbyCache.set(block_id, content)
  ├─ COMPRESS ───→ merge → collapse depth → ACTIVE 或 STANDBY
  └─ COLD ───────→ ColdTier.buffer_write(block_id, summary, content)
                        │
                        ▼
                  recall_from_cold(query)  ←  LLM session.recall tool
```

---

## 4. Core Mechanism Design

### 4.1 Data Model

```rust
/// 每条消息或压缩块的 triage 决策
#[derive(Debug, Clone, PartialEq)]
enum TriageAction {
    /// 注入 active context
    Inject,
    /// 移入 standby cache（内存，1ms 召回）
    Standby { recall_id: String },
    /// 二次压缩
    Compress { depth: u32 },
    /// 移入 cold tier（SQLite，tool recall）
    Cold { recall_id: String, summary: String },
    /// 丢弃（已知被后续内容覆盖、或深度 >3 的旧压缩块）
    Discard,
}

/// Triage 评分快照（cached，避免每轮重算）
#[derive(Debug, Clone)]
struct ScoreSnapshot {
    score: f64,                // 综合评分 [0.0, 1.0]
    computed_at_turn: u32,
    ref_count_at_compute: u32,
    // 以下字段触发 recalc:
    //   - query_relevance_tokens 变了
    //   - turn 差 >5
    //   - ref_count 变了
    query_relevance_tokens: u64,  // MinHash 与 query 的相似度
}

/// Standby cache 条目
struct StandbyEntry {
    recall_id: String,
    content: Vec<Message>,
    summary: String,
    content_hash: u64,           // 用于校验过时
    source_turn: u32,            // 从 active 移入时的 turn
    last_active_turn: u32,       // 最后在 active 中的 turn
    created_at: Instant,         // TTL 起点
    warm_count: u32,             // 被 warm-up 次数（防高频弹跳）
}
```

### 4.2 Triage Score

评分公式（与现有 `CompositeScorer` 同构，保持 cache 兼容）：

```
score = 0.30 × relevance(query, text)
       + 0.25 × importance(ref_count, recency)
       + 0.15 × role_weight(role)        // User > Assistant > Tool > System
       + 0.10 × content_signal(text)     // 决策/错误/代码标记
       + 0.10 × tool_protocol_bonus      // tool 协议消息保底
       - 0.10 × compress_depth_penalty   // 已压缩次数越多分越低

relevance(query, text) = MinHashJaccard(text, query) × 1.0
                      + token_overlap_rate(text, query) × 0.5

importance(ref_count, recency):
  ref_factor = 1.0 - 0.7^ref_count    // 饱和，同现有 logic
  recency    = 0.5^(dist / 10)        // 半衰期 10 turn
  return ref_factor * 0.6 + recency * 0.4
```

### 4.3 Classification Thresholds

```
        ACTIVE (保留原文或截断)
            ↑  INJECT_THRESHOLD = 0.65
            │  (滞回: 当前 INJECT 的项目下降低于 0.50 才移出)
    ────────┼────────────────────────────────────
            │  STANDBY_ADMISSION = 0.40
        STANDBY
            │  COLD_ADMISSION = 0.20
    ────────┼────────────────────────────────────
        COLD
            │  DISCARD_THRESHOLD = 0.05
        DISCARD

Sticky bonus: 当前 INJECT 的项目 +0.10 bonus，持续 3 turn
Cooldown: 进入 COLD 后 10 turn 内不可 warm-up
Depth cutoff: compress_depth >= 3 → 强制 COLD 或 DISCARD（不再 COMPRESS）
```

### 4.4 Summary Block Injection

当 triage 移出了内容（STANDBY / COLD），在 messages 中 inject 一个紧凑的通知，替代当前 `[Compressed: N msgs, ~X tok, recover_id=...]` 格式：

```text
[Content Triage: 12 messages routed to cold (4,582 tok),
 3 blocks in standby cache.
 Use session.recall("keyword") to query cold-stored content.
 Standby content auto-warms on query match.]
```

新格式比旧格式更短，且不包含每条消息的 snippet（语义帧从"这里有摘要"改为"信息在冷端可查"）。

---

## 5. Accuracy Preservation Analysis

这是本方案的核心约束，逐层论证。

### 5.1 什么情况下 LLM 会丢精度

| 场景 | 影响 | 概率 |
|------|------|------|
| Triage 把必要上下文移出，LLM 没看到 | 决策基于不完整信息 | 需要消除 |
| Cold 中的内容 recall 失败（查不到/查错） | 决策基于不完整信息 | 低 |
| 用户查询了某话题，但相关历史在 standby 未自动 warm-up | LLM 没看到相关历史 | 中 |
| 旧压缩块被二次合并时丢失关键 decision | LLM 没看到决策 | 需要消除 |

### 5.2 防护一：基于已有信号，不引入新不确定性

Triage 评分用的全部是**已有的确定性信号**：
- `reference_counts`（post_process 每 turn 扫描 final_response 更新）
- `MinHashSig`（V41 auto_compress 已算）
- `content_type`（tool_protocol / decision / error / code — 正则匹配，现有 `importance_score` 逻辑）
- `compress_depth`（自增计数器，严格单调）

**不需要 LLM call，不需要 embedding，不引入新不确定因素**。

### 5.3 防护二：工具协议消息永不降温

```rust
// 在 triage 评分后、执行 action 前
fn enforce_invariants(blocks: &mut Vec<TriageBlock>) {
    for block in blocks.iter_mut() {
        // 工具协议消息：无条件 INJECT
        if has_tool_protocol(&block.messages) {
            block.action = TriageAction::Inject;
        }
        // 包含决策/error 标记：提高一级
        if has_decision_or_error(&block.messages) {
            block.score = block.score.max(0.70);
        }
    }
}
```

### 5.4 防护三：压缩合并的决策保留链

合并旧压缩块时，`[Preserved decisions: ...]` 从各子块提取，**合并而非替换**：

```
输入:
  [Compressed: 5 msgs, 1200 tok, recover_id=a]
    [Preserved decisions: 方案A; 选择了Rust]
  [Compressed: 3 msgs, 800 tok, recover_id=b]
    [Preserved decisions: 确认技术栈]

输出:
  [Merged: 2 blocks, 2000 tok, recover_ids=a,b]
    [Preserved decisions: 方案A; 选择了Rust; 确认技术栈]
```

### 5.5 防护四：Standby warm-up 匹配置信度

不是任意匹配都 warm-up，必须满足：

```
warm_threshold = 0.60  // 比 STANDBY_ADMISSION 高 50%

// 额外约束：至少有一个被标记的 decision/error/result 匹配
// 或 query_relevance > 0.75
```

防止低质量 recall 污染 active context。

### 5.6 一致性断言

```
不变量 1: 任何被 triage 移出的内容，在 LLM 提问时可通过 session.recall 召回
不变量 2: 工具执行链路 （tool_calls + responses） 完整保留在 active 中
不变量 3: 决策/错误/结果标记的内容 score >= 0.70（防护二）
不变量 4: standby 内容 warm-up 时校验 content_hash，不一致则丢弃
不变量 5: compress_depth > 3 的块不再 COMPRESS（防递归）
```

---

## 6. Risk Register

| ID | Risk | P | I | Mitigation |
|----|------|---|---|------------|
| R1 | Tier thrashing | M | H | Hysteresis (0.15 deadband) + sticky bonus + cooldown timer |
| R2 | Triage latency | M | M | Incremental score (cached, decay-based update for old blocks) |
| R3 | Standby cache pollution | L | M | Dual admission threshold + TTL evict |
| R4 | Cold write latency | H | L | Batch buffer (cap 20, flush per 3 turn) + dedup by hash |
| R5 | Triage/compress race | M | H | TriageState flag, compress skips INJECT blocks <3 turn old |
| R6 | Standby content stale | L | M | content_hash + last_active_turn validation on warm-up |
| R7 | Recursive compress | L | M | depth cap=3, flatten merge format |
| R8 | Query relevance false positive | M | M | No embedding; combo of MinHash + token overlap + TF signal |

P=Probability (L/M/H), I=Impact (L/M/H)

**R1 和 R5 是最高风险**，缓解方案已有，但需要实测验证。

---

## 7. Success Metrics

### 7.1 Primary (硬性)

| Metric | Target | Measurement |
|--------|--------|-------------|
| LLM 决策准确率对比 | 无统计显著差异 | A/B: triage on vs off, 100-turns, 50 queries each |
| session.recall 召回率 | >= 99% | 从 cold 召回测试集的成功比例 |
| messages.clone() 每轮耗时 | <= 60% baseline | baseline = 全部 clone, target = 只 clone INJECT 子集 |
| 摘要块叠加数 | <= 3 (per block) | baseline = 无上限, target = depth <= 3 |

### 7.2 Secondary (优化)

| Metric | Baseline | Target |
|--------|----------|--------|
| 每轮注入 token 数 | 100% | <= 70% |
| Tool definitions 重建率 | 每轮 100% | <= 30%（稳定工具 cache hit >= 70%） |
| System prompt 重算率 | 每轮 100% | <= 40%（稳定段 cache hit >= 60%） |
| 超长 session (200+ turn) messages 长度 | 200+ | <= 120 |

---

## 8. Testing & Validation Strategy

### 8.1 层次

| Layer | What | How |
|-------|------|-----|
| Unit | 每个评分维度独立正确性 | `cargo test`，与现有 auto_compress 结果对比 |
| Integration | TriageEngine 输入/输出 | Fixture: 50 条合成 messages + 10 query 变体，验证三路分流合理性 |
| Decision Audit | 每次 triage 记录日志 | `AuditRecord { block_id, score_breakdown, action, turn }`，可离线 replay 审查 |
| A/B | 精度对比 | 同一 session 走 triage vs 不走，比较 final_response 的决策一致性 |
| Adversarial | 压力测试 | 200+ turn session 模拟；高频工具调用；快速 topic 切换；乱序 query |

### 8.2 Decision Audit 机制

每个 triage 决策记录一个不可变事件：

```rust
#[derive(Debug, Clone, Serialize)]
pub struct TriageAuditRecord {
    pub turn: u32,
    pub block_id: String,
    pub block_type: BlockType,   // original / compressed / tool_protocol
    pub score: f64,
    pub score_breakdown: ScoreBreakdown,
    pub action: TriageAction,
    pub token_saved: usize,
    pub compress_depth: u32,
    pub is_tool_protocol: bool,
}
```

审计流式写入文件（每 session 一个），支持离线 `triage-audit-tool` 做 replay 分析。

### 8.3 Rollout Gate

```
triage_enabled = false  (config default)
   ↓
testing: triage_enabled = false + TriageAudit (记录但不执行动作)
   ↓
canary:  triage_enabled = true for selected sessions
   ↓
GA:      triage_enabled = true (backup compress 作为 safety net)
```

---

## 9. Phased Implementation Plan

| Phase | Scope | Dependencies | Est. Effort | Key Risk |
|-------|-------|-------------|-------------|----------|
| P0 | 数据模型（TriageAction, ScoreSnapshot, StandbyEntry）+ AuditRecord | None | 1d | - |
| P1 | TriageEngine.scorer（复用 CompositeScorer 已有逻辑，扩展为三路） | P0 | 2d | R8（评分不准确） |
| P2 | TriageEngine.classifier + enforce_invariants | P1 | 1d | R1（thrashing） |
| P3 | action_executor：INJECT/COMPREST/COMPRESS（替换 auto_compress 内部逻辑） | P2 | 2d | R5（竞态） |
| P4 | StandbyCache（DashMap + TTL + warm-up bridge） | P0 | 2d | R3/R6 |
| P5 | ColdTier 扩展（消息块级 + batch buffer） | P0 | 2d | R4 |
| P6 | Tool definitions standby（hash 比较跳过重建） | P4 的简化版 | 1d | cache 时效 |
| P7 | System segments 预计算 + hash cache | 无 | 1d | 稳定段判定条件 |
| P8 | Pipeline 集成（setup() 阶段插入 triage）+ config gating | P2-P7 | 1d | R5（集成风险） |
| P9 | Decision Audit 离线分析工具 | P0 | 1d | - |

**总估计：13 天。** P1-P3（5 天）可单独上线获取 early benefit。

---

## 10. Full-Stack Integration Map

### 10.1 需要适配的子系统及变动幅度

```
┌──────────────────────────────────────────────────────────────────────┐
│                         LEGEND                                        │
│  ● 核心变更（必须改）  ○ 可选增强  — 无影响                           │
└──────────────────────────────────────────────────────────────────────┘

子系统               P1-P3 P4  P5  P6  P7  P8  P9  备注
────────────────────────────────────────────────────────────────────
abacus-core:
  triage.rs (新)       ●   ●   ●   ●   ●   ●   ●   TriageEngine 本体
  standby_cache.rs(新)  -   ●   -   ●   -   -   -   StandbyCache
  context.rs           -   -   ●   -   -   ●   -   冷端扩展 + 集成入口
  compress_math.rs     ○   -   -   -   -   -   -   复用 MinHash/Scorer
  pipeline/mod.rs      -   -   -   -   -   ●   -   setup() 插入 triage
  pipeline/post.rs     -   -   -   -   -   ○   -   compress_tracker 扩展
  session_store.rs     -   -   ●   -   -   -   -   SQLite schema 扩展
  mag_chain.rs         -   -   -   -   -   ○   -   TriageHook 注册

abacus-core/llm:
  stream.rs            -   -   -   -   -   ●   -   +StreamChunk::TriageResult
  providers/*          -   -   -   -   -   ○   -   LlmRequest triage 标记

abacus-cli:
  tui/run.rs           -   -   -   -   -   ●   -   流式事件路由
  tui/components/*     -   -   -   -   -   ○   -   看板新增 triage 指示器
  tui/state/mod.rs     -   -   -   -   -   ●   -   状态扩展（可选显示用）
  tui/api/mod.rs       -   -   -   -   -   ○   -   RequestContext 扩展
  commands/*           -   -   -   -   -   -   ○   session list 增强

  triage-audit (新)    -   -   -   -   -   -   ●   离线 CLI 分析工具
```

### 10.2 版本依赖关系

```
P0  (数据模型)
 └→ P1 (评分引擎) ─→ P2 (分类器) ─→ P3 (执行器)
      └→ P4 (StandbyCache) ─→ P6 (ToolDefs cache)
      └→ P5 (ColdTier) ─→ P8 (Pipeline 集成)
                              └→ P8 依赖 P2-P7 全部就绪
P7 (System cache) 独立，无前置依赖
P9 (审计工具) 依赖 P0，其余独立
```

**版本兼容性矩阵**：

| 版本号 | Triaged | 向后兼容 | 变更 |
|--------|---------|----------|------|
| v2.0.x | ❌ | ✅ | 基线版本，无 triage 代码 |
| v2.1.0 | ⚠️ Audit only | ✅ | 加 TriageAudit 数据模型 + 日志写位，triage 不执行 |
| v2.2.0 | ✅ Canary | ✅ | 加 TriageEngine + config gate，默认关闭 |
| v2.3.0 | ✅ GA | ⚠️ | 默认开启，旧 session 数据兼容，SQLite schema 迁移 |

---

## 11. Amplification Effects（跨子系统放大增益）

### 11.1 Triage × Memory Palace → 跨 session 经验积累

**效应**：Triage 决策（哪些块被降温、哪些被 recall）记录为 palace 行为模式 → 下次同类型输入时 palace 提前预测 triage 结果。

```
Turn N:  Triage 把"调 API 的对话" → COLD
         ↓ 记录为 palace.behavior("api_call_conversation", "triage_cold")
         ↓
Turn N+M: palace.classify_input("怎么调 OpenAI API?")
          → domain=("api_call", 0.85), triage_hint=("cold", 0.80)
          → TriageEngine 直接采纳 hint，跳过评分
```

**增益**：评分延迟从 ~5ms → ~0μs（cache hit），且随 session 数自动提高准确率。

**需要适配**：
- `MemoryPalace::record_triage_action(action, score, block_id)` — 新方法
- `classify_input()` 返回字段扩展 `Option<TriageHint>`
- PalaceAbsorbHook 扩展：cold demote 时记录 triage 历史

### 11.2 Triage × Knowledge Store → 评分质量放大

**效应**：KB 已经存储了文件层级 + FTS5 trigram + embedding 三重索引。Triage 的 relevance 评分可以直接复用。

```
relevance(query, block) = max(
    KB FTS5 BM25 score(query, block_text),   // 0-cost 已有索引
    KB semantic score(query, block)           // 有 embedder 时
)
```

**增益**：不必自建 relevance 索引，KB 已有的 FTS5 全文搜索直接提供 BM25 评分（0 额外成本）。当用户配置了 embedder（vLLM/MLX）时，语义评分自动提升精度。

**需要适配**：
- `KnowledgeStore::relevance_score(query, text) → f64` — 新方法，封装 FTS5 + 可选 semantic
- 无需改 schema，FTS5 已经支持

### 11.3 StandbyCache × ICL Primer → 合并注入路径

**效应**：当前 ICL 每轮独立检索 KB → `user_message_preamble`。StandbyCache 可以缓存 ICL 结果 + KB chunks。

```
当前: 每轮 query KB → 3 results → inject preamble
      ICL 结果不跨轮复用
      
Triage 后: ICL 结果写入 StandbyCache
          下一轮若 query 语义接近 → Standby warm-up → 省一次 KB query
          若 query 完全不同 → 走正常 KB query，新结果覆盖
```

**增益**：ICL-rich session 下减少 50%+ KB query 次数（类似 query 连续出现时，如"继续重构"）。

### 11.4 ToolDefs Cache × Pipeline → 减 iteration 重建

**效应**：`execute_loop` 内每次 `loop_iter` 都重建 tool_defs（工具 schema）。对稳定工具集完全浪费。

```
当前: loop_iter=0 → build_tool_defs() → LLM call
      loop_iter=1 → build_tool_defs() ← 完全相同   重复
      loop_iter=2 → build_tool_defs() ← 完全相同   重复

Triage 后: tool_defs_hash = hash(all_defs)
           cache[hash] = serialized_defs
           下次 hash 未变 → 跳过 build_tool_defs_for
```

**增益**：多 iteration 场景（工具密集型 turn）减少 30-80% 的 tool_defs 重建时间。

### 11.5 Triage × MagChain → 安全放大

**效应**：TriageHook 作为 `PipelineHook` 注入 - 三处接入点：

```
TurnStart     → triage 输入 → 如果高风险 → HookAction::Abort（避过 LLM 处理恶意输入）
PromptBuilt   → triage 已完成的 signal → EpistemicGuard 消费（调整知识约束强度）
TurnPostFanOut → was_triaged 标记 → 下游 hook 根据标记做差异化处理
```

**增益**：现有安全系统（EpistemicGuard、PiiRedactor、CircuitBreaker）全部可以消费 triage 信号，形成一个**级联安全网**——triage 在第一层拦截明显可降温的内容，降低后续安全 hook 的负载。

### 11.6 Triage × Deduction Engine → 质量归因

**效应**：Deduction Engine（`deduction/` 目录）记录每轮 context_usage + was_compressed。triage 新增 `was_triaged` + `triage_summary` 字段后，deduction 可以分析"triage 是否导致决策质量变化"。

```
SELECT turn_number, was_triaged, decision_confidence
FROM deduction_analysis
WHERE was_triaged = true
  AND decision_confidence < 0.5
```
→ 自动发现 triage 导致的假阴性案例。

**增益**：对"零认知精度损失"这一核心约束提供可观测性，不依赖 A/B 测试。

---

## 12. User Experience Constraints

### 12.1 用户感知层要求

| 感知维度 | 用户关心吗 | Triage 带来的影响 | 呈现策略 |
|----------|-----------|-------------------|----------|
| **Token 消耗** | ✅ 很关心（成本） | 正面：注入减少 → 用量下降 | 不需要告知 triage，用户的 token 账单自然说明一切 |
| **回复质量** | ✅ 核心诉求 | 零差异（设计约束） | 有差异就是 bug，audit 会自动告警 |
| **等待时间** | ✅ 敏感 | 微弱正面：messages 变短 → 序列化 + 传输更快 | 不需要告知 |
| **交互过程** | ✅ 敏感 | 工具链路完整 → 感知无变化 | 注入的新 summary 用户看到了也看不懂（[Content Triage: ...]），应改为纯 system 角色，TUI 不渲染 |
| **Session 历史** | ✅ 查阅历史时 | session.recall / messages_recover 仍可用 | recall 接口不变，用户无感知 |

### 12.2 显示策略

- **看板（dashboard）**：不新增 TUI 元素。如果用户打开 `context_status` 工具能看到 triage 统计（已压缩块数、standby 大小），但默认不看。
- **Streaming 事件**：Triage 决策在 setup() 阶段完成，不产生新的 StreamChunk 事件。如果事后 compress 被触发，按现有 CompressStart/CompressEnd 显示。用户不需要知道"事前审校"这件事。
- **错误/异常**：如果 standby warm-up 因 content_hash 不一致而丢弃，**静默忽略**，不 toast、不日志告警（这是正常操作，不是异常）。

### 12.3 冷却内容对用户的可见性

当用户主动问了一个问题，而该问题涉及已进入 COLD 的内容时：

```
用户: "之前我们讨论的 API 设计方案是什么？"
  ↓
Triage: 检测到 query 含 "API" + "设计方案"
  → Standby warm-up: 未命中
  → 注入 summary block 含 [Content Triage: ... session.recall("API 设计方案") ...]
  ↓
LLM 看到 summary block，调 session.recall("API 设计方案")
  → Cold tier FTS5 命中 → 返回原内容
  → LLM 基于完整信息回复
```

用户视角：**没有感知到 triage**。LLM 调了 `session.recall`（这是一个公开的工具，LLM 本来就会用）。一切正常。

### 12.4 LLM 对 triage 的知情程度

- LLM **看到** triage summary block（系统级通知，User 角色）
- LLM **知道**有 `session.recall` 和 `messages_recover` 工具可用（已有工具定义）
- LLM **不需要知道** triage 引擎的存在——它只需要知道"有些内容不在 active context 里了，但你随时可以调工具查"
- Standby warm-up 对 LLM 完全透明：`warm_count >= 1` 的内容 warm-up 后跟从未移出过一样

### 12.5 失败降级（User-Facing）

| 故障 | 用户看到什么 | 影响 |
|------|------------|------|
| TriageEngine panic | 无（catch_unwind + fallback to full inject） | 本轮多花 token |
| StandbyCache OOM | 无（FIFO evict 天然防 OOM） | 无 |
| Cold tier write 失败 | 无（batch buffer 延迟重试，告警记录日志） | 冷数据可能丢失，不影响本轮 |
| Warm-up 误触发 | 无（content_hash 校验失败 → 丢弃） | 浪费几 ms |
| Triage 把必要内容移入 COLD 且 recall 无法访问 | LLM 回"我不确定，之前讨论过吗？" | 信息丢失（最坏情况，靠 audit 预防） |

---

## 13. Revised Phased Implementation Plan

### 13.1 各 Phase 的子系统改动明细

| Phase | Core | CLI/TUI | KB/Palace | Config | 预估 |
|-------|------|---------|-----------|--------|------|
| P0: 数据模型 | `triage.rs`(新建) | - | - | - | 1d |
| P1: 评分引擎 | `compress_math.rs`(扩展), `triage.rs`(Scorer) | - | - | `CoreConfig` + `TriagedConfig` | 2d |
| P2: 分类器+不变量 | `triage.rs`(Classifier) | - | - | - | 1d |
| P3: 执行器 | `triage.rs`(Executor), `context.rs`(集成) | - | - | `TriagedConfig.enabled` | 2d |
| P4: StandbyCache | `standby_cache.rs`(新建) | - | - | - | 2d |
| P5: ColdTier 扩展 | `session_store.rs`, `context.rs` | - | - | - | 2d |
| P6: ToolDefs cache | `triage.rs`(ToolCache 子模块) | - | - | - | 1d |
| P7: System cache | `triage.rs`(SystemCache 子模块) | - | - | - | 1d |
| P8: 集成 + 管线 | `pipeline/mod.rs` | `tui/run.rs`, `stream.rs` | - | - | 1d |
| P9: 审计工具 | - | `triage-audit`(新建 CLI) | - | - | 1d |
| **放大增益** | | | | | |
| A1: Palace × Triage | - | - | `memory_palace.rs` | - | 1d |
| A2: KB × Triage | - | - | `knowledge_store.rs` | - | 1d |
| A3: MagChain × Triage | `mag_chain.rs` | - | - | - | 1d |
| A4: Deduction × Triage | `deduction/` | - | - | - | 0.5d |

### 13.2 推荐执行顺序

```
Week 1-2: P0 → P1 → P2 → P3（核心引擎，5d，无用户感知）
Week 3:   P4 (StandbyCache) + P5 (ColdTier) 并行（4d）
Week 4:   P6 + P7 + P8 并行 + P9（4d，此时可开启 canary）
Week 5:   A1 + A2 + A3 + A4 放大增益并行（3.5d）
```

**总预估：13 + 4（放大）= 17 天**，每个 Week 结束状态可上线。

### 13.3 版本标记与回滚

```
triage_enabled:
  - explicit false  → 走完全原有路径，triage 代码不执行，0 风险
  - explicit true   → 走 triage，backup compress 作为 safety net
  - unset (默认)    → Audit only（记录但不执行动作）

回滚条件（auto-disable trigger）:
  - TriageAudit 分析发现决策准确率低于 baseline > 3%
  - Cold recall 失败率 > 1%
  - 任何 enforce_invariants 触发（工具协议消息被降温）
```

---

## 14. Open Questions

| # | Question | Suggested Approach |
|---|----------|-------------------|
| Q1 | Standby 容量 200 条是否够？极端 session 可能更多 | 可配置，先从 200 开始，观测命中率 |
| Q2 | Cold tier block-level 存储是否和现有 snapshot 共存？ | 扩展 schema，两种记录类型用 type 字段区分 |
| Q3 | Triage 和 backup compress 同时启用时的精确协调策略？ | TriageState 标记 + compress 检查，P3 细化 |
| Q4 | 旧版 auto_compress 注入的 `[Compressed: ...]` 块是否兼容？ | Triage 识别旧格式，转为新格式+写入 archive |
| Q5 | 是否需要 `session.standby_list` 工具让 LLM 查看 standby 内容？ | 可选，P4 后考虑 |
| Q6 | Triage summary block 应该用 User 还是 System 角色？ | 用 System 角色（当前 auto_compress 用 User 是 bug）。System 不会破 user/assistant 交替序列 |
| Q7 | Triage 在热路径上的延迟预算？ | <=10ms（从 pipeline setup() 入口到 produce triage decision，超出则回退到 full inject）|
| Q8 | 旧压缩块升级到新 triage 格式后，旧的 `messages_recover` 工具是否还能用？ | 兼容：archive 中的 recover_id 保持不变，旧格式的新格式共存 |
| Q9 | Triage × Palace 的模式是 session 内学习还是跨 session？ | 跨 session（Palace 是全局的），但 triage_hint 冷却 = 3 session 内有效 |
| Q10 | 用户能看到 token 消耗下降，是否应该显示"省了多少"？ | 不应该：用户的看板只显示当前 turn 的用量，不显示"没发生"的用量 |

---

## Appendix A: Comparison with Current System

| Aspect | Current | Proposed |
|--------|---------|----------|
| Trigger | Context >= 85% (reactive) | Every turn (proactive) |
| Decision | Keep tail + Compress middle | 5-way: Inject/Standby/Compress/Cold/Discard |
| Messages Vec | Monotonic growth | Can shrink (standby/cold removal) |
| Summary format | `[Compressed: N msgs, snippets...]` | `[Content Triage: N blocks to cold, M in standby]` |
| Tool definitions | Rebuilt every iteration | Hash-based cache |
| System prompt | Full assembly | Stable segments pre-cached |
| Accuracy risk | Summary may lose detail | Invariant enforcement + audit |

## Appendix B: Code Change Estimate

```
Modified files:
  pkg/crates/abacus-core/src/core/context.rs        (scorer + triage integration)
  pkg/crates/abacus-core/src/core/pipeline/mod.rs   (setup() insertion)
  pkg/crates/abacus-core/src/core/pipeline/post.rs  (compress_tracker 扩展)
  pkg/crates/abacus-core/src/core/session_store.rs  (cold tier 扩展)
  pkg/crates/abacus-core/src/core/compress_math.rs  (MinHash 复用 + 评分扩展)

New files:
  pkg/crates/abacus-core/src/core/triage.rs          (TriageEngine)
  pkg/crates/abacus-core/src/core/standby_cache.rs   (StandbyCache)
  pkg/crates/abacus-core/src/core/triage_audit.rs    (AuditRecord)
  pkg/crates/abacus-triage-audit/                    (离线分析 CLI tool)

Cargo.toml:
  abacus-core: +dashmap
