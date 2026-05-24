//! undo — 文件操作撤销基础设施（Phase 1 数据层）
//!
//! ## 设计文档
//! `docs/design/file-undo.md` 完整设计；本模块仅落地 § 3.1 数据层 + § 3.2.1 接口骨架
//!
//! ## 子模块
//! - `paths`：项目级 undo 子目录派生（接收显式 project_dir，与 ABACUS_HOME 解耦）
//! - `entry`：log.jsonl 单条记录定义
//! - `storage`：snapshot 后端 trait + plain 默认实现（决策 1 推迟）
//! - `logger`：UndoLogger + PendingEntry（two-phase commit）
//!
//! ## 引用关系（横向）
//! - 上游：`crate::paths`（仅 `UndoLogger::new(cwd)` 生产入口；测试用 `new_at`）
//! - 下游（Phase 2）：`crate::tool::builtin::filengine`（写工具注入）
//! - 下游（Phase 3）：`crate::undo::engine` 待实现的 UndoEngine
//!
//! ## 生命周期
//! - 模块本身无状态；UndoLogger 实例由 SessionState 持有（Arc）
//! - log.jsonl 与 snapshot 文件由 Phase 7 清理 hook 在 7 天后过期回收

pub mod paths;
pub mod entry;
pub mod storage;
pub mod logger;
pub mod engine;

pub use entry::{LogEntry, OpKind, FileMeta};
pub use logger::{UndoLogger, PendingEntry};
pub use storage::{SnapshotStorage, PlainSnapshotStorage, sha256_hex};
pub use engine::{UndoEngine, UndoResult, UndoAction, UndoConflict, HistoryEntry};

#[cfg(test)]
mod tests {
    //! 测试用 `UndoLogger::new_at(tempdir_path, sid)` 直接注入 project_dir，
    //! **不改 ABACUS_HOME**——避免污染并发跑的其他测试。

    use super::*;
    use tempfile::TempDir;

    /// 在 tempdir 内创建一个隔离的 logger（测试 helper）
    /// 返回 (TempDir, UndoLogger)：TempDir 必须保持 in-scope 否则被 drop 清理
    fn setup_logger(session_id: &str) -> (TempDir, UndoLogger) {
        let tmp = TempDir::new().unwrap();
        // tempdir 本身就当作 project_dir 使用（结构是 <project_dir>/undo/<sid>/...）
        let logger = UndoLogger::new_at(tmp.path().to_path_buf(), session_id.into()).unwrap();
        (tmp, logger)
    }

    // ─── snapshot_before：内容快照 ─────────────────────────

    #[tokio::test]
    async fn snapshot_before_creates_bin_with_correct_content() {
        let (tmp, logger) = setup_logger("sess-1");
        let target = tmp.path().join("target.txt");
        std::fs::write(&target, b"original content").unwrap();

        let pending = logger.snapshot_before("fs_write", &target).await.unwrap();
        let entry = pending.entry();
        assert_eq!(entry.op, OpKind::Overwrite);

        let snap_name = entry.before_snapshot.as_ref().expect("snapshot filename");
        // 命名形态：{seq:08}-{prefix}.bin
        assert!(snap_name.starts_with("00000001-"));
        assert!(snap_name.ends_with(".bin"));

        let snap_path = logger.snapshot_dir().join(snap_name);
        let snap_content = std::fs::read(&snap_path).unwrap();
        assert_eq!(snap_content, b"original content");

        let bm = entry.before_meta.as_ref().unwrap();
        assert_eq!(bm.size, 16);
    }

    #[tokio::test]
    async fn snapshot_before_nonexistent_path_yields_create_op() {
        let (tmp, logger) = setup_logger("sess-2");
        let target = tmp.path().join("nonexistent.txt");
        let pending = logger.snapshot_before("fs_write", &target).await.unwrap();
        let entry = pending.entry();
        assert_eq!(entry.op, OpKind::Create);
        assert!(entry.before_snapshot.is_none());
        assert!(entry.before_meta.is_none());
    }

    // ─── record_move / record_mkdir：不快照 ────────────────

    #[tokio::test]
    async fn record_move_does_not_create_snapshot_file() {
        let (tmp, logger) = setup_logger("sess-mv");
        let src = tmp.path().join("a.txt");
        std::fs::write(&src, b"x").unwrap();
        let dst = tmp.path().join("b.txt");

        let pending = logger.record_move(&src, &dst).await.unwrap();
        let entry = pending.entry();
        assert_eq!(entry.op, OpKind::Move);
        assert!(entry.before_snapshot.is_none(), "move 不应写 snapshot");
        assert_eq!(entry.move_to.as_deref(), Some(dst.to_string_lossy().as_ref()));
        // before_meta 仍计算（撤销时校验 dst 内容）
        assert!(entry.before_meta.is_some());

        // snapshot dir 应为空
        let n_snaps = std::fs::read_dir(logger.snapshot_dir()).unwrap().count();
        assert_eq!(n_snaps, 0);
    }

