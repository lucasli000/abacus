//! 双宫殿记忆系统 — 行为宫殿 + 知识宫殿
//!
//! ## 依赖
//! - `serde/serde_json`: 序列化
//! - `rusqlite`: 持久化存储
//!
//! ## 引用关系
//! - 被 `CoreLoop` 在 session 结束时调用
//! - 被 `SkillEngine` 查询相关知识
//! - 被 `PromptAssembly` 注入活跃知识
//!
//! ## 架构
//! ```text
//! 行为宫殿 (Behavior Palace)
//!   └── 交互模式记忆 (用户偏好、工具使用习惯、纠正历史)
//!       └── 通过关系链桥接 → 知识宫殿
//!
//! 知识宫殿 (Knowledge Palace)
//!   └── 领域知识 (技术栈最佳实践、项目结构、领域规则)
//!       └── SM-2 衰减更新
//! ```

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use serde::{Deserialize, Serialize};

// ─── 行为宫殿 ───────────────────────────────────────────────────────────

/// 行为记忆条目
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BehaviorMemory {
    pub id: String,
    pub pattern: String,
    pub frequency: u32,
    pub last_seen: i64,
    pub confidence: f64,
    pub tags: Vec<String>,
    pub created_at: i64,
}

/// 行为宫殿（最大 2000 条目，超出时 LFU 淘汰）
pub struct BehaviorPalace {
    memories: RwLock<HashMap<String, BehaviorMemory>>,
}

const MAX_BEHAVIOR_ENTRIES: usize = 2000;

impl BehaviorPalace {
    pub fn new() -> Self {
        Self {
            memories: RwLock::new(HashMap::new()),
        }
    }

    pub async fn store(&self, memory: BehaviorMemory) {
        let mut memories = self.memories.write().await;
        memories.insert(memory.id.clone(), memory);
    }

    pub async fn get(&self, id: &str) -> Option<BehaviorMemory> {
        self.memories.read().await.get(id).cloned()
    }

    /// cross-session: 当前条目数（仅活跃，含冷却态）
    /// 引用：CoreLoop::handle_interaction_tool magchain_status 上报
    pub async fn len(&self) -> usize {
        self.memories.read().await.len()
    }

    /// 配套 is_empty（与 len() 同源；clippy::len_without_is_empty 要求）
    pub async fn is_empty(&self) -> bool {
        self.memories.read().await.is_empty()
    }

    pub async fn search(&self, tags: &[String]) -> Vec<BehaviorMemory> {
        let memories = self.memories.read().await;
        memories.values()
            // 冷却态条目（confidence < 0.1）不参与搜索匹配
            .filter(|m| m.confidence >= 0.1)
            .filter(|m| m.tags.iter().any(|t| tags.contains(t)))
            .cloned()
            .collect()
    }

    /// Phase γ-Palace-C：取行为内存全快照（用于 sync_from_palace）
    pub async fn snapshot(&self) -> HashMap<String, BehaviorMemory> {
        self.memories.read().await.clone()
    }

    pub async fn record_interaction(&self, pattern: &str, tags: &[String]) {
        let mut memories = self.memories.write().await;
        let now = chrono::Utc::now().timestamp();
        let id = pattern.to_string();
        let entry = memories.entry(id.clone()).or_insert_with(|| BehaviorMemory {
            id,
            pattern: pattern.to_string(),
            frequency: 0,
            last_seen: now,
            confidence: 0.5,
            tags: tags.to_vec(),
            created_at: now,
        });
        entry.frequency += 1;
        entry.last_seen = now;
        entry.confidence = (entry.confidence + 0.1).min(1.0);

        // LFU eviction when over capacity
        if memories.len() > MAX_BEHAVIOR_ENTRIES {
            let weakest = memories.values()
                .min_by_key(|m| m.frequency)
                .map(|m| m.id.clone());
            if let Some(id) = weakest {
                memories.remove(&id);
            }
        }
    }
}

impl Default for BehaviorPalace {
    fn default() -> Self { Self::new() }
}

// ─── 知识宫殿 ───────────────────────────────────────────────────────────

/// 知识条目
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnowledgeEntry {
    pub id: String,
    pub title: String,
    pub content: String,
    pub domain: String,
    pub sm2_ease: f64,
    pub sm2_interval_days: f64,
    pub sm2_repetitions: u32,
    pub last_reviewed: i64,
    pub next_review: i64,
    pub tags: Vec<String>,
}

impl KnowledgeEntry {
    pub fn new(id: impl Into<String>, title: impl Into<String>, content: impl Into<String>, domain: impl Into<String>) -> Self {
        let now = chrono::Utc::now().timestamp();
        Self {
            id: id.into(),
            title: title.into(),
            content: content.into(),
            domain: domain.into(),
            sm2_ease: 2.5,
            sm2_interval_days: 1.0,
            sm2_repetitions: 0,
            last_reviewed: now,
            next_review: now + 86400,
            tags: vec![],
        }
    }

    /// SM-2 算法更新
    pub fn sm2_update(&mut self, quality: f64) {
        let quality = quality.clamp(0.0, 5.0);
        if quality >= 3.0 {
            self.sm2_repetitions += 1;
            if self.sm2_repetitions == 1 {
                self.sm2_interval_days = 1.0;
            } else if self.sm2_repetitions == 2 {
                self.sm2_interval_days = 6.0;
            } else {
                self.sm2_interval_days *= self.sm2_ease;
            }
            self.sm2_ease += 0.1 - (5.0 - quality) * (0.08 + (5.0 - quality) * 0.02);
            self.sm2_ease = self.sm2_ease.max(1.3);
        } else {
            self.sm2_repetitions = 0;
            self.sm2_interval_days = 1.0;
        }
        self.last_reviewed = chrono::Utc::now().timestamp();
        self.next_review = self.last_reviewed + (self.sm2_interval_days * 86400.0) as i64;
    }
}

/// 知识宫殿（最大 5000 条目，超出时按 SM-2 interval 最长的淘汰）
pub struct KnowledgePalace {
    entries: RwLock<HashMap<String, KnowledgeEntry>>,
}

const MAX_KNOWLEDGE_ENTRIES: usize = 5000;

