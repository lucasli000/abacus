pub mod backend;
pub mod l1;
pub mod warm;

pub use backend::CacheBackend;
pub use l1::L1MemoryCache;
pub use warm::{WarmCacheable, WarmStats, WarmTier};

pub type CacheError = Box<dyn std::error::Error + Send + Sync>;
pub type CacheResult<T> = std::result::Result<T, CacheError>;
