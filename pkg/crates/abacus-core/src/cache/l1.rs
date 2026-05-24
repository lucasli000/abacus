use std::num::NonZeroUsize;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use lru::LruCache;

use super::backend::CacheBackend;
use super::CacheResult;

struct Entry {
    value: Vec<u8>,
    expires_at: Instant,
}

pub struct L1MemoryCache {
    inner: Mutex<LruCache<String, Entry>>,
    default_ttl: Duration,
}

impl L1MemoryCache {
    pub fn new(capacity: usize, default_ttl: Duration) -> Self {
        let cap = NonZeroUsize::new(capacity).unwrap_or(NonZeroUsize::new(1024).unwrap());
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
}
