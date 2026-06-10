//! Standby Cache — 短 TTL 内容缓存层
//!
//! 当 TriageEngine 将消息标记为 STANDBY 时，消息被移入此缓存。
//! warm_up 机制通过 MinHash 相似度在后续轮次中自动召回相关内容。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::RwLock;

use crate::core::compress_math::MinHashSig;
use crate::llm::provider::Message;

/// Standby 缓存条目
#[derive(Debug, Clone)]
pub struct StandbyEntry {
    pub recall_id: String,
    pub content: Arc<Vec<Message>>,
    pub summary: String,
    pub content_hash: u64,
    pub source_turn: u32,
    pub last_active_turn: u32,
    pub created_at: Instant,
    pub warm_count: u32,
    pub minhash_sig: Arc<MinHashSig>,
}

impl StandbyEntry {
    pub fn new(recall_id: String, messages: Vec<Message>, summary: String, turn: u32) -> Self {
        let content = Arc::new(messages);
        let text = summary.clone();
        let minhash_sig = Arc::new(MinHashSig::from_text(&text));
        let content_hash = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            text.hash(&mut h);
            h.finish()
        };
        Self {
            recall_id,
            content,
            summary,
            content_hash,
            source_turn: turn,
            last_active_turn: turn,
            created_at: Instant::now(),
            warm_count: 0,
            minhash_sig,
        }
    }
}

/// 内存 Standby Cache——带 TTL 和 LRU 逐出
pub struct StandbyCache {
    entries: RwLock<HashMap<String, StandbyEntry>>,
    order: RwLock<Vec<String>>,
    cap: usize,
    default_ttl_turns: u32,
    warm_threshold: f64,
}

impl StandbyCache {
    pub fn new(cap: usize) -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
            order: RwLock::new(Vec::new()),
            cap,
            default_ttl_turns: 50,
            warm_threshold: 0.60,
        }
    }

    pub fn with_ttl(cap: usize, ttl_turns: u32, warm_threshold: f64) -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
            order: RwLock::new(Vec::new()),
            cap,
            default_ttl_turns: ttl_turns,
            warm_threshold,
        }
    }

    /// 写入缓存
    pub async fn set(&self, entry: StandbyEntry) {
        let key = entry.recall_id.clone();
        {
            let mut entries = self.entries.write().await;
            // 已达容量 → 移除最旧条目
            if entries.len() >= self.cap {
                let mut order = self.order.write().await;
                if let Some(oldest) = order.first().cloned() {
                    entries.remove(&oldest);
                    order.retain(|k| k != &oldest);
                }
            }
            entries.insert(key.clone(), entry);
        }
        let mut order = self.order.write().await;
        order.retain(|k| k != &key);
        order.push(key);
    }

    /// 按 recall_id 取回
    pub async fn get(&self, recall_id: &str) -> Option<StandbyEntry> {
        let entries = self.entries.read().await;
        entries.get(recall_id).cloned()
    }

    /// 按 query 查找相关条目（warm-up）
    pub async fn warm_up(&self, query: &str, current_turn: u32) -> Vec<StandbyEntry> {
        if query.is_empty() {
            return Vec::new();
        }
        let query_sig = MinHashSig::from_text(query);
        let entries = self.entries.read().await;
        let mut matches: Vec<(f64, StandbyEntry)> = entries
            .values()
            .filter_map(|e| {
                let sim = query_sig.jaccard_similarity(&e.minhash_sig);
                if sim >= self.warm_threshold {
                    // TTL check: 如果条目超过 default_ttl_turns 没有被 warm-up，忽略
                    let turn_age = current_turn.saturating_sub(e.last_active_turn);
                    if turn_age > self.default_ttl_turns {
                        return None;
                    }
                    Some((sim, e.clone()))
                } else {
                    None
                }
            })
            .collect();
        matches.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        matches.into_iter().map(|(_, e)| e).collect()
    }

    /// 检查是否含某 recall_id
    pub async fn contains(&self, recall_id: &str) -> bool {
        let entries = self.entries.read().await;
        entries.contains_key(recall_id)
    }

    /// 统计数据
    pub async fn stats(&self) -> (usize, usize) {
        let entries = self.entries.read().await;
        let total_tokens: usize = entries
            .values()
            .map(|e| e.content.len())
            .sum();
        (entries.len(), total_tokens)
    }
}
