use std::time::Duration;
use crate::cache::CacheResult;

#[async_trait::async_trait]
pub trait CacheBackend: Send + Sync {
    async fn get(&self, key: &str) -> CacheResult<Option<Vec<u8>>>;
    async fn set(&self, key: &str, value: Vec<u8>, ttl: Duration) -> CacheResult<()>;
    async fn remove(&self, key: &str) -> CacheResult<()>;
    async fn clear(&self) -> CacheResult<()>;
}