impl KnowledgePalace {
    pub fn new() -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
        }
    }

    /// cross-session: 当前条目数
    /// 引用：CoreLoop::handle_interaction_tool magchain_status 上报跨 session 知识量
    pub async fn len(&self) -> usize {
        self.entries.read().await.len()
    }

    /// 配套 is_empty（clippy::len_without_is_empty）
    pub async fn is_empty(&self) -> bool {
        self.entries.read().await.is_empty()
    }

    /// 写入（持锁检测+插入，消除 TOCTOU 竞态）。
    ///
    /// 同域重复 → 拒绝并返回 false
    /// 返回 (是否写入, 跨域相似条目ID列表) — 调用方处理关系链
    pub async fn store(&self, entry: KnowledgeEntry) -> (bool, Vec<String>) {
        let mut entries = self.entries.write().await;
        if entries.values().any(|e| e.content == entry.content) {
            return (false, vec![]);
        }
        if same_domain_duplicate(&entry, &entries) {
            return (false, vec![]);
        }
        let similar = find_similar_cross_domain(&entry, &entries);
        entries.insert(entry.id.clone(), entry);

        // 容量淘汰: 移除 SM-2 interval 最长（最冷）的条目
        if entries.len() > MAX_KNOWLEDGE_ENTRIES {
            let coldest = entries.values()
                .max_by(|a, b| a.sm2_interval_days.partial_cmp(&b.sm2_interval_days).unwrap_or(std::cmp::Ordering::Equal))
                .map(|e| e.id.clone());
            if let Some(id) = coldest {
                entries.remove(&id);
            }
        }

        (true, similar)
    }

    /// 获取 memory 中已有条目的 `(domain, id)` 供 bridge 使用
    pub async fn entry_ids_by_domain(&self, domain: &str) -> Vec<String> {
        let entries = self.entries.read().await;
        entries.values()
            .filter(|e| e.domain == domain)
            .map(|e| e.id.clone())
            .collect()
    }
}

/// 同域标题模糊匹配检测
fn same_domain_duplicate(entry: &KnowledgeEntry, entries: &HashMap<String, KnowledgeEntry>) -> bool {
    let title_words: std::collections::HashSet<&str> = entry.title
        .split_whitespace().filter(|w| w.len() > 1).collect();
    if title_words.is_empty() { return false; }
    entries.values().filter(|e| e.domain == entry.domain).any(|e| {
        let other: std::collections::HashSet<&str> = e.title
            .split_whitespace().filter(|w| w.len() > 1).collect();
        if other.is_empty() { return false; }
        let intersection = title_words.intersection(&other).count();
        intersection as f64 / title_words.len().max(other.len()) as f64 > 0.5
    })
}

/// 跨域关键词重叠检测 > 60% → 返回匹配的条目 ID 列表
fn find_similar_cross_domain(entry: &KnowledgeEntry, entries: &HashMap<String, KnowledgeEntry>) -> Vec<String> {
    let words: std::collections::HashSet<&str> = entry.content.split_whitespace()
        .filter(|w| w.len() > 2).collect();
    if words.is_empty() { return vec![]; }
    entries.values()
        .filter(|e| e.domain != entry.domain)
        .filter_map(|e| {
            let e_words: std::collections::HashSet<&str> = e.content.split_whitespace()
                .filter(|w| w.len() > 2).collect();
            if e_words.is_empty() { return None; }
            let intersection = words.intersection(&e_words).count();
            let overlap = intersection as f64 / words.len().max(e_words.len()) as f64;
            if overlap > 0.6 {
                Some(e.id.clone())
            } else {
                None
            }
        })
        .collect()
}

impl KnowledgePalace {
    pub async fn get(&self, id: &str) -> Option<KnowledgeEntry> {
        self.entries.read().await.get(id).cloned()
    }

    /// 获取到期需 review 的条目（遗忘曲线：SM-2 next_review ≤ now）
    pub async fn get_due_for_review(&self) -> Vec<KnowledgeEntry> {
        let now = chrono::Utc::now().timestamp();
        let entries = self.entries.read().await;
        entries.values()
            .filter(|e| e.next_review <= now)
            .cloned()
            .collect()
    }

    pub async fn review(&self, id: &str, quality: f64) {
        let mut entries = self.entries.write().await;
        if let Some(entry) = entries.get_mut(id) {
            entry.sm2_update(quality);
        }
    }

    pub async fn search(&self, query: &str) -> Vec<KnowledgeEntry> {
        let entries = self.entries.read().await;
        let lower = query.to_lowercase();
        entries.values()
            .filter(|e| {
                e.title.to_lowercase().contains(&lower) ||
                e.content.to_lowercase().contains(&lower) ||
                e.domain.to_lowercase().contains(&lower) ||
                e.tags.iter().any(|t| t.to_lowercase().contains(&lower))
            })
            .cloned()
            .collect()
    }
}

impl Default for KnowledgePalace {
    fn default() -> Self { Self::new() }
}

// ─── 关系链桥接 ─────────────────────────────────────────────────────────

impl RelationType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ParentChild => "ParentChild",
            Self::SummaryDetail => "SummaryDetail",
            Self::SeeAlso => "SeeAlso",
            Self::DependsOn => "DependsOn",
            Self::Similar => "Similar",
            Self::Exclusive => "Exclusive",
            Self::Replaces => "Replaces",
            Self::ComposesTo => "ComposesTo",
            Self::TagAggregator => "TagAggregator",
            Self::DraftOf => "DraftOf",
            Self::RevisionOf => "RevisionOf",
            Self::Supersedes => "Supersedes",
            Self::RequiresKnowledge => "RequiresKnowledge",
            Self::SupportsBehavior => "SupportsBehavior",
            Self::RelatedBehavior => "RelatedBehavior",
            Self::RelatedKnowledge => "RelatedKnowledge",
        }
    }

    /// 返回关系所属的维度分类
    pub fn category(&self) -> &'static str {
        match self {
            Self::ParentChild | Self::SummaryDetail => "structural",
            Self::SeeAlso | Self::DependsOn => "reference",
            Self::Similar | Self::Exclusive | Self::Replaces => "comparison",
            Self::ComposesTo | Self::TagAggregator => "composition",
            Self::DraftOf | Self::RevisionOf | Self::Supersedes => "evolution",
            Self::RequiresKnowledge | Self::SupportsBehavior |
            Self::RelatedBehavior | Self::RelatedKnowledge => "bridge",
        }
    }

    /// 关系是否可逆（双向）
    pub fn is_bidirectional(&self) -> bool {
        matches!(self, Self::Similar | Self::RelatedBehavior |
            Self::RelatedKnowledge | Self::Exclusive)
    }
}

