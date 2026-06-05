use std::num::NonZeroUsize;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use lru::LruCache;
use thiserror::Error;

use super::backend::CacheBackend;
use super::CacheResult;

struct Entry {
    value: Vec<u8>,
    expires_at: Instant,
}

/// 容量配置错误（治本：替换 `unwrap_or(1024)` 静默 fallback）
#[derive(Debug, Error)]
pub enum CacheConfigError {
    #[error("capacity must be > 0, got 0 (LRU requires at least 1 entry)")]
    ZeroCapacity,
}

pub struct L1MemoryCache {
    inner: Mutex<LruCache<String, Entry>>,
    default_ttl: Duration,
}

impl L1MemoryCache {
    /// Result 形式构造（治本：显式错误让调用方决定）
    ///
    /// 旧 `new()` 在 capacity=0 时 fallback 到 1024——可能掩盖配置错误。
    /// `try_new()` 强制调用方**显式处理** ZeroCapacity 错误（用 default / 报错 / log）。
    pub fn try_new(capacity: usize, default_ttl: Duration) -> Result<Self, CacheConfigError> {
        let cap = NonZeroUsize::new(capacity).ok_or(CacheConfigError::ZeroCapacity)?;
        Ok(Self {
            inner: Mutex::new(LruCache::new(cap)),
            default_ttl,
        })
    }

    /// 旧 `new` 保留（向后兼容）—— 内部用 1024 fallback，dev mode `debug_assert!` 暴露问题
    ///
    /// ## 设计取舍
    /// 不直接 panic：避免单点配置错误炸整个 cache 子系统（cache 失败应让 app 用 no-cache 降级运行）。
    /// 但**保留** `debug_assert!` 让 dev 立即看到，release 静默 fallback（tracing::warn 仍记录）。
    pub fn new(capacity: usize, default_ttl: Duration) -> Self {
        debug_assert!(
            capacity > 0,
            "L1MemoryCache::new: capacity must be > 0, got 0 (falling back to 1024). \
             Use try_new() to handle this error explicitly."
        );
        if capacity == 0 {
            tracing::warn!(
                "L1MemoryCache::new: capacity=0 is invalid; using 1024 as fallback. \
                 Set capacity > 0 in config or use try_new() for explicit error handling."
            );
        }
        let cap = NonZeroUsize::new(capacity.max(1)).expect("max(1) is always non-zero");
        Self {
            inner: Mutex::new(LruCache::new(cap)),
            default_ttl,
        }
    }

    pub fn default_ttl(&self) -> Duration {
        self.default_ttl
    }

    pub async fn set_default(&self, key: &str, value: Vec<u8>) -> CacheResult<()> {
        self.set(key, value, self.default_ttl).await
    }

    fn is_expired(entry: &Entry) -> bool {
        Instant::now() >= entry.expires_at
    }
}

#[async_trait::async_trait]
impl CacheBackend for L1MemoryCache {
    async fn get(&self, key: &str) -> CacheResult<Option<Vec<u8>>> {
        let mut cache = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = cache.get(key) {
            if Self::is_expired(entry) {
                cache.pop(key);
                return Ok(None);
            }
            Ok(Some(entry.value.clone()))
        } else {
            Ok(None)
        }
    }

    async fn set(&self, key: &str, value: Vec<u8>, ttl: Duration) -> CacheResult<()> {
        let mut cache = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        cache.put(
            key.to_string(),
            Entry {
                value,
                expires_at: Instant::now() + ttl,
            },
        );
        Ok(())
    }

    async fn remove(&self, key: &str) -> CacheResult<()> {
        let mut cache = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        cache.pop(key);
        Ok(())
    }

    async fn clear(&self) -> CacheResult<()> {
        let mut cache = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        cache.clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn test_set_get() {
        let cache = L1MemoryCache::new(100, Duration::from_secs(60));
        cache.set("k1", vec![1, 2, 3], Duration::from_secs(60)).await.unwrap();
        let val = cache.get("k1").await.unwrap();
        assert_eq!(val, Some(vec![1, 2, 3]));
    }

    #[tokio::test]
    async fn test_miss() {
        let cache = L1MemoryCache::new(100, Duration::from_secs(60));
        let val = cache.get("nonexistent").await.unwrap();
        assert_eq!(val, None);
    }

    #[tokio::test]
    async fn test_ttl_expiry() {
        let cache = L1MemoryCache::new(100, Duration::from_secs(60));
        cache.set("k1", vec![42], Duration::from_millis(1)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
        let val = cache.get("k1").await.unwrap();
        assert_eq!(val, None);
    }

    #[tokio::test]
    async fn test_lru_eviction() {
        let cache = L1MemoryCache::new(2, Duration::from_secs(60));
        cache.set("a", vec![1], Duration::from_secs(60)).await.unwrap();
        cache.set("b", vec![2], Duration::from_secs(60)).await.unwrap();
        cache.set("c", vec![3], Duration::from_secs(60)).await.unwrap();
        assert_eq!(cache.get("a").await.unwrap(), None);
        assert_eq!(cache.get("b").await.unwrap(), Some(vec![2]));
        assert_eq!(cache.get("c").await.unwrap(), Some(vec![3]));
    }

    #[tokio::test]
    async fn test_remove() {
        let cache = L1MemoryCache::new(100, Duration::from_secs(60));
        cache.set("k1", vec![1], Duration::from_secs(60)).await.unwrap();
        cache.remove("k1").await.unwrap();
        assert_eq!(cache.get("k1").await.unwrap(), None);
    }

    #[tokio::test]
    async fn test_clear() {
        let cache = L1MemoryCache::new(100, Duration::from_secs(60));
        cache.set("a", vec![1], Duration::from_secs(60)).await.unwrap();
        cache.set("b", vec![2], Duration::from_secs(60)).await.unwrap();
        cache.clear().await.unwrap();
        assert_eq!(cache.get("a").await.unwrap(), None);
        assert_eq!(cache.get("b").await.unwrap(), None);
    }

    #[tokio::test]
    async fn test_set_overwrite() {
        let cache = L1MemoryCache::new(100, Duration::from_secs(60));
        cache.set("k1", vec![1], Duration::from_secs(60)).await.unwrap();
        cache.set("k1", vec![2], Duration::from_secs(60)).await.unwrap();
        assert_eq!(cache.get("k1").await.unwrap(), Some(vec![2]));
    }

    /// Capacity=1 最小合法容量：能存一项
    #[tokio::test]
    async fn test_min_capacity() {
        let cache = L1MemoryCache::new(1, Duration::from_secs(60));
        cache.set("k1", vec![1], Duration::from_secs(60)).await.unwrap();
        assert_eq!(cache.get("k1").await.unwrap(), Some(vec![1]));
    }

    // ─── try_new: 治本路径 ──────────────────────────────────────────────

    #[test]
    fn try_new_zero_capacity_returns_error() {
        let result = L1MemoryCache::try_new(0, Duration::from_secs(60));
        match result {
            Err(e) => {
                let msg = format!("{e}");
                assert!(msg.contains("capacity must be > 0"), "got: {msg}");
            }
            Ok(_) => panic!("try_new(0) should return Err"),
        }
    }

    #[test]
    fn try_new_valid_capacity_succeeds() {
        let result = L1MemoryCache::try_new(100, Duration::from_secs(60));
        assert!(result.is_ok());
    }

    #[test]
    fn try_new_one_capacity_works() {
        // 1 是 NonZeroUsize 的最小合法值
        let result = L1MemoryCache::try_new(1, Duration::from_secs(60));
        assert!(result.is_ok());
    }
}
