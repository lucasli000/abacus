//! Warm tier: 通用 "已加载但未暴露" 资源池。
//!
//! 与 [`super::backend::CacheBackend`] 的区别：
//! - 字节缓存（CacheBackend）解决跨进程 / 持久化问题，每次访问需 (de)serialize。
//! - Warm 层（本模块）解决"热加载但暂未启用"的进程内典型资源（子系统、工具结果、prompt 模板、KB chunk
//!   等），通过 `Arc<T>` 共享 + 类型化 API 避免序列化成本。
//!
//! 生命周期钩子：
//! - `on_promote` 在首次升温时触发一次（如子系统升温时注册其工具）。
//! - `on_demote` 在被 LRU/TTL/手动逐出时触发一次（如反注册工具）。
//! - 钩子是同步调用，应保持轻量。需要异步副作用的实现可在内部 spawn。
//!
//! 引用关系：
//! - 上游消费者：`tool::subsystem_policy`（W1 自适应子系统）、`core::pipeline`
//!   （W2 工具结果去重）、`core::prompt_assembly`（W3 模板）、`knowledge_store`（W6 KB chunk）。
//! - 下游依赖：仅 `std` + `lru` workspace。
//!
//! 容量策略：基于 `size_hint()` 加权 LRU；总权重超过 capacity 时按访问最近度逐出。
//! TTL 在 `get` 路径惰性检查，避免后台线程。

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Warm 层可缓存对象的标记 trait。
///
/// 实现者通常是 `Arc<Self>` 的内层载荷（避免共享时的克隆代价）。
pub trait WarmCacheable: Send + Sync + 'static {
    /// 容量加权（字节、token 或任意单位，调用方自行约定）。默认 1。
    fn size_hint(&self) -> usize {
        1
    }
    /// 升温副作用钩子；只在每个 key 首次 `promote` 时调用一次。
    fn on_promote(&self) {}
    /// 降温副作用钩子；在 LRU 逐出 / TTL 过期 / 手动 `demote` 时调用一次。
    fn on_demote(&self) {}
}

/// Warm 层运行时统计（外部审计、`audit_report` 消费）。
#[derive(Debug, Clone, Default)]
pub struct WarmStats {
    pub entries: usize,
    pub total_weight: usize,
    pub capacity: usize,
    pub hits: u64,
    pub misses: u64,
    pub evictions_lru: u64,
    pub evictions_ttl: u64,
}

impl WarmStats {
    pub fn hit_rate(&self) -> f64 {
        let total = self.hits + self.misses;
        if total == 0 {
            0.0
        } else {
            self.hits as f64 / total as f64
        }
    }
}

struct WarmEntry<T> {
    value: Arc<T>,
    last_access: Instant,
    inserted_at: Instant,
    weight: usize,
}

struct Inner<K, T>
where
    K: Eq + Hash + Clone,
    T: WarmCacheable,
{
    map: HashMap<K, WarmEntry<T>>,
    total_weight: usize,
    hits: u64,
    misses: u64,
    evictions_lru: u64,
    evictions_ttl: u64,
}

/// 类型化、加权 LRU + TTL 的 warm 池。
///
/// 线程安全：内部 `Mutex` 串行化所有写操作；读操作（`get`）也获取 `Mutex` 以更新 LRU 顺序，
/// 但持锁时间极短（hash 查询 + Instant 写入）。
pub struct WarmTier<K, T>
where
    K: Eq + Hash + Clone + Send + Sync + 'static,
    T: WarmCacheable,
{
    inner: Mutex<Inner<K, T>>,
    capacity_weight: usize,
    ttl: Duration,
}