/// 关系类型 — 五维知识关系体系
///
/// | 维度 | 关系 | 语义 | 用例 |
/// |------|------|------|------|
/// | 🏗 结构化 | ParentChild | 父子/大类含小类 | "技术文档"→"API说明" |
/// | | SummaryDetail | 总分/概述→具体章节 | "架构概览"→"模块详解" |
/// | 🔗 关联引用 | SeeAlso | 参见/相关概念跳转 | "登录"→"密码找回" |
/// | | DependsOn | 前置依赖/A必须了解B | "HTTPS"→"TLS" |
/// | ⚖️ 对比互斥 | Similar | 相似路径同一结论 | "快排"↔"归并" |
/// | | Exclusive | 同一前提对立结论 | "乐观锁"↔"悲观锁" |
/// | | Replaces | 版本替代/新版替旧版 | "v2"→"v1"（旧版） |
/// | 🧩 组成聚合 | ComposesTo | 多前提构成目标 | "HTTPS"→"证书"+"TLS" |
/// | | TagAggregator | 标签横向聚合 | 多篇文章同话题串联 |
/// | 🔄 演变版本 | DraftOf | 草稿←正式发布 | 待审内容→已发布 |
/// | | RevisionOf | 历史版本回溯 | 当前→历史修订 |
/// | | Supersedes | 废弃替代 | 旧标准→新标准 |
/// | 跨域桥接 | RequiresKnowledge | 行为→领域知识 | 偏好→对应知识 |
/// | | SupportsBehavior | 知识→行为支撑 | 知识→偏好 |
/// | | RelatedBehavior | 行为↔行为 | 偏好相关 |
/// | | RelatedKnowledge | 知识↔知识（通用） | 通用关联 |
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum RelationType {
    // ── 🏗 结构化 ──
    /// 父子：大类包含小类（"技术文档"→"API说明"）
    ParentChild,
    /// 总分：概述指向具体章节（"架构概览"→"模块详解"）
    SummaryDetail,

    // ── 🔗 关联引用 ──
    /// 参见：相关概念跳转链接
    SeeAlso,
    /// 前置依赖：A必须了解B后才能理解C
    DependsOn,

    // ── ⚖️ 对比互斥 ──
    /// 相似：不同路径指向同一结论
    Similar,
    /// 相斥：同一前提得出对立结论
    Exclusive,
    /// 版本替代：新版本替换旧版本
    Replaces,

    // ── 🧩 组成聚合 ──
    /// 组合：多个前提构成一个目标
    ComposesTo,
    /// 标签聚合：跨类别内容横向串联
    TagAggregator,

    // ── 🔄 演变版本 ──
    /// 草稿与发布：待审内容→正式内容
    DraftOf,
    /// 历史版本：当前文档→历史修订记录
    RevisionOf,
    /// 废弃替代：旧标准→新标准（不可逆）
    Supersedes,

    // ── 跨域桥接（行为×知识） ──
    RequiresKnowledge,
    SupportsBehavior,
    RelatedBehavior,
    RelatedKnowledge,
}

/// 关系
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryRelation {
    pub from_id: String,
    pub to_id: String,
    pub relation_type: RelationType,
    pub strength: f64,
}

/// 关系链
pub struct MemoryBridge {
    relations: RwLock<Vec<MemoryRelation>>,
}

impl MemoryBridge {
    pub fn new() -> Self {
        Self {
            relations: RwLock::new(Vec::new()),
        }
    }

    /// 添加关系（自动去重：相同 from + to + type 不重复添加）
    pub async fn add_relation(&self, relation: MemoryRelation) {
        let mut relations = self.relations.write().await;
        let exists = relations.iter().any(|r| {
            r.from_id == relation.from_id && r.to_id == relation.to_id
                && r.relation_type == relation.relation_type
        });
        if !exists {
            relations.push(relation);
        }
    }

    pub async fn get_related(&self, id: &str) -> Vec<MemoryRelation> {
        let relations = self.relations.read().await;
        relations.iter()
            .filter(|r| r.from_id == id || r.to_id == id)
            .cloned()
            .collect()
    }

    /// 删除所有涉及 `ids` 中任一 ID 的关系（用于 prune 清理悬空引用）
    pub async fn remove_relations_for(&self, ids: &[String]) {
        let mut relations = self.relations.write().await;
        relations.retain(|r| !ids.contains(&r.from_id) && !ids.contains(&r.to_id));
    }
}

impl Default for MemoryBridge {
    fn default() -> Self { Self::new() }
}

// ─── 双宫殿管理器 ───────────────────────────────────────────────────────
// 双宫殿记忆系统管理器（孤立 doc 已转 plain comment——历史 item 已被移走/重命名）
// ─── Embedding 服务接口 ─────────────────────────────────────────────────

/// 本地 embedding 服务接口（预留给本地 embedding 模型如 BGE-M3、Nomic 等）
///
/// ## 设计
/// - 异步接口，支持本地推理（CPU/GPU）或本地 HTTP 服务（如 Ollama embedding endpoint）
/// - `embed_text` 返回向量（维度由具体实现决定）
/// - `similarity` 计算两段文本的语义相似度 [0, 1]
/// - `batch_embed` 支持批量处理（减少启动开销）
///
/// ## 生命周期
/// - 由 DualPalaceMemory 持有（Option<Arc<dyn MemoryEmbedder>>）
/// - 注入时机：CoreLoop 初始化时通过 `set_embedder()` 设置
/// - 未注入时：退化到关键词/标签匹配（当前行为不变）
///
/// ## 预期实现
/// - `OllamaEmbedder`: HTTP 调用本地 Ollama embedding endpoint
/// - `OnnxEmbedder`: 直接加载 ONNX 模型在进程内推理
/// - `NgramFallback`: 零依赖 n-gram Jaccard（当前默认）
#[async_trait::async_trait]
pub trait MemoryEmbedder: Send + Sync {
    /// 将文本编码为向量
    async fn embed_text(&self, text: &str) -> Result<Vec<f32>, String>;

    /// 批量编码（默认实现逐条调用，具体实现可覆盖以提升吞吐）
    async fn batch_embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, String> {
        let mut results = Vec::with_capacity(texts.len());
        for text in texts {
            results.push(self.embed_text(text).await?);
        }
        Ok(results)
    }

    /// 计算两段文本的语义相似度 [0.0, 1.0]
    async fn similarity(&self, a: &str, b: &str) -> Result<f64, String> {
        let va = self.embed_text(a).await?;
        let vb = self.embed_text(b).await?;
        Ok(cosine_similarity(&va, &vb))
    }

    /// 返回向量维度（用于存储预分配）
    fn dimension(&self) -> usize;

    /// 模型标识（用于日志/调试）
    fn model_name(&self) -> &str;
}

/// 余弦相似度计算
fn cosine_similarity(a: &[f32], b: &[f32]) -> f64 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f64 = a.iter().zip(b.iter()).map(|(x, y)| (*x as f64) * (*y as f64)).sum();
    let norm_a: f64 = a.iter().map(|x| (*x as f64) * (*x as f64)).sum::<f64>().sqrt();
    let norm_b: f64 = b.iter().map(|x| (*x as f64) * (*x as f64)).sum::<f64>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    (dot / (norm_a * norm_b)).clamp(0.0, 1.0)
}

// ─── DualPalaceMemory ──────────────────────────────────────────────────

pub struct DualPalaceMemory {
    pub behavior: BehaviorPalace,
    pub knowledge: KnowledgePalace,
    pub bridge: MemoryBridge,
    /// Per-session write quota for knowledge entries
    write_quota: std::sync::atomic::AtomicU32,
    /// SQLite 持久化层（可选 — 未注入时仅内存运行）
    store: Option<Arc<SqlitePalaceStore>>,
    /// 本地 embedding 服务（可选 — 未注入时退化到关键词匹配）
    embedder: tokio::sync::RwLock<Option<Arc<dyn MemoryEmbedder>>>,
}

impl DualPalaceMemory {
    /// 创建纯内存实例（测试用）
    pub fn new() -> Self {
        Self::with_store(None)
    }

    /// 创建实例并注入 SQLite 持久化层
    pub fn with_store(db_store: Option<Arc<SqlitePalaceStore>>) -> Self {
        Self {
            behavior: BehaviorPalace::new(),
            knowledge: KnowledgePalace::new(),
            bridge: MemoryBridge::new(),
            write_quota: std::sync::atomic::AtomicU32::new(0),
            store: db_store,
            embedder: tokio::sync::RwLock::new(None),
        }
    }

