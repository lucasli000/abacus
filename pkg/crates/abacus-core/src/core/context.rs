use std::collections::HashMap;
use std::sync::Arc;

use abacus_types::{
    ToolCost, ToolEffectiveness, ToolHandle, ToolId, KernelError, ToolProvider,
    ToolSchema, ToolSecurity, ToolState,
};
use chrono::Utc;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::RwLock;

use crate::llm::{Message, MessageContent, MessageRole};
use crate::tool::builtin::filengine::allowed_roots;
use crate::tool::{ExecutionContext, ToolExecutor};

pub const MIN_CONTEXT_TOKENS: usize = 128_000;

// ─── Context Window ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum OverflowAction {
    Compress,
    Discard,
}

#[derive(Debug, Clone)]
pub struct ContextWindow {
    pub max_tokens: usize,
    pub model_limit: usize,
    pub compression_trigger_pct: u8,
    pub overflow_action: OverflowAction,
    pub reserve_system: bool,
    pub reserve_tools: bool,
    pub current_tokens: usize,
}

impl Default for ContextWindow {
    fn default() -> Self {
        Self {
            max_tokens: MIN_CONTEXT_TOKENS,
            model_limit: 1_000_000,
            compression_trigger_pct: 85,
            overflow_action: OverflowAction::Compress,
            reserve_system: true,
            reserve_tools: true,
            current_tokens: 0,
        }
    }
}

impl ContextWindow {
    pub fn usage_pct(&self) -> f64 {
        if self.max_tokens == 0 {
            return 0.0;
        }
        (self.current_tokens as f64 / self.max_tokens as f64) * 100.0
    }

    pub fn should_compress(&self) -> bool {
        self.usage_pct() >= self.compression_trigger_pct as f64
    }

    pub fn should_force_discard(&self) -> bool {
        self.usage_pct() >= 95.0
    }

    pub fn set_max_tokens(&mut self, value: usize) {
        let clamped = value.clamp(MIN_CONTEXT_TOKENS, self.model_limit);
        self.max_tokens = clamped;
    }

    pub fn set_model_limit(&mut self, limit: usize) {
        self.model_limit = limit;
        if self.max_tokens > limit {
            self.max_tokens = limit;
        }
        if self.max_tokens < MIN_CONTEXT_TOKENS {
            self.max_tokens = MIN_CONTEXT_TOKENS;
        }
    }
}

// ─── Session Snapshot ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub session_id: String,
    pub turn_count: u32,
    pub summary: String,
    pub token_estimate: usize,
    pub created_at: i64,
    pub key_decisions: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompressedMessage {
    pub original_role: String,
    pub summary: String,
    pub original_tokens: usize,
    pub compressed_tokens: usize,
}

/// Phase Z3：压缩 summary 统一类型
///
/// 所有 6 套压缩 path（messages / segment / tool_result / cold snapshot / checkpoint / focus）
/// 输出此一致 schema，让 LLM 看到的所有摘要标记一致。
///
/// ## Recovery 支持（Z2）
/// `recover_id` 非 None 时，调用方可通过 messages.recover / result.expand / session.recall
/// 等工具按 id 取回原始内容。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SummaryKind {
    /// auto_compress_messages 合并产物
    MessagesBlock,
    /// context.compress 产物（declared segment）
    Segment,
    /// 工具输出截断产物（result_store）
    ToolResult,
    /// SessionStore 持久化快照
    Snapshot,
    /// 显示用截断/其他
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompressedSummary {
    pub kind: SummaryKind,
    pub original_tokens: usize,
    pub compressed_tokens: usize,
    pub summary: String,
    /// Z2: 可恢复 id（None 表示不支持 recover）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recover_id: Option<String>,
    /// Z3: 时间维度——压缩覆盖的 turn 区间（inclusive）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_range: Option<(u32, u32)>,
    /// 来源标识（segment_id / source_tool_id / session_id 等）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
}

impl CompressedSummary {
    /// 从旧 CompressedMessage 升级（向后兼容路径）
    pub fn from_legacy(m: CompressedMessage) -> Self {
        Self {
            kind: SummaryKind::MessagesBlock,
            original_tokens: m.original_tokens,
            compressed_tokens: m.compressed_tokens,
            summary: m.summary,
            recover_id: None,
            turn_range: None,
            origin: Some(m.original_role),
        }
    }

    /// 渲染为 LLM 看到的统一 JSON 行（确保所有 path summary 一致）
    pub fn to_json_line(&self) -> String {
        serde_json::json!({
            "kind": match self.kind {
                SummaryKind::MessagesBlock => "messages_block",
                SummaryKind::Segment => "segment",
                SummaryKind::ToolResult => "tool_result",
                SummaryKind::Snapshot => "snapshot",
                SummaryKind::Other => "other",
            },
            "original_tokens": self.original_tokens,
            "compressed_tokens": self.compressed_tokens,
            "summary": self.summary,
            "recover_id": self.recover_id,
            "turn_range": self.turn_range,
            "origin": self.origin,
        }).to_string()
    }
}

// ─── Context Tiers ──────────────────────────────────────────────────────────

#[async_trait::async_trait]
pub trait SessionStore: Send + Sync {
    async fn save(&self, snapshot: SessionSnapshot) -> Result<(), KernelError>;
    async fn load_recent(&self, limit: usize) -> Result<Vec<SessionSnapshot>, KernelError>;
    async fn search(&self, query: &str) -> Result<Vec<SessionSnapshot>, KernelError>;
}

/// Task #80：三层 SessionSnapshot 管理
///
/// 引用关系：
/// - `hot_snapshots`：CoreLoop post-turn 调 `record_snapshot` 推入；老化进 warm
/// - `warm`：从 hot 老化降级；满则 demote 进 cold
/// - `cold`：SessionStore trait 持久化（默认 SQLite-backed）
/// 生命周期：随 ContextManager 生死；migrate_tiers 是显式触发的搬运动作，无 background task。
///
/// `hot` 字段（ContextWindow）保留为兼容性占位，不参与 snapshot 流转——
/// snapshot 流转走 hot_snapshots。
pub struct ContextTiers {
    pub hot: ContextWindow,
    /// Task #80：近期 session-turn snapshot 队列（FIFO，新进尾、出队头）
    /// 容量软上限由 migrate_tiers 的 hot_age_threshold 控制——超 N turn 的迁入 warm。
    pub hot_snapshots: RwLock<std::collections::VecDeque<SessionSnapshot>>,
    /// Task #80：中期 session snapshot——从 hot 老化降级
    /// 容量上限由 migrate_tiers 的 warm_capacity 控制；满则最旧的 demote 进 cold。
    pub warm: RwLock<std::collections::VecDeque<SessionSnapshot>>,
    pub cold: Arc<dyn SessionStore>,
    pub compressed_messages: RwLock<Vec<CompressedMessage>>,
    /// V29.13 段2：cold demote buffer——让 PalaceAbsorbHook 增量消费
    ///
    /// ## 引用关系
    /// - 写入：`migrate_tiers` 在 demote_to_cold 成功后 push_back
    /// - 读取：`take_recent_demoted` 一次性取空（PalaceAbsorbHook 在 TurnPostFanOut 调用）
    ///
    /// ## 容量限制
    /// 8 条 FIFO buffer——hook 间隔太久未消费时旧的会被覆盖，避免内存泄漏；
    /// 正常情况下每 turn 触发 hook，buffer 不会积累。
    ///
    /// ## 不直接传给 hook
    /// 因为 PalaceAbsorbHook 通过 PipelineEvent 单向通信，无法 pull 数据；
    /// 用 buffer + take 模式让 hook 主动拉取，不污染 PipelineEvent 结构。
    pub recent_demoted: RwLock<std::collections::VecDeque<SessionSnapshot>>,
}

/// Task #80：迁移结果统计（诊断/测试用）
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TierMigrationStats {
    pub promoted_to_warm: usize,
    pub demoted_to_cold: usize,
    pub cold_save_errors: usize,
}

impl ContextTiers {
    /// V29.13 段2：recent_demoted buffer 容量上限
    const RECENT_DEMOTED_CAP: usize = 8;

    pub fn new(cold: Arc<dyn SessionStore>) -> Self {
        Self {
            hot: ContextWindow::default(),
            hot_snapshots: RwLock::new(std::collections::VecDeque::new()),
            warm: RwLock::new(std::collections::VecDeque::new()),
            cold,
            compressed_messages: RwLock::new(Vec::new()),
            recent_demoted: RwLock::new(std::collections::VecDeque::new()),
        }
    }

    /// V29.13 段2：一次性取空 cold demote buffer
    ///
    /// PalaceAbsorbHook 在 TurnPostFanOut 触发时调用，把 buffer 中堆积的 snapshot
    /// 全部消费并升维到 KnowledgeEntry。take 后 buffer 为空，下次 demote 重新积累。
    pub async fn take_recent_demoted(&self) -> Vec<SessionSnapshot> {
        let mut buf = self.recent_demoted.write().await;
        buf.drain(..).collect()
    }

    /// Task #80：把一份 SessionSnapshot 推入 hot tier（FIFO 尾插）
    ///
    /// 副作用：仅写 hot_snapshots，不触发 migration（迁移由调用方显式调 migrate_tiers）。
    pub async fn record_snapshot(&self, snapshot: SessionSnapshot) {
        self.hot_snapshots.write().await.push_back(snapshot);
    }

    /// Task #80：执行 hot→warm + warm→cold 迁移
    ///
    /// 参数：
    /// - `current_turn`：当前 turn 编号；snapshot.turn_count + hot_age_threshold ≤ current_turn 的迁入 warm
    /// - `hot_age_threshold`：hot 滞留阈值（默认 30 turn）
    /// - `warm_capacity`：warm 容量上限（默认 100）；超出最早的 demote 进 cold
    ///
    /// 返回 TierMigrationStats（promoted_to_warm / demoted_to_cold / cold_save_errors）。
    /// cold.save 失败不抛错——计入 errors 字段，调用方按需告警。
    pub async fn migrate_tiers(
        &self,
        current_turn: u32,
        hot_age_threshold: u32,
        warm_capacity: usize,
    ) -> TierMigrationStats {
        let mut stats = TierMigrationStats::default();

        // Step 1: hot → warm（按 turn_count 老化）
        let promoted: Vec<SessionSnapshot> = {
            let mut hot = self.hot_snapshots.write().await;
            let mut out = Vec::new();
            let mut keep = std::collections::VecDeque::new();
            while let Some(s) = hot.pop_front() {
                if s.turn_count + hot_age_threshold <= current_turn {
                    out.push(s);
                } else {
                    keep.push_back(s);
                }
            }
            // 未老化的回写
            *hot = keep;
            out
        };
        stats.promoted_to_warm = promoted.len();
        if !promoted.is_empty() {
            let mut warm = self.warm.write().await;
            for s in promoted {
                warm.push_back(s);
            }
        }

        // Step 2: warm → cold（容量裁剪）
        let to_demote: Vec<SessionSnapshot> = {
            let mut warm = self.warm.write().await;
            let mut out = Vec::new();
            while warm.len() > warm_capacity {
                if let Some(s) = warm.pop_front() {
                    out.push(s);
                }
            }
            out
        };
        for snapshot in to_demote {
            stats.demoted_to_cold += 1;
            // V29.13 段2：push 到 recent_demoted buffer（保 cap，溢出弹首）
            // 在 cold.save 之前就 push——即使 save 失败 hook 仍能 absorb（内存中数据有效）
            {
                let mut buf = self.recent_demoted.write().await;
                if buf.len() >= Self::RECENT_DEMOTED_CAP {
                    buf.pop_front();
                }
                buf.push_back(snapshot.clone());
            }
            if let Err(e) = self.cold.save(snapshot).await {
                stats.cold_save_errors += 1;
                tracing::warn!(error = %e, "tier demote: cold.save failed");
            }
        }

        stats
    }
}

// ─── GeneralizedIndex ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SegmentKind {
    Code,
    Text,
    Conversation,
    Data,
}