    #[tokio::test]
    async fn record_mkdir_does_not_create_snapshot() {
        let (tmp, logger) = setup_logger("sess-mkdir");
        let dir = tmp.path().join("newdir");
        let pending = logger.record_mkdir(&dir).await.unwrap();
        assert_eq!(pending.entry().op, OpKind::Mkdir);
        assert!(pending.entry().before_snapshot.is_none());
        assert!(pending.entry().before_meta.is_none());
    }

    // ─── commit：append log + after_meta ─────────────────

    #[tokio::test]
    async fn commit_appends_valid_jsonl_line() {
        let (tmp, logger) = setup_logger("sess-c");
        let target = tmp.path().join("commit-test.txt");
        std::fs::write(&target, b"hello").unwrap();

        let pending = logger.snapshot_before("fs_write", &target).await.unwrap();
        // 模拟工具执行后落盘
        std::fs::write(&target, b"world!").unwrap();
        pending.commit(7, &logger).await.unwrap();

        let log_content = std::fs::read_to_string(logger.log_path()).unwrap();
        let line = log_content.lines().next().unwrap();
        let entry: LogEntry = serde_json::from_str(line).unwrap();
        assert_eq!(entry.seq, 1);
        assert_eq!(entry.turn, 7);
        assert_eq!(entry.tool, "fs_write");
        let am = entry.after_meta.as_ref().unwrap();
        assert_eq!(am.size, 6);
    }

    // ─── seq counter：严格递增 ────────────────────────────

    #[tokio::test]
    async fn seq_counter_strictly_monotonic_across_commits() {
        let (tmp, logger) = setup_logger("sess-seq");
        let mut seqs = Vec::new();
        for i in 0..5 {
            let p = tmp.path().join(format!("f{i}.txt"));
            std::fs::write(&p, b"x").unwrap();
            let pending = logger.snapshot_before("fs_write", &p).await.unwrap();
            seqs.push(pending.seq());
            pending.commit(0, &logger).await.unwrap();
        }
        assert_eq!(seqs, vec![1, 2, 3, 4, 5]);
    }

    // ─── 容量回收：> 100 条修剪最早 ────────────────────────

    #[tokio::test]
    async fn log_trims_to_max_entries_after_overflow() {
        let (tmp, logger) = setup_logger("sess-trim");
        for i in 0..105 {
            let p = tmp.path().join(format!("f{i}.txt"));
            std::fs::write(&p, b"x").unwrap();
            let pending = logger.snapshot_before("fs_write", &p).await.unwrap();
            pending.commit(0, &logger).await.unwrap();
        }
        let log_content = std::fs::read_to_string(logger.log_path()).unwrap();
        let lines: Vec<&str> = log_content.lines().collect();
        assert_eq!(lines.len(), 100, "应仅保留 MAX_LOG_ENTRIES 行");
        let first: LogEntry = serde_json::from_str(lines[0]).unwrap();
        let last: LogEntry = serde_json::from_str(lines.last().unwrap()).unwrap();
        assert_eq!(first.seq, 6, "最早应是 seq=6（1..=5 被修剪）");
        assert_eq!(last.seq, 105);
    }

    // ─── seq 恢复：重新打开 logger 从末行+1 续 ──────────────

    #[tokio::test]
    async fn next_seq_recovers_from_existing_log() {
        let tmp = TempDir::new().unwrap();
        let project_dir = tmp.path().to_path_buf();

        // 第一阶段：写 3 条 commit
        {
            let logger = UndoLogger::new_at(project_dir.clone(), "sess-recover".into()).unwrap();
            for i in 0..3 {
                let p = tmp.path().join(format!("r{i}.txt"));
                std::fs::write(&p, b"x").unwrap();
                let pending = logger.snapshot_before("fs_write", &p).await.unwrap();
                pending.commit(0, &logger).await.unwrap();
            }
        }
        // 第二阶段：重启 logger，下一次 seq 应从 4 开始
        let logger2 = UndoLogger::new_at(project_dir, "sess-recover".into()).unwrap();
        let p = tmp.path().join("r-after.txt");
        std::fs::write(&p, b"y").unwrap();
        let pending = logger2.snapshot_before("fs_write", &p).await.unwrap();
        assert_eq!(pending.seq(), 4, "重启后 seq 从末行+1 续");
    }

    // ─── 生产入口：UndoLogger::new(cwd) 透传 paths::project_dir ───

    #[test]
    fn production_entry_compiles() {
        // 仅编译期断言：UndoLogger::new 接收 &Path + String，与 logger.rs API 同步
        // 不实际执行，避免改 ABACUS_HOME 污染其他测试
        fn _check(cwd: &std::path::Path, sid: String) -> std::io::Result<UndoLogger> {
            UndoLogger::new(cwd, sid)
        }
        let _ = _check; // suppress unused warning
    }
}