    /// 返回底层的 SQLite store 引用（用于持久化预热等操作）
    pub fn sqlite_store(&self) -> Option<&Arc<SqlitePalaceStore>> {
        self.store.as_ref()
    }

    /// Reset write quota for a new session (called by session init)
    pub fn reset_write_quota(&self) {
        self.write_quota.store(0, std::sync::atomic::Ordering::Relaxed);
    }

    /// Max knowledge writes per session (soft limit)
    const MAX_WRITES_PER_SESSION: u32 = 10;

    /// Record interaction and automatically update behavior palace
    pub async fn record_interaction(&self, pattern: &str, tags: &[String]) {
        self.behavior.record_interaction(pattern, tags).await;
    }

    /// Store knowledge entry with dedup + cross-domain linking + write-through
    pub async fn store_knowledge(&self, entry: KnowledgeEntry) -> bool {
        let quota = self.write_quota.load(std::sync::atomic::Ordering::Relaxed);
        if quota >= Self::MAX_WRITES_PER_SESSION {
            tracing::debug!("write_quota exceeded, skipping knowledge store");
            return false;
        }
        let entry_id = entry.id.clone();
        let (stored, similar) = self.knowledge.store(entry).await;
        if !stored {
            return false;
        }
        // 跨域相似 → 建立 Similar 关系链
        for other_id in &similar {
            self.bridge.add_relation(MemoryRelation {
                from_id: entry_id.clone(),
                to_id: other_id.clone(),
                relation_type: RelationType::Similar,
                strength: 0.7,
            }).await;
        }
        self.write_quota.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if let Some(ref store) = self.store {
            if let Some(stored_entry) = self.knowledge.get(&entry_id).await {
                let _ = store.persist_knowledge(&stored_entry).await;
            }
        }
        true
    }

    /// 注入本地 embedding 服务（运行时可切换）。
    ///
    /// ## 调用时机
    /// CoreLoop 初始化后、首次查询前。支持热切换（新模型加载完毕后替换旧的）。
    pub async fn set_embedder(&self, embedder: Arc<dyn MemoryEmbedder>) {
        *self.embedder.write().await = Some(embedder);
    }

    /// 检查是否有 embedding 服务可用
    pub async fn has_embedder(&self) -> bool {
        self.embedder.read().await.is_some()
    }

    /// 语义搜索知识宫殿（需要 embedder 注入）。
    ///
    /// 对所有知识条目计算与 query 的语义相似度，返回 top-k 结果。
    /// 未注入 embedder 时退化到关键词 search()。
    ///
    /// ## 性能
    /// O(n) 遍历所有条目。大规模场景应配合向量索引（HNSW），
    /// 当前规模（≤5000）线性扫描足够快（<50ms for 768d embeddings）。
    pub async fn semantic_search(&self, query: &str, top_k: usize) -> Vec<(KnowledgeEntry, f64)> {
        let embedder = self.embedder.read().await;
        let embedder = match embedder.as_ref() {
            Some(e) => e.clone(),
            None => {
                // Fallback: 关键词匹配，score 固定 0.5
                let results = self.knowledge.search(query).await;
                return results.into_iter().take(top_k).map(|e| (e, 0.5)).collect();
            }
        };

        let query_vec = match embedder.embed_text(query).await {
            Ok(v) => v,
            Err(_) => {
                // Embedding 失败时退化到关键词
                let results = self.knowledge.search(query).await;
                return results.into_iter().take(top_k).map(|e| (e, 0.5)).collect();
            }
        };

        let entries = self.knowledge.entries.read().await;
        let mut scored: Vec<(KnowledgeEntry, f64)> = Vec::new();

        for entry in entries.values() {
            // 对每个条目的 title + content 计算 embedding
            let text = format!("{} {}", entry.title, entry.content);
            if let Ok(entry_vec) = embedder.embed_text(&text).await {
                let sim = cosine_similarity(&query_vec, &entry_vec);
                if sim > 0.3 {
                    scored.push((entry.clone(), sim));
                }
            }
        }

        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(top_k);
        scored
    }

    /// 混合搜索（关键词 + 语义），结果融合排序。
    ///
    /// 当 embedder 可用时：关键词得分 0.4 权重 + 语义得分 0.6 权重。
    /// 当 embedder 不可用时：纯关键词搜索。
    pub async fn hybrid_search(&self, query: &str, top_k: usize) -> Vec<(KnowledgeEntry, f64)> {
        let keyword_results = self.knowledge.search(query).await;

        if !self.has_embedder().await {
            return keyword_results.into_iter().take(top_k).map(|e| (e, 0.5)).collect();
        }

        let semantic_results = self.semantic_search(query, top_k * 2).await;

        // 融合：keyword hits 给 0.4 底分，semantic score 给 0.6 权重
        let mut scored_map: HashMap<String, (KnowledgeEntry, f64)> = HashMap::new();

        for entry in &keyword_results {
            scored_map.insert(entry.id.clone(), (entry.clone(), 0.4));
        }

        for (entry, sim) in semantic_results {
            let existing = scored_map.entry(entry.id.clone()).or_insert((entry.clone(), 0.0));
            existing.1 += sim * 0.6;
        }

        let mut results: Vec<(KnowledgeEntry, f64)> = scored_map.into_values().collect();
        results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        results.truncate(top_k);
        results
    }

    /// 查询相关知识 (通过行为标签桥接)
    pub async fn query_by_behavior(&self, behavior_tags: &[String]) -> Vec<KnowledgeEntry> {
        let behaviors = self.behavior.search(behavior_tags).await;
        let mut knowledge = Vec::new();
        for behavior in behaviors {
            let relations = self.bridge.get_related(&behavior.id).await;
            for rel in relations {
                if rel.relation_type == RelationType::RequiresKnowledge {
                    if let Some(entry) = self.knowledge.get(&rel.to_id).await {
                        knowledge.push(entry);
                    }
                }
            }
        }
        knowledge
    }

    // ─── 自动维护机制 ────────────────────────────────────────────────

    /// V29.13 段2：吸收 SessionSnapshot 升维成 KnowledgeEntry
    ///
    /// ## 使用场景
    /// 当 ContextTiers 把一个 SessionSnapshot 从 warm demote 到 cold（持久化）时，
    /// 同步触发本方法把 snapshot.key_decisions 提取为知识条目，让 cold 不只是
    /// "session 历史归档"，也成为"长期可检索知识"。
    ///
    /// ## 引用关系
    /// - 上游：`PalaceAbsorbHook` 在 `TurnPostFanOut` 事件中调用
    /// - 下游：内部走 `store_knowledge` 路径——继承 quota / 桥接 / write-through
    ///
    /// ## 失败处理
    /// 静默 false（quota 耗尽 / 内容空 / 重复条目）；不抛错——hook 路径不应阻塞 turn
    pub async fn absorb_snapshot(&self, session_id: &str, turn: u32, summary: &str, key_decisions: &[String]) -> bool {
        if key_decisions.is_empty() && summary.trim().is_empty() {
            return false;
        }
        // 用 session_id+turn+content_hash 作为稳定 id（防重复 absorb）
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut h = DefaultHasher::new();
        summary.hash(&mut h);
        for d in key_decisions { d.hash(&mut h); }
        let id = format!("absorbed_{}_{turn}_{:016x}", session_id, h.finish());

        let title = if !key_decisions.is_empty() {
            format!("Session decisions @ turn {turn}")
        } else {
            format!("Session summary @ turn {turn}")
        };
        let content = if !key_decisions.is_empty() {
            key_decisions.join("\n")
        } else {
            summary.to_string()
        };

        let mut entry = KnowledgeEntry::new(id, title, content, "session_history");
        entry.tags = vec![
            "absorbed".into(),
            "session".into(),
            format!("turn:{turn}"),
        ];
        self.store_knowledge(entry).await
    }