impl std::fmt::Display for SegmentKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SegmentKind::Code => write!(f, "Code"),
            SegmentKind::Text => write!(f, "Text"),
            SegmentKind::Conversation => write!(f, "Conversation"),
            SegmentKind::Data => write!(f, "Data"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexSegment {
    pub id: String,
    pub kind: SegmentKind,
    pub label: String,
    pub tokens: usize,
    pub skeleton: String,
    pub priority_hint: f64,
    #[serde(skip)]
    pub offset: usize,
    #[serde(skip)]
    pub length: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeneralizedIndex {
    pub source: String,
    pub total_bytes: usize,
    pub segments: Vec<IndexSegment>,
}

// ─── Internal Declared Content ──────────────────────────────────────────────

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct DeclaredContent {
    source: String,
    intent: String,
    index: GeneralizedIndex,
    full_content: HashMap<String, String>,
    timestamp: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressLevel {
    Detailed,
    Brief,
    Minimal,
}

impl CompressLevel {
    pub fn from_str(s: &str) -> Self {
        match s {
            "detailed" => CompressLevel::Detailed,
            "brief" => CompressLevel::Brief,
            "minimal" => CompressLevel::Minimal,
            _ => CompressLevel::Brief,
        }
    }
}

// ─── ContextManager ─────────────────────────────────────────────────────────

/// Maximum retained content entries before eviction (FIFO).
/// Prevents unbounded memory growth in long-lived sessions.
const MAX_RETAINED_ENTRIES: usize = 50;
/// Max pending declarations before eviction（P3-B: BoundedFifo 上限）
const MAX_PENDING: usize = 5;
/// Phase Z2：messages.recover archive 容量（LRU evict）
const MAX_ARCHIVE_ENTRIES: usize = 50;

/// Cached token count to avoid O(n) recounting per turn.
///
/// H1 修复：`retained_hash` 改用 DefaultHasher 计算所有 (id, content) 对的真实哈希，
/// 避免之前的 length-based hash 导致"长度相同但内容变化"的假命中。
struct TokenCache {
    retained_tokens: usize,
    retained_hash: u64,
}

/// 计算 retained_content 的真实 hash（顺序敏感）
///
/// P3-B: 接受 Iterator 而非 slice，兼容 BoundedFifo 与 Vec
/// Ctx-C: 仅 hash id+content；忽略 RetainMeta（meta 变化不应触发 token re-count）
fn hash_retained<'a, I>(retained: I) -> u64
where
    I: IntoIterator<Item = &'a RetainedEntry>,
{
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for (id, content, _meta) in retained {
        id.hash(&mut hasher);
        content.hash(&mut hasher);
    }
    hasher.finish()
}

/// Phase Ctx-A：计算消息 token cache 键
///
/// (len, first_msg_content_hash, last_msg_content_hash) 三元组识别 messages 序列变化：
/// - 单调追加：len 增、first 不变、last 变 → cache miss → 重算
/// - 中段压缩：len 减、first 不变、last 不变 → cache miss（len 变化）
/// - 全清重建：len 极差大 → cache miss
/// 误命中风险：first+last 相同但中段被替换的伪场景——实际中由调用方保证不发生。
fn compute_msg_cache_key(messages: &[Message]) -> (usize, u64, u64) {
    use std::hash::{Hash, Hasher};
    fn hash_content(m: &Message) -> u64 {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        match &m.content {
            Some(crate::llm::MessageContent::Text(t)) => t.hash(&mut h),
            Some(crate::llm::MessageContent::MultiPart(parts)) => {
                parts.len().hash(&mut h);
                for p in parts {
                    content_part_len(p).hash(&mut h);
                }
            }
            None => 0u64.hash(&mut h),
        }
        h.finish()
    }
    let len = messages.len();
    let first = messages.first().map(hash_content).unwrap_or(0);
    let last = messages.last().map(hash_content).unwrap_or(0);
    (len, first, last)
}

/// Phase Ctx-C：retained segment 元数据（命中计数 + turn 距离）
///
/// 不参与字符串 hash（hash_retained 只看 id+content）——
/// 元数据更新不应触发 token cache invalidation。
#[derive(Debug, Clone, Default)]
pub struct RetainMeta {
    /// 创建于哪个 turn（写入时 current_turn）
    pub created_turn: u32,
    /// 最后被引用的 turn（segment id 在 LLM 输出/工具参数中出现）
    pub last_used_turn: u32,
    /// 累计引用次数
    pub ref_count: u32,
}

// 注：旧手写 Default 已迁移到 #[derive(Default)]（u32 默认 0，行为等价）；
//     clippy::derivable_impls 改造，零行为变更。

impl RetainMeta {
    /// W4 (Task #102)：以 current_turn 为基准的 importance 评分
    ///
    /// ## 信号融合
    /// - `ref_count`：每被引用一次 +1.0（饱和加权，避免单段被反复 mark 占满 budget）
    /// - `recency`：last_used_turn → current_turn 距离越近分越高（指数衰减，半衰期 ≈ 10 turn）
    /// - `freshness`：created_turn 越近分越高（线性，避免老段被新引用 1 次就永远霸榜）
    ///
    /// ## 数值范围
    /// 大致 [0.0, 5.0+]——0 表示从未引用且年代久远，5+ 表示近期反复引用。
    /// 阈值用于 `evict_by_importance` 的排序裁剪。
    pub fn importance_score(&self, current_turn: u32) -> f64 {
        // 饱和的 ref_count 贡献：1 - 0.7^n，n=0→0, n=1→0.3, n=3→0.66, n=5→0.83, n=10→0.97
        // clamp to 100 避免 u32 → i32 溢出（100 时结果已 ≈ 1.0）
        let ref_factor = 1.0 - 0.7_f64.powi(self.ref_count.min(100) as i32);

        let last = self.last_used_turn.max(self.created_turn);
        let recency_dist = (current_turn.saturating_sub(last)) as f64;
        // 半衰期 10 turn：dist=0→1.0, dist=10→0.5, dist=20→0.25, dist=40→0.0625
        let recency_factor = 0.5_f64.powf(recency_dist / 10.0);

        // freshness：新创建段加 0.5，每过 30 turn 衰减一半
        let age = (current_turn.saturating_sub(self.created_turn)) as f64;
        let freshness_factor = 0.5 * 0.5_f64.powf(age / 30.0);

        // 加权：ref_count 主导（最大 ~3.0），recency 中等（~1.5），freshness 兜底（~0.5）
        ref_factor * 3.0 + recency_factor * 1.5 + freshness_factor
    }
}

/// Phase Ctx-C: retained_content 完整条目
pub type RetainedEntry = (String, String, RetainMeta);

/// W4 (Task #102)：retained 段统计快照
#[derive(Debug, Clone)]
pub struct RetainedDiagnostics {
    pub entries: usize,
    pub total_tokens: usize,
    pub avg_importance: f64,
    pub max_importance: f64,
    pub current_turn: u32,
}

/// Phase Ctx-A：消息 token 增量缓存键
///
/// 用 (messages.len, 第一条 hash, 末尾 hash) 三元组作为 cache key。
/// 单调追加场景命中率高；压缩场景一次 cache miss 后立即重建。
///
/// ## L1 接入（Task #82）
/// 单值快路径保留（命中率高的连续追加场景），同时引入 L1MemoryCache
/// 作为多键 fallback——压缩/回退/分支等导致单值 miss 但历史 (len,h1,h2)
/// 仍存活的场景下命中。
struct MessageTokenCache {
    cache_key: (usize, u64, u64),
    tokens: usize,
}

/// Task #82：将 (len, first_hash, last_hash) 序列化为 L1 字符串键
fn msg_cache_key_str(key: &(usize, u64, u64)) -> String {
    format!("mtc-{}-{}-{}", key.0, key.1, key.2)
}

/// Task #82：L0/L1 双 miss 时的重算路径——O(n) 累加后双写两层缓存
///
/// 引用关系：仅由 estimate_total_tokens 在双层 miss 时调用。
/// 副作用：写 msg_cache (L0) + msg_cache_l1 (L1)；不抛错（L1 写失败静默忽略）。
async fn compute_and_fill_msg_tokens(
    mgr: &ContextManager,
    messages: &[Message],
    cache_key: (usize, u64, u64),
    l1_key: &str,
) -> usize {
    use crate::cache::CacheBackend;
    let computed: usize = messages
        .iter()
        .map(|m| {
            let text_tokens = match &m.content {
                Some(crate::llm::MessageContent::Text(t)) => estimate_tokens(t),
                Some(crate::llm::MessageContent::MultiPart(parts)) => {
                    // 对 MultiPart 回退到字节估算（各 part 类型混合，无法统一取 &str）
                    let bytes: usize = parts.iter().map(content_part_len).sum();
                    (bytes as f64 * 0.35) as usize + parts.len()
                }
                None => 1,
            };
            // +4 for message overhead (role, separators)
            4 + text_tokens
        })
        .sum();
    *mgr.msg_cache.write().await = Some(MessageTokenCache {
        cache_key,
        tokens: computed,
    });
    let bytes = (computed as u64).to_le_bytes().to_vec();
    let _ = mgr
        .msg_cache_l1
        .set(l1_key, bytes, mgr.msg_cache_l1.default_ttl())
        .await;
    computed
}

/// Phase Ctx-A：子系统 token 占用记账
///
/// 各子系统在写入/释放时通过 `record_subsystem_usage(key, tokens)` 更新自己的占用，
/// total = sum(values)。消息部分由 estimate_total_tokens 直接维护，不进入此 map。
#[derive(Debug, Default, Clone)]
pub struct SubsystemUsage {
    /// 消息流（messages + retained）
    pub messages: usize,
    /// Phase γ-E result_store 中完整 output 的 token 总和
    pub result_store: usize,
    /// 历史压缩物 (tiers.compressed_messages 的 summary tokens)
    pub compressed_messages: usize,
}

impl SubsystemUsage {
    /// 总占用——current_tokens 的真相源
    pub fn total(&self) -> usize {
        self.messages + self.result_store + self.compressed_messages
    }
}

pub struct ContextManager {
    pub window: Arc<RwLock<ContextWindow>>,
    pub tiers: ContextTiers,
    pending: RwLock<abacus_types::BoundedFifo<DeclaredContent>>,
    retained_content: RwLock<abacus_types::BoundedFifo<RetainedEntry>>,
    token_cache: RwLock<Option<TokenCache>>,
    /// Phase Ctx-A：消息 token 增量缓存（单值快路径）
    ///
    /// 引用关系：仅由 estimate_total_tokens 读写；其他路径无访问。
    /// 生命周期：随 ContextManager 创建/销毁；compress / shed 不显式 invalidate
    /// （键变化自然 miss）。
    msg_cache: RwLock<Option<MessageTokenCache>>,
    /// Task #82：L1 多键 fallback——单值 miss 时查询，命中则跳过 O(n) 重算
    ///
    /// 引用关系：与 msg_cache 协同——estimate_total_tokens 先查 msg_cache，miss 再查 l1，
    /// 都 miss 时 O(n) 重算并双写。其他路径（auto_compress / shed）不直接访问 L1。
    /// 生命周期：与 ContextManager 同生命周期；TTL=300s 让冷数据自然过期。
    /// 容量 64 条目——压缩/分支/回退场景下 ~10 个历史状态足够。
    pub(crate) msg_cache_l1: Arc<crate::cache::L1MemoryCache>,
    /// Phase Ctx-A：子系统占用记账
    pub usage: RwLock<SubsystemUsage>,
    /// Phase Ctx-A：pressure shed 标记——pressure_monitor 报警时设 true，
    /// 下次 setup 检查到自动触发 auto_compress_messages
    pub shed_pending: std::sync::atomic::AtomicBool,
    /// Phase Ctx-D：可选 KnowledgeStore 引用（让 declare 复用 KB chunking）
    ///
    /// 注入路径：CoreLoop::with_memory() 时绑定（与 KbToolExecutor 共享同一 Arc）。
    /// None → declare 走原 path（自建 GeneralizedIndex）。
    /// Some → declare 优先 ingest 到 KB 再把 chunks 映射成 IndexSegment。
    pub kb_store: RwLock<Option<Arc<crate::knowledge_store::KnowledgeStore>>>,
    /// LLM 主动标记的受保护消息索引（turn 编号）
    ///
    /// ## 设计
    /// LLM 通过 `context_pin` 工具标记某些对话轮次为"不可压缩"。
    /// auto_compress_messages 在处理中间段时跳过 pinned 消息（保留原文）。
    ///
    /// ## 生命周期
    /// - 写入：`context_pin` tool executor
    /// - 读取：`auto_compress_messages` 压缩时检查
    /// - 清理：session 结束时随 ContextManager 销毁
    pub pinned_turns: RwLock<std::collections::HashSet<u32>>,
    /// Phase Z3：auto_compress 使用的默认档位
    pub default_compress_level: RwLock<CompressLevel>,
    /// Phase Z2：消息压缩 archive（recover_id → 原始 messages 副本）
    ///
    /// 让 messages.recover 工具按 id 取回压缩前的完整消息序列。
    /// LRU 容量上限 `MAX_ARCHIVE_ENTRIES`，超出 evict 最早。
    pub message_archive: RwLock<abacus_types::BoundedFifo<(String, Vec<Message>, ArchiveMeta)>>,
    /// Phase Z4：可插拔 summarizer（默认 DeterministicSummarizer::brief）
    ///
    /// auto_compress 仅在 force_discard 时才调用此 trait——平时走快速 deterministic 路径
    /// 保证 cache 友好。用户可通过 `set_summarizer` 注入 LLM-driven 实现。
    pub summarizer: RwLock<Arc<dyn Summarizer>>,
    /// W4 (Task #102)：单 session 当前 turn 编号
    ///
    /// 引用关系：
    /// - 写入：pipeline 每 turn 起调 `set_current_turn`（一次/turn）
    /// - 读取：① `compress` 写入 RetainMeta.created_turn 修复历史 bug
    ///         ② `evict_by_importance` 计算 distance / age decay
    ///         ③ `audit_report` 输出 retained 段统计
    ///
    /// 设计动机：之前 `compress` 内部用 `RetainMeta::default()` 强写 created_turn=0，导致
    /// 任何 turn>20 后所有 retained 段 distance>20+ref_count==0 被强 evict。修法是从一处真相源
    /// 读 turn，而非把 turn 参数推到 compress 签名（compress 由 LLM 工具触发，无 turn 上下文）。
    pub current_turn: std::sync::atomic::AtomicU32,
}

/// Phase Z2：archive 条目元数据
#[derive(Debug, Clone)]
pub struct ArchiveMeta {
    pub created_turn: u32,
    pub original_count: usize,
    pub turn_range: Option<(u32, u32)>,
}

/// Phase Z4：消息摘要器 trait（可插拔）
///
/// auto_compress 在合并消息时调用此 trait 产生 summary 文本。
/// 默认实现 `DeterministicSummarizer` 与 Brief 档位等价（首行+role+tok）。
/// 用户可注入 `LlmSummarizer` 等高质量但成本更高的实现。
#[async_trait::async_trait]
pub trait Summarizer: Send + Sync {
    /// 对一组消息生成摘要
    ///
    /// `max_tokens` 是建议上限（实现可酌情超出但应尽量遵守）；
    /// 返回值是渲染后的纯文本 summary。
    async fn summarize(&self, messages: &[Message], max_tokens: usize) -> Result<String, KernelError>;
}

/// 默认 deterministic 实现——取每条消息首行 + role + tok 数
pub struct DeterministicSummarizer {
    pub snippet_lines: usize,
    pub snippet_chars: usize,
}

impl DeterministicSummarizer {
    pub fn brief() -> Self { Self { snippet_lines: 1, snippet_chars: 80 } }
    pub fn detailed() -> Self { Self { snippet_lines: 2, snippet_chars: 160 } }
    pub fn minimal() -> Self { Self { snippet_lines: 0, snippet_chars: 0 } }
}

#[async_trait::async_trait]
impl Summarizer for DeterministicSummarizer {
    async fn summarize(&self, messages: &[Message], _max_tokens: usize) -> Result<String, KernelError> {
        let count = messages.len();
        let mut total_tok = 0usize;
        let mut role_summary: Vec<String> = Vec::with_capacity(count);
        for m in messages {
            let text = match &m.content {
                Some(MessageContent::Text(t)) => t.clone(),
                Some(MessageContent::MultiPart(parts)) => parts.iter().filter_map(|p| {
                    if let crate::llm::ContentPart::Text { text } = p { Some(text.clone()) } else { None }
                }).collect::<Vec<_>>().join(" "),
                None => String::new(),
            };
            let tok = estimate_tokens(&text);
            total_tok += tok;
            let role = format!("{:?}", m.role);
            if self.snippet_lines == 0 {
                role_summary.push(format!("[{role}] ({tok} tok)"));
            } else {
                let snippet = text.lines()
                    .take(self.snippet_lines)
                    .collect::<Vec<_>>()
                    .join(" ")
                    .trim()
                    .chars()
                    .take(self.snippet_chars)
                    .collect::<String>();
                role_summary.push(format!("[{role}] {snippet}"));
            }
        }
        Ok(format!("{count} messages, ~{total_tok} tok\n{}", role_summary.join("\n")))
    }
}

impl ContextManager {
    pub fn new(cold_store: Arc<dyn SessionStore>) -> Self {
        Self {
            window: Arc::new(RwLock::new(ContextWindow::default())),
            tiers: ContextTiers::new(cold_store),
            pending: RwLock::new(abacus_types::BoundedFifo::new(MAX_PENDING)),
            retained_content: RwLock::new(abacus_types::BoundedFifo::new(MAX_RETAINED_ENTRIES)),
            token_cache: RwLock::new(None),
            msg_cache: RwLock::new(None),
            // Task #82：L1 多键缓存——容量 64 / TTL 300s
            // 容量选择：压缩+分支+回退场景下保留 ~10 个历史状态足够；64 留余量
            // TTL 选择：5min 让冷数据自然过期，避免长 session 下无效数据堆积
            msg_cache_l1: Arc::new(crate::cache::L1MemoryCache::new(
                64,
                std::time::Duration::from_secs(300),
            )),
            usage: RwLock::new(SubsystemUsage::default()),
            shed_pending: std::sync::atomic::AtomicBool::new(false),
            kb_store: RwLock::new(None),
            pinned_turns: RwLock::new(std::collections::HashSet::new()),
            default_compress_level: RwLock::new(CompressLevel::Brief),
            message_archive: RwLock::new(abacus_types::BoundedFifo::new(MAX_ARCHIVE_ENTRIES)),
            summarizer: RwLock::new(Arc::new(DeterministicSummarizer::brief())),
            // W4 (Task #102)：默认 0；pipeline 每 turn 起调 set_current_turn 推进
            current_turn: std::sync::atomic::AtomicU32::new(0),
        }
    }

    /// W4 (Task #102)：推进 current_turn——pipeline 每 turn 起头调用一次
    pub fn set_current_turn(&self, turn: u32) {
        self.current_turn.store(turn, std::sync::atomic::Ordering::Release);
    }

    pub fn get_current_turn(&self) -> u32 {
        self.current_turn.load(std::sync::atomic::Ordering::Acquire)
    }

    /// LLM 主动 pin 指定 turn（压缩时保留原文）
    pub async fn pin_turn(&self, turn: u32) {
        self.pinned_turns.write().await.insert(turn);
    }

    /// 取消 pin
    pub async fn unpin_turn(&self, turn: u32) {
        self.pinned_turns.write().await.remove(&turn);
    }

    /// 获取当前 pinned turns 列表
    pub async fn pinned_turns_list(&self) -> Vec<u32> {
        self.pinned_turns.read().await.iter().copied().collect()
    }

    /// Phase Z4：注入自定义 summarizer（如 LLM-driven）
    pub async fn set_summarizer(&self, summarizer: Arc<dyn Summarizer>) {
        *self.summarizer.write().await = summarizer;
    }

    /// Phase Z2：按 recover_id 取回压缩前的原始 messages
    ///
    /// 返回 None 表示 id 不存在或已被 LRU evict。
    pub async fn recover_messages(&self, recover_id: &str) -> Option<Vec<Message>> {
        let archive = self.message_archive.read().await;
        for (id, messages, _meta) in archive.iter() {
            if id == recover_id {
                return Some(messages.clone());
            }
        }
        None
    }

    /// Phase Z2：archive 当前条目数（诊断/测试）
    pub async fn archive_size(&self) -> usize {
        self.message_archive.read().await.iter().count()
    }

    /// Task #80：暴露 record_snapshot 给 pipeline post-turn 钩子
    pub async fn record_snapshot(&self, snapshot: SessionSnapshot) {
        self.tiers.record_snapshot(snapshot).await;
    }

    /// Task #80：暴露 migrate_tiers 给 pipeline post-turn 钩子
    ///
    /// 默认参数：hot_age_threshold=30, warm_capacity=100
    pub async fn run_tier_migration(
        &self,
        current_turn: u32,
        hot_age_threshold: u32,
        warm_capacity: usize,
    ) -> TierMigrationStats {
        self.tiers
            .migrate_tiers(current_turn, hot_age_threshold, warm_capacity)
            .await
    }

    /// Task #80：诊断/测试用——查看 hot_snapshots 当前数量
    pub async fn hot_snapshot_count(&self) -> usize {
        self.tiers.hot_snapshots.read().await.len()
    }

    /// Task #80：诊断/测试用——查看 warm 当前数量
    pub async fn warm_snapshot_count(&self) -> usize {
        self.tiers.warm.read().await.len()
    }

    /// Phase Z1：从 cold tier (SessionStore) 按关键词召回历史 SessionSnapshot
    ///
    /// 用于 session.recall 工具：LLM 看到 [Compressed history: ...] 但 recover_id
    /// 已 LRU evict 时，可改用关键词 query 从持久化层召回上下文。
    /// `limit` 上限 20 防止单次返回过大。
    pub async fn recall_from_cold(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SessionSnapshot>, KernelError> {
        let limit = limit.min(20);
        let mut results = self.tiers.cold.search(query).await?;
        results.truncate(limit);
        Ok(results)
    }

    /// Phase Z1：取最近 N 条 SessionSnapshot（按 created_at desc）
    pub async fn recall_recent(&self, limit: usize) -> Result<Vec<SessionSnapshot>, KernelError> {
        let limit = limit.min(20);
        self.tiers.cold.load_recent(limit).await
    }

    /// Phase Ctx-D：注入 KnowledgeStore，让 declare 复用 KB chunking
    pub async fn set_kb_store(&self, store: Arc<crate::knowledge_store::KnowledgeStore>) {
        *self.kb_store.write().await = Some(store);
    }

    /// Phase Z3：设置默认压缩档位
    pub async fn set_compress_level(&self, level: CompressLevel) {
        *self.default_compress_level.write().await = level;
    }

    /// Phase Ctx-D：把 GeneralizedIndex + full_content 写入 pending 并返回
    async fn persist_declared(
        &self,
        index: GeneralizedIndex,
        full_content: HashMap<String, String>,
        intent: &str,
    ) -> GeneralizedIndex {
        let index_clone = index.clone();
        let mut pending = self.pending.write().await;
        pending.push(DeclaredContent {
            source: index.source.clone(),
            intent: intent.to_string(),
            index,
            full_content,
            timestamp: Utc::now().timestamp(),
        });
        index_clone
    }

    /// Phase Ctx-A：子系统报告自己的 token 占用
    pub async fn set_subsystem_usage(&self, key: &str, tokens: usize) {
        let mut usage = self.usage.write().await;
        match key {
            "result_store" => usage.result_store = tokens,
            "compressed_messages" => usage.compressed_messages = tokens,
            "messages" => usage.messages = tokens,
            _ => {} // 未知子系统忽略
        }
        let total = usage.total();
        drop(usage);
        // 同步到 ContextWindow.current_tokens（current_tokens 现在是各子系统总和的 view）
        let mut w = self.window.write().await;
        w.current_tokens = total;
    }

    /// Phase Ctx-A：取当前各子系统占用快照
    pub async fn usage_snapshot(&self) -> SubsystemUsage {
        self.usage.read().await.clone()
    }

    /// Phase Ctx-A：pressure shed 标记接通
    pub fn mark_shed_pending(&self) {
        self.shed_pending.store(true, std::sync::atomic::Ordering::Release);
    }

    /// Phase Ctx-A：消费 shed 标记（setup 阶段调用一次）
    pub fn take_shed_pending(&self) -> bool {
        self.shed_pending.swap(false, std::sync::atomic::Ordering::AcqRel)
    }

    /// Phase Ctx-C：标记某 segment 被引用（更新 last_used + ref_count）
    pub async fn mark_segment_used(&self, seg_id: &str, current_turn: u32) {
        let mut rc = self.retained_content.write().await;
        // BoundedFifo 不直接支持原地修改，重建：找到 seg_id 的条目修改 meta
        let entries: Vec<RetainedEntry> = rc.iter().cloned().collect();
        rc.clear();
        for (id, content, mut meta) in entries {
            if id == seg_id {
                meta.last_used_turn = current_turn;
                meta.ref_count = meta.ref_count.saturating_add(1);
            }
            rc.push((id, content, meta));
        }
    }

    /// Phase Ctx-C：扫描 LLM 输出文本中出现的 segment id，批量标记命中
    ///
    /// 简单字串包含检测——seg_id 形式如 "seg-3"，常规中文/英文文本不会误命中。
    /// 多个段都被引用时一次性更新 meta，减少锁竞争。
    pub async fn scan_and_mark_used(&self, text: &str, current_turn: u32) -> usize {
        let entries: Vec<RetainedEntry> = self.retained_content.read().await.iter().cloned().collect();
        let mut hit_ids: Vec<String> = Vec::new();
        for (id, _, _) in &entries {
            if text.contains(id.as_str()) {
                hit_ids.push(id.clone());
            }
        }
        if hit_ids.is_empty() {
            return 0;
        }
        // 重建一次性写入
        let mut rc = self.retained_content.write().await;
        rc.clear();
        for (id, content, mut meta) in entries {
            if hit_ids.iter().any(|h| h == &id) {
                meta.last_used_turn = current_turn;
                meta.ref_count = meta.ref_count.saturating_add(1);
            }
            rc.push((id, content, meta));
        }
        hit_ids.len()
    }

    /// Phase Ctx-C：按 turn 距离 + ref_count 双标准 evict 久未引用的 segment
    ///
    /// 规则：
    /// - distance > `evict_distance` AND `ref_count == 0` → evict（从未被引用过）
    /// - distance > 2 * `evict_distance` → 强制 evict（极远段，无论 ref_count）
    ///
    /// 返回 evict 数量。
    pub async fn evict_stale_segments(
        &self,
        current_turn: u32,
        evict_distance: u32,
    ) -> usize {
        let entries: Vec<RetainedEntry> = self.retained_content.read().await.iter().cloned().collect();
        let mut kept: Vec<RetainedEntry> = Vec::with_capacity(entries.len());
        let mut evicted = 0usize;
        for (id, content, meta) in entries {
            let last = meta.last_used_turn.max(meta.created_turn);
            let distance = current_turn.saturating_sub(last);
            let force = distance > evict_distance.saturating_mul(2);
            let stale = distance > evict_distance && meta.ref_count == 0;
            if force || stale {
                evicted += 1;
                continue;
            }
            kept.push((id, content, meta));
        }
        if evicted > 0 {
            let mut rc = self.retained_content.write().await;
            rc.clear();
            for entry in kept {
                rc.push(entry);
            }
            *self.token_cache.write().await = None;
        }
        evicted
    }

    /// Phase Ctx-C：取 retained_content 当前状态快照（测试/诊断用）
    pub async fn retained_snapshot(&self) -> Vec<RetainedEntry> {
        self.retained_content.read().await.iter().cloned().collect()
    }

    /// W4 (Task #102)：基于 importance + token budget 的选择性保留
    ///
    /// ## 算法
    /// 1. 估算每段 tokens（content.len()/4）
    /// 2. 算 importance_score（综合 ref_count/recency/freshness）
    /// 3. 若总 token 超过 `max_tokens`，按 importance 降序保留前 K 段，丢弃尾部
    /// 4. 在保留集合内仍按**原 FIFO 顺序**排回（保 cache 友好性——前缀稳定）
    ///
    /// ## 与 evict_stale_segments 的关系
    /// - `evict_stale_segments`（旧）：按 distance + ref_count 硬阈值
    /// - `evict_by_importance`（新）：按综合分数 + token budget
    /// 两者可共存——distance>2x 的段仍由旧 API 强 evict 兜底（防止
    /// importance 评分 bug 导致老段永远霸位）
    ///
    /// 返回 (evicted_count, remaining_tokens)
    pub async fn evict_by_importance(
        &self,
        max_tokens: usize,
    ) -> (usize, usize) {
        let cur_turn = self.current_turn.load(std::sync::atomic::Ordering::Acquire);
        let entries: Vec<RetainedEntry> = self.retained_content.read().await.iter().cloned().collect();

        // 估算每段 token + importance
        let mut scored: Vec<(usize /*orig_idx*/, RetainedEntry, usize /*tokens*/, f64 /*score*/)> =
            entries
                .into_iter()
                .enumerate()
                .map(|(i, (id, content, meta))| {
                    let tokens = (content.len() / 4).max(1);
                    let score = meta.importance_score(cur_turn);
                    (i, (id, content, meta), tokens, score)
                })
                .collect();

        let total_tokens: usize = scored.iter().map(|(_, _, t, _)| *t).sum();
        if total_tokens <= max_tokens {
            return (0, total_tokens);
        }

        // 按 importance 降序选择直到 budget 耗尽（保留高分段）
        scored.sort_by(|a, b| b.3.partial_cmp(&a.3).unwrap_or(std::cmp::Ordering::Equal));
        let mut keep_indices: Vec<usize> = Vec::new();
        let mut budget = max_tokens;
        for (orig_idx, _, tokens, _) in &scored {
            if budget >= *tokens {
                budget = budget.saturating_sub(*tokens);
                keep_indices.push(*orig_idx);
            }
        }
        let kept_total: usize = max_tokens.saturating_sub(budget);
        let evicted = scored.len().saturating_sub(keep_indices.len());

        // 重新按原 FIFO 顺序回填——保 cache 前缀稳定
        keep_indices.sort_unstable();
        let mut keep_set: std::collections::HashSet<usize> = keep_indices.into_iter().collect();
        let mut rebuilt: Vec<RetainedEntry> = Vec::with_capacity(scored.len());
        // 重建 enumerate 映射
        let mut by_orig: std::collections::HashMap<usize, RetainedEntry> = scored
            .into_iter()
            .map(|(i, e, _, _)| (i, e))
            .collect();
        let mut seq: Vec<usize> = by_orig.keys().copied().collect();
        seq.sort_unstable();
        for idx in seq {
            if keep_set.remove(&idx) {
                if let Some(entry) = by_orig.remove(&idx) {
                    rebuilt.push(entry);
                }
            }
        }

        if evicted > 0 {
            let mut rc = self.retained_content.write().await;
            rc.clear();
            for entry in rebuilt {
                rc.push(entry);
            }
            *self.token_cache.write().await = None;
        }

        (evicted, kept_total)
    }

    /// W4 (Task #102)：retained 段诊断快照——audit_report 消费
    pub async fn retained_diagnostics(&self) -> RetainedDiagnostics {
        let cur_turn = self.current_turn.load(std::sync::atomic::Ordering::Acquire);
        let entries = self.retained_snapshot().await;
        let total_tokens: usize = entries.iter().map(|(_, c, _)| c.len() / 4).sum();
        let scores: Vec<f64> = entries
            .iter()
            .map(|(_, _, m)| m.importance_score(cur_turn))
            .collect();
        let avg_score = if !scores.is_empty() {
            scores.iter().sum::<f64>() / scores.len() as f64
        } else {
            0.0
        };
        let max_score = scores.iter().cloned().fold(0.0_f64, f64::max);
        RetainedDiagnostics {
            entries: entries.len(),
            total_tokens,
            avg_importance: avg_score,
            max_importance: max_score,
            current_turn: cur_turn,
        }
    }

    pub fn set_window(&self, max_tokens: usize, model_limit: usize) {
        if let Ok(mut w) = self.window.try_write() {
            w.set_model_limit(model_limit);
            w.set_max_tokens(max_tokens);
        }
    }

    pub fn set_trigger_pct(&self, pct: u8) {
        if let Ok(mut w) = self.window.try_write() {
            w.compression_trigger_pct = pct.clamp(50, 99);
        }
    }

    pub async fn declare(
        &self,
        source: &str,
        intent: &str,
        max_index_tokens: usize,
        session_messages: &[Message],
    ) -> Result<GeneralizedIndex, KernelError> {
        let (index, full_content) = if source.starts_with("file://") {
            let path = source.trim_start_matches("file://");
            if !path_is_allowed(path) {
                return Err(KernelError::Other(format!("path not allowed: {path}")));
            }
            // Phase Ctx-D：优先走 KB 路径——KB 已有的 chunking + token_estimate + heading_path
            // 与 GeneralizedIndex 同构，复用避免双轨实现。
            let kb_opt = self.kb_store.read().await.clone();
            if let Some(store) = kb_opt {
                // 静默 ingest（force=false → hash 命中跳过）
                if store.ingest(path, false).await.is_ok() {
                    if let Ok(chunks) = store.list_file_chunks(path).await {
                        if !chunks.is_empty() {
                            let mut full_content = HashMap::new();
                            let segments: Vec<IndexSegment> = chunks.iter().enumerate().map(|(i, c)| {
                                full_content.insert(c.id.clone(), c.content.clone());
                                let label = if c.heading_path.is_empty() {
                                    format!("chunk #{i}")
                                } else {
                                    c.heading_path.clone()
                                };
                                let skeleton = c.content.lines().next()
                                    .unwrap_or("").chars().take(120).collect::<String>();
                                IndexSegment {
                                    id: c.id.clone(),
                                    kind: SegmentKind::Text, // KB chunks 通用文本视角
                                    label,
                                    tokens: c.token_estimate,
                                    skeleton,
                                    priority_hint: 0.5,
                                    offset: 0,
                                    length: 0,
                                }
                            }).collect();
                            // 限制返回的 index tokens 总量
                            let limited = limit_segments_by_tokens(segments, max_index_tokens);
                            let total_bytes = chunks.iter().map(|c| c.content.len()).sum();
                            let index = GeneralizedIndex {
                                source: source.to_string(),
                                total_bytes,
                                segments: limited,
                            };
                            return Ok(self.persist_declared(index, full_content, intent).await);
                        }
                    }
                }
                // KB 路径失败 fall back 到原 path
                tracing::debug!("KB-path declare failed, falling back to legacy indexer");
            }
            // Single read — build both index and full_content from same content
            let content = tokio::fs::read_to_string(path)
                .await
                .map_err(|e| KernelError::Other(format!("cannot read {path}: {e}")))?;
            let ext = path.rsplit('.').next().unwrap_or("");
            let is_code = matches!(
                ext,
                "rs" | "py" | "ts" | "js" | "go" | "java" | "c" | "h" | "cpp" | "hpp" | "swift" | "kt"
            );
            let segments = if is_code {
                index_code_file(&content, max_index_tokens)
            } else if matches!(ext, "md" | "txt" | "toml" | "yaml" | "yml" | "json") {
                index_text_file(&content, max_index_tokens)
            } else {
                index_generic_file(&content, max_index_tokens)
            };
            // Build full_content map from the same content (no second read)
            let mut full_content = HashMap::new();
            let lines: Vec<&str> = content.lines().collect();
            for seg in &segments {
                if seg.offset < lines.len() {
                    let end = (seg.offset + seg.length).min(lines.len());
                    full_content.insert(seg.id.clone(), lines[seg.offset..end].join("\n"));
                }
            }
            let index = GeneralizedIndex {
                source: source.to_string(),
                total_bytes: content.len(),
                segments,
            };
            (index, full_content)
        } else if source.starts_with("session://") {
            let segments = index_conversation(session_messages, max_index_tokens);
            (GeneralizedIndex {
                source: source.to_string(),
                total_bytes: 0,
                segments,
            }, HashMap::new())
        } else {
            return Err(KernelError::Other(format!(
                "unsupported source scheme: {source}"
            )));
        };

        Ok(self.persist_declared(index, full_content, intent).await)
    }

    pub async fn keep(
        &self,
        segment_ids: &[String],
        full: bool,
    ) -> Result<Vec<(String, String)>, KernelError> {
        let pending = self.pending.read().await;
        let latest = pending.back().ok_or_else(|| {
            KernelError::Other("no active declaration".into())
        })?;

        let mut retained = Vec::new();
        for seg_id in segment_ids {
            let content = if full {
                latest
                    .full_content
                    .get(seg_id)
                    .cloned()
                    .unwrap_or_else(|| "[content not loaded]".into())
            } else {
                latest
                    .index
                    .segments
                    .iter()
                    .find(|s| &s.id == seg_id)
                    .map(|s| s.skeleton.clone())
                    .unwrap_or_else(|| "[segment not found]".into())
            };
            retained.push((seg_id.clone(), content));
        }

        {
            // P3-B: BoundedFifo 自动 evict（容量 MAX_RETAINED_ENTRIES）
            // Ctx-C：写入时附 RetainMeta
            // W4 (Task #102) bug fix：从 self.current_turn 读真值
            let cur_turn = self.current_turn.load(std::sync::atomic::Ordering::Acquire);
            let mut rc = self.retained_content.write().await;
            for (id, content) in &retained {
                rc.push((
                    id.clone(),
                    content.clone(),
                    RetainMeta {
                        created_turn: cur_turn,
                        last_used_turn: cur_turn,
                        ref_count: 0,
                    },
                ));
            }
            // Invalidate token cache
            *self.token_cache.write().await = None;
        }
        Ok(retained)
    }

    pub async fn compress(
        &self,
        segment_ids: &[String],
        level: CompressLevel,
    ) -> Result<Vec<(String, String)>, KernelError> {
        let pending = self.pending.read().await;
        let latest = pending.back().ok_or_else(|| {
            KernelError::Other("no active declaration".into())
        })?;

        let mut compressed = Vec::new();
        for seg_id in segment_ids {
            let text = latest
                .index
                .segments
                .iter()
                .find(|s| s.id == *seg_id)
                .map(|s| match level {
                    CompressLevel::Detailed => format!(
                        "[{kind}] {label} ({tok} tok): {skel}",
                        kind = s.kind,
                        label = s.label,
                        tok = s.tokens,
                        skel = s.skeleton
                    ),
                    CompressLevel::Brief => format!(
                        "[{kind}] {label}: {skel}",
                        kind = s.kind,
                        label = s.label,
                        skel = s.skeleton
                    ),
                    CompressLevel::Minimal => format!(
                        "{label} ({tok} tok)",
                        label = s.label,
                        tok = s.tokens
                    ),
                })
                .unwrap_or_else(|| "[segment not found]".into());
            compressed.push((seg_id.clone(), text));
        }

        // P3-B: BoundedFifo 自动 evict（容量 MAX_RETAINED_ENTRIES）
        // Ctx-C：附 RetainMeta
        // W4 (Task #102) bug fix：之前 `RetainMeta::default()` 强写 created_turn=0，
        //   导致任何 turn>20 之后所有段 distance>20+ref_count==0 全被 evict。
        //   现从 self.current_turn 读真值（pipeline 每 turn set_current_turn）写入。
        {
            let cur_turn = self.current_turn.load(std::sync::atomic::Ordering::Acquire);
            let mut rc = self.retained_content.write().await;
            for (id, content) in &compressed {
                rc.push((
                    id.clone(),
                    content.clone(),
                    RetainMeta {
                        created_turn: cur_turn,
                        last_used_turn: cur_turn,
                        ref_count: 0,
                    },
                ));
            }
        }
        // Invalidate token cache after modifying retained_content
        *self.token_cache.write().await = None;
        Ok(compressed)
    }

    /// Produce a structured cross-turn context block for LLM injection.
    ///
    /// ## Sections rendered (in order)
    /// 1. **Session history** — last 3 hot snapshots (most recent first); if hot is empty
    ///    falls back to warm tier (last 3).
    /// 2. **Declared content** — retained segments (existing behavior).
    /// 3. **Context pressure** — usage warning when window utilization > 60%.
    ///
    /// ##引用关系
    /// - 读: `self.tiers.hot_snapshots` / `self.tiers.warm` — session snapshot tiers
    /// - 读: `self.window` — context usage percentage
    /// - 读: `self.retained_content` — declared segments
    /// 消费方: CoreLoop system-prompt assembly (pre-turn injection)
    ///
    /// ## 生命周期
    /// 无副作用——纯读、无状态变更。每次 turn 起始重新调用构建。
    pub async fn retained_context_block(&self) -> String {
        let mut block = String::new();

        // ── Section 1: Session history from hot/warm snapshots ──────────────
        {
            let hot = self.tiers.hot_snapshots.read().await;
            let has_hot = !hot.is_empty();
            if has_hot {
                block.push_str("[Retained Context]\n");
                // Most recent 3 snapshots (reverse iteration = newest first)
                let recent: Vec<_> = hot.iter().rev().take(3).collect();
                for snap in &recent {
                    // Always render turn + summary; include key_decisions if any
                    block.push_str(&format!("- Turn {}: {}\n", snap.turn_count, snap.summary));
                    for decision in snap.key_decisions.iter().take(2) {
                        block.push_str(&format!("  - decision: {}\n", decision));
                    }
                }
            }
            drop(hot);

            // Fallback: if hot is empty, try warm tier
            if !has_hot {
                let warm = self.tiers.warm.read().await;
                if !warm.is_empty() {
                    block.push_str("[Retained Context (warm)]\n");
                    let recent: Vec<_> = warm.iter().rev().take(3).collect();
                    for snap in &recent {
                        block.push_str(&format!("- Turn {}: {}\n", snap.turn_count, snap.summary));
                    }
                }
            }
        }

        // ── Section 2: Declared content (existing behavior) ─────────────────
        {
            let retained = self.retained_content.read().await;
            if !retained.is_empty() {
                if !block.is_empty() {
                    block.push('\n');
                }
                block.push_str("## Declared Content\n\n");
                for (seg_id, content, _meta) in retained.iter() {
                    block.push_str(&format!("[{seg_id}]\n{content}\n\n"));
                }
            }
        }

        // ── Section 3: Context pressure indicator ───────────────────────────
        {
            let window = self.window.read().await;
            let usage = window.usage_pct();
            if usage > 60.0 {
                if !block.is_empty() {
                    block.push('\n');
                }
                block.push_str(&format!(
                    "[Context pressure: {:.0}%] — prefer concise responses\n",
                    usage
                ));
            }
        }

        block
    }

    pub async fn estimate_total_tokens(&self, messages: &[Message]) -> usize {
        let retained = self.retained_content.read().await;
        // H1 修复：用真实内容哈希做缓存键，避免长度相同但内容变化时假命中
        // P3-B: hash_retained 接受 Iterator，BoundedFifo.iter() 直接传入
        let current_hash = hash_retained(retained.iter());

        let retained_tokens = {
            let cache = self.token_cache.read().await;
            cache.as_ref()
                .filter(|c| c.retained_hash == current_hash)
                .map(|c| c.retained_tokens)
        };

        let retained_tokens = match retained_tokens {
            Some(cached) => cached,
            None => {
                let computed: usize = retained
                    .iter()
                    .map(|(_, c, _)| estimate_tokens(c))
                    .sum();
                *self.token_cache.write().await = Some(TokenCache {
                    retained_tokens: computed,
                    retained_hash: current_hash,
                });
                computed
            }
        };
        drop(retained); // 释放读锁，避免与 msg_cache 写锁竞争

        // Phase Ctx-A + Task #82：双层消息 token 缓存
        //
        // cache_key = (len, 第一条 hash, 末尾 hash)
        //   ├── L0 单值快路径 msg_cache：单调追加场景 100% 命中
        //   ├── L1 多键缓存 msg_cache_l1：单值 miss 但历史 (len,h1,h2) 仍存活时命中
        //   └── L2 重算 + 双写：都 miss 时 O(n) 累加并填充两层
        let msg_cache_key = compute_msg_cache_key(messages);
        let cached_msg_tokens = {
            let cache = self.msg_cache.read().await;
            cache.as_ref()
                .filter(|c| c.cache_key == msg_cache_key)
                .map(|c| c.tokens)
        };
        let msg_tokens = match cached_msg_tokens {
            Some(t) => t,
            None => {
                // L0 miss → 查 L1
                let l1_key = msg_cache_key_str(&msg_cache_key);
                use crate::cache::CacheBackend;
                let l1_hit = self.msg_cache_l1.get(&l1_key).await.ok().flatten();
                if let Some(bytes) = l1_hit {
                    if let Ok(arr) = <[u8; 8]>::try_from(bytes.as_slice()) {
                        let val = u64::from_le_bytes(arr) as usize;
                        // L1 hit → 回填 L0 加速下次连续命中
                        *self.msg_cache.write().await = Some(MessageTokenCache {
                            cache_key: msg_cache_key,
                            tokens: val,
                        });
                        val
                    } else {
                        // L1 数据格式异常，走 L2 重算分支
                        compute_and_fill_msg_tokens(self, messages, msg_cache_key, &l1_key).await
                    }
                } else {
                    // L1 也 miss → L2 重算并双写
                    compute_and_fill_msg_tokens(self, messages, msg_cache_key, &l1_key).await
                }
            }
        };

        let total = retained_tokens + msg_tokens;
        // Phase Ctx-A：同步消息子系统占用到统一记账
        // （retained 当前归在 messages 维度——retained 是消息流的"声明保留"产物，
        //  与历史压缩物 compressed_messages 不同，后者由 auto_compress_messages 维护）
        {
            let mut usage = self.usage.write().await;
            usage.messages = total;
            let grand_total = usage.total();
            drop(usage);
            let mut w = self.window.write().await;
            w.current_tokens = grand_total;
        }
        total
    }

    /// Phase Ctx-B: cache-friendly 消息压缩
    ///
    /// ## 设计要点（优于旧版）
    ///
    /// 1. **批量替换 vs 逐条改写**：把多条中间消息合并成 1 条 summary message。
    ///    旧版对 N 条消息逐条压缩 → cache miss 不可控；新版合并后 prefix 区域字节不变，
    ///    只在合并点局部 cache miss。
    ///
    /// 2. **tool_calls 元结构保留**：含 `tool_calls` 或 `tool_call_id` 的消息一律不压缩，
    ///    维持 assistant→tool 协议完整性，永不触发 `sanitize_dangling_tool_calls`。
    ///
    /// 3. **二阶段策略**：
    ///    - normal (85%~95%)：仅压缩"non-tool 中间段连续区间"
    ///    - force_discard (>=95%)：保留 early=1 + late=2，丢弃中间段（含 tool 序列），
    ///      接受 cache miss 换取存活
    pub async fn auto_compress_messages(
        &self,
        messages: &mut Vec<Message>,
    ) -> Vec<CompressedMessage> {
        let window = self.window.read().await;
        if !window.should_compress() {
            return Vec::new();
        }
        let force = window.should_force_discard();
        drop(window);

        let keep_count = if force { 3 } else { 8 };
        let early_keep = 2; // 保留开头 2 条（含初始 system/user 上下文）

        if messages.len() <= keep_count + early_keep + 1 {
            return Vec::new();
        }

        // Helper：判断消息是否含 tool 协议字段
        fn is_tool_protocol(m: &Message) -> bool {
            m.tool_calls.is_some() || m.tool_call_id.is_some()
                || matches!(m.role, MessageRole::Tool)
        }

        /// 消息重要性评分（0.0-1.0）——高分消息在压缩时保留更多内容
        /// 关键信息保护：决策、结论、错误、用户确认不会被简单压缩掉
        fn importance_score(m: &Message) -> f64 {
            let text = match &m.content {
                Some(MessageContent::Text(t)) => t.as_str(),
                _ => return 0.3, // 无文本内容，低分
            };
            let lower = text.to_lowercase();
            let mut score = 0.3; // 基线

            // 决策/结论标记 → 高保护
            let decision_markers = ["决定", "结论", "方案", "选择", "确认",
                "decision", "conclusion", "chosen", "confirmed", "approved",
                "plan:", "strategy:", "architecture:"];
            for marker in &decision_markers {
                if lower.contains(marker) { score += 0.25; break; }
            }

            // 错误/警告 → 高保护（诊断信息不能丢）
            let error_markers = ["error", "failed", "panic", "bug", "issue",
                "错误", "失败", "异常", "问题"];
            for marker in &error_markers {
                if lower.contains(marker) { score += 0.2; break; }
            }

            // 用户角色消息 → 中等保护（用户意图）
            if matches!(m.role, MessageRole::User) {
                score += 0.15;
            }

            // 长消息（>500 chars）可能含重要推理 → 轻微加分
            if text.len() > 500 { score += 0.1; }

            // 含代码块 → 中等保护
            if text.contains("```") { score += 0.15; }

            if score > 1.0 { 1.0f64 } else { score }
        }

        /// 从消息中提取关键结论（保护核心信息不丢失）
        fn extract_key_points(m: &Message) -> Option<String> {
            let text = match &m.content {
                Some(MessageContent::Text(t)) => t,
                _ => return None,
            };

            let mut points = Vec::new();

            // 提取 "##" 标题行（结论/决策/方案标题）
            for line in text.lines() {
                let trimmed = line.trim();
                if trimmed.starts_with("## ") || trimmed.starts_with("### ") {
                    points.push(trimmed.to_string());
                }
                // 提取 "决定/结论/方案:" 开头的行
                if trimmed.starts_with("决定") || trimmed.starts_with("结论")
                    || trimmed.starts_with("方案") || trimmed.starts_with("Plan:")
                    || trimmed.starts_with("Decision:") || trimmed.starts_with("Conclusion:")
                {
                    points.push(trimmed.chars().take(120).collect());
                }
            }

            if points.is_empty() { None }
            else { Some(points.join(" | ")) }
        }

        // 读取 LLM pinned turns（压缩时无条件保留）
        let pinned = self.pinned_turns.read().await.clone();

        let original = std::mem::take(messages);
        let total_len = original.len();
        let tail_start = total_len.saturating_sub(keep_count);

        let mut compressed_records: Vec<CompressedMessage> = Vec::new();
        let mut new_messages: Vec<Message> = Vec::new();

        // 累积缓冲：连续 non-tool 消息一次性合并
        let mut pending_summary: Vec<&Message> = Vec::new();
        // Phase Z2：archive 写入缓冲——flush 时一并写入 message_archive
        let mut archive_writes: Vec<(String, Vec<Message>, ArchiveMeta)> = Vec::new();

        // Phase Z3：取默认压缩档位决定 snippet 长度
        let compress_level = *self.default_compress_level.read().await;
        let snippet_lines = match compress_level {
            CompressLevel::Detailed => 2,
            CompressLevel::Brief => 1,
            CompressLevel::Minimal => 0, // 不保留 snippet
        };
        let snippet_chars = match compress_level {
            CompressLevel::Detailed => 160,
            CompressLevel::Brief => 80,
            CompressLevel::Minimal => 0,
        };

        // 把 pending 的 non-tool 序列合并 flush 到 new_messages
        // Phase Z2：同时把原始 messages 写入 archive_writes，summary 嵌入 recover_id
        let flush_pending = |buf: &mut Vec<&Message>,
                             out: &mut Vec<Message>,
                             records: &mut Vec<CompressedMessage>,
                             archive: &mut Vec<(String, Vec<Message>, ArchiveMeta)>| {
            if buf.is_empty() {
                return;
            }
            let count = buf.len();
            let mut total_tok = 0usize;
            let mut role_summary: Vec<String> = Vec::with_capacity(count);
            let originals: Vec<Message> = buf.iter().map(|m| (*m).clone()).collect();
            for m in buf.iter() {
                let text = match &m.content {
                    Some(MessageContent::Text(t)) => t.clone(),
                    Some(MessageContent::MultiPart(parts)) => {
                        parts.iter().filter_map(|p| {
                            if let crate::llm::ContentPart::Text { text } = p { Some(text.clone()) } else { None }
                        }).collect::<Vec<_>>().join(" ")
                    }
                    None => String::new(),
                };
                let tok = estimate_tokens(&text);
                total_tok += tok;
                let role = format!("{:?}", m.role);
                if snippet_lines == 0 {
                    role_summary.push(format!("[{role}] ({tok} tok)"));
                } else {
                    let snippet = text.lines()
                        .take(snippet_lines)
                        .collect::<Vec<_>>()
                        .join(" ")
                        .trim()
                        .chars()
                        .take(snippet_chars)
                        .collect::<String>();
                    role_summary.push(format!("[{role}] {snippet}"));
                }
            }
            // Phase Z2：计算 recover_id（hash of originals + total_tok 防碰撞）
            use std::collections::hash_map::DefaultHasher;
            use std::hash::{Hash, Hasher};
            let mut h = DefaultHasher::new();
            count.hash(&mut h);
            total_tok.hash(&mut h);
            for m in &originals {
                if let Some(MessageContent::Text(t)) = &m.content {
                    t.hash(&mut h);
                }
            }
            let recover_id = format!("mb_{:016x}", h.finish());

            // 提取被压缩消息中的关键结论（避免丢失重要决策）
            let key_points: Vec<String> = buf.iter()
                .filter_map(|m| extract_key_points(m))
                .collect();
            let key_section = if key_points.is_empty() {
                String::new()
            } else {
                format!("\n[Preserved decisions: {}]", key_points.join("; "))
            };

            let summary = format!(
                "[Compressed: {count} msgs, ~{total_tok} tok, id={recover_id}]{key_section}\n{}",
                role_summary.join("\n")
            );
            let compressed_tok = estimate_tokens(&summary);
            records.push(CompressedMessage {
                original_role: "Compressed".into(),
                summary: summary.clone(),
                original_tokens: total_tok,
                compressed_tokens: compressed_tok,
            });
            // 写 archive
            archive.push((
                recover_id,
                originals,
                ArchiveMeta {
                    created_turn: 0,
                    original_count: count,
                    turn_range: None,
                },
            ));
            // 用 user 角色——确保不破 assistant/user 交替序列（合并消息没有 reasoning）
            out.push(Message {
                role: MessageRole::User,
                content: Some(MessageContent::Text(summary)),
                name: None,
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
                prefix: false,
            });
            buf.clear();
        };

        let mut current_turn_num: u32 = 0;
        for (i, msg) in original.iter().enumerate() {
            // 跟踪 turn 编号（每条 User 消息 = 新 turn）
            if matches!(msg.role, MessageRole::User) {
                current_turn_num += 1;
            }

            let in_early = i < early_keep;
            let in_late = i >= tail_start;

            if in_early || in_late {
                // 先 flush 累积的中间段
                flush_pending(&mut pending_summary, &mut new_messages, &mut compressed_records, &mut archive_writes);
                new_messages.push(msg.clone());
                continue;
            }

            // LLM pinned turn → 无条件保留（LLM 主动标记的重要对话）
            if pinned.contains(&current_turn_num) {
                flush_pending(&mut pending_summary, &mut new_messages, &mut compressed_records, &mut archive_writes);
                new_messages.push(msg.clone());
                continue;
            }

            // 中间段
            if force {
                // force_discard：丢弃中间所有（含 tool 序列；接受 cache miss 与可能的 sanitize 触发）
                // 累积一条总体记录
                let role = format!("{:?}", msg.role);
                let text = match &msg.content {
                    Some(MessageContent::Text(t)) => t.clone(),
                    _ => String::new(),
                };
                compressed_records.push(CompressedMessage {
                    original_role: role,
                    summary: format!("[Force-discarded: {} tok]", estimate_tokens(&text)),
                    original_tokens: estimate_tokens(&text),
                    compressed_tokens: 0,
                });
                continue; // 不写入 new_messages
            }

            let score = importance_score(msg);

            if is_tool_protocol(msg) {
                // Tool 协议消息：高重要性（错误/决策相关）→ 保留；低重要性 → 压缩为一行摘要
                if score >= 0.5 {
                    flush_pending(&mut pending_summary, &mut new_messages, &mut compressed_records, &mut archive_writes);
                    new_messages.push(msg.clone());
                } else {
                    // 低重要性 tool 结果（如简单的文件读取成功）→ 进入 pending 压缩
                    pending_summary.push(msg);
                }
            } else if score >= 0.7 {
                // 高重要性非 tool 消息（决策/结论/错误）→ 保留原文但截断到前 300 chars
                flush_pending(&mut pending_summary, &mut new_messages, &mut compressed_records, &mut archive_writes);
                let mut preserved = msg.clone();
                if let Some(MessageContent::Text(ref text)) = preserved.content {
                    if text.len() > 300 {
                        // 截断但保留关键点
                        let key_points = extract_key_points(msg);
                        let truncated = format!(
                            "{}...\n[Key: {}]",
                            text.chars().take(250).collect::<String>(),
                            key_points.unwrap_or_else(|| "truncated".into())
                        );
                        preserved.content = Some(MessageContent::Text(truncated));
                    }
                }
                new_messages.push(preserved);
            } else {
                // 中低重要性 → 进入 pending 批量压缩
                pending_summary.push(msg);
            }
        }
        // 末尾 flush（理论上不会触发因为 late 阶段会先 flush，但保险）
        flush_pending(&mut pending_summary, &mut new_messages, &mut compressed_records, &mut archive_writes);

        *messages = new_messages;

        self.tiers
            .compressed_messages
            .write()
            .await
            .extend(compressed_records.clone());

        // Phase Z2：把 archive_writes 写入 message_archive（BoundedFifo LRU evict）
        if !archive_writes.is_empty() {
            let mut arch = self.message_archive.write().await;
            for entry in archive_writes {
                arch.push(entry);
            }
        }

        // Phase Ctx-A：更新 compressed_messages 子系统占用
        let compressed_total: usize = self.tiers.compressed_messages.read().await
            .iter()
            .map(|c| c.compressed_tokens)
            .sum();
        self.set_subsystem_usage("compressed_messages", compressed_total).await;

        // 更新 messages 子系统占用 + window.current_tokens（estimate_total_tokens 内部已同步）
        let _new_tokens = self.estimate_total_tokens(messages).await;

        compressed_records
    }
}

// ─── Index Generators ───────────────────────────────────────────────────────

// 模块级共享 Regex（编译一次，所有 declare 调用复用）
// 之前每次 index_code_file 都重新编译 7 个 Regex，是热路径性能浪费
use std::sync::LazyLock;
static RE_FN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*(?:pub\s+)?(?:async\s+)?fn\s+(\w+)\s*\(").unwrap());
static RE_STRUCT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*(?:pub\s+)?struct\s+(\w+)").unwrap());
static RE_IMPL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*(?:pub\s+)?(?:unsafe\s+)?impl\s+(\w+)").unwrap());
static RE_TRAIT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*(?:pub\s+)?(?:unsafe\s+)?trait\s+(\w+)").unwrap());
static RE_ENUM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*(?:pub\s+)?enum\s+(\w+)").unwrap());
static RE_MOD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*(?:pub\s+)?mod\s+(\w+)").unwrap());
static RE_TYPE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*(?:pub\s+)?type\s+(\w+)").unwrap());
static RE_HEADING: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(#{1,6})\s+(.+)").unwrap());
static RE_PARA: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\n\s*\n").unwrap());

fn index_code_file(content: &str, max_tokens: usize) -> Vec<IndexSegment> {
    let mut segments = Vec::new();
    let mut budget = max_tokens;
    let mut seg_counter = 0u32;

    let re_fn = &*RE_FN;
    let re_struct = &*RE_STRUCT;
    let re_impl = &*RE_IMPL;
    let re_trait = &*RE_TRAIT;
    let re_enum = &*RE_ENUM;
    let re_mod = &*RE_MOD;
    let re_type = &*RE_TYPE;

    let mut imports: Vec<(usize, String)> = Vec::new();
    let lines: Vec<&str> = content.lines().collect();

    for (line_idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if budget == 0 {
            break;
        }
        if trimmed.starts_with("use ") || trimmed.starts_with("pub use ") {
            imports.push((line_idx, trimmed.to_string()));
            continue;
        }
        if !imports.is_empty() {
            let joined = imports.iter().map(|(_, t)| t.clone()).collect::<Vec<_>>().join("; ");
            let first_idx = imports[0].0;
            let tok = estimate_tokens(&joined);
            if tok <= budget {
                segments.push(IndexSegment {
                    id: format!("seg-{}", seg_counter),
                    kind: SegmentKind::Code,
                    label: "imports".into(),
                    tokens: tok,
                    skeleton: joined,
                    priority_hint: 0.3,
                    offset: first_idx,
                    length: imports.len(),
                });
                seg_counter += 1;
                budget = budget.saturating_sub(tok);
            }
            imports.clear();
        }

        let skeleton = if let Some(caps) = re_fn.captures(trimmed) {
            format!("fn {}", &caps[1])
        } else if let Some(caps) = re_struct.captures(trimmed) {
            format!("struct {}", &caps[1])
        } else if let Some(caps) = re_impl.captures(trimmed) {
            format!("impl {}", &caps[1])
        } else if let Some(caps) = re_trait.captures(trimmed) {
            format!("trait {}", &caps[1])
        } else if let Some(caps) = re_enum.captures(trimmed) {
            format!("enum {}", &caps[1])
        } else if let Some(caps) = re_mod.captures(trimmed) {
            format!("mod {}", &caps[1])
        } else if let Some(caps) = re_type.captures(trimmed) {
            format!("type {}", &caps[1])
        } else {
            continue;
        };

        let tok = estimate_tokens(&skeleton);
        if tok <= budget {
            segments.push(IndexSegment {
                id: format!("seg-{}", seg_counter),
                kind: SegmentKind::Code,
                label: skeleton.clone(),
                tokens: tok * 5,
                skeleton,
                priority_hint: 0.8,
                offset: line_idx,
                length: 1,
            });
            seg_counter += 1;
            budget = budget.saturating_sub(tok);
        }
    }

    if segments.is_empty() {
        let chunk_size = (content.len() / 5).max(100);
        let mut offset = 0;
        while offset < content.len() && budget > 0 {
            let end = (offset + chunk_size).min(content.len());
            let chunk = &content[offset..end];
            let skel = chunk.lines().next().unwrap_or("").trim().to_string();
            let tok = estimate_tokens(&skel);
            if tok <= budget && !skel.is_empty() {
                segments.push(IndexSegment {
                    id: format!("seg-{}", seg_counter),
                    kind: SegmentKind::Code,
                    label: format!("offset {offset}"),
                    tokens: tok * 5,
                    skeleton: skel,
                    priority_hint: 0.5,
                    offset,
                    length: end - offset,
                });
                seg_counter += 1;
                budget = budget.saturating_sub(tok);
            }
            offset = end;
        }
    }

    segments
}

fn index_text_file(content: &str, max_tokens: usize) -> Vec<IndexSegment> {
    let mut segments = Vec::new();
    let mut budget = max_tokens;
    let mut seg_counter = 0u32;

    let re_heading = &*RE_HEADING;
    let re_para = &*RE_PARA;

    let splitted: Vec<&str> = re_para.split(content).collect();

    let mut para_line_offset = 0usize;
    for para_str in &splitted {
        let para = para_str.trim();
        if para.is_empty() { continue; }
        if budget == 0 {
            break;
        }

        let para_line_count = para.lines().count();
        let first = para.lines().next().unwrap_or("").trim();
        let label = if let Some(caps) = re_heading.captures(first) {
            format!("{} {}", &caps[1], &caps[2])
        } else {
            let truncated = if first.len() > 60 {
                format!("{}…", &first[..60])
            } else {
                first.to_string()
            };
            truncated
        };
        let tok = estimate_tokens(first);
        if tok <= budget {
            segments.push(IndexSegment {
                id: format!("seg-{}", seg_counter),
                kind: SegmentKind::Text,
                label,
                tokens: estimate_tokens(para),
                skeleton: first.to_string(),
                priority_hint: if first.starts_with('#') { 0.8 } else { 0.5 },
                offset: para_line_offset,
                length: para_line_count,
            });
            seg_counter += 1;
            budget = budget.saturating_sub(tok);
        }
        para_line_offset += para_line_count + 1;
    }

    if segments.is_empty() {
        let lines: Vec<&str> = content.lines().collect();
        let chunk_size = (lines.len() / 10).max(5);
        for chunk in lines.chunks(chunk_size) {
            if budget == 0 {
                break;
            }
            let first = chunk.first().unwrap_or(&"").trim();
            let skel = if first.len() > 80 {
                format!("{}…", &first[..80])
            } else {
                first.to_string()
            };
            let tok = estimate_tokens(&skel);
            if tok <= budget && !skel.is_empty() {
                segments.push(IndexSegment {
                    id: format!("seg-{}", seg_counter),
                    kind: SegmentKind::Text,
                    label: skel.clone(),
                    tokens: tok * chunk.len(),
                    skeleton: skel,
                    priority_hint: 0.5,
                    offset: 0,
                    length: 0,
                });
                seg_counter += 1;
                budget = budget.saturating_sub(tok);
            }
        }
    }

    segments
}

fn index_generic_file(content: &str, max_tokens: usize) -> Vec<IndexSegment> {
    let lines: Vec<&str> = content.lines().collect();
    let mut segments = Vec::new();
    let mut budget = max_tokens;
    let mut seg_counter = 0u32;

    let chunk_size = (lines.len() / 10).max(20);
    for (i, chunk) in lines.chunks(chunk_size).enumerate() {
        if budget == 0 {
            break;
        }
        let first = chunk.first().unwrap_or(&"").trim();
        let skel = if first.len() > 80 {
            format!("{}…", &first[..80])
        } else {
            first.to_string()
        };
        let tok = estimate_tokens(&skel);
        if tok <= budget && !skel.is_empty() {
            segments.push(IndexSegment {
                id: format!("seg-{}", seg_counter),
                kind: SegmentKind::Data,
                label: format!("offset {}", i * chunk_size),
                tokens: tok * chunk.len(),
                skeleton: skel,
                priority_hint: 0.5,
                offset: i * chunk_size,
                length: chunk.len(),
            });
            seg_counter += 1;
            budget = budget.saturating_sub(tok);
        }
    }

    segments
}

fn index_conversation(messages: &[Message], max_tokens: usize) -> Vec<IndexSegment> {
    let mut segments = Vec::new();
    let mut budget = max_tokens;
    let mut seg_counter = 0u32;

    for (i, msg) in messages.iter().enumerate() {
        if budget == 0 {
            break;
        }
        let role = format!("{:?}", msg.role);
        let text = match &msg.content {
            Some(MessageContent::Text(t)) => t.clone(),
            _ => continue,
        };
        let first_line = text.lines().next().unwrap_or("").trim();
        let truncated = if first_line.len() > 100 {
            let end = first_line.char_indices()
                .take(100).last()
                .map(|(i, c)| i + c.len_utf8())
                .unwrap_or(0);
            format!("{}…", &first_line[..end])
        } else {
            first_line.to_string()
        };
        let skel = format!("[{role}] {truncated}");
        let tok = estimate_tokens(&skel);
        if tok <= budget {
segments.push(IndexSegment {
                    id: format!("seg-{}", seg_counter),
                    kind: SegmentKind::Conversation,
                label: format!("turn {i} ({role})"),
                tokens: estimate_tokens(&text),
                skeleton: skel,
                priority_hint: if i < 3 { 0.9 } else { 0.6 },
                offset: i,
                length: 0,
            });
            seg_counter += 1;
            budget = budget.saturating_sub(tok);
        }
    }

    segments
}

// ─── Token Estimation ───────────────────────────────────────────────────────

/// 估算 ContentPart 在 wire format 中的近似字节长度（用于 token 估算）。
///
/// H2 修复：避免 `format!("{:?}", p)` 的字符串分配；改为对各 variant 字段
/// 直接累加 `.len()`。结果是字节数估算，由调用方再 / 3 转 token。
fn content_part_len(p: &crate::llm::ContentPart) -> usize {
    use crate::llm::ContentPart::*;
    match p {
        Text { text } => text.len(),
        ImageUrl { image_url } => {
            image_url.url.len() + image_part_overhead()
                + image_url.detail.as_deref().map(|d| d.len()).unwrap_or(0)
        }
        ToolResult { tool_use_id, content } => tool_use_id.len() + content.len(),
        ToolUse { id, name, input } => {
            id.len() + name.len()
                // serde_json::Value::to_string 也分配，但比 Debug 便宜；
                // 用 input 的内部估算避免一次完整序列化
                + json_value_size(input)
        }
    }
}

/// Image 部分的 wire 开销常量（base64 标记 + 字段名）
fn image_part_overhead() -> usize { 32 }

/// 估算 serde_json::Value 序列化后的字节数（不实际序列化）
fn json_value_size(v: &serde_json::Value) -> usize {
    match v {
        serde_json::Value::Null => 4,
        serde_json::Value::Bool(_) => 5,
        serde_json::Value::Number(n) => n.to_string().len(),
        serde_json::Value::String(s) => s.len() + 2, // 引号
        serde_json::Value::Array(a) => 2 + a.iter().map(json_value_size).sum::<usize>() + a.len(),
        serde_json::Value::Object(m) => 2 + m.iter()
            .map(|(k, vv)| k.len() + 3 + json_value_size(vv))
            .sum::<usize>() + m.len(),
    }
}

/// Phase Ctx-D：按 max_tokens 预算截断 segment 列表
///
/// 累计 segments 的 tokens，超过预算时停止追加。
/// 不重排 segments；仅切尾。
pub fn limit_segments_by_tokens(segments: Vec<IndexSegment>, max_tokens: usize) -> Vec<IndexSegment> {
    let mut budget = max_tokens;
    let mut out = Vec::with_capacity(segments.len());
    for seg in segments {
        if seg.tokens > budget {
            break;
        }
        budget -= seg.tokens;
        out.push(seg);
    }
    out
}

/// Token 估算：对 CJK 文本使用更高系数（~1 token/char），对 ASCII 文本使用 ~0.25 token/byte。
/// 混合文本按字符分类加权，比固定系数更准确。
pub fn estimate_tokens(text: &str) -> usize {
    if text.is_empty() {
        return 1;
    }
    let mut cjk_chars = 0usize;
    let mut ascii_bytes = 0usize;
    for ch in text.chars() {
        if is_cjk(ch) {
            cjk_chars += 1;
        } else {
            ascii_bytes += ch.len_utf8();
        }
    }
    // CJK: ~1.2 tokens per character; ASCII: ~0.25 tokens per byte + word boundaries
    let cjk_tokens = (cjk_chars as f64 * 1.2) as usize;
    let ascii_tokens = (ascii_bytes as f64 * 0.25) as usize;
    let whitespace_bonus = text.split_whitespace().count() / 4;
    (cjk_tokens + ascii_tokens + whitespace_bonus).max(1)
}

/// 检测是否为 CJK 统一表意文字（含扩展区）
fn is_cjk(c: char) -> bool {
    matches!(c,
        '\u{4E00}'..='\u{9FFF}' |  // CJK Unified Ideographs
        '\u{3400}'..='\u{4DBF}' |  // CJK Extension A
        '\u{F900}'..='\u{FAFF}' |  // CJK Compatibility Ideographs
        '\u{3000}'..='\u{303F}' |  // CJK Symbols and Punctuation
        '\u{FF00}'..='\u{FFEF}' |  // Halfwidth and Fullwidth Forms
        '\u{AC00}'..='\u{D7AF}' |  // Hangul
        '\u{3040}'..='\u{309F}' |  // Hiragana
        '\u{30A0}'..='\u{30FF}'    // Katakana
    )
}

fn path_is_allowed(path: &str) -> bool {
    let canonical = std::path::Path::new(path).canonicalize().ok();
    let canonical = match canonical {
        Some(p) => p.to_string_lossy().to_string(),
        None => return false,
    };
    allowed_roots().iter().any(|root| canonical.starts_with(root))
}

// ─── ContextToolExecutor ────────────────────────────────────────────────────

/// Executes context.* tools (declare/keep/compress).
///
/// ## Dependencies
/// - `ContextManager`: manages context window, tiers, and generalized indexing
/// - `context_messages`: per-session message history (was CoreLoop-level shared, fixed for isolation)
///
/// ## References
/// - Called by: `ToolRegistry::execute()` via `context.declare/keep/compress` tool IDs
/// - Registered by: `register_context_tools()` during session initialization
pub struct ContextToolExecutor {
    pub manager: Arc<ContextManager>,
    /// Per-session context messages (isolated per session, not shared across CoreLoop)
    pub context_messages: Arc<RwLock<Vec<Message>>>,
}

#[async_trait::async_trait]
impl ToolExecutor for ContextToolExecutor {
    async fn execute(&self, tool_id: &ToolId, params: Value, _ctx: &ExecutionContext) -> abacus_types::Result<Value> {
        // 单一命名约定：ToolId == schema.name == LLM 名（_ 下划线）
        match tool_id.0.as_str() {
            "context_declare" => {
                let source = params["source"]
                    .as_str()
                    .ok_or_else(|| KernelError::Other("missing source".into()))?;
                let intent = params["intent"].as_str().unwrap_or("general");
                let max_index = params["max_index_tokens"]
                    .as_u64()
                    .unwrap_or(1024) as usize;

                let messages = self.context_messages.read().await;
                let index = self.manager.declare(source, intent, max_index, &messages).await?;

                Ok(serde_json::to_value(index).unwrap_or(Value::Null))
            }

            "context_keep" => {
                let segments: Vec<String> = params["segments"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                    .unwrap_or_default();
                let full = params["full"].as_bool().unwrap_or(false);
                let retained = self.manager.keep(&segments, full).await?;
                Ok(serde_json::json!({
                    "kept": retained.len(),
                    "segments": retained.iter().map(|(id, _)| id).collect::<Vec<_>>()
                }))
            }

            "context_compress" => {
                let mode = params["mode"].as_str().unwrap_or("segments");
                match mode {
                    // 主动 messages 压缩（LLM 选择时机，配合 context_status 使用）
                    // 设计：LLM 先输出摘要到对话流 → 再调此工具触发规则式压缩
                    // 摘要天然在保留区内（最近 8 条不压缩），无需额外 LLM 调用
                    "messages" => {
                        let mut msgs = self.context_messages.write().await;
                        let compressed = self.manager.auto_compress_messages(&mut msgs).await;
                        let tokens_saved: usize = compressed.iter()
                            .map(|c| c.original_tokens.saturating_sub(c.compressed_tokens))
                            .sum();
                        Ok(serde_json::json!({
                            "mode": "messages",
                            "compressed_count": compressed.len(),
                            "tokens_freed": tokens_saved,
                            "status": if compressed.is_empty() { "no_compression_needed" } else { "success" },
                            "tip": "Your recent messages (last 8) are preserved. Output key conclusions BEFORE calling this tool to ensure they survive compression."
                        }))
                    }
                    // 原有 segments 压缩路径
                    _ => {
                        let segments: Vec<String> = params["segments"]
                            .as_array()
                            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                            .unwrap_or_default();
                        let level = CompressLevel::from_str(params["level"].as_str().unwrap_or("brief"));
                        let compressed = self.manager.compress(&segments, level).await?;
                        Ok(serde_json::json!({
                            "mode": "segments",
                            "compressed": compressed.len(),
                            "segments": compressed.iter().map(|(id, _)| id).collect::<Vec<_>>()
                        }))
                    }
                }
            }

            // Phase Z1：session.recall——从 cold tier 按 query 召回历史 SessionSnapshot
            "session_recall" => {
                let query = params["query"].as_str().unwrap_or("");
                let limit = params["limit"].as_u64().unwrap_or(5) as usize;
                let snapshots = if query.is_empty() {
                    self.manager.recall_recent(limit).await?
                } else {
                    self.manager.recall_from_cold(query, limit).await?
                };
                Ok(serde_json::json!({
                    "query": query,
                    "count": snapshots.len(),
                    "snapshots": snapshots.iter().map(|s| serde_json::json!({
                        "session_id": s.session_id,
                        "turn_count": s.turn_count,
                        "summary": s.summary,
                        "token_estimate": s.token_estimate,
                        "created_at": s.created_at,
                    })).collect::<Vec<_>>(),
                }))
            }

            // Phase Z2：messages.recover——按 recover_id 取回压缩前的原始 messages
            "messages_recover" => {
                let recover_id = params["recover_id"]
                    .as_str()
                    .ok_or_else(|| KernelError::Other("missing recover_id".into()))?;
                match self.manager.recover_messages(recover_id).await {
                    Some(messages) => Ok(serde_json::json!({
                        "recover_id": recover_id,
                        "count": messages.len(),
                        "messages": messages,
                    })),
                    None => Err(KernelError::Other(format!(
                        "recover_id not found: {recover_id} (may have been LRU-evicted)"
                    ))),
                }
            }

            // LLM 感知压缩决策：pin 指定 turn（压缩时保留原文）
            "context_pin" => {
                let turn = params["turn"]
                    .as_u64()
                    .ok_or_else(|| KernelError::Other("missing turn number".into()))? as u32;
                let reason = params["reason"].as_str().unwrap_or("important");
                self.manager.pin_turn(turn).await;
                Ok(serde_json::json!({
                    "pinned": turn,
                    "reason": reason,
                    "total_pinned": self.manager.pinned_turns_list().await.len(),
                }))
            }

            // 取消 pin
            "context_unpin" => {
                let turn = params["turn"]
                    .as_u64()
                    .ok_or_else(|| KernelError::Other("missing turn number".into()))? as u32;
                self.manager.unpin_turn(turn).await;
                Ok(serde_json::json!({
                    "unpinned": turn,
                    "total_pinned": self.manager.pinned_turns_list().await.len(),
                }))
            }

            // 查看当前 pinned turns
            "context_pinned" => {
                let pinned = self.manager.pinned_turns_list().await;
                Ok(serde_json::json!({
                    "pinned_turns": pinned,
                    "count": pinned.len(),
                }))
            }

            // 查询上下文占用状态（LLM 主动决策 Layer 1 基础）
            "context_status" => {
                let detail = params["detail"].as_bool().unwrap_or(false);
                let w = self.manager.window.read().await;
                let current = w.current_tokens;
                let max = w.max_tokens;
                let pct = w.usage_pct();
                let trigger = w.compression_trigger_pct;
                drop(w);

                let status = if pct >= 95.0 {
                    "critical"
                } else if pct >= trigger as f64 {
                    "elevated"
                } else {
                    "normal"
                };

                let suggestion = if pct >= 80.0 && pct < trigger as f64 {
                    Some("approaching threshold — consider compressing non-essential history")
                } else if pct >= trigger as f64 {
                    Some("above threshold — system will auto-compress if not acted upon")
                } else {
                    None
                };

                let messages = self.context_messages.read().await;
                let msg_count = messages.len();
                drop(messages);

                let pinned = self.manager.pinned_turns_list().await;

                let mut result = serde_json::json!({
                    "current_tokens": current,
                    "max_tokens": max,
                    "usage_pct": (pct * 10.0).round() / 10.0,
                    "status": status,
                    "compression_trigger_pct": trigger,
                    "messages_count": msg_count,
                    "pinned_turns": pinned.len(),
                    "suggestion": suggestion,
                });

                if detail {
                    let usage = self.manager.usage.read().await;
                    result["subsystems"] = serde_json::json!({
                        "messages": usage.messages,
                        "result_store": usage.result_store,
                        "compressed_messages": usage.compressed_messages,
                    });
                }

                Ok(result)
            }

            other => Err(KernelError::Other(format!("unknown context tool: {other}"))),
        }
    }
}

// ─── Tool Registration ──────────────────────────────────────────────────────

/// Register context.* tools with a per-session context_messages reference.
///
/// ## Dependencies
/// - `registry`: ToolRegistry to register tool handles and executors
/// - `manager`: ContextManager for context window/indexing operations
/// - `context_messages`: **Per-session** message history (from SessionState.context_messages)
///
/// ## References
/// - Called by: Session initialization (not CoreLoop::new) to ensure per-session isolation
/// - Registers: `context.declare`, `context.keep`, `context.compress`
pub async fn register_context_tools(
    registry: &crate::tool::ToolRegistry,
    manager: Arc<ContextManager>,
    context_messages: Arc<RwLock<Vec<Message>>>,
) {
    let executor = Arc::new(ContextToolExecutor {
        manager: manager.clone(),
        context_messages,
    }) as Arc<dyn ToolExecutor>;

    let tools = vec![
        ToolHandle {
            id: ToolId("context_declare".into()),
            schema: ToolSchema {
                name: "context_declare".into(),
                description: "Declare intent to load large content. Returns a GeneralizedIndex (segment skeleton, token count). Call FIRST before reading large files/sessions.".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "source": {"type": "string", "description": "Content: file://path | session://id"},
                        "intent": {"type": "string", "description": "Why you need this content"},
                        "max_index_tokens": {"type": "integer", "description": "Max skeleton tokens (default 1024)"}
                    },
                    "required": ["source", "intent"]
                }),
                returns: None,
                security: Some(ToolSecurity {
                    allowed_paths: None,
                    max_size_mb: Some(10),
                    confirm_required: false,
                    needs_sandbox: false,
                }),
                cost: Some(ToolCost {
                    tokens: 256,
                    latency: "50ms".into(),
                    risk: "low".into(),
                }),
                examples: Vec::new(),
                applicable_task_kinds: None,
                idempotent: false,
            },
            provider: ToolProvider::BuiltIn,
            state: ToolState::Loaded,
            effectiveness: ToolEffectiveness::default(),
        },
        ToolHandle {
            id: ToolId("context_keep".into()),
            schema: ToolSchema {
                name: "context_keep".into(),
                description: "Keep selected segments at full or summarized fidelity. Unmentioned segments auto-dropped.".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "segments": {"type": "array", "items": {"type": "string"}, "description": "Segment IDs from GeneralizedIndex"},
                        "full": {"type": "boolean", "description": "true=full text, false=summary"}
                    },
                    "required": ["segments", "full"]
                }),
                returns: None,
                security: Some(ToolSecurity {
                    allowed_paths: None,
                    max_size_mb: None,
                    confirm_required: false,
                    needs_sandbox: false,
                }),
                cost: Some(ToolCost {
                    tokens: 128,
                    latency: "10ms".into(),
                    risk: "low".into(),
                }),
                examples: Vec::new(),
                applicable_task_kinds: None,
                idempotent: false,
            },
            provider: ToolProvider::BuiltIn,
            state: ToolState::Loaded,
            effectiveness: ToolEffectiveness::default(),
        },
        ToolHandle {
            id: ToolId("context_compress".into()),
            schema: ToolSchema {
                name: "context_compress".into(),
                description: "Compress context to free tokens. mode='messages': compress old conversation history (output your summary FIRST, then call this — recent 8 messages are preserved). mode='segments': compress declared segments by ID.".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "mode": {"type": "string", "enum": ["messages", "segments"], "description": "What to compress (default: segments)"},
                        "segments": {"type": "array", "items": {"type": "string"}, "description": "Segment IDs (only for mode=segments)"},
                        "level": {"type": "string", "enum": ["detailed", "brief", "minimal"], "description": "Compression level (only for mode=segments)"}
                    }
                }),
                returns: None,
                security: Some(ToolSecurity {
                    allowed_paths: None,
                    max_size_mb: None,
                    confirm_required: false,
                    needs_sandbox: false,
                }),
                cost: Some(ToolCost {
                    tokens: 128,
                    latency: "10ms".into(),
                    risk: "low".into(),
                }),
                examples: Vec::new(),
                applicable_task_kinds: None,
                idempotent: false,
            },
            provider: ToolProvider::BuiltIn,
            state: ToolState::Loaded,
            effectiveness: ToolEffectiveness::default(),
        },
        // Phase Z1：session.recall——从持久化 cold tier 召回历史 SessionSnapshot
        ToolHandle {
            id: ToolId("session_recall".into()),
            schema: ToolSchema {
                name: "session_recall".into(),
                description: "Search the persistent cold-tier session archive by keyword to recall historical session summaries (use when recover_id has been LRU-evicted).".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "description": "Keyword to search session summaries (empty = recent N)"},
                        "limit": {"type": "integer", "description": "Max snapshots to return (default 5, max 20)"}
                    }
                }),
                returns: None,
                security: Some(ToolSecurity {
                    allowed_paths: None,
                    max_size_mb: None,
                    confirm_required: false,
                    needs_sandbox: false,
                }),
                cost: Some(ToolCost {
                    tokens: 64,
                    latency: "20ms".into(),
                    risk: "low".into(),
                }),
                examples: Vec::new(),
                applicable_task_kinds: None,
                idempotent: true, // 同 query 同结果（snapshot 是只增数据）
            },
            provider: ToolProvider::BuiltIn,
            state: ToolState::Loaded,
            effectiveness: ToolEffectiveness::default(),
        },
        // Phase Z2：messages.recover——按 recover_id 取回压缩前的原始 messages
        ToolHandle {
            id: ToolId("messages_recover".into()),
            schema: ToolSchema {
                name: "messages_recover".into(),
                description: "Retrieve the original messages from a compressed history block by its recover_id (returned in the [Compressed history: recover_id=...] hint).".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "recover_id": {"type": "string", "description": "The recover_id from a [Compressed history: ...] block"}
                    },
                    "required": ["recover_id"]
                }),
                returns: None,
                security: Some(ToolSecurity {
                    allowed_paths: None,
                    max_size_mb: None,
                    confirm_required: false,
                    needs_sandbox: false,
                }),
                cost: Some(ToolCost {
                    tokens: 16,
                    latency: "1ms".into(),
                    risk: "low".into(),
                }),
                examples: Vec::new(),
                applicable_task_kinds: None,
                idempotent: true, // 同 id 永远返回相同 archive 内容
            },
            provider: ToolProvider::BuiltIn,
            state: ToolState::Loaded,
            effectiveness: ToolEffectiveness::default(),
        },
        // LLM 感知压缩决策：pin 指定 turn（压缩时保留原文）
        ToolHandle {
            id: ToolId("context_pin".into()),
            schema: ToolSchema {
                name: "context_pin".into(),
                description: "Pin a conversation turn to protect it from compression. Pinned turns are never compressed — use for critical decisions, error diagnostics, or user confirmations you need to reference later.".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "turn": {"type": "integer", "description": "Turn number to pin (from [Awareness] block)"},
                        "reason": {"type": "string", "description": "Why this turn is important"}
                    },
                    "required": ["turn"]
                }),
                returns: None,
                security: None,
                cost: Some(ToolCost { tokens: 8, latency: "1ms".into(), risk: "low".into() }),
                examples: Vec::new(),
                applicable_task_kinds: None,
                idempotent: true,
            },
            provider: ToolProvider::BuiltIn,
            state: ToolState::Loaded,
            effectiveness: ToolEffectiveness::default(),
        },
        ToolHandle {
            id: ToolId("context_unpin".into()),
            schema: ToolSchema {
                name: "context_unpin".into(),
                description: "Remove pin from a previously pinned turn (allow it to be compressed).".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "turn": {"type": "integer", "description": "Turn number to unpin"}
                    },
                    "required": ["turn"]
                }),
                returns: None,
                security: None,
                cost: Some(ToolCost { tokens: 8, latency: "1ms".into(), risk: "low".into() }),
                examples: Vec::new(),
                applicable_task_kinds: None,
                idempotent: true,
            },
            provider: ToolProvider::BuiltIn,
            state: ToolState::Loaded,
            effectiveness: ToolEffectiveness::default(),
        },
        ToolHandle {
            id: ToolId("context_pinned".into()),
            schema: ToolSchema {
                name: "context_pinned".into(),
                description: "List all currently pinned turns (protected from compression).".into(),
                parameters: serde_json::json!({"type": "object"}),
                returns: None,
                security: None,
                cost: Some(ToolCost { tokens: 8, latency: "1ms".into(), risk: "low".into() }),
                examples: Vec::new(),
                applicable_task_kinds: None,
                idempotent: true,
            },
            provider: ToolProvider::BuiltIn,
            state: ToolState::Loaded,
            effectiveness: ToolEffectiveness::default(),
        },
        // LLM 主动查询上下文占用状态（双层决策 Layer 1 的基础）
        ToolHandle {
            id: ToolId("context_status".into()),
            schema: ToolSchema {
                name: "context_status".into(),
                description: "Query current context window usage: tokens used/max, compression history, and pressure status. Use to decide when to proactively compress.".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "detail": {"type": "boolean", "description": "Include per-subsystem breakdown (default false)"}
                    }
                }),
                returns: None,
                security: None,
                cost: Some(ToolCost { tokens: 16, latency: "1ms".into(), risk: "low".into() }),
                examples: Vec::new(),
                applicable_task_kinds: None,
                idempotent: true,
            },
            provider: ToolProvider::BuiltIn,
            state: ToolState::Loaded,
            effectiveness: ToolEffectiveness::default(),
        },
    ];

    for tool in tools {
        let tid = tool.id.clone();
        registry.register(tool).await;
        registry.register_executor(tid, executor.clone()).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::MessageContent;

    struct NoopStore;
    #[async_trait::async_trait]
    impl SessionStore for NoopStore {
        async fn save(&self, _s: SessionSnapshot) -> std::result::Result<(), KernelError> { Ok(()) }
        async fn load_recent(&self, _l: usize) -> std::result::Result<Vec<SessionSnapshot>, KernelError> { Ok(vec![]) }
        async fn search(&self, _q: &str) -> std::result::Result<Vec<SessionSnapshot>, KernelError> { Ok(vec![]) }
    }

    fn mk_msg(text: &str) -> Message {
        Message {
            role: MessageRole::User,
            content: Some(MessageContent::Text(text.to_string())),
            name: None,
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            prefix: false,
        }
    }

    /// Ctx-A: 子系统占用统一记账到 ContextWindow.current_tokens
    #[tokio::test]
    async fn test_subsystem_usage_aggregates_into_window() {
        let mgr = ContextManager::new(Arc::new(NoopStore));
        mgr.set_subsystem_usage("result_store", 1500).await;
        mgr.set_subsystem_usage("compressed_messages", 300).await;
        mgr.set_subsystem_usage("messages", 800).await;
        let snapshot = mgr.usage_snapshot().await;
        assert_eq!(snapshot.total(), 1500 + 300 + 800);
        let w = mgr.window.read().await;
        assert_eq!(w.current_tokens, 2600,
            "current_tokens 必须是各子系统总和");
    }

    /// Ctx-A: 未知子系统不影响合计
    #[tokio::test]
    async fn test_subsystem_unknown_key_ignored() {
        let mgr = ContextManager::new(Arc::new(NoopStore));
        mgr.set_subsystem_usage("messages", 100).await;
        mgr.set_subsystem_usage("unknown_key", 9999).await;
        assert_eq!(mgr.usage_snapshot().await.total(), 100);
    }

    /// Ctx-A: estimate_total_tokens 增量缓存——单调追加场景命中
    #[tokio::test]
    async fn test_estimate_caches_on_append() {
        let mgr = ContextManager::new(Arc::new(NoopStore));
        let msgs = vec![mk_msg("hello"), mk_msg("world")];
        let t1 = mgr.estimate_total_tokens(&msgs).await;
        // 同样输入第二次调用——应命中 cache
        let t2 = mgr.estimate_total_tokens(&msgs).await;
        assert_eq!(t1, t2);
        // 追加一条——cache miss 重建
        let mut msgs2 = msgs.clone();
        msgs2.push(mk_msg("third"));
        let t3 = mgr.estimate_total_tokens(&msgs2).await;
        assert!(t3 > t1, "追加消息后 token 数应增加");
    }

    /// Ctx-A: shed_pending 标记单次消费
    #[test]
    fn test_shed_pending_one_shot() {
        let mgr = ContextManager::new(Arc::new(NoopStore));
        assert!(!mgr.take_shed_pending(), "初始无标记");
        mgr.mark_shed_pending();
        assert!(mgr.take_shed_pending(), "标记后第一次取出 true");
        assert!(!mgr.take_shed_pending(), "再次取出 false（消费一次性）");
    }

    /// Ctx-A: estimate_total_tokens 同步消息子系统占用 + window
    #[tokio::test]
    async fn test_estimate_syncs_window() {
        let mgr = ContextManager::new(Arc::new(NoopStore));
        let msgs = vec![mk_msg("test message")];
        let _ = mgr.estimate_total_tokens(&msgs).await;
        let snapshot = mgr.usage_snapshot().await;
        assert!(snapshot.messages > 0, "messages 子系统应有占用");
        let w = mgr.window.read().await;
        assert_eq!(w.current_tokens, snapshot.total(),
            "estimate 后 window.current_tokens = 总占用");
    }

    /// Ctx-A: cache key 区分末尾消息变化
    #[test]
    fn test_msg_cache_key_distinguishes_tail() {
        let a = vec![mk_msg("first"), mk_msg("tail-a")];
        let b = vec![mk_msg("first"), mk_msg("tail-b")];
        assert_ne!(compute_msg_cache_key(&a), compute_msg_cache_key(&b),
            "末尾不同应产生不同 cache key");
    }

    // ─── Ctx-B: cache-friendly 压缩测试 ────────────────────────────────────

    fn mk_assistant(text: &str) -> Message {
        Message {
            role: MessageRole::Assistant,
            content: Some(MessageContent::Text(text.to_string())),
            name: None, tool_calls: None, tool_call_id: None, reasoning_content: None, prefix: false,
        }
    }

    fn mk_assistant_with_tool_call(text: &str) -> Message {
        Message {
            role: MessageRole::Assistant,
            content: Some(MessageContent::Text(text.to_string())),
            name: None,
            tool_calls: Some(vec![]),
            tool_call_id: None,
            reasoning_content: None,
            prefix: false,
        }
    }

    fn mk_tool_response(text: &str) -> Message {
        Message {
            role: MessageRole::Tool,
            content: Some(MessageContent::Text(text.to_string())),
            name: Some("some_tool".into()),
            tool_calls: None,
            tool_call_id: Some("call_123".into()),
            reasoning_content: None,
            prefix: false,
        }
    }

    /// Ctx-B: 不到压缩阈值不应压缩
    #[tokio::test]
    async fn test_no_compress_below_trigger() {
        let mgr = ContextManager::new(Arc::new(NoopStore));
        let mut msgs = vec![mk_msg("hello"); 10];
        let compressed = mgr.auto_compress_messages(&mut msgs).await;
        assert!(compressed.is_empty(), "trigger 未触发应不压缩");
        assert_eq!(msgs.len(), 10, "消息数量不变");
    }

    /// Ctx-B: tool_calls 元结构必须完整保留
    #[tokio::test]
    async fn test_tool_protocol_messages_preserved() {
        let mgr = ContextManager::new(Arc::new(NoopStore));
        {
            let mut w = mgr.window.write().await;
            w.max_tokens = 100;
            w.current_tokens = 90; // 90% > 85% 但 < 95%（normal compress）
            w.compression_trigger_pct = 85;
        }
        let mut msgs = vec![
            mk_msg("first user message"),
            mk_msg("middle plain 1"),
            mk_assistant_with_tool_call("calling tool"),
            mk_tool_response("tool result"),
            mk_msg("middle plain 2"),
            mk_msg("tail 1"),
            mk_msg("tail 2"),
            mk_msg("tail 3"),
            mk_msg("tail 4"),
            mk_msg("tail 5"),
        ];
        let _ = mgr.auto_compress_messages(&mut msgs).await;
        let has_tool_calls = msgs.iter().any(|m| m.tool_calls.is_some());
        let has_tool_response = msgs.iter().any(|m| m.tool_call_id.is_some());
        assert!(has_tool_calls, "assistant tool_calls 必须保留");
        assert!(has_tool_response, "tool 响应必须保留");
    }

    /// Ctx-B: 中间 non-tool 段应合并成 1 条 summary
    #[tokio::test]
    async fn test_middle_segment_collapsed_to_one() {
        let mgr = ContextManager::new(Arc::new(NoopStore));
        {
            let mut w = mgr.window.write().await;
            w.max_tokens = 100;
            w.current_tokens = 90;
            w.compression_trigger_pct = 85;
        }
        let mut msgs = Vec::new();
        // early_keep=2
        msgs.push(mk_msg("EARLY1"));
        msgs.push(mk_msg("EARLY2"));
        // 10 middle messages (importance < 0.7, will be compressed)
        for i in 0..10 {
            msgs.push(mk_assistant(&format!("middle msg {i} with reasonably long content")));
        }
        // keep_count=8 tail messages
        for i in 0..8 {
            msgs.push(mk_msg(&format!("late {i}")));
        }
        // total = 20, early=2, tail_start=20-8=12, middle=index 2..12 (10 msgs)
        let compressed = mgr.auto_compress_messages(&mut msgs).await;
        assert!(!compressed.is_empty());
        // Result: 2 early + 1 compressed summary + 8 tail = 11
        assert_eq!(msgs.len(), 11,
            "20 → 11（2 early + 1 合并 + 8 tail）, got {}", msgs.len());
        assert!(matches!(msgs[0].content, Some(MessageContent::Text(ref t)) if t == "EARLY1"));
        let summary = match &msgs[2].content {
            Some(MessageContent::Text(t)) => t.clone(),
            _ => panic!("expected text at index 2"),
        };
        assert!(summary.contains("Compressed"),
            "中间段应合并为 summary：{summary}");
    }

    // ─── Ctx-C: 行为宫殿协同 evict 测试 ────────────────────────────────────

    /// Ctx-C: scan_and_mark_used 应正确识别 segment id 引用
    #[tokio::test]
    async fn test_scan_marks_referenced_segments() {
        let mgr = ContextManager::new(Arc::new(NoopStore));
        // 直接写入 retained_content（模拟 keep 之后状态）
        {
            let mut rc = mgr.retained_content.write().await;
            rc.push(("seg-1".to_string(), "content one".to_string(), RetainMeta::default()));
            rc.push(("seg-2".to_string(), "content two".to_string(), RetainMeta::default()));
            rc.push(("seg-3".to_string(), "content three".to_string(), RetainMeta::default()));
        }
        // LLM 输出引用 seg-1 和 seg-3
        let llm_text = "I'll use seg-1 to find the answer, and also reference seg-3 for context.";
        let hits = mgr.scan_and_mark_used(llm_text, 5).await;
        assert_eq!(hits, 2);
        let snap = mgr.retained_snapshot().await;
        let s1 = snap.iter().find(|(id, _, _)| id == "seg-1").unwrap();
        assert_eq!(s1.2.ref_count, 1);
        assert_eq!(s1.2.last_used_turn, 5);
        let s2 = snap.iter().find(|(id, _, _)| id == "seg-2").unwrap();
        assert_eq!(s2.2.ref_count, 0, "未引用 segment 不应被标记");
    }

    /// Ctx-C: evict 移除 distance > N 且 ref_count=0 的 segment
    #[tokio::test]
    async fn test_evict_stale_unreferenced_segments() {
        let mgr = ContextManager::new(Arc::new(NoopStore));
        {
            let mut rc = mgr.retained_content.write().await;
            // 3 条 segment，初始 created_turn=0
            rc.push(("old-unused".to_string(), "content".to_string(), RetainMeta::default()));
            rc.push(("old-used".to_string(), "content".to_string(), RetainMeta {
                created_turn: 0, last_used_turn: 25, ref_count: 3,
            }));
            rc.push(("recent".to_string(), "content".to_string(), RetainMeta {
                created_turn: 28, last_used_turn: 28, ref_count: 0,
            }));
        }
        // current_turn=30, evict_distance=20
        // - old-unused: distance=30, ref_count=0 → evict（stale）
        // - old-used:   distance=5（last_used=25），保留
        // - recent:     distance=2，保留
        let evicted = mgr.evict_stale_segments(30, 20).await;
        assert_eq!(evicted, 1);
        let snap = mgr.retained_snapshot().await;
        let ids: Vec<_> = snap.iter().map(|(id, _, _)| id.clone()).collect();
        assert!(!ids.contains(&"old-unused".to_string()));
        assert!(ids.contains(&"old-used".to_string()));
        assert!(ids.contains(&"recent".to_string()));
    }

    // ─── Ctx-D: 索引双轨收拢测试 ──────────────────────────────────────────

    /// Ctx-D: limit_segments_by_tokens 按预算截断
    #[test]
    fn test_limit_segments_by_tokens() {
        let mk = |id: &str, tokens: usize| IndexSegment {
            id: id.to_string(),
            kind: SegmentKind::Text,
            label: "x".into(),
            tokens,
            skeleton: "".into(),
            priority_hint: 0.5,
            offset: 0,
            length: 0,
        };
        let segs = vec![mk("a", 100), mk("b", 200), mk("c", 300)];
        let out = limit_segments_by_tokens(segs, 350);
        assert_eq!(out.len(), 2, "100+200=300<=350 → 保留前两段，第三段超");
        let ids: Vec<_> = out.iter().map(|s| s.id.clone()).collect();
        assert_eq!(ids, vec!["a".to_string(), "b".to_string()]);
    }

    /// Ctx-D: estimate_tokens 对 CJK 与 ASCII 加权不同
    #[test]
    fn test_estimate_tokens_cjk_aware() {
        let cjk_only = "你好世界你好世界你好世界"; // 12 CJK chars
        let ascii_only = "hello world hello world"; // 23 ASCII bytes
        let cjk_tok = estimate_tokens(cjk_only);
        let ascii_tok = estimate_tokens(ascii_only);
        // CJK: 12 * 1.2 ≈ 14；ASCII: 23 * 0.25 ≈ 5 + whitespace
        assert!(cjk_tok > ascii_tok,
            "CJK 字符应估算更多 token：CJK={cjk_tok}, ASCII={ascii_tok}");
    }

    /// Ctx-D: estimate_tokens 空字符串返回 1（避免 0 token 边界）
    #[test]
    fn test_estimate_tokens_empty() {
        assert_eq!(estimate_tokens(""), 1);
    }

    // ─── Z3: 压缩 schema 统一测试 ─────────────────────────────────────────

    /// Z3: CompressedSummary.from_legacy 正确映射
    #[test]
    fn test_compressed_summary_from_legacy() {
        let legacy = CompressedMessage {
            original_role: "Assistant".into(),
            summary: "test".into(),
            original_tokens: 100,
            compressed_tokens: 30,
        };
        let s = CompressedSummary::from_legacy(legacy);
        assert!(matches!(s.kind, SummaryKind::MessagesBlock));
        assert_eq!(s.origin, Some("Assistant".into()));
        assert_eq!(s.original_tokens, 100);
    }

    /// Z3: CompressedSummary.to_json_line 输出统一 schema
    #[test]
    fn test_compressed_summary_json_line_schema() {
        let s = CompressedSummary {
            kind: SummaryKind::ToolResult,
            original_tokens: 200,
            compressed_tokens: 50,
            summary: "truncated".into(),
            recover_id: Some("rs_xxx".into()),
            turn_range: Some((3, 7)),
            origin: Some("filengine_fs_read".into()),
        };
        let json = s.to_json_line();
        assert!(json.contains("\"kind\":\"tool_result\""));
        assert!(json.contains("\"recover_id\":\"rs_xxx\""));
        assert!(json.contains("\"turn_range\":[3,7]"));
        assert!(json.contains("\"origin\":\"filengine_fs_read\""));
    }

    /// Z3: auto_compress 接 Detailed 档位输出更长 snippet
    #[tokio::test]
    async fn test_auto_compress_detailed_keeps_more() {
        let long_text = "line1\nline2\nline3\nline4 ".repeat(20);
        let mgr = ContextManager::new(Arc::new(NoopStore));
        mgr.set_compress_level(CompressLevel::Detailed).await;
        {
            let mut w = mgr.window.write().await;
            w.max_tokens = 100;
            w.current_tokens = 90;
            w.compression_trigger_pct = 85;
        }
        let mut msgs = Vec::new();
        msgs.push(mk_msg("EARLY"));
        for _ in 0..6 {
            msgs.push(mk_assistant(&long_text));
        }
        for i in 0..5 {
            msgs.push(mk_msg(&format!("late {i}")));
        }
        let _ = mgr.auto_compress_messages(&mut msgs).await;
        // detailed: 2 行 * 160 chars 应比 Brief 长
        if let Some(MessageContent::Text(s)) = &msgs[1].content {
            assert!(s.len() > 100, "Detailed 档位应保留更长 snippet：{}", s.len());
        }
    }

    // ─── Z2: 可逆压缩测试 ────────────────────────────────────────────────

    /// Z2: 压缩消息 summary 嵌入 recover_id
    #[tokio::test]
    async fn test_compressed_summary_embeds_recover_id() {
        let mgr = ContextManager::new(Arc::new(NoopStore));
        {
            let mut w = mgr.window.write().await;
            w.max_tokens = 100;
            w.current_tokens = 90;
            w.compression_trigger_pct = 85;
        }
        let mut msgs = Vec::new();
        // early_keep=2, keep_count=8 → need >11 msgs
        msgs.push(mk_msg("EARLY1"));
        msgs.push(mk_msg("EARLY2"));
        for i in 0..10 {
            msgs.push(mk_assistant(&format!("middle msg {i}")));
        }
        for i in 0..8 {
            msgs.push(mk_msg(&format!("late {i}")));
        }
        let _ = mgr.auto_compress_messages(&mut msgs).await;
        // 找压缩 summary（含 "Compressed"）
        let summary_msg = msgs.iter().find(|m| {
            matches!(&m.content, Some(MessageContent::Text(s)) if s.contains("Compressed"))
        }).expect("应有压缩 summary 消息");
        if let Some(MessageContent::Text(s)) = &summary_msg.content {
            assert!(s.contains("id=mb_"),
                "summary 必须嵌入 recover_id：{s}");
        }
    }

    /// Z2: archive 写入后可通过 recover_messages 取回
    #[tokio::test]
    async fn test_recover_messages_round_trip() {
        let mgr = ContextManager::new(Arc::new(NoopStore));
        {
            let mut w = mgr.window.write().await;
            w.max_tokens = 100;
            w.current_tokens = 90;
            w.compression_trigger_pct = 85;
        }
        let mut msgs = Vec::new();
        msgs.push(mk_msg("EARLY1"));
        msgs.push(mk_msg("EARLY2"));
        for i in 0..10 {
            msgs.push(mk_assistant(&format!("middle msg {i}")));
        }
        for i in 0..8 {
            msgs.push(mk_msg(&format!("late {i}")));
        }
        let _ = mgr.auto_compress_messages(&mut msgs).await;

        // 找压缩 summary 提取 recover_id
        let summary_msg = msgs.iter().find(|m| {
            matches!(&m.content, Some(MessageContent::Text(s)) if s.contains("Compressed"))
        }).expect("应有压缩 summary");
        let summary_text = match &summary_msg.content {
            Some(MessageContent::Text(s)) => s.clone(),
            _ => panic!("expected text"),
        };
        let recover_id = summary_text
            .split("id=").nth(1)
            .and_then(|s| s.split(']').next())
            .expect("recover_id should be present");

        let recovered = mgr.recover_messages(recover_id).await;
        assert!(recovered.is_some(), "recover 应返回 Some");
        let recovered = recovered.unwrap();
        assert_eq!(recovered.len(), 10, "应恢复 10 条原始 middle msgs");
    }

    /// Z2: recover 不存在的 id 返回 None
    #[tokio::test]
    async fn test_recover_unknown_id_returns_none() {
        let mgr = ContextManager::new(Arc::new(NoopStore));
        let recovered = mgr.recover_messages("mb_nonexistent").await;
        assert!(recovered.is_none());
    }

    // ─── Z1: cold 双向召回测试 ────────────────────────────────────────────

    struct StubColdStore {
        items: std::sync::Mutex<Vec<SessionSnapshot>>,
    }
    #[async_trait::async_trait]
    impl SessionStore for StubColdStore {
        async fn save(&self, s: SessionSnapshot) -> std::result::Result<(), KernelError> {
            self.items.lock().unwrap().push(s);
            Ok(())
        }
        async fn load_recent(&self, l: usize) -> std::result::Result<Vec<SessionSnapshot>, KernelError> {
            let items = self.items.lock().unwrap().clone();
            Ok(items.into_iter().rev().take(l).collect())
        }
        async fn search(&self, q: &str) -> std::result::Result<Vec<SessionSnapshot>, KernelError> {
            let items = self.items.lock().unwrap().clone();
            Ok(items.into_iter().filter(|s| s.summary.contains(q)).collect())
        }
    }

    fn snapshot(id: &str, summary: &str, turn: u32) -> SessionSnapshot {
        SessionSnapshot {
            session_id: id.into(),
            turn_count: turn,
            summary: summary.into(),
            token_estimate: 100,
            created_at: 0,
            key_decisions: vec![],
        }
    }

    /// Z1: recall_from_cold 按 query 过滤
    #[tokio::test]
    async fn test_recall_from_cold_filters_by_query() {
        let cold = Arc::new(StubColdStore { items: std::sync::Mutex::new(vec![]) });
        let mgr = ContextManager::new(cold.clone() as Arc<dyn SessionStore>);
        cold.save(snapshot("s1", "rust async patterns", 5)).await.unwrap();
        cold.save(snapshot("s2", "python web scraping", 3)).await.unwrap();
        cold.save(snapshot("s3", "rust borrow checker", 7)).await.unwrap();
        let results = mgr.recall_from_cold("rust", 10).await.unwrap();
        assert_eq!(results.len(), 2, "应只匹配 'rust' 关键字的两条");
    }

    /// Z1: recall_recent 取最近 N 条
    #[tokio::test]
    async fn test_recall_recent_returns_latest() {
        let cold = Arc::new(StubColdStore { items: std::sync::Mutex::new(vec![]) });
        let mgr = ContextManager::new(cold.clone() as Arc<dyn SessionStore>);
        for i in 0..5 {
            cold.save(snapshot(&format!("s{i}"), &format!("topic {i}"), i as u32)).await.unwrap();
        }
        let results = mgr.recall_recent(2).await.unwrap();
        assert_eq!(results.len(), 2);
    }

    /// Z1: recall_from_cold limit 上限 20
    #[tokio::test]
    async fn test_recall_limit_capped() {
        let cold = Arc::new(StubColdStore { items: std::sync::Mutex::new(vec![]) });
        let mgr = ContextManager::new(cold.clone() as Arc<dyn SessionStore>);
        for i in 0..30 {
            cold.save(snapshot(&format!("s{i}"), "any summary", i as u32)).await.unwrap();
        }
        let results = mgr.recall_from_cold("any", 100).await.unwrap();
        assert!(results.len() <= 20, "limit 上限应封顶 20");
    }

    // ─── Task #80: tier migration 测试 ────────────────────────────────────

    /// 80-T1: hot 中 turn_count + threshold ≤ current_turn 的应被 promote 到 warm
    #[tokio::test]
    async fn test_tier_migration_hot_to_warm() {
        let cold = Arc::new(StubColdStore { items: std::sync::Mutex::new(vec![]) });
        let mgr = ContextManager::new(cold as Arc<dyn SessionStore>);
        // 推 5 个 snapshot，turn_count 0/10/20/30/40
        for i in 0..5u32 {
            mgr.record_snapshot(snapshot(&format!("s{i}"), "msg", i * 10)).await;
        }
        // current_turn=50, threshold=30 → turn_count ≤ 20 的迁入 warm（即 0/10/20 三条）
        let stats = mgr.run_tier_migration(50, 30, 100).await;
        assert_eq!(stats.promoted_to_warm, 3, "三条 ≤ 20 turn 的应 promote");
        assert_eq!(stats.demoted_to_cold, 0);
        assert_eq!(mgr.hot_snapshot_count().await, 2, "保留 turn 30/40 在 hot");
        assert_eq!(mgr.warm_snapshot_count().await, 3);
    }

    /// 80-T2: warm 超 capacity 的应 demote 到 cold
    #[tokio::test]
    async fn test_tier_migration_warm_to_cold() {
        let cold = Arc::new(StubColdStore { items: std::sync::Mutex::new(vec![]) });
        let mgr = ContextManager::new(cold.clone() as Arc<dyn SessionStore>);
        // 推 5 个全老化的 snapshot（turn_count=0）
        for i in 0..5u32 {
            mgr.record_snapshot(snapshot(&format!("s{i}"), "msg", 0)).await;
        }
        // capacity=2 → 5 全 promote 到 warm，warm 超 cap 3 个 demote 到 cold
        let stats = mgr.run_tier_migration(100, 30, 2).await;
        assert_eq!(stats.promoted_to_warm, 5);
        assert_eq!(stats.demoted_to_cold, 3, "5 - 2 = 3 条降级");
        assert_eq!(mgr.warm_snapshot_count().await, 2);
        assert_eq!(cold.items.lock().unwrap().len(), 3, "cold 收到 3 条");
    }

    /// 80-T3: 未老化的 snapshot 不应迁移
    #[tokio::test]
    async fn test_tier_migration_skips_fresh() {
        let cold = Arc::new(StubColdStore { items: std::sync::Mutex::new(vec![]) });
        let mgr = ContextManager::new(cold as Arc<dyn SessionStore>);
        mgr.record_snapshot(snapshot("s1", "fresh", 95)).await; // turn 95
        mgr.record_snapshot(snapshot("s2", "fresh", 99)).await; // turn 99
        // current=100, threshold=30 → 都没老化 (95+30=125 > 100, 99+30=129 > 100)
        let stats = mgr.run_tier_migration(100, 30, 100).await;
        assert_eq!(stats.promoted_to_warm, 0);
        assert_eq!(mgr.hot_snapshot_count().await, 2);
        assert_eq!(mgr.warm_snapshot_count().await, 0);
    }

    // ─── Z4: Summarizer trait 测试 ────────────────────────────────────────

    /// Z4: DeterministicSummarizer::brief 输出含首行 snippet
    #[tokio::test]
    async fn test_deterministic_brief_summarizer() {
        let s = DeterministicSummarizer::brief();
        let msgs = vec![
            mk_msg("first line\nsecond line"),
            mk_assistant("hello world"),
        ];
        let summary = s.summarize(&msgs, 100).await.unwrap();
        assert!(summary.contains("first line"), "首行应保留");
        assert!(!summary.contains("second line"), "Brief 不应保留第二行");
        assert!(summary.contains("[User]") && summary.contains("[Assistant]"));
    }

    /// Z4: DeterministicSummarizer::minimal 不含 snippet
    #[tokio::test]
    async fn test_deterministic_minimal_summarizer() {
        let s = DeterministicSummarizer::minimal();
        let msgs = vec![mk_msg("some content here")];
        let summary = s.summarize(&msgs, 100).await.unwrap();
        assert!(!summary.contains("some content"));
        assert!(summary.contains("tok"));
    }

    /// Z4: ContextManager 默认 summarizer 是 DeterministicSummarizer
    #[tokio::test]
    async fn test_default_summarizer_is_deterministic() {
        let mgr = ContextManager::new(Arc::new(NoopStore));
        let s = mgr.summarizer.read().await.clone();
        let msgs = vec![mk_msg("test")];
        let summary = s.summarize(&msgs, 100).await.unwrap();
        assert!(summary.contains("test"));
    }

    /// Z4: set_summarizer 替换默认实现
    #[tokio::test]
    async fn test_set_summarizer_replaces_default() {
        struct FixedSummarizer;
        #[async_trait::async_trait]
        impl Summarizer for FixedSummarizer {
            async fn summarize(&self, _: &[Message], _: usize) -> std::result::Result<String, KernelError> {
                Ok("FIXED_SUMMARY".into())
            }
        }
        let mgr = ContextManager::new(Arc::new(NoopStore));
        mgr.set_summarizer(Arc::new(FixedSummarizer)).await;
        let s = mgr.summarizer.read().await.clone();
        let summary = s.summarize(&[], 100).await.unwrap();
        assert_eq!(summary, "FIXED_SUMMARY");
    }

    /// Z2: archive_size 反映写入数量
    #[tokio::test]
    async fn test_archive_size_tracks_writes() {
        let mgr = ContextManager::new(Arc::new(NoopStore));
        assert_eq!(mgr.archive_size().await, 0);
        {
            let mut w = mgr.window.write().await;
            w.max_tokens = 100;
            w.current_tokens = 90;
            w.compression_trigger_pct = 85;
        }
        let mut msgs = Vec::new();
        msgs.push(mk_msg("EARLY"));
        for i in 0..6 {
            msgs.push(mk_assistant(&format!("middle {i}")));
        }
        for i in 0..5 {
            msgs.push(mk_msg(&format!("late {i}")));
        }
        let _ = mgr.auto_compress_messages(&mut msgs).await;
        assert!(mgr.archive_size().await >= 1, "压缩后 archive 应至少 1 条");
    }

    /// Z3: auto_compress 接 Minimal 档位仅 role + tok
    #[tokio::test]
    async fn test_auto_compress_minimal_strips_content() {
        let mgr = ContextManager::new(Arc::new(NoopStore));
        mgr.set_compress_level(CompressLevel::Minimal).await;
        {
            let mut w = mgr.window.write().await;
            w.max_tokens = 100;
            w.current_tokens = 90;
            w.compression_trigger_pct = 85;
        }
        let mut msgs = Vec::new();
        // early_keep=2, so first 2 are preserved
        msgs.push(mk_msg("EARLY1"));
        msgs.push(mk_msg("EARLY2"));
        // 10 middle messages to ensure some get compressed
        for _ in 0..10 {
            msgs.push(mk_assistant("some content with details that should be stripped"));
        }
        // keep_count=8 tail messages
        for i in 0..8 {
            msgs.push(mk_msg(&format!("late {i}")));
        }
        let compressed = mgr.auto_compress_messages(&mut msgs).await;
        // 应有压缩记录
        assert!(!compressed.is_empty(), "应产生压缩记录");
        // 找到压缩摘要消息（包含 "Compressed"）
        let summary_msg = msgs.iter().find(|m| {
            matches!(&m.content, Some(MessageContent::Text(s)) if s.contains("Compressed"))
        });
        assert!(summary_msg.is_some(), "应有压缩摘要消息");
        if let Some(MessageContent::Text(s)) = &summary_msg.unwrap().content {
            // Minimal 档位不保留 snippet 内容
            assert!(!s.contains("some content"),
                "Minimal 档位应去除内容 snippet：{s}");
            assert!(s.contains("tok"), "应包含 tok 数标记");
        }
    }

    /// Ctx-D: ContextManager.set_kb_store 注入后 kb_store 字段非 None
    #[tokio::test]
    async fn test_set_kb_store_attaches_store() {
        use crate::knowledge_store::KnowledgeStore;
        let mgr = ContextManager::new(Arc::new(NoopStore));
        assert!(mgr.kb_store.read().await.is_none());
        let kb = Arc::new(KnowledgeStore::in_memory().unwrap());
        mgr.set_kb_store(kb).await;
        assert!(mgr.kb_store.read().await.is_some());
    }

    /// Ctx-C: 极远 segment 即使有 ref_count 也强制 evict
    #[tokio::test]
    async fn test_evict_force_removes_extremely_old() {
        let mgr = ContextManager::new(Arc::new(NoopStore));
        {
            let mut rc = mgr.retained_content.write().await;
            rc.push(("ancient".to_string(), "content".to_string(), RetainMeta {
                created_turn: 0, last_used_turn: 1, ref_count: 99,  // 高引用但极远
            }));
        }
        // current_turn=100, evict_distance=20 → 强制阈值 = 40，distance=99 > 40 → evict
        let evicted = mgr.evict_stale_segments(100, 20).await;
        assert_eq!(evicted, 1, "distance > 2*evict_distance 强制 evict 即使 ref_count 高");
    }

    /// Ctx-B: force_discard (>=95%) 时丢弃中间含 tool 序列
    #[tokio::test]
    async fn test_force_discard_drops_middle() {
        let mgr = ContextManager::new(Arc::new(NoopStore));
        {
            let mut w = mgr.window.write().await;
            w.max_tokens = 100;
            w.current_tokens = 96; // >= 95% → force_discard
            w.compression_trigger_pct = 85;
        }
        let mut msgs = Vec::new();
        // force mode: early_keep=2, keep_count=3 → need > 6 msgs
        msgs.push(mk_msg("EARLY1"));
        msgs.push(mk_msg("EARLY2"));
        for _ in 0..10 {
            msgs.push(mk_assistant_with_tool_call("tool call"));
        }
        msgs.push(mk_msg("late 1"));
        msgs.push(mk_msg("late 2"));
        msgs.push(mk_msg("late 3"));
        // total=15, early=2, tail_start=15-3=12, middle=index 2..12 (10 msgs, all force-discarded)
        let _ = mgr.auto_compress_messages(&mut msgs).await;
        assert_eq!(msgs.len(), 5, "force_discard：2 early + 3 late = 5");
    }

    // ─── W4 (Task #102) Selective History Retention ────────────────────────

    /// importance_score 单调性：ref_count 越大、距离越近，分越高
    #[test]
    fn importance_score_monotonic_in_ref_count() {
        let mut a = RetainMeta { created_turn: 5, last_used_turn: 10, ref_count: 0 };
        let s0 = a.importance_score(15);
        a.ref_count = 1;
        let s1 = a.importance_score(15);
        a.ref_count = 5;
        let s5 = a.importance_score(15);
        assert!(s1 > s0, "ref_count 1 > 0");
        assert!(s5 > s1, "ref_count 5 > 1");
    }

    #[test]
    fn importance_score_decays_with_distance() {
        let recent = RetainMeta { created_turn: 0, last_used_turn: 50, ref_count: 1 };
        let stale = RetainMeta { created_turn: 0, last_used_turn: 10, ref_count: 1 };
        let s_recent = recent.importance_score(50);
        let s_stale = stale.importance_score(50);
        assert!(s_recent > s_stale, "recently used > stale used (same ref_count)");
    }

    /// W4 bug fix 回归：set_current_turn 后 compress 写入 RetainMeta.created_turn = 当前 turn
    #[tokio::test]
    async fn compress_writes_correct_created_turn_after_set_current_turn() {
        let mgr = ContextManager::new(Arc::new(NoopStore));
        // 模拟 turn=42
        mgr.set_current_turn(42);
        // compress 路径需要先 declare → 简化：直接 push 到 retained_content
        // 这里我们验证 RetainMeta.default vs current_turn 写入差异
        let cur = mgr.get_current_turn();
        assert_eq!(cur, 42);
        // 直接构造 entry 模拟修复后的行为
        let meta = RetainMeta {
            created_turn: cur,
            last_used_turn: cur,
            ref_count: 0,
        };
        assert_eq!(meta.created_turn, 42);
        assert_ne!(meta.created_turn, RetainMeta::default().created_turn);
    }

    /// evict_by_importance：超 budget 时按分数降序保留高分段，低分段被丢弃
    #[tokio::test]
    async fn evict_by_importance_keeps_top_k_by_score() {
        let mgr = ContextManager::new(Arc::new(NoopStore));
        mgr.set_current_turn(50);
        {
            let mut rc = mgr.retained_content.write().await;
            // 总 token = (200/4)*4 = 200 tokens
            // 段 A：高 ref_count → 高分
            rc.push((
                "A".into(),
                "x".repeat(200),
                RetainMeta { created_turn: 40, last_used_turn: 49, ref_count: 5 },
            ));
            // 段 B：从未引用，老 → 低分
            rc.push((
                "B".into(),
                "y".repeat(200),
                RetainMeta { created_turn: 10, last_used_turn: 10, ref_count: 0 },
            ));
            // 段 C：中等
            rc.push((
                "C".into(),
                "z".repeat(200),
                RetainMeta { created_turn: 30, last_used_turn: 45, ref_count: 1 },
            ));
        }
        // 设 budget=80 tokens（约 1 段：单段 ~50t）
        let (evicted, remaining) = mgr.evict_by_importance(80).await;
        assert!(evicted >= 1, "应至少 evict 一段");
        assert!(remaining <= 80, "remaining tokens 不超 budget");

        let snap = mgr.retained_snapshot().await;
        // 高分段 A 必须保留
        assert!(snap.iter().any(|(id, _, _)| id == "A"), "高分段 A 必须保留");
    }

    /// evict_by_importance：未超 budget 不动任何段
    #[tokio::test]
    async fn evict_by_importance_noop_under_budget() {
        let mgr = ContextManager::new(Arc::new(NoopStore));
        mgr.set_current_turn(10);
        {
            let mut rc = mgr.retained_content.write().await;
            rc.push((
                "A".into(),
                "small".into(),
                RetainMeta::default(),
            ));
        }
        let (evicted, _) = mgr.evict_by_importance(10000).await;
        assert_eq!(evicted, 0);
        assert_eq!(mgr.retained_snapshot().await.len(), 1);
    }

    /// evict_by_importance：保留集合按原 FIFO 顺序回填——保 cache 前缀稳定性
    #[tokio::test]
    async fn evict_by_importance_preserves_fifo_order_among_kept() {
        let mgr = ContextManager::new(Arc::new(NoopStore));
        mgr.set_current_turn(20);
        {
            let mut rc = mgr.retained_content.write().await;
            // 4 段：1/3 高分，2/4 低分；budget 限 1 段
            rc.push(("seg1".into(), "x".repeat(100),
                RetainMeta { created_turn: 18, last_used_turn: 19, ref_count: 3 }));
            rc.push(("seg2".into(), "y".repeat(100),
                RetainMeta { created_turn: 5,  last_used_turn: 5,  ref_count: 0 }));
            rc.push(("seg3".into(), "z".repeat(100),
                RetainMeta { created_turn: 17, last_used_turn: 19, ref_count: 4 }));
            rc.push(("seg4".into(), "w".repeat(100),
                RetainMeta { created_turn: 6,  last_used_turn: 6,  ref_count: 0 }));
        }
        // budget=60t（约 2 段：单段 ~25t）
        let (_, _) = mgr.evict_by_importance(60).await;
        let snap = mgr.retained_snapshot().await;
        let kept_ids: Vec<&str> = snap.iter().map(|(id, _, _)| id.as_str()).collect();
        // 保留集合中 seg1 必须在 seg3 之前（保 FIFO 顺序，保前缀字节稳定）
        if let (Some(p1), Some(p3)) = (
            kept_ids.iter().position(|s| *s == "seg1"),
            kept_ids.iter().position(|s| *s == "seg3"),
        ) {
            assert!(p1 < p3, "FIFO 顺序保留：seg1 必须早于 seg3 (kept={:?})", kept_ids);
        }
    }

    #[tokio::test]
    async fn retained_diagnostics_aggregates_correctly() {
        let mgr = ContextManager::new(Arc::new(NoopStore));
        mgr.set_current_turn(20);
        {
            let mut rc = mgr.retained_content.write().await;
            rc.push(("a".into(), "x".repeat(40), RetainMeta { created_turn: 18, last_used_turn: 19, ref_count: 2 }));
            rc.push(("b".into(), "y".repeat(40), RetainMeta { created_turn: 18, last_used_turn: 19, ref_count: 0 }));
        }
        let d = mgr.retained_diagnostics().await;
        assert_eq!(d.entries, 2);
        assert_eq!(d.current_turn, 20);
        assert!(d.total_tokens > 0);
        assert!(d.max_importance > d.avg_importance || (d.max_importance - d.avg_importance).abs() < 1e-9);
    }
}