impl<K, T> WarmTier<K, T>
where
    K: Eq + Hash + Clone + Send + Sync + 'static,
    T: WarmCacheable,
{
    pub fn new(capacity_weight: usize, ttl: Duration) -> Self {
        Self {
            inner: Mutex::new(Inner {
                map: HashMap::new(),
                total_weight: 0,
                hits: 0,
                misses: 0,
                evictions_lru: 0,
                evictions_ttl: 0,
            }),
            capacity_weight,
            ttl,
        }
    }

    /// 升温：插入或刷新 entry。首次插入时调用 `on_promote`。返回是否新插入。
    pub fn promote(&self, key: K, value: Arc<T>) -> bool {
        let weight = value.size_hint().max(1);
        let now = Instant::now();
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());

        // 命中刷新：仅更新 last_access，不重复触发 on_promote。
        if let Some(existing) = inner.map.get_mut(&key) {
            existing.last_access = now;
            return false;
        }

        // 新插入：先调用 on_promote（在锁外更安全，但为简化保持锁内；钩子约定轻量）。
        value.on_promote();

        inner.map.insert(
            key,
            WarmEntry {
                value,
                last_access: now,
                inserted_at: now,
                weight,
            },
        );
        inner.total_weight += weight;

        // 超容则按 LRU 逐出。
        Self::evict_until_within_capacity(&mut inner, self.capacity_weight);
        true
    }

    /// 命中查询：刷新 `last_access`，惰性检查 TTL。
    pub fn get(&self, key: &K) -> Option<Arc<T>> {
        let now = Instant::now();
        let ttl = self.ttl;
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());

        // 先判断是否过期；过期触发 demote。
        let expired = inner
            .map
            .get(key)
            .map(|e| now.duration_since(e.inserted_at) > ttl)
            .unwrap_or(false);
        if expired {
            if let Some(entry) = inner.map.remove(key) {
                inner.total_weight = inner.total_weight.saturating_sub(entry.weight);
                inner.evictions_ttl += 1;
                inner.misses += 1;
                drop(inner); // 钩子在锁外
                entry.value.on_demote();
                return None;
            }
        }

        let cloned = if let Some(entry) = inner.map.get_mut(key) {
            entry.last_access = now;
            Some(entry.value.clone())
        } else {
            None
        };
        if cloned.is_some() {
            inner.hits += 1;
        } else {
            inner.misses += 1;
        }
        cloned
    }

    /// 手动降温。返回被移除的 Arc（供调用方做后续动作）。
    pub fn demote(&self, key: &K) -> Option<Arc<T>> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = inner.map.remove(key) {
            inner.total_weight = inner.total_weight.saturating_sub(entry.weight);
            drop(inner);
            entry.value.on_demote();
            Some(entry.value)
        } else {
            None
        }
    }

    /// 主动扫描并逐出过期 entry，返回逐出数量。后台调度可周期性调用。
    pub fn evict_expired(&self) -> usize {
        let now = Instant::now();
        let ttl = self.ttl;
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());

        let expired_keys: Vec<K> = inner
            .map
            .iter()
            .filter(|(_, e)| now.duration_since(e.inserted_at) > ttl)
            .map(|(k, _)| k.clone())
            .collect();

        let mut freed = Vec::with_capacity(expired_keys.len());
        for k in &expired_keys {
            if let Some(entry) = inner.map.remove(k) {
                inner.total_weight = inner.total_weight.saturating_sub(entry.weight);
                inner.evictions_ttl += 1;
                freed.push(entry.value);
            }
        }
        drop(inner);
        for v in &freed {
            v.on_demote();
        }
        freed.len()
    }

    pub fn stats(&self) -> WarmStats {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        WarmStats {
            entries: inner.map.len(),
            total_weight: inner.total_weight,
            capacity: self.capacity_weight,
            hits: inner.hits,
            misses: inner.misses,
            evictions_lru: inner.evictions_lru,
            evictions_ttl: inner.evictions_ttl,
        }
    }

    pub fn clear(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let drained: Vec<Arc<T>> = inner.map.drain().map(|(_, e)| e.value).collect();
        inner.total_weight = 0;
        drop(inner);
        for v in &drained {
            v.on_demote();
        }
    }

    fn evict_until_within_capacity(inner: &mut Inner<K, T>, cap: usize) {
        while inner.total_weight > cap && !inner.map.is_empty() {
            // 找最久未访问的 key
            let victim_key: Option<K> = inner
                .map
                .iter()
                .min_by_key(|(_, e)| e.last_access)
                .map(|(k, _)| k.clone());
            if let Some(k) = victim_key {
                if let Some(entry) = inner.map.remove(&k) {
                    inner.total_weight = inner.total_weight.saturating_sub(entry.weight);
                    inner.evictions_lru += 1;
                    // 在持锁中调用钩子（保持简单；约定钩子轻量）。
                    entry.value.on_demote();
                }
            } else {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Probe {
        id: u32,
        size: usize,
        promoted: Arc<AtomicUsize>,
        demoted: Arc<AtomicUsize>,
    }

    impl WarmCacheable for Probe {
        fn size_hint(&self) -> usize {
            self.size
        }
        fn on_promote(&self) {
            self.promoted.fetch_add(1, Ordering::SeqCst);
        }
        fn on_demote(&self) {
            self.demoted.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn mk(id: u32, size: usize, p: &Arc<AtomicUsize>, d: &Arc<AtomicUsize>) -> Arc<Probe> {
        Arc::new(Probe {
            id,
            size,
            promoted: p.clone(),
            demoted: d.clone(),
        })
    }

    #[test]
    fn promote_and_get_runs_hooks_once() {
        let p = Arc::new(AtomicUsize::new(0));
        let d = Arc::new(AtomicUsize::new(0));
        let warm: WarmTier<u32, Probe> = WarmTier::new(100, Duration::from_secs(60));
        warm.promote(1, mk(1, 10, &p, &d));
        // re-promote 同 key 不应触发新的 on_promote
        warm.promote(1, mk(1, 10, &p, &d));
        let got = warm.get(&1).expect("present");
        assert_eq!(got.id, 1);
        assert_eq!(p.load(Ordering::SeqCst), 1, "on_promote 仅触发一次");
        assert_eq!(d.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn lru_eviction_calls_on_demote() {
        let p = Arc::new(AtomicUsize::new(0));
        let d = Arc::new(AtomicUsize::new(0));
        let warm: WarmTier<u32, Probe> = WarmTier::new(20, Duration::from_secs(60));
        warm.promote(1, mk(1, 10, &p, &d));
        warm.promote(2, mk(2, 10, &p, &d));
        // 触一次 get(1) 让其更近，再插 3 应逐出 2
        let _ = warm.get(&1);
        warm.promote(3, mk(3, 10, &p, &d));

        let stats = warm.stats();
        assert_eq!(stats.entries, 2);
        assert_eq!(stats.evictions_lru, 1);
        assert_eq!(d.load(Ordering::SeqCst), 1);
        assert!(warm.get(&2).is_none(), "key=2 应已被逐出");
        assert!(warm.get(&1).is_some());
        assert!(warm.get(&3).is_some());
    }

    #[test]
    fn ttl_expiry_demotes() {
        let p = Arc::new(AtomicUsize::new(0));
        let d = Arc::new(AtomicUsize::new(0));
        let warm: WarmTier<u32, Probe> = WarmTier::new(100, Duration::from_millis(5));
        warm.promote(1, mk(1, 10, &p, &d));
        std::thread::sleep(Duration::from_millis(10));
        assert!(warm.get(&1).is_none(), "TTL 后 get 应 miss");
        assert_eq!(d.load(Ordering::SeqCst), 1);
        assert_eq!(warm.stats().evictions_ttl, 1);
    }

    #[test]
    fn manual_demote_returns_arc() {
        let p = Arc::new(AtomicUsize::new(0));
        let d = Arc::new(AtomicUsize::new(0));
        let warm: WarmTier<u32, Probe> = WarmTier::new(100, Duration::from_secs(60));
        warm.promote(1, mk(1, 10, &p, &d));
        let removed = warm.demote(&1).expect("present");
        assert_eq!(removed.id, 1);
        assert_eq!(d.load(Ordering::SeqCst), 1);
        assert!(warm.demote(&1).is_none());
    }

    #[test]
    fn weighted_capacity_respected() {
        let p = Arc::new(AtomicUsize::new(0));
        let d = Arc::new(AtomicUsize::new(0));
        let warm: WarmTier<u32, Probe> = WarmTier::new(50, Duration::from_secs(60));
        // 单个权重 30 + 30 = 60 > 50，应触发逐出 1 个
        warm.promote(1, mk(1, 30, &p, &d));
        warm.promote(2, mk(2, 30, &p, &d));
        assert!(warm.stats().total_weight <= 50);
        assert_eq!(warm.stats().entries, 1);
        assert_eq!(warm.stats().evictions_lru, 1);
    }

    #[test]
    fn hit_miss_counters() {
        let p = Arc::new(AtomicUsize::new(0));
        let d = Arc::new(AtomicUsize::new(0));
        let warm: WarmTier<u32, Probe> = WarmTier::new(100, Duration::from_secs(60));
        warm.promote(1, mk(1, 10, &p, &d));
        let _ = warm.get(&1);
        let _ = warm.get(&1);
        let _ = warm.get(&999);
        let s = warm.stats();
        assert_eq!(s.hits, 2);
        assert_eq!(s.misses, 1);
        assert!((s.hit_rate() - 2.0 / 3.0).abs() < 1e-9);
    }
}