    /// 从工具调用记录自动提取行为模式并存入行为宫殿。
    ///
    /// 每条工具调用 → 一条行为模式，标签从 tool_id 的域名提取。
    /// 频率累计 + 置信度递增。桥接自动去重。
    ///
    /// ## V29.13 命名约定修复
    /// 旧实现：`tool_id.split('.').next()`——依赖 dot 分隔，当工具名从 `filengine.fs.read`
    /// 改成 `filengine_fs_read` 后整个 tool_id 被当 domain，导致 BehaviorPalace tag 错乱。
    /// 新实现：兼容 dot+underscore 两种分隔（先 split('.')，无 dot 时再 split('_')）。
    pub async fn record_tool_behavior(&self, tool_id: &str, success: bool) {
        let domain = extract_tool_domain(tool_id);
        let tags: Vec<String> = vec![
            domain.to_string(),
            "tool".to_string(),
            if success { "success".into() } else { "failure".into() },
        ];
        let pattern = format!("tool_call:{}", tool_id);
        self.behavior.record_interaction(&pattern, &tags).await;

        // 自动桥接（去重由 add_relation 保证）
        let candidates = self.knowledge.search(domain).await;
        for entry in candidates.iter().take(3) {
            let rel = MemoryRelation {
                from_id: pattern.clone(),
                to_id: entry.id.clone(),
                relation_type: RelationType::RequiresKnowledge,
                strength: 0.5 + (if success { 0.3 } else { 0.0 }),
            };
            self.bridge.add_relation(rel).await;
        }

        // Write-through
        if let Some(ref store) = self.store {
            let memories = self.behavior.memories.read().await;
            if let Some(bm) = memories.get(&pattern) {
                let _ = store.persist_behavior(bm).await;
            }
        }

        // Lazy prune: 每 100 次 tool interaction 执行一次清理
        let count = self.write_quota.fetch_add(0, std::sync::atomic::Ordering::Relaxed);
        // 借用 write_quota 作为 interaction counter（非精确，仅触发条件）
        let behavior_count = self.behavior.memories.read().await.len();
        if behavior_count > MAX_BEHAVIOR_ENTRIES / 2 && count.is_multiple_of(100) {
            self.prune().await;
        }
    }

    /// 老化处理：低频记忆进入冷却态（降低置信度），仅当容量超限时才物理移除。
    ///
    /// ## 策略
    /// - behavior：frequency < 3 且 confidence < 0.3 → **冷却**（confidence 衰减至 0.05）
    ///   仅当容量超 MAX_BEHAVIOR_ENTRIES 时，冷却态中最老的条目才被移除。
    ///   冷却态的模式不会参与 search 匹配（confidence < 0.1 过滤），但数据保留
    ///   以便未来再次触发时恢复而非重建。
    /// - knowledge：sm2_interval > 180 天且从未 review → 降低 ease（触发更早 review）
    pub async fn prune(&self) {
        let now = chrono::Utc::now().timestamp();
        let day_ago = now - 86400;

        // Phase 1: 冷却低频 behavior（降低 confidence 但不删除）
        {
            let mut memories = self.behavior.memories.write().await;
            for m in memories.values_mut() {
                if m.frequency < 3 && m.confidence < 0.3 && m.last_seen < day_ago {
                    // 冷却：将 confidence 压低到 0.05，search 时 confidence < 0.1 的自然过滤
                    m.confidence = 0.05;
                }
            }

            // Phase 2: 仅当容量超限时，物理移除最老的冷却态条目
            if memories.len() > MAX_BEHAVIOR_ENTRIES {
                let overflow = memories.len() - MAX_BEHAVIOR_ENTRIES;
                // 收集冷却态条目（confidence <= 0.05），按 last_seen 升序排列
                let mut cold_entries: Vec<(String, i64)> = memories.values()
                    .filter(|m| m.confidence <= 0.05)
                    .map(|m| (m.id.clone(), m.last_seen))
                    .collect();
                cold_entries.sort_by_key(|(_, ts)| *ts);
                // 仅移除足够数量使容量回到限制内
                let to_remove: Vec<String> = cold_entries.into_iter()
                    .take(overflow)
                    .map(|(id, _)| id)
                    .collect();
                for id in &to_remove {
                    memories.remove(id);
                }
                // 清理对应的悬空桥接关系
                drop(memories);
                if !to_remove.is_empty() {
                    self.bridge.remove_relations_for(&to_remove).await;
                }
            }
        }

        // Phase 3: 标记冷 knowledge（降低 ease，触发下次 review）
        {
            let mut entries = self.knowledge.entries.write().await;
            for entry in entries.values_mut() {
                if entry.sm2_interval_days > 180.0 && entry.last_reviewed < day_ago {
                    entry.sm2_ease = (entry.sm2_ease * 0.8).max(1.3);
                }
            }
        }
    }

    /// 获取到期需 review 的知识条目数
    pub async fn due_review_count(&self) -> usize {
        self.knowledge.get_due_for_review().await.len()
    }

    // ─── 基于记忆宫殿的决策树 ─────────────────────────────────────────

    /// 根据输入从行为宫匹配历史模式，推荐 TaskKind。
    ///
    /// 搜索行为宫 → 按 frequency×confidence 排序 → 取最高匹配域作为分类。
    pub async fn classify_input(&self, input: &str) -> (String, f64) {
        let memories = self.behavior.memories.read().await;
        let lower = input.to_lowercase();
        let input_words: std::collections::HashSet<&str> = lower.split_whitespace().collect();

        let mut best_score = 0.0f64;
        let mut best_domain = "general_chat".to_string();

        for memory in memories.values() {
            let pattern_words: std::collections::HashSet<&str> = memory.pattern
                .split_whitespace().collect();
            let overlap = if pattern_words.is_empty() { 0.0 } else {
                let intersection = input_words.intersection(&pattern_words).count();
                intersection as f64 / pattern_words.len() as f64
            };
            if overlap == 0.0 { continue; }

            let score = overlap * memory.confidence * (memory.frequency as f64).ln_1p();
            if score > best_score {
                best_score = score;
                // 从 pattern 中提取 domain: "tool_call:fs.read" → "fs"
                let domain = memory.pattern.split(':').nth(1)
                    .and_then(|s| s.split('.').next())
                    .unwrap_or("general");
                best_domain = domain.to_string();
            }
        }

        (best_domain, best_score)
    }

