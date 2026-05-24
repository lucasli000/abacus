//! undo::paths — 项目级 undo 子目录路径派生
//!
//! ## 引用关系
//! - 调用方：`undo::logger::UndoLogger::new()` 创建必要目录
//! - 依赖：`crate::paths::{project_dir, escape_cwd}`（D 模型路径约定）
//!
//! ## 生命周期
//! - 纯函数；无副作用（仅 ensure_* 调 create_dir_all）
//! - 路径形态：`<project_dir(cwd)>/undo/<session-uuid>/{log.jsonl, snapshots/}` + `_shared/lock.flock`
//!
//! ## 设计要点
//! - **项目内隔离**：跨项目互不干扰；同项目下多 session 各自子目录
//! - **`_shared/`**：跨 session 仅 undo 操作锁（Phase 3 才用），不锁正常写
//! - 路径派生与 `crate::paths::current_sessions_dir / current_logs_dir` 同一根

//! ## 设计契约
//! 所有 fn 接收**显式 project_dir**（即 `<global>/projects/<escaped-cwd>/`），不直接读 ABACUS_HOME。
//! 这让 undo 子系统的路径派生与 `crate::paths` 解耦——测试可注入 tempdir，
//! 避免改全局 env var 污染并发测试。生产入口在 `UndoLogger::new(cwd)` 中调
//! `crate::paths::project_dir(cwd)` 派生。

use std::path::{Path, PathBuf};

use crate::paths::project_dir;

/// 由 cwd 派生 project_dir（生产路径入口；底层走 paths::project_dir）
pub fn project_dir_from_cwd(cwd: &Path) -> PathBuf {
    project_dir(cwd)
}

/// 项目根 undo 容器：`<project_dir>/undo/`
pub fn project_undo_dir(project_dir: &Path) -> PathBuf {
    project_dir.join("undo")
}

/// 指定 session 的 undo 工作目录：`<project_dir>/undo/<session-uuid>/`
pub fn session_undo_dir(project_dir: &Path, session_id: &str) -> PathBuf {
    project_undo_dir(project_dir).join(session_id)
}

/// session 的 log.jsonl 路径
pub fn session_log_path(project_dir: &Path, session_id: &str) -> PathBuf {
    session_undo_dir(project_dir, session_id).join("log.jsonl")
}

/// session 的 snapshots 子目录
pub fn session_snapshot_dir(project_dir: &Path, session_id: &str) -> PathBuf {
    session_undo_dir(project_dir, session_id).join("snapshots")
}

/// 跨 session 共享目录（仅 undo 串行 flock 用，Phase 3）
pub fn project_shared_dir(project_dir: &Path) -> PathBuf {
    project_undo_dir(project_dir).join("_shared")
}

/// 跨 session undo 锁文件路径
pub fn project_undo_lock(project_dir: &Path) -> PathBuf {
    project_shared_dir(project_dir).join("lock.flock")
}

/// 确保 session undo 工作目录及子目录存在
///
/// 副作用：调 `std::fs::create_dir_all` 创建 `<project_dir>/undo/<session>/snapshots/`
/// 调用时机：`UndoLogger::new` 内（每个 session 一次）
pub fn ensure_session_undo_dirs(project_dir: &Path, session_id: &str) -> std::io::Result<()> {
    let snap = session_snapshot_dir(project_dir, session_id);
    std::fs::create_dir_all(&snap)?;
    Ok(())
}

/// 确保跨 session 共享目录存在（Phase 3 flock 前调用）
pub fn ensure_project_shared_dir(project_dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(project_shared_dir(project_dir))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_paths_are_under_project_undo() {
        // 直接传 project_dir，不经 ABACUS_HOME（无 env 污染）
        let project_dir = Path::new("/tmp/some-fake-project");
        let sid = "abc-123";
        let log = session_log_path(project_dir, sid);
        let snap = session_snapshot_dir(project_dir, sid);
        assert!(log.ends_with("undo/abc-123/log.jsonl"));
        assert!(snap.ends_with("undo/abc-123/snapshots"));
        let pud = project_undo_dir(project_dir);
        assert!(log.starts_with(&pud));
        assert!(snap.starts_with(&pud));
    }

    #[test]
    fn shared_path_distinct_from_session() {
        let project_dir = Path::new("/tmp/some-fake-project");
        let shared = project_shared_dir(project_dir);
        let session = session_undo_dir(project_dir, "any-uuid");
        assert!(shared.ends_with("undo/_shared"));
        assert_ne!(shared, session);
    }
}
