//! undo::logger — UndoLogger + PendingEntry
//!
//! ## 引用关系
//! - 调用方（Phase 2）：`tool::builtin::filengine::NativeFilengine::execute()` 在写工具前后注入
//! - 依赖：`undo::{paths, entry, storage}` + `tokio::fs` + `sha2`
//!
//! ## 生命周期
//! - 创建：每个 session 启动时 `UndoLogger::new(cwd, session_id)` 一次
//! - 持有：随 `FilengineSession` 共享（Arc<UndoLogger>）
//! - 销毁：session 退出时 drop；log.jsonl + snapshots 留在磁盘等过期清理（Phase 7）
//!
//! ## 关键状态
//! - `seq_counter: AtomicU64` — 进程级单调递增（启动时从 log.jsonl 末行恢复）
//! - 日志/快照都是文件副作用，无内存缓存（断电安全）
//!
//! ## 设计要点
//! - **two-phase commit**：snapshot_before/record_* 立即写 snapshot 但不写 log；
//!   tool 执行成功后 PendingEntry::commit 才 append log entry。失败的工具不留半截 log。
//! - **容量回收**：每次 commit 后检查行数 > 100 → 摊销修剪最早。零稳态开销，超量时一次 trim。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::Utc;
use tokio::sync::Mutex;

use super::entry::{FileMeta, LogEntry, OpKind};
use super::paths::{ensure_session_undo_dirs, project_dir_from_cwd, session_log_path, session_snapshot_dir};
use super::storage::{PlainSnapshotStorage, SnapshotStorage, sha256_hex};

/// 容量上限（每 session）— 超出修剪最早，决策点见 Phase 1 spawn prompt
const MAX_LOG_ENTRIES: usize = 100;

/// undo 操作日志记录器
///
/// 字段：
/// - `cwd`：用于 log path 解析（始终是 session 启动时的 cwd）
/// - `session_id`：UUID，与 sessions/<uuid>.json 同源
/// - `log_path` / `snapshot_dir`：派生缓存，节省每次重算
/// - `seq_counter`：进程级单调递增；启动时从 log.jsonl 末行恢复
/// - `storage`：snapshot 后端（默认 PlainSnapshotStorage；决策 1 推迟）
/// - `log_lock`：log.jsonl 文件级写锁——避免同进程多线程交错（Phase 2 多 turn 并发可能）
pub struct UndoLogger {
    /// project_dir（已派生，不再追溯 cwd 或 ABACUS_HOME）
    /// 形态：`<global>/projects/<escaped-cwd>/`，但测试可直接传 tempdir 子路径
    project_dir: PathBuf,
    session_id: String,
    log_path: PathBuf,
    snapshot_dir: PathBuf,
    seq_counter: AtomicU64,
    storage: Arc<dyn SnapshotStorage>,
    log_lock: Mutex<()>,
}

impl UndoLogger {
    /// 创建 logger（生产入口）— 由 cwd 派生 project_dir
    ///
    /// 内部调用 `crate::paths::project_dir(cwd)`，因此**依赖 ABACUS_HOME** 解析全局根。
    /// 测试请用 `new_at(project_dir, session_id)` 直接注入路径，避免 env 污染。
    pub fn new(cwd: &Path, session_id: String) -> std::io::Result<Self> {
        Self::new_at(project_dir_from_cwd(cwd), session_id)
    }

    /// 创建 logger（注入入口）— 直接指定 project_dir，绕开 paths::project_dir / ABACUS_HOME
    ///
    /// 用途：
    /// - 测试：传 tempdir 子路径，纯净隔离
    /// - 未来 sandbox/integration：复用项目目录但不修改全局 env
    pub fn new_at(project_dir: PathBuf, session_id: String) -> std::io::Result<Self> {
        ensure_session_undo_dirs(&project_dir, &session_id)?;
        let log_path = session_log_path(&project_dir, &session_id);
        let snapshot_dir = session_snapshot_dir(&project_dir, &session_id);

        // 启动时恢复 seq — 读最后一行的 seq + 1；空文件从 1 开始
        let next_seq = recover_next_seq(&log_path)?;

        Ok(Self {
            project_dir,
            session_id,
            log_path,
            snapshot_dir,
            seq_counter: AtomicU64::new(next_seq),
            storage: Arc::new(PlainSnapshotStorage),
            log_lock: Mutex::new(()),
        })
    }