    /// 根据当前上下文和工具历史，推荐下一步可能使用的工具。
    ///
    /// 通过关系链：当前工具 → Similar → 其他高频关联工具。
    pub async fn recommend_next_tools(&self, current_tool: &str) -> Vec<(String, f64)> {
        let pattern = format!("tool_call:{}", current_tool);
        let relations = self.bridge.get_related(&pattern).await;

        let mut recommendations = Vec::new();
        for rel in &relations {
            if rel.relation_type == RelationType::Similar
                || rel.relation_type == RelationType::RelatedKnowledge
            {
                let target = if rel.from_id == pattern { &rel.to_id } else { &rel.from_id };
                if target.starts_with("tool_call:") {
                    let tool_name = target.strip_prefix("tool_call:").unwrap_or(target);
                    recommendations.push((tool_name.to_string(), rel.strength));
                }
            }
        }

        // 按 strength 降序
        recommendations.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        recommendations
    }
}

impl Default for DualPalaceMemory {
    fn default() -> Self { Self::new() }
    // Note: with_store(None) for default
}

/// V29.13 段2：从 tool_id 提取 domain（namespace），兼容 dot+underscore 两种分隔
///
/// ## 引用关系
/// - 被 `record_tool_behavior` 用于打 BehaviorMemory tag
/// - 被 `record_tool_behavior` 用于 knowledge.search() 桥接候选
///
/// ## 历史 bug
/// 旧实现仅 `split('.').next()`——命名约定从 dot 改为 underscore 后，
/// `"filengine_fs_read".split('.').next() == "filengine_fs_read"`（整段当 domain），
/// 导致 BehaviorPalace tag 全部错位 + bridge 桥接拉不到正确 KnowledgeEntry。
///
/// ## 新策略
/// - 优先 dot 分隔（向后兼容外部 MCP `mcp__filengine__file_read` 等少数 dot 名）
/// - 无 dot 时 underscore 分隔取首段（filengine_fs_read → "filengine"）
/// - 都无分隔时整段当 domain（短工具名如 "code"）
pub(crate) fn extract_tool_domain(tool_id: &str) -> &str {
    if let Some(idx) = tool_id.find('.') {
        return &tool_id[..idx];
    }
    if let Some(idx) = tool_id.find('_') {
        return &tool_id[..idx];
    }
    tool_id
}

// ─── SQLite 持久化层 ────────────────────────────────────────────────────

/// Memory Palace 的 SQLite 持久化存储
///
/// ## 场景
/// 让 Behavior/Knowledge/Relations 跨 session 存活（程序重启不丢失）。
///
/// ## 策略
/// - 启动时：从 SQLite 预热到 HashMap（DualPalaceMemory 仍用内存态运行）
/// - 写入时：write-through（HashMap + SQLite 双写）
/// - 查询时：走 HashMap（快，无 IO）
///
/// ## 生命周期
/// - 创建：app 启动时
/// - DB 文件：~/.abacus/palace.db（独立于 memory.db 和 knowledge.db）
pub struct SqlitePalaceStore {
    conn: std::sync::Arc<tokio::sync::Mutex<rusqlite::Connection>>,
}

