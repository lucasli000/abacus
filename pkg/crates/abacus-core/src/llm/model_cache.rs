//! # ModelCache — 模型发现结果持久化缓存
//!
//! ## 场景
//! AbacusServer / CLI 启动时（或显式 `abacus models discover`），各 provider 调用
//! `discover_models()` 拉取真实可用模型；结果合并去重后写入 ~/.abacus/models.cache.json，
//! 后续 `/api/v1/models` 端点可优先读 cache，降低 provider /v1/models API 调用频率。
//!
//! ## 数据结构
//! ```json
//! {
//!   "version": 1,
//!   "discovered_at": 1716480000,
//!   "providers": {
//!     "deepseek": ["deepseek-chat", "deepseek-reasoner", ...],
//!     "openai-compatible": ["gpt-4o", "gpt-4o-mini", ...],
//!     "anthropic": ["claude-opus-4", "claude-sonnet-4-6", ...]
//!   }
//! }
//! ```
//!
//! ## 引用关系
//! - 写入：`CoreLoop::discover_all_models()` 完成后
//! - 读取：`/api/v1/models` 端点 fallback；CLI `abacus models list --cached`
//!
//! ## 边界
//! - cache 永远是 best-effort：磁盘失败静默降级（不影响主流程）
//! - 没有 TTL：用户显式 `abacus models discover` 才刷新

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCache {
    pub version: u32,
    pub discovered_at: i64, // unix timestamp
    pub providers: BTreeMap<String, Vec<String>>,
}

impl ModelCache {
    pub fn new() -> Self {
        Self {
            version: 1,
            discovered_at: chrono::Utc::now().timestamp(),
            providers: BTreeMap::new(),
        }
    }

    /// 默认 cache 路径：~/.abacus/models.cache.json
    pub fn default_path() -> PathBuf {
        std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(".abacus")
            .join("models.cache.json")
    }

    /// 从磁盘加载；文件不存在或解析失败返回 Ok(None)（视为 cold start）
    pub fn load(path: &Path) -> Result<Option<Self>, std::io::Error> {
        match std::fs::read_to_string(path) {
            Ok(s) => match serde_json::from_str::<Self>(&s) {
                Ok(c) => Ok(Some(c)),
                Err(_) => Ok(None), // 损坏视为不存在
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// 持久化到磁盘（atomic：先写 .tmp 再 rename）
    pub fn save(&self, path: &Path) -> Result<(), std::io::Error> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        let serialized = serde_json::to_string_pretty(self)
            .map_err(std::io::Error::other)?;
        std::fs::write(&tmp, serialized)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// 取所有 provider 的模型 union（去重排序）
    pub fn all_models(&self) -> Vec<String> {
        let mut all: Vec<String> = self.providers.values().flat_map(|v| v.iter().cloned()).collect();
        all.sort();
        all.dedup();
        all
    }
}

impl Default for ModelCache {
    fn default() -> Self { Self::new() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cache_round_trip() {
        let mut cache = ModelCache::new();
        cache.providers.insert("p1".into(), vec!["m1".into(), "m2".into()]);
        cache.providers.insert("p2".into(), vec!["m2".into(), "m3".into()]);

        let tmp_dir = std::env::temp_dir().join(format!("abacus_test_{}", chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)));
        std::fs::create_dir_all(&tmp_dir).unwrap();
        let path = tmp_dir.join("cache.json");

        cache.save(&path).unwrap();
        let loaded = ModelCache::load(&path).unwrap().unwrap();
        assert_eq!(loaded.providers.len(), 2);
        assert_eq!(loaded.all_models(), vec!["m1", "m2", "m3"]);

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn test_load_missing_returns_none() {
        let path = std::env::temp_dir().join("abacus_nonexistent_models.cache.json");
        let _ = std::fs::remove_file(&path);
        assert!(ModelCache::load(&path).unwrap().is_none());
    }
}
