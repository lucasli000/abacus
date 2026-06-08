//! undo::storage — snapshot 后端抽象 + plain 默认实现
//!
//! ## 引用关系
//! - 调用方：`undo::logger::PendingEntry::commit()` 写 snapshot；Phase 3 `UndoEngine` 读 snapshot
//! - 依赖：`tokio::fs`（异步 I/O）；sha2 在 logger 层算 hash
//!
//! ## 设计意图（决策 1 推迟）
//! `SnapshotStorage` trait 抽象后端 → Phase 1 仅 plain；后期 zstd 仅替换实现，logger / engine 不动。
//! 命名约定：`{seq:08}-{sha256_prefix:8}.bin`（8 位 zfill seq 容纳 1e8 操作；8 字符 hash prefix 防同 seq 冲突）
//!
//! ## 生命周期
//! - 创建：`UndoLogger::new` 时实例化（Arc 持有，跨多个 commit 复用）
//! - 销毁：随 logger drop；snapshot 文件本身由 `prune` 显式清理

use std::path::Path;

use async_trait::async_trait;

/// snapshot 后端 trait
#[async_trait]
pub trait SnapshotStorage: Send + Sync {
    /// 写 snapshot 内容；返回 snapshot 文件名（不含目录，相对 snapshot_dir）
    async fn store(&self, snapshot_dir: &Path, seq: u64, sha256_hex: &str, content: &[u8])
        -> Result<String, std::io::Error>;

    /// 读 snapshot 内容（snapshot_dir + filename）
    async fn load(&self, snapshot_dir: &Path, filename: &str) -> Result<Vec<u8>, std::io::Error>;

    /// 删除 snapshot 文件（修剪/撤销后清理）
    async fn prune(&self, snapshot_dir: &Path, filename: &str) -> Result<(), std::io::Error>;
}

/// 默认明文后端：直接写 .bin
///
/// 决策 1 推迟：未来切 zstd 只替换此处 → store/load 加压解压
pub struct PlainSnapshotStorage;

#[async_trait]
impl SnapshotStorage for PlainSnapshotStorage {
    async fn store(&self, snapshot_dir: &Path, seq: u64, sha256_hex: &str, content: &[u8])
        -> Result<String, std::io::Error>
    {
        // 命名：{seq:08}-{sha256_prefix:8}.bin
        // 8 位 zfill seq 排序友好（lexicographic == numeric for same width）
        let prefix = &sha256_hex[..sha256_hex.len().min(8)];
        let filename = format!("{seq:08}-{prefix}.bin");
        let path = snapshot_dir.join(&filename);
        tokio::fs::write(&path, content).await?;
        Ok(filename)
    }

    async fn load(&self, snapshot_dir: &Path, filename: &str) -> Result<Vec<u8>, std::io::Error> {
        let path = snapshot_dir.join(filename);
        tokio::fs::read(&path).await
    }

    async fn prune(&self, snapshot_dir: &Path, filename: &str) -> Result<(), std::io::Error> {
        let path = snapshot_dir.join(filename);
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            // 已不存在视作成功（幂等清理）
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }
}

/// 计算 sha256 hex
///
/// 工具函数 — logger 在 store 前调用以派生文件名 prefix
pub fn sha256_hex(content: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(content);
    let mut s = String::with_capacity(64);
    for b in digest.iter() {
        use std::fmt::Write;
        // Writing to a String can never fail; the use_import_braces lint flags unused Result.
        write!(&mut s, "{b:02x}").expect("writing to String is infallible");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn snap_dir() -> (TempDir, PathBuf) {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().to_path_buf();
        (tmp, p)
    }

    #[tokio::test]
    async fn plain_store_and_load_round_trip() {
        let (_keep, dir) = snap_dir();
        let storage = PlainSnapshotStorage;
        let content = b"hello snapshot";
        let hash = sha256_hex(content);
        let filename = storage.store(&dir, 7, &hash, content).await.unwrap();
        // 命名形态校验
        assert!(filename.starts_with("00000007-"));
        assert!(filename.ends_with(".bin"));
        // 读回内容一致
        let loaded = storage.load(&dir, &filename).await.unwrap();
        assert_eq!(loaded, content);
    }

    #[tokio::test]
    async fn plain_prune_idempotent_when_missing() {
        let (_keep, dir) = snap_dir();
        let storage = PlainSnapshotStorage;
        // 不存在的文件 prune 不报错
        let r = storage.prune(&dir, "nonexistent.bin").await;
        assert!(r.is_ok());
    }

    #[test]
    fn sha256_hex_known_vector() {
        // SHA256("abc") = ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad
        let h = sha256_hex(b"abc");
        assert_eq!(h, "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }
}
