//! # MeetingStore — 多 Meeting 实例的共享容器
//!
//! ## 场景
//! 应用层（server / CLI 长时进程）需要同时持有多个进行中的 MeetingSessionHandle。
//! 该 store 提供与 `team::TeamManager` 同构的 register/get/list/remove API，
//! 让 routes 层不需要直接管理 Arc<RwLock<HashMap>>。
//!
//! ## 引用关系
//! - 被 `abacus-server::AppState` 持有为 `Arc<MeetingStore>`
//! - 持有 `Arc<RwLock<MeetingSessionHandle>>` × N（每个对应一个 Meeting）
//!
//! ## 生命周期
//! - 创建：`MeetingStore::new()`（应用启动时一次）
//! - 写入：`register()` 在 `POST /meetings` 成功后；`remove()` 在 `DELETE /meetings/:id`
//! - 读取：`get()` 在所有需要操作 handle 的 handler 中
//! - 销毁：随 server 进程终止（无显式 close）
//!
//! ## 边界
//! - `remove()` 在删除前调用 `cancel()` 转移到 `Cancelled` 状态（软取消）
//! - 内部使用 `tokio::sync::RwLock`（与 routes handler async 路径一致）
//! - 不做容量上限（应用层决定）；如需上限可后续在 register 添加 max check

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::meeting::builder::MeetingSessionHandle;

pub struct MeetingStore {
    inner: RwLock<HashMap<String, Arc<RwLock<MeetingSessionHandle>>>>,
}

impl MeetingStore {
    pub fn new() -> Self {
        Self { inner: RwLock::new(HashMap::new()) }
    }

    /// 注册一个新 Meeting handle，返回共享 Arc 句柄。
    pub async fn register(
        &self,
        id: String,
        handle: MeetingSessionHandle,
    ) -> Arc<RwLock<MeetingSessionHandle>> {
        let arc = Arc::new(RwLock::new(handle));
        self.inner.write().await.insert(id, arc.clone());
        arc
    }

    /// 按 id 查询。返回 None 表示不存在。
    pub async fn get(&self, id: &str) -> Option<Arc<RwLock<MeetingSessionHandle>>> {
        self.inner.read().await.get(id).cloned()
    }

    /// 返回所有 meeting id（顺序不保证）。
    pub async fn list(&self) -> Vec<String> {
        self.inner.read().await.keys().cloned().collect()
    }

    /// 软删除：先 cancel 转 Cancelled 状态，再从 map 移除。
    /// 返回 true 表示存在并已删除。
    pub async fn remove(&self, id: &str) -> bool {
        let removed = self.inner.write().await.remove(id);
        if let Some(handle) = removed {
            // 软取消，忽略状态机非法迁移错误（已 Completed 等）
            let _ = handle.write().await.cancel();
            true
        } else {
            false
        }
    }
}

impl Default for MeetingStore {
    fn default() -> Self { Self::new() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meeting::builder::MeetingSessionBuilder;

    #[tokio::test]
    async fn test_store_register_list_get_remove() {
        let store = MeetingStore::new();
        assert!(store.list().await.is_empty());

        let handle = MeetingSessionBuilder::new("t1")
            .with_specialist("coder")
            .build().await.expect("build meeting");
        store.register("m1".into(), handle).await;

        let ids = store.list().await;
        assert_eq!(ids.len(), 1);
        assert_eq!(ids[0], "m1");

        assert!(store.get("m1").await.is_some());
        assert!(store.get("m2").await.is_none());

        assert!(store.remove("m1").await);
        assert!(!store.remove("m1").await); // 幂等
        assert!(store.list().await.is_empty());
    }
}