    /// 注入自定义 storage（测试 / Phase >1 切 zstd 用）
    pub fn with_storage(mut self, storage: Arc<dyn SnapshotStorage>) -> Self {
        self.storage = storage;
        self
    }

    pub fn session_id(&self) -> &str { &self.session_id }
    pub fn log_path(&self) -> &Path { &self.log_path }
    pub fn snapshot_dir(&self) -> &Path { &self.snapshot_dir }
    pub fn project_dir(&self) -> &Path { &self.project_dir }

    /// 派发下一个 seq（线程安全）
    fn next_seq(&self) -> u64 {
        self.seq_counter.fetch_add(1, Ordering::SeqCst)
    }

    /// 写操作前 snapshot — 用于 fs.write / fs.edit
    ///
    /// 路径**必须是绝对**（filengine 已在 Phase 2 调 resolve 后传入）。
    /// 路径不存在 → before_snapshot=None, op=Create（Phase 3 撤销=delete）
    /// 路径存在 → 读内容写 snapshot，op=Overwrite（fs.write）或 Edit（fs.edit）
    pub async fn snapshot_before(&self, tool: &str, abs_path: &Path) -> std::io::Result<PendingEntry> {
        let seq = self.next_seq();
        let path_str = abs_path.to_string_lossy().to_string();

        let (op, before_snapshot, before_meta) = match tokio::fs::read(abs_path).await {
            Ok(content) => {
                let hash = sha256_hex(&content);
                let filename = self.storage.store(&self.snapshot_dir, seq, &hash, &content).await?;
                let mtime = systime_to_utc(tokio::fs::metadata(abs_path).await.ok().and_then(|m| m.modified().ok()));
                let meta = FileMeta { size: content.len() as u64, sha256: hash, mtime };
                let kind = if tool == "fs_edit" { OpKind::Edit } else { OpKind::Overwrite };
                (kind, Some(filename), Some(meta))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                (OpKind::Create, None, None)
            }
            Err(e) => return Err(e),
        };

        let mut entry = LogEntry::new(seq, self.session_id.clone(), 0, tool.into(), path_str, op);
        entry.before_snapshot = before_snapshot;
        entry.before_meta = before_meta;

        Ok(PendingEntry { entry, abs_path: abs_path.to_path_buf() })
    }

    /// fs.move 记录（不快照内容）— src/dst 都是绝对路径
    pub async fn record_move(&self, src_abs: &Path, dst_abs: &Path) -> std::io::Result<PendingEntry> {
        let seq = self.next_seq();
        let mut entry = LogEntry::new(
            seq, self.session_id.clone(), 0,
            "fs_move".into(),
            src_abs.to_string_lossy().to_string(),
            OpKind::Move,
        );
        entry.move_to = Some(dst_abs.to_string_lossy().to_string());

        // before_meta：src 当前的 sha256 / size（撤销时验 dst 是否仍是这个内容）
        if let Ok(content) = tokio::fs::read(src_abs).await {
            let hash = sha256_hex(&content);
            let mtime = systime_to_utc(tokio::fs::metadata(src_abs).await.ok().and_then(|m| m.modified().ok()));
            entry.before_meta = Some(FileMeta { size: content.len() as u64, sha256: hash, mtime });
        }

        Ok(PendingEntry { entry, abs_path: dst_abs.to_path_buf() })
    }

    /// fs.mkdir 记录（不快照）
    pub async fn record_mkdir(&self, dir_abs: &Path) -> std::io::Result<PendingEntry> {
        let seq = self.next_seq();
        let entry = LogEntry::new(
            seq, self.session_id.clone(), 0,
            "fs_mkdir".into(),
            dir_abs.to_string_lossy().to_string(),
            OpKind::Mkdir,
        );
        Ok(PendingEntry { entry, abs_path: dir_abs.to_path_buf() })
    }

    /// 内部：append 一行 + 容量回收（Phase 2 起 pub(crate) 给 PendingEntry 调用）
    pub(crate) async fn append_log_line(&self, entry: &LogEntry) -> std::io::Result<()> {
        let _g = self.log_lock.lock().await;
        let line = serde_json::to_string(entry)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let mut existing = match tokio::fs::read_to_string(&self.log_path).await {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e),
        };
        existing.push_str(&line);
        existing.push('\n');