impl SqlitePalaceStore {
    /// 打开或创建 palace DB
    pub fn new(db_path: &std::path::Path) -> Result<Self, String> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create palace db dir: {e}"))?;
        }
        let conn = rusqlite::Connection::open(db_path)
            .map_err(|e| format!("cannot open palace db: {e}"))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA busy_timeout=5000;"
        ).map_err(|e| format!("pragma: {e}"))?;

        Self::init_schema(&conn)?;
        Ok(Self { conn: std::sync::Arc::new(tokio::sync::Mutex::new(conn)) })
    }

    /// 内存模式（测试用）
    pub fn in_memory() -> Result<Self, String> {
        let conn = rusqlite::Connection::open_in_memory()
            .map_err(|e| format!("in_memory: {e}"))?;
        Self::init_schema(&conn)?;
        Ok(Self { conn: std::sync::Arc::new(tokio::sync::Mutex::new(conn)) })
    }

    fn init_schema(conn: &rusqlite::Connection) -> Result<(), String> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS knowledge_entries (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                content TEXT NOT NULL,
                domain TEXT NOT NULL,
                sm2_ease REAL NOT NULL DEFAULT 2.5,
                sm2_interval_days REAL NOT NULL DEFAULT 1.0,
                sm2_repetitions INTEGER NOT NULL DEFAULT 0,
                last_reviewed INTEGER NOT NULL,
                next_review INTEGER NOT NULL,
                tags TEXT NOT NULL DEFAULT '[]',
                created_at INTEGER NOT NULL DEFAULT (unixepoch()),
                updated_at INTEGER NOT NULL DEFAULT (unixepoch())
            );

            CREATE TABLE IF NOT EXISTS behavior_memories (
                id TEXT PRIMARY KEY,
                pattern TEXT NOT NULL,
                frequency INTEGER NOT NULL DEFAULT 1,
                last_seen INTEGER NOT NULL,
                confidence REAL NOT NULL DEFAULT 0.5,
                tags TEXT NOT NULL DEFAULT '[]',
                created_at INTEGER NOT NULL DEFAULT (unixepoch())
            );

            CREATE TABLE IF NOT EXISTS memory_relations (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                from_id TEXT NOT NULL,
                to_id TEXT NOT NULL,
                relation_type TEXT NOT NULL,
                strength REAL NOT NULL DEFAULT 0.5,
                created_at INTEGER NOT NULL DEFAULT (unixepoch())
            );
            CREATE INDEX IF NOT EXISTS idx_mrel_from ON memory_relations(from_id);
            CREATE INDEX IF NOT EXISTS idx_mrel_to ON memory_relations(to_id);"
        ).map_err(|e| format!("schema: {e}"))?;
        Ok(())
    }

    // ─── 预热：从 SQLite 加载到内存 ─────────────────────────────────

    /// 从 DB 加载所有数据，填充 DualPalaceMemory
    pub async fn warmup(&self, palace: &DualPalaceMemory) -> Result<(), String> {
        let conn = self.conn.lock().await;

        // 加载 knowledge entries
        {
            let mut stmt = conn.prepare(
                "SELECT id, title, content, domain, sm2_ease, sm2_interval_days,
                        sm2_repetitions, last_reviewed, next_review, tags
                 FROM knowledge_entries"
            ).map_err(|e| e.to_string())?;

            let entries: Vec<KnowledgeEntry> = stmt.query_map([], |row| {
                let tags_json: String = row.get(9)?;
                let tags: Vec<String> = serde_json::from_str(&tags_json).unwrap_or_else(|e| {
                    tracing::warn!("failed to parse tags JSON: {e}, raw: {}", &tags_json[..tags_json.len().min(200)]);
                    Vec::new()
                });
                Ok(KnowledgeEntry {
                    id: row.get(0)?,
                    title: row.get(1)?,
                    content: row.get(2)?,
                    domain: row.get(3)?,
                    sm2_ease: row.get(4)?,
                    sm2_interval_days: row.get(5)?,
                    sm2_repetitions: row.get(6)?,
                    last_reviewed: row.get(7)?,
                    next_review: row.get(8)?,
                    tags,
                })
            }).map_err(|e| e.to_string())?
            .filter_map(|r| r.ok())
            .collect();

            let mut knowledge = palace.knowledge.entries.write().await;
            for entry in entries {
                knowledge.insert(entry.id.clone(), entry);
            }
        }

        // 加载 behavior memories
        {
            let mut stmt = conn.prepare(
                "SELECT id, pattern, frequency, last_seen, confidence, tags, created_at
                 FROM behavior_memories"
            ).map_err(|e| e.to_string())?;

            let memories: Vec<BehaviorMemory> = stmt.query_map([], |row| {
                let tags_json: String = row.get(5)?;
                let tags: Vec<String> = serde_json::from_str(&tags_json).unwrap_or_else(|e| {
                    tracing::warn!("failed to parse tags JSON: {e}, raw: {}", &tags_json[..tags_json.len().min(200)]);
                    Vec::new()
                });
                Ok(BehaviorMemory {
                    id: row.get(0)?,
                    pattern: row.get(1)?,
                    frequency: row.get(2)?,
                    last_seen: row.get(3)?,
                    confidence: row.get(4)?,
                    tags,
                    created_at: row.get(6)?,
                })
            }).map_err(|e| e.to_string())?
            .filter_map(|r| r.ok())
            .collect();

            let mut behaviors = palace.behavior.memories.write().await;
            for m in memories {
                behaviors.insert(m.id.clone(), m);
            }
        }

        // 加载 relations
        {
            let mut stmt = conn.prepare(
                "SELECT from_id, to_id, relation_type, strength FROM memory_relations"
            ).map_err(|e| e.to_string())?;

            let rels: Vec<MemoryRelation> = stmt.query_map([], |row| {
                let rt_str: String = row.get(2)?;
                    // Deserialize from string → uses as_str() output format
                    let relation_type = match rt_str.as_str() {
                        "ParentChild" => RelationType::ParentChild,
                        "SummaryDetail" => RelationType::SummaryDetail,
                        "SeeAlso" => RelationType::SeeAlso,
                        "DependsOn" => RelationType::DependsOn,
                        "Similar" => RelationType::Similar,
                        "Exclusive" => RelationType::Exclusive,
                        "Replaces" => RelationType::Replaces,
                        "ComposesTo" => RelationType::ComposesTo,
                        "TagAggregator" => RelationType::TagAggregator,
                        "DraftOf" => RelationType::DraftOf,
                        "RevisionOf" => RelationType::RevisionOf,
                        "Supersedes" => RelationType::Supersedes,
                        "RequiresKnowledge" => RelationType::RequiresKnowledge,
                        "SupportsBehavior" => RelationType::SupportsBehavior,
                        "RelatedBehavior" => RelationType::RelatedBehavior,
                        _ => RelationType::RelatedKnowledge,
                    };
                Ok(MemoryRelation {
                    from_id: row.get(0)?,
                    to_id: row.get(1)?,
                    relation_type,
                    strength: row.get(3)?,
                })
            }).map_err(|e| e.to_string())?
            .filter_map(|r| r.ok())
            .collect();

            let mut bridge = palace.bridge.relations.write().await;
            for r in rels {
                bridge.push(r);
            }
        }

        Ok(())
    }

    // ─── Write-through：写入 SQLite ─────────────────────────────────

    /// 持久化 knowledge entry（write-through）
    pub async fn persist_knowledge(&self, entry: &KnowledgeEntry) -> Result<(), String> {
        let conn = self.conn.lock().await;
        let tags_json = serde_json::to_string(&entry.tags).unwrap_or_else(|_| "[]".into());
        conn.execute(
            "INSERT OR REPLACE INTO knowledge_entries
             (id, title, content, domain, sm2_ease, sm2_interval_days,
              sm2_repetitions, last_reviewed, next_review, tags, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, unixepoch())",
            rusqlite::params![
                entry.id, entry.title, entry.content, entry.domain,
                entry.sm2_ease, entry.sm2_interval_days, entry.sm2_repetitions,
                entry.last_reviewed, entry.next_review, tags_json,
            ],
        ).map_err(|e| format!("persist knowledge: {e}"))?;
        Ok(())
    }

    /// 持久化 behavior memory（write-through）
    pub async fn persist_behavior(&self, memory: &BehaviorMemory) -> Result<(), String> {
        let conn = self.conn.lock().await;
        let tags_json = serde_json::to_string(&memory.tags).unwrap_or_else(|_| "[]".into());
        conn.execute(
            "INSERT OR REPLACE INTO behavior_memories
             (id, pattern, frequency, last_seen, confidence, tags, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                memory.id, memory.pattern, memory.frequency,
                memory.last_seen, memory.confidence, tags_json,
                memory.created_at,
            ],
        ).map_err(|e| format!("persist behavior: {e}"))?;
        Ok(())
    }

    /// 持久化 relation（append）
    pub async fn persist_relation(&self, rel: &MemoryRelation) -> Result<(), String> {
        let conn = self.conn.lock().await;
        let rt = rel.relation_type.as_str();
        conn.execute(
            "INSERT INTO memory_relations (from_id, to_id, relation_type, strength)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![rel.from_id, rel.to_id, rt, rel.strength],
        ).map_err(|e| format!("persist relation: {e}"))?;
        Ok(())
    }

    /// 获取统计信息
    pub async fn stats(&self) -> Result<(usize, usize, usize), String> {
        let conn = self.conn.lock().await;
        let k: i64 = conn.query_row("SELECT count(*) FROM knowledge_entries", [], |r| r.get(0))
            .unwrap_or(0);
        let b: i64 = conn.query_row("SELECT count(*) FROM behavior_memories", [], |r| r.get(0))
            .unwrap_or(0);
        let r: i64 = conn.query_row("SELECT count(*) FROM memory_relations", [], |r| r.get(0))
            .unwrap_or(0);
        Ok((k as usize, b as usize, r as usize))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── V29.13 段2：extract_tool_domain + absorb_snapshot 回归 ─────────────

    #[test]
    fn extract_tool_domain_dot_form() {
        // 兼容外部 MCP 的 dot 命名：mcp__server__tool 形式不会进这里，但内部曾用过 dot
        assert_eq!(extract_tool_domain("filengine.fs.read"), "filengine");
        assert_eq!(extract_tool_domain("db.query"), "db");
        assert_eq!(extract_tool_domain("kb.search"), "kb");
    }

    #[test]
    fn extract_tool_domain_underscore_form() {
        // V29.13 命名约定：所有内部工具用下划线
        assert_eq!(extract_tool_domain("filengine_fs_read"), "filengine");
        assert_eq!(extract_tool_domain("filengine_bash_exec"), "filengine");
        assert_eq!(extract_tool_domain("db_query"), "db");
        assert_eq!(extract_tool_domain("kb_search"), "kb");
        assert_eq!(extract_tool_domain("orchestrate_assess"), "orchestrate");
    }

    #[test]
    fn extract_tool_domain_no_separator() {
        // 短工具名（无分隔）整段当 domain
        assert_eq!(extract_tool_domain("code"), "code");
        assert_eq!(extract_tool_domain("undo"), "undo");
    }

    #[test]
    fn extract_tool_domain_dot_takes_precedence() {
        // dot 优先于 underscore（兼容性：旧名 + 新名混合环境）
        assert_eq!(extract_tool_domain("foo.bar_baz"), "foo");
    }

    #[tokio::test]
    async fn absorb_snapshot_creates_knowledge_entry() {
        let palace = DualPalaceMemory::new();
        let absorbed = palace.absorb_snapshot(
            "test_session",
            5,
            "Working on auth refactor",
            &["chose JWT over session".into(), "deferred RBAC to phase 2".into()],
        ).await;
        assert!(absorbed, "absorb_snapshot 应成功");
        // 验证 KnowledgePalace 中查得到
        let results = palace.knowledge.search("absorbed").await;
        assert!(!results.is_empty(), "应能 search 到 absorbed entry");
        assert_eq!(results[0].domain, "session_history");
    }

    #[tokio::test]
    async fn absorb_snapshot_skips_empty_content() {
        let palace = DualPalaceMemory::new();
        let absorbed = palace.absorb_snapshot("s", 0, "  ", &[]).await;
        assert!(!absorbed, "空 summary + 空 decisions 应跳过");
    }

    #[tokio::test]
    async fn absorb_snapshot_idempotent_via_id_hash() {
        let palace = DualPalaceMemory::new();
        let session = "sess";
        let summary = "fixed bug";
        let decisions = vec!["use mutex".to_string()];
        let first = palace.absorb_snapshot(session, 1, summary, &decisions).await;
        let second = palace.absorb_snapshot(session, 1, summary, &decisions).await;
        assert!(first, "first call should succeed");
        // 第二次 store 应被 dedup 拒绝（store 内部 dedup 逻辑）；但不一定 false——
        // 重要的是 KnowledgePalace 中只有 1 条 absorbed entry
        let results = palace.knowledge.search("absorbed").await;
        assert_eq!(results.len(), 1, "重复 absorb 不应产生多条 entry");
        let _ = second; // 标记使用避免 warning
    }

    #[tokio::test]
    async fn record_tool_behavior_extracts_domain_from_underscore_name() {
        // 验证 V29.13 修复：旧 bug 是整个 tool_id 当 domain
        let palace = DualPalaceMemory::new();
        palace.record_tool_behavior("filengine_fs_read", true).await;
        let memories = palace.behavior.search(&["filengine".to_string()]).await;
        assert!(!memories.is_empty(), "应能按 'filengine' domain 搜到 behavior");
    }

    #[tokio::test]
    async fn test_behavior_palace() {
        let palace = BehaviorPalace::new();
        palace.record_interaction("user prefers short responses", &["preference".to_string(), "response".to_string()]).await;
        palace.record_interaction("user prefers short responses", &["preference".to_string(), "response".to_string()]).await;

        let memories = palace.search(&["preference".to_string()]).await;
        assert_eq!(memories.len(), 1);
        assert_eq!(memories[0].frequency, 2);
    }

    #[tokio::test]
    async fn test_knowledge_palace() {
        let palace = KnowledgePalace::new();
        let entry = KnowledgeEntry::new("rust-async", "Rust Async Best Practices", "Use tokio...", "rust");
        palace.store(entry).await;

        let results = palace.search("rust").await;
        assert_eq!(results.len(), 1);
    }

    #[tokio::test]
    async fn test_sm2_algorithm() {
        let mut entry = KnowledgeEntry::new("test", "Test", "Content", "test");
        // First positive review: interval stays at 1.0
        entry.sm2_update(5.0);
        assert_eq!(entry.sm2_repetitions, 1);
        assert_eq!(entry.sm2_interval_days, 1.0);

        // Second positive review: interval becomes 6.0
        entry.sm2_update(5.0);
        assert_eq!(entry.sm2_repetitions, 2);
        assert_eq!(entry.sm2_interval_days, 6.0);

        // Third positive review: interval grows by ease factor
        entry.sm2_update(5.0);
        assert!(entry.sm2_interval_days > 6.0);

        // Negative review resets
        entry.sm2_update(0.0);
        assert_eq!(entry.sm2_interval_days, 1.0);
        assert_eq!(entry.sm2_repetitions, 0);
    }

    #[tokio::test]
    async fn test_dual_palace() {
        let memory = DualPalaceMemory::new();
        memory.record_interaction("rust async patterns", &["rust".to_string(), "async".to_string()]).await;
        memory.store_knowledge(KnowledgeEntry::new("rust-async", "Rust Async", "tokio...", "rust")).await;

        let results = memory.knowledge.search("rust").await;
        assert_eq!(results.len(), 1);
    }

    #[tokio::test]
    async fn test_sqlite_palace_persist_and_warmup() {
        // Phase 1: 创建 store，写入数据
        let store = SqlitePalaceStore::in_memory().unwrap();
        let entry = KnowledgeEntry::new("k1", "Rust Patterns", "Pattern matching guide", "rust");
        store.persist_knowledge(&entry).await.unwrap();

        let bm = BehaviorMemory {
            id: "b1".into(),
            pattern: "user likes concise".into(),
            frequency: 5,
            last_seen: 1000,
            confidence: 0.8,
            tags: vec!["preference".into()],
            created_at: 500,
        };
        store.persist_behavior(&bm).await.unwrap();

        let rel = MemoryRelation {
            from_id: "b1".into(),
            to_id: "k1".into(),
            relation_type: RelationType::RequiresKnowledge,
            strength: 0.9,
        };
        store.persist_relation(&rel).await.unwrap();

        // Phase 2: 创建空的 DualPalaceMemory，用 warmup 填充
        let palace = DualPalaceMemory::new();
        store.warmup(&palace).await.unwrap();

        // 验证 knowledge 已加载
        let results = palace.knowledge.search("rust").await;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Rust Patterns");

        // 验证 behavior 已加载
        let behaviors = palace.behavior.search(&["preference".into()]).await;
        assert_eq!(behaviors.len(), 1);
        assert_eq!(behaviors[0].frequency, 5);

        // 验证 relations 已加载
        let rels = palace.bridge.get_related("b1").await;
        assert_eq!(rels.len(), 1);
        assert_eq!(rels[0].to_id, "k1");
    }

    #[tokio::test]
    async fn test_sqlite_palace_stats() {
        let store = SqlitePalaceStore::in_memory().unwrap();
        store.persist_knowledge(&KnowledgeEntry::new("k1", "T1", "C1", "d1")).await.unwrap();
        store.persist_knowledge(&KnowledgeEntry::new("k2", "T2", "C2", "d2")).await.unwrap();
        store.persist_behavior(&BehaviorMemory {
            id: "b1".into(), pattern: "p".into(), frequency: 1,
            last_seen: 0, confidence: 0.5, tags: vec![], created_at: 0,
        }).await.unwrap();

        let (k, b, r) = store.stats().await.unwrap();
        assert_eq!(k, 2);
        assert_eq!(b, 1);
        assert_eq!(r, 0);
    }
}