        // 容量回收：行数 > MAX → 仅保留末 MAX 行
        let total_lines = existing.lines().count();
        let final_content = if total_lines > MAX_LOG_ENTRIES {
            let keep_from = total_lines - MAX_LOG_ENTRIES;
            let kept: Vec<&str> = existing.lines().skip(keep_from).collect();
            // prune 被丢弃行的 snapshot 文件（best-effort，不阻塞）
            for line in existing.lines().take(keep_from) {
                if let Ok(e) = serde_json::from_str::<LogEntry>(line) {
                    if let Some(ref fname) = e.before_snapshot {
                        let _ = self.storage.prune(&self.snapshot_dir, fname).await;
                    }
                }
            }
            let mut s = kept.join("\n");
            s.push('\n');
            s
        } else {
            existing
        };

        tokio::fs::write(&self.log_path, final_content.as_bytes()).await?;
        Ok(())
    }
}

/// 待提交 entry — 由 snapshot_before/record_* 返回，工具执行成功后 commit
///
/// 生命周期：
/// - 创建：snapshot_before 等方法返回 PendingEntry（snapshot 已落盘但 log 未写）
/// - 销毁：commit 后；或 drop 时丢弃（snapshot 文件留下，下次 commit 才 prune）
///
/// **设计**：不持有 `&UndoLogger` reference — 让 PendingEntry 可跨 await 传递
/// （如 ToolExecutor::execute 中 native.execute().await 期间持有）。commit 时
/// 由调用方传 logger 引用。
pub struct PendingEntry {
    entry: LogEntry,
    /// 绝对路径（计算 after_meta 用）
    abs_path: PathBuf,
}

impl PendingEntry {
    pub fn seq(&self) -> u64 { self.entry.seq }
    pub fn op(&self) -> OpKind { self.entry.op }

    /// 工具执行成功后 commit — 计算 after_meta、append log
    ///
    /// turn：来自 ExecutionContext.turn_number（Phase 2 注入）
    /// logger：从 FilengineSession.undo_logger 取（Arc 借用 deref）
    pub async fn commit(mut self, turn: u32, logger: &UndoLogger) -> std::io::Result<()> {
        self.entry.turn = turn;
        self.entry.timestamp = Utc::now();

        // after_meta：Create/Overwrite/Edit/Move 都重读文件元数据；Mkdir 跳过
        match self.entry.op {
            OpKind::Create | OpKind::Overwrite | OpKind::Edit | OpKind::Move => {
                if let Ok(content) = tokio::fs::read(&self.abs_path).await {
                    let hash = sha256_hex(&content);
                    let mtime = systime_to_utc(
                        tokio::fs::metadata(&self.abs_path).await.ok().and_then(|m| m.modified().ok())
                    );
                    self.entry.after_meta = Some(FileMeta {
                        size: content.len() as u64, sha256: hash, mtime
                    });
                }
            }
            OpKind::Mkdir => {}
        }

        logger.append_log_line(&self.entry).await
    }

    /// 暴露内部 entry 用于测试
    #[cfg(test)]
    pub fn entry(&self) -> &LogEntry { &self.entry }
}

/// 把 SystemTime 转 chrono::DateTime<Utc>，None / 转换失败时用 Utc::now() fallback
fn systime_to_utc(t: Option<std::time::SystemTime>) -> chrono::DateTime<Utc> {
    t.map(chrono::DateTime::<Utc>::from).unwrap_or_else(Utc::now)
}

/// 启动时从 log.jsonl 末行恢复 next_seq
///
/// - 文件不存在 → 1
/// - 末行解析失败 → 安全 fallback：扫所有合法行取 max(seq) + 1
fn recover_next_seq(log_path: &Path) -> std::io::Result<u64> {
    let content = match std::fs::read_to_string(log_path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(1),
        Err(e) => return Err(e),
    };
    let mut max_seq = 0u64;
    for line in content.lines() {
        if let Ok(e) = serde_json::from_str::<LogEntry>(line) {
            max_seq = max_seq.max(e.seq);
        }
    }
    Ok(max_seq + 1)
}
