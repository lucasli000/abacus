//! undo::engine — UndoEngine（撤销 / 重做 / 历史 / 跨 session 审计）
//!
//! ## 引用关系
//! - 上游：`undo::{paths, entry, storage, logger}` 数据层
//! - 下游（Phase 4）：TUI slash commands；Phase 5 CLI subcommand；Phase 6 timeline
//!
//! ## 生命周期
//! - 创建：CoreLoop 启动时一次（持 Arc<UndoEngine>），跨 session 复用
//! - 销毁：进程退出时；redo 栈丢失（决策 3：内存式）
//!
//! ## 关键状态
//! - `undo_lock: Mutex<()>` — 进程内 undo 操作串行；跨进程并发是 known limitation
//! - `redo_stack: Mutex<HashMap<session_id, Vec<RedoFrame>>>` — 每 session 独立 redo 栈
//!
//! ## 撤销决策表
//! | OpKind   | 撤销动作         | 冲突检查（return UndoConflict）                     |
//! |----------|------------------|----------------------------------------------------|
//! | Create   | remove_file      | FileGone（已被外部删）/ ExternalModification (sha) |
//! | Overwrite| 写回 snapshot    | FileGone / ExternalModification                    |
//! | Edit     | 同 Overwrite     | 同上                                               |
//! | Move     | rename(dst, src) | DestinationOccupied(src 已存在) / ExternalModification |
//! | Mkdir    | remove_dir       | DirectoryNotEmpty / FileGone                       |
//!
//! ## 跨进程并发（known limitation）
//! 当前 lock 仅进程内。两个 abacus 实例同 cwd 同时 `/undo` 在 redo 栈和 log
//! 写入上可能交错——属"病态场景"，文档化不阻止。未来引入 fs2/nix advisory
//! lock 仅需替换 `undo_lock` 实现。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use tokio::sync::Mutex;

use super::entry::{LogEntry, OpKind};
use super::paths::{
    project_undo_dir, session_log_path, session_snapshot_dir,
};
use super::storage::{sha256_hex, PlainSnapshotStorage, SnapshotStorage};

/// 撤销结果（成功的真实改动 + 冲突的诊断结果）
#[derive(Debug, Clone)]
pub struct UndoResult {
    pub seq: u64,
    pub session_id: String,
    pub path: PathBuf,
    pub action: UndoAction,
    /// 非 None = 冲突；上层 UI 决定是否 force
    pub conflict: Option<UndoConflict>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UndoAction {
    /// 把 snapshot 内容写回 path（Create 撤销 = 删，写回为 RemovedFile）
    RestoredContent,
    RemovedFile,
    RemovedDir,
    ReverseMoved,
    /// 因冲突中止
    Aborted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UndoConflict {
    /// 文件 sha256 不再是 after_meta 期望值
    ExternalModification {
        observed_sha256: String,
        expected_sha256: String,
    },
    /// 文件已被外部删除
    FileGone,
    /// mkdir 撤销时目录非空
    DirectoryNotEmpty { entries: Vec<String> },
    /// move 撤销时 src 位置已被占用（不能 rename 回去）
    DestinationOccupied,
}

/// History 列表条目
#[derive(Debug, Clone)]
pub struct HistoryEntry {
    pub seq: u64,
    pub session_id: String,
    pub turn: u32,
    pub timestamp: DateTime<Utc>,
    pub tool: String,
    pub path: String,
    pub op: OpKind,
    pub undone: bool,
}

/// Redo 栈帧 — 由 undo 操作产生，redo 时消费
struct RedoFrame {
    log_entry: LogEntry,
    /// undo 前的"现存"文件内容快照（redo 时写回此内容）
    /// None 表示 undo 前文件本就不存在（即原 op=Create 已被外部删）
    saved_post_undo_content: Option<Vec<u8>>,
}

/// 读取 session 的所有 entries（free fn，治本：可被 spawn_blocking 调用而不持有 &self）
///
/// ## 为什么是 free fn
/// 原 `UndoEngine::read_session_entries(&self, ...)` 是同步方法，被 async 函数
/// (`undo_last` 等) 调用时直接阻塞 worker thread。治本：把 I/O 部分抽成 free fn，
/// async 路径用 `tokio::task::spawn_blocking` 隔离阻塞工作。
pub(crate) fn read_session_entries_at(
    project_dir: &std::path::Path,
    session_id: &str,
) -> std::io::Result<Vec<LogEntry>> {
    let log_path = super::paths::session_log_path(project_dir, session_id);
    let content = match std::fs::read_to_string(&log_path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut out = Vec::new();
    for line in content.lines() {
        if line.is_empty() {
            continue;
        }
        if let Ok(e) = serde_json::from_str::<LogEntry>(line) {
            out.push(e);
        }
    }
    Ok(out)
}

/// 撤销引擎
pub struct UndoEngine {
    project_dir: PathBuf,
    storage: Arc<dyn SnapshotStorage>,
    /// 进程内 undo 串行
    undo_lock: Mutex<()>,
    /// per-session redo 栈
    redo_stack: Mutex<HashMap<String, Vec<RedoFrame>>>,
}

impl UndoEngine {
    pub fn new(project_dir: PathBuf) -> Self {
        Self {
            project_dir,
            storage: Arc::new(PlainSnapshotStorage),
            undo_lock: Mutex::new(()),
            redo_stack: Mutex::new(HashMap::new()),
        }
    }

    pub fn with_storage(mut self, storage: Arc<dyn SnapshotStorage>) -> Self {
        self.storage = storage;
        self
    }

    pub fn project_dir(&self) -> &Path {
        &self.project_dir
    }

    // ─── 读 log 工具 ─────────────────────────────────────────────

    /// 列出指定 session 的所有 entries（已撤销的也列出）
    fn read_session_entries(&self, session_id: &str) -> std::io::Result<Vec<LogEntry>> {
        read_session_entries_at(&self.project_dir, session_id)
    }

    /// 列出所有 session（扫 project/undo/<*> 目录，排除 _shared）
    fn list_sessions(&self) -> std::io::Result<Vec<String>> {
        let undo_root = project_undo_dir(&self.project_dir);
        let mut out = Vec::new();
        let entries = match std::fs::read_dir(&undo_root) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name == "_shared" {
                continue;
            }
            if e.metadata().ok().map(|m| m.is_dir()).unwrap_or(false) {
                out.push(name);
            }
        }
        Ok(out)
    }

    /// 重写 session log.jsonl（撤销时回填 undone 标记）
    fn rewrite_log(&self, session_id: &str, entries: &[LogEntry]) -> std::io::Result<()> {
        let log_path = session_log_path(&self.project_dir, session_id);
        let mut content = String::new();
        for e in entries {
            content.push_str(&serde_json::to_string(e).map_err(|e| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, e)
            })?);
            content.push('\n');
        }
        std::fs::write(log_path, content)
    }

    // ─── undo ─────────────────────────────────────────────────────

    /// 撤销最后一条**未撤销**的 entry（指定 session 或全 session 选最新）
    pub async fn undo_last(&self, session_id: Option<&str>) -> std::io::Result<UndoResult> {
        let _g = self.undo_lock.lock().await;

        // 选目标 session：显式 → 用之；否则跨 session 取 timestamp 最大未撤销
        let target_session = match session_id {
            Some(s) => s.to_string(),
            None => self.find_latest_active_session().await?,
        };
        // 🟡#8/#14 治本：read_session_entries 含 std::fs::read_to_string + 反序列化，
        // 阻塞 syscall 不能在 async fn 里直接调（worker thread 阻塞会 stall reactor）。
        // 治本：spawn_blocking 隔离阻塞工作，让 tokio 继续调度其他 task。
        let project_dir = self.project_dir.clone();
        let target_session_clone = target_session.clone();
        let entries = tokio::task::spawn_blocking(move || {
            read_session_entries_at(&project_dir, &target_session_clone)
        })
        .await
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other,
            format!("spawn_blocking panicked: {e}")))??;
        let target = entries
            .iter().rfind(|e| !e.undone)
            .cloned()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no undoable entry"))?;
        self.do_undo_internal(&target_session, &target, &entries).await
    }

    /// 撤销指定 seq
    pub async fn undo_seq(&self, session_id: &str, seq: u64) -> std::io::Result<UndoResult> {
        let _g = self.undo_lock.lock().await;
        // 🟡#8/#14 治本：spawn_blocking 隔离阻塞 I/O
        let project_dir = self.project_dir.clone();
        let session_id_owned = session_id.to_string();
        let entries = tokio::task::spawn_blocking(move || {
            read_session_entries_at(&project_dir, &session_id_owned)
        })
        .await
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other,
            format!("spawn_blocking panicked: {e}")))??;
        let target = entries
            .iter()
            .find(|e| e.seq == seq && !e.undone)
            .cloned()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "seq not found or already undone"))?;
        self.do_undo_internal(session_id, &target, &entries).await
    }

    /// 撤销整个 turn 的所有 op（按 seq 倒序，最新先撤）
    pub async fn undo_turn(&self, session_id: &str, turn: u32) -> std::io::Result<Vec<UndoResult>> {
        let _g = self.undo_lock.lock().await;
        // 🟡#8/#14 治本
        let project_dir = self.project_dir.clone();
        let session_id_owned = session_id.to_string();
        let entries = tokio::task::spawn_blocking(move || {
            read_session_entries_at(&project_dir, &session_id_owned)
        })
        .await
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other,
            format!("spawn_blocking panicked: {e}")))??;
        let targets: Vec<LogEntry> = entries.iter()
            .filter(|e| e.turn == turn && !e.undone)
            .cloned()
            .collect();
        let mut results = Vec::new();
        // 倒序撤销：最新 seq 先
        for t in targets.iter().rev() {
            // 每次内层调用会重新读 log（含上一轮的 undone 标记）；保持一致性
            let cur_entries = self.read_session_entries(session_id)?;
            results.push(self.do_undo_internal(session_id, t, &cur_entries).await?);
        }
        Ok(results)
    }

    /// 内部：执行单条 undo + 回填 undone + 入 redo 栈
    async fn do_undo_internal(
        &self,
        session_id: &str,
        target: &LogEntry,
        entries: &[LogEntry],
    ) -> std::io::Result<UndoResult> {
        let path = PathBuf::from(&target.path);

        // 撤销前读当前文件内容（用于 redo 栈）
        let pre_undo_content = tokio::fs::read(&path).await.ok();

        let (action, conflict) = match target.op {
            OpKind::Create => self.undo_create(target).await?,
            OpKind::Overwrite | OpKind::Edit => self.undo_overwrite(target).await?,
            OpKind::Move => self.undo_move(target).await?,
            OpKind::Mkdir => self.undo_mkdir(target).await?,
        };

        // 仅当真的执行了撤销（非 Aborted）才回填 + 入 redo 栈
        if action != UndoAction::Aborted {
            // 回填 undone
            let mut new_entries = entries.to_vec();
            for e in new_entries.iter_mut() {
                if e.seq == target.seq {
                    e.undone = true;
                    e.undone_at = Some(Utc::now());
                }
            }
            self.rewrite_log(session_id, &new_entries)?;

            // 入 redo 栈
            let frame = RedoFrame {
                log_entry: target.clone(),
                saved_post_undo_content: pre_undo_content,
            };
            let mut stack = self.redo_stack.lock().await;
            stack.entry(session_id.to_string()).or_default().push(frame);
        }

        Ok(UndoResult {
            seq: target.seq,
            session_id: session_id.to_string(),
            path,
            action,
            conflict,
        })
    }

    /// undo Create → remove_file（冲突：FileGone / ExternalMod）
    async fn undo_create(&self, target: &LogEntry) -> std::io::Result<(UndoAction, Option<UndoConflict>)> {
        let path = PathBuf::from(&target.path);
        match tokio::fs::read(&path).await {
            Ok(content) => {
                if let Some(after) = &target.after_meta {
                    let observed = sha256_hex(&content);
                    if observed != after.sha256 {
                        return Ok((UndoAction::Aborted, Some(UndoConflict::ExternalModification {
                            observed_sha256: observed,
                            expected_sha256: after.sha256.clone(),
                        })));
                    }
                }
                tokio::fs::remove_file(&path).await?;
                Ok((UndoAction::RemovedFile, None))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Ok((UndoAction::Aborted, Some(UndoConflict::FileGone)))
            }
            Err(e) => Err(e),
        }
    }

    /// undo Overwrite/Edit → 写回 before_snapshot
    async fn undo_overwrite(&self, target: &LogEntry) -> std::io::Result<(UndoAction, Option<UndoConflict>)> {
        let path = PathBuf::from(&target.path);
        let observed = match tokio::fs::read(&path).await {
            Ok(content) => content,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok((UndoAction::Aborted, Some(UndoConflict::FileGone)));
            }
            Err(e) => return Err(e),
        };
        if let Some(after) = &target.after_meta {
            let observed_hash = sha256_hex(&observed);
            if observed_hash != after.sha256 {
                return Ok((UndoAction::Aborted, Some(UndoConflict::ExternalModification {
                    observed_sha256: observed_hash,
                    expected_sha256: after.sha256.clone(),
                })));
            }
        }

        // 写回 before_snapshot 内容（None 不应到这——overwrite/edit 前文件存在必有 snapshot）
        let snap_filename = target.before_snapshot.as_ref().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "missing before_snapshot for Overwrite/Edit")
        })?;
        let snap_dir = session_snapshot_dir(&self.project_dir, &target.session_id);
        let snap_content = self.storage.load(&snap_dir, snap_filename).await?;
        tokio::fs::write(&path, &snap_content).await?;
        Ok((UndoAction::RestoredContent, None))
    }

    /// undo Move → rename(dst, src)
    async fn undo_move(&self, target: &LogEntry) -> std::io::Result<(UndoAction, Option<UndoConflict>)> {
        let src = PathBuf::from(&target.path); // 原 source
        let dst = PathBuf::from(target.move_to.as_deref().unwrap_or(""));

        // dst 当前内容（应当是 after_meta）→ 校验 sha256
        let dst_content = match tokio::fs::read(&dst).await {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok((UndoAction::Aborted, Some(UndoConflict::FileGone)));
            }
            Err(e) => return Err(e),
        };
        if let Some(after) = &target.after_meta {
            let observed = sha256_hex(&dst_content);
            if observed != after.sha256 {
                return Ok((UndoAction::Aborted, Some(UndoConflict::ExternalModification {
                    observed_sha256: observed,
                    expected_sha256: after.sha256.clone(),
                })));
            }
        }

        // src 必须不存在（否则会覆盖）
        if src.exists() {
            return Ok((UndoAction::Aborted, Some(UndoConflict::DestinationOccupied)));
        }

        tokio::fs::rename(&dst, &src).await?;
        Ok((UndoAction::ReverseMoved, None))
    }

    /// undo Mkdir → remove_dir（仅当目录为空）
    async fn undo_mkdir(&self, target: &LogEntry) -> std::io::Result<(UndoAction, Option<UndoConflict>)> {
        let path = PathBuf::from(&target.path);
        let mut entries = match tokio::fs::read_dir(&path).await {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok((UndoAction::Aborted, Some(UndoConflict::FileGone)));
            }
            Err(e) => return Err(e),
        };
        let mut child_names = Vec::new();
        while let Ok(Some(e)) = entries.next_entry().await {
            child_names.push(e.file_name().to_string_lossy().to_string());
            if child_names.len() > 10 {
                child_names.push("...".into());
                break;
            }
        }
        if !child_names.is_empty() {
            return Ok((UndoAction::Aborted, Some(UndoConflict::DirectoryNotEmpty {
                entries: child_names,
            })));
        }
        tokio::fs::remove_dir(&path).await?;
        Ok((UndoAction::RemovedDir, None))
    }

    /// 跨 session 找最新未撤销 entry 的 session
    async fn find_latest_active_session(&self) -> std::io::Result<String> {
        let sessions = self.list_sessions()?;
        let mut latest: Option<(DateTime<Utc>, String)> = None;
        for sid in sessions {
            for e in self.read_session_entries(&sid)? {
                if !e.undone {
                    let ts = e.timestamp;
                    if latest.as_ref().map(|(t, _)| ts > *t).unwrap_or(true) {
                        latest = Some((ts, sid.clone()));
                    }
                }
            }
        }
        latest.map(|(_, s)| s).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "no active undoable entry across sessions")
        })
    }

    // ─── redo ─────────────────────────────────────────────────────

    /// Redo session 的栈顶（用 undo 时保存的 post-undo 内容写回）
    ///
    /// 决策 3：内存式——重启丢失。
    pub async fn redo(&self, session_id: &str) -> std::io::Result<UndoResult> {
        let _g = self.undo_lock.lock().await;

        let frame = {
            let mut stack = self.redo_stack.lock().await;
            stack.get_mut(session_id).and_then(|s| s.pop())
                .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "redo stack empty"))?
        };

        let path = PathBuf::from(&frame.log_entry.path);

        // 写回 saved_post_undo_content；None 表示 undo 前文件本就不存在 → 不创建
        let action = match (&frame.log_entry.op, &frame.saved_post_undo_content) {
            (OpKind::Mkdir, _) => {
                // mkdir 的 redo：重建目录
                tokio::fs::create_dir_all(&path).await?;
                UndoAction::RestoredContent
            }
            (OpKind::Move, _) => {
                // move 的 redo：从 src 再 rename 到 dst
                let dst = PathBuf::from(frame.log_entry.move_to.as_deref().unwrap_or(""));
                tokio::fs::rename(&path, &dst).await?;
                UndoAction::ReverseMoved
            }
            (_, Some(content)) => {
                tokio::fs::write(&path, content).await?;
                UndoAction::RestoredContent
            }
            (_, None) => UndoAction::Aborted,
        };

        // 翻转 undone 标记回 false
        let entries = self.read_session_entries(session_id)?;
        let mut new_entries = entries.clone();
        for e in new_entries.iter_mut() {
            if e.seq == frame.log_entry.seq {
                e.undone = false;
                e.undone_at = None;
            }
        }
        self.rewrite_log(session_id, &new_entries)?;

        Ok(UndoResult {
            seq: frame.log_entry.seq,
            session_id: session_id.to_string(),
            path,
            action,
            conflict: None,
        })
    }

    // ─── history / timeline ─────────────────────────────────────

    /// 列出 session 的最近 N 条（含已撤销的，倒序）
    pub fn history(&self, session_id: Option<&str>, limit: usize) -> std::io::Result<Vec<HistoryEntry>> {
        let mut all: Vec<LogEntry> = match session_id {
            Some(s) => self.read_session_entries(s)?,
            None => {
                // 全 session 合并（同 timeline 但不限时间）
                let sessions = self.list_sessions()?;
                let mut acc = Vec::new();
                for s in sessions {
                    acc.extend(self.read_session_entries(&s)?);
                }
                acc
            }
        };
        all.sort_by(|a, b| b.timestamp.cmp(&a.timestamp).then(b.seq.cmp(&a.seq)));
        Ok(all.into_iter().take(limit).map(to_history).collect())
    }

    // ─── Phase 7 清理 hook ───────────────────────────────────────

    /// 清理过期 session 目录 — 启动时调用，**不阻塞**业务路径
    ///
    /// ## 算法
    /// 扫 `<project_dir>/undo/<*>/` 子目录（除 `_shared`）：
    /// - 读 log.jsonl 文件 mtime；空 / 不存在 → fallback 到 session 目录 mtime
    /// - mtime < (now - threshold) → 整个 session 目录递归删
    ///
    /// ## 返回
    /// 清理的 session 数量；失败不 panic，仅记 tracing::warn
    ///
    /// ## 引用关系
    /// - 调用方（Phase 7）：`engine_init.rs` 启动序列 tokio::spawn(cleanup_stale)
    /// - 不阻塞：调用方应在 background spawn，非同步等待
    pub async fn cleanup_stale(&self, threshold: chrono::Duration) -> std::io::Result<usize> {
        let cutoff = Utc::now() - threshold;
        // 🟡#8/#14 治本：整个 body 含 list_sessions + metadata + remove_dir_all，
        // 全部阻塞 I/O。spawn_blocking 一并隔离。
        let project_dir = self.project_dir.clone();
        let result = tokio::task::spawn_blocking(move || -> std::io::Result<usize> {
            let engine = UndoEngine::new(project_dir);
            let sessions = engine.list_sessions()?;
            let mut cleaned = 0usize;

            for sid in sessions {
                let session_dir = super::paths::session_undo_dir(&engine.project_dir, &sid);
                let log_path = super::paths::session_log_path(&engine.project_dir, &sid);

                let mtime = match std::fs::metadata(&log_path) {
                    Ok(m) => m.modified().ok().map(chrono::DateTime::<Utc>::from),
                    Err(_) => match std::fs::metadata(&session_dir) {
                        Ok(m) => m.modified().ok().map(chrono::DateTime::<Utc>::from),
                        Err(_) => None,
                    },
                };

                let stale = match mtime {
                    Some(t) => t < cutoff,
                    None => false,
                };

                if stale {
                    if let Err(e) = std::fs::remove_dir_all(&session_dir) {
                        tracing::warn!(session = %sid, error = %e,
                            "failed to remove stale session undo dir");
                    } else {
                        tracing::info!(session = %sid, "cleaned stale undo session dir");
                        cleaned += 1;
                    }
                }
            }

            Ok(cleaned)
        })
        .await
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other,
            format!("spawn_blocking panicked: {e}")))??;
        Ok(result)
    }

    /// 跨 session 时间线（since 时间之后的所有 entries，时间倒序）
    pub fn timeline(&self, since: DateTime<Utc>) -> std::io::Result<Vec<HistoryEntry>> {
        let sessions = self.list_sessions()?;
        let mut all = Vec::new();
        for s in sessions {
            for e in self.read_session_entries(&s)? {
                if e.timestamp >= since {
                    all.push(e);
                }
            }
        }
        // 同时间戳冲突：(session_id, seq) 二级排序
        all.sort_by(|a, b| {
            b.timestamp.cmp(&a.timestamp)
                .then(a.session_id.cmp(&b.session_id))
                .then(b.seq.cmp(&a.seq))
        });
        Ok(all.into_iter().map(to_history).collect())
    }
}

fn to_history(e: LogEntry) -> HistoryEntry {
    HistoryEntry {
        seq: e.seq,
        session_id: e.session_id,
        turn: e.turn,
        timestamp: e.timestamp,
        tool: e.tool,
        path: e.path,
        op: e.op,
        undone: e.undone,
    }
}

// ─── 测试 ─────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::logger::UndoLogger;
    use tempfile::TempDir;

    /// 设置 (project_dir, logger) — logger 已 attached，写一些日志
    async fn setup() -> (TempDir, PathBuf, Arc<UndoLogger>) {
        let tmp = TempDir::new().unwrap();
        let project_dir = tmp.path().to_path_buf();
        let logger = Arc::new(UndoLogger::new_at(project_dir.clone(), "sess-x".into()).unwrap());
        (tmp, project_dir, logger)
    }

    /// 写一个文件 + 调 logger 记录（生成一条 fs.write Create entry）
    async fn write_and_log(
        tmp: &Path, logger: &UndoLogger, fname: &str, content: &[u8],
    ) -> PathBuf {
        let path = tmp.join(fname);
        let pending = logger.snapshot_before("fs_write", &path).await.unwrap();
        tokio::fs::write(&path, content).await.unwrap();
        pending.commit(1, logger).await.unwrap();
        path
    }

    /// 覆盖一个已存在文件 + 记日志（生成 Overwrite entry）
    async fn overwrite_and_log(
        tmp: &Path, logger: &UndoLogger, fname: &str, new_content: &[u8],
    ) -> PathBuf {
        let path = tmp.join(fname);
        let pending = logger.snapshot_before("fs_write", &path).await.unwrap();
        tokio::fs::write(&path, new_content).await.unwrap();
        pending.commit(1, logger).await.unwrap();
        path
    }

    // ─── undo_create / undo_overwrite ──────────────────────────

    #[tokio::test]
    async fn undo_create_removes_file() {
        let (tmp, project_dir, logger) = setup().await;
        let path = write_and_log(tmp.path(), &logger, "new.txt", b"hello").await;
        assert!(path.exists());

        let engine = UndoEngine::new(project_dir);
        let r = engine.undo_last(Some("sess-x")).await.unwrap();
        assert_eq!(r.action, UndoAction::RemovedFile);
        assert!(r.conflict.is_none());
        assert!(!path.exists(), "undo Create 应删文件");
    }

    #[tokio::test]
    async fn undo_overwrite_restores_content() {
        let (tmp, project_dir, logger) = setup().await;
        let path = tmp.path().join("x.txt");
        // 先有原内容（不经 logger）
        std::fs::write(&path, b"original").unwrap();
        // 走 logger 覆盖
        overwrite_and_log(tmp.path(), &logger, "x.txt", b"updated").await;
        assert_eq!(std::fs::read(&path).unwrap(), b"updated");

        let engine = UndoEngine::new(project_dir);
        let r = engine.undo_last(Some("sess-x")).await.unwrap();
        assert_eq!(r.action, UndoAction::RestoredContent);
        assert_eq!(std::fs::read(&path).unwrap(), b"original", "snapshot 内容已写回");
    }

    // ─── undo_move ─────────────────────────────────────────────

    #[tokio::test]
    async fn undo_move_renames_back() {
        let (tmp, project_dir, logger) = setup().await;
        let src = tmp.path().join("a.txt");
        let dst = tmp.path().join("b.txt");
        std::fs::write(&src, b"data").unwrap();

        let pending = logger.record_move(&src, &dst).await.unwrap();
        std::fs::rename(&src, &dst).unwrap();
        pending.commit(1, &logger).await.unwrap();
        assert!(!src.exists() && dst.exists());

        let engine = UndoEngine::new(project_dir);
        let r = engine.undo_last(Some("sess-x")).await.unwrap();
        assert_eq!(r.action, UndoAction::ReverseMoved);
        assert!(src.exists() && !dst.exists(), "应反向 rename 回去");
    }

    // ─── undo_mkdir ────────────────────────────────────────────

    #[tokio::test]
    async fn undo_mkdir_removes_empty_dir() {
        let (tmp, project_dir, logger) = setup().await;
        let dir = tmp.path().join("newdir");
        let pending = logger.record_mkdir(&dir).await.unwrap();
        std::fs::create_dir(&dir).unwrap();
        pending.commit(1, &logger).await.unwrap();

        let engine = UndoEngine::new(project_dir);
        let r = engine.undo_last(Some("sess-x")).await.unwrap();
        assert_eq!(r.action, UndoAction::RemovedDir);
        assert!(!dir.exists());
    }

    #[tokio::test]
    async fn undo_mkdir_aborts_when_dir_not_empty() {
        let (tmp, project_dir, logger) = setup().await;
        let dir = tmp.path().join("notempty");
        let pending = logger.record_mkdir(&dir).await.unwrap();
        std::fs::create_dir(&dir).unwrap();
        // 让目录非空（外部加一个文件）
        std::fs::write(dir.join("intruder"), b"x").unwrap();
        pending.commit(1, &logger).await.unwrap();

        let engine = UndoEngine::new(project_dir);
        let r = engine.undo_last(Some("sess-x")).await.unwrap();
        assert_eq!(r.action, UndoAction::Aborted);
        match r.conflict {
            Some(UndoConflict::DirectoryNotEmpty { ref entries }) => {
                assert!(entries.contains(&"intruder".to_string()));
            }
            _ => panic!("expected DirectoryNotEmpty conflict, got {:?}", r.conflict),
        }
        // 目录应仍存在
        assert!(dir.exists());
    }

    // ─── 冲突：ExternalModification ─────────────────────────────

    #[tokio::test]
    async fn undo_overwrite_aborts_on_external_modification() {
        let (tmp, project_dir, logger) = setup().await;
        let path = tmp.path().join("conflict.txt");
        std::fs::write(&path, b"v1").unwrap();
        overwrite_and_log(tmp.path(), &logger, "conflict.txt", b"v2-by-agent").await;
        // 模拟外部改动
        std::fs::write(&path, b"v3-external").unwrap();

        let engine = UndoEngine::new(project_dir);
        let r = engine.undo_last(Some("sess-x")).await.unwrap();
        assert_eq!(r.action, UndoAction::Aborted);
        match r.conflict {
            Some(UndoConflict::ExternalModification { .. }) => {}
            _ => panic!("expected ExternalModification, got {:?}", r.conflict),
        }
        // 文件内容没动
        assert_eq!(std::fs::read(&path).unwrap(), b"v3-external");
    }

    // ─── 冲突：FileGone ────────────────────────────────────────

    #[tokio::test]
    async fn undo_create_aborts_when_file_gone() {
        let (tmp, project_dir, logger) = setup().await;
        let path = write_and_log(tmp.path(), &logger, "ephemeral.txt", b"x").await;
        // 外部删
        std::fs::remove_file(&path).unwrap();

        let engine = UndoEngine::new(project_dir);
        let r = engine.undo_last(Some("sess-x")).await.unwrap();
        assert_eq!(r.action, UndoAction::Aborted);
        assert!(matches!(r.conflict, Some(UndoConflict::FileGone)));
    }

    // ─── undo_seq + undo_turn ──────────────────────────────────

    #[tokio::test]
    async fn undo_seq_targets_specific_entry() {
        let (tmp, project_dir, logger) = setup().await;
        let p1 = write_and_log(tmp.path(), &logger, "a.txt", b"1").await;
        let _p2 = write_and_log(tmp.path(), &logger, "b.txt", b"2").await;

        let engine = UndoEngine::new(project_dir);
        // seq 1 = a.txt，但 redo 测试需要先撤这个
        let r = engine.undo_seq("sess-x", 1).await.unwrap();
        assert_eq!(r.seq, 1);
        assert!(!p1.exists());
    }

    #[tokio::test]
    async fn undo_turn_reverts_all_in_turn_in_descending_order() {
        let (tmp, project_dir, logger) = setup().await;
        // turn=7 内三条 fs.write
        for i in 0..3 {
            let path = tmp.path().join(format!("t{i}.txt"));
            let pending = logger.snapshot_before("fs_write", &path).await.unwrap();
            tokio::fs::write(&path, format!("c{i}")).await.unwrap();
            pending.commit(7, &logger).await.unwrap();
        }
        let engine = UndoEngine::new(project_dir);
        let results = engine.undo_turn("sess-x", 7).await.unwrap();
        assert_eq!(results.len(), 3);
        // seq 倒序：3,2,1
        assert_eq!(results[0].seq, 3);
        assert_eq!(results[2].seq, 1);
        for i in 0..3 {
            assert!(!tmp.path().join(format!("t{i}.txt")).exists());
        }
    }

    // ─── redo ─────────────────────────────────────────────────

    #[tokio::test]
    async fn redo_after_undo_restores_state() {
        let (tmp, project_dir, logger) = setup().await;
        let path = tmp.path().join("redo.txt");
        std::fs::write(&path, b"original").unwrap();
        overwrite_and_log(tmp.path(), &logger, "redo.txt", b"by-agent").await;

        let engine = UndoEngine::new(project_dir);
        engine.undo_last(Some("sess-x")).await.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"original");
        engine.redo("sess-x").await.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"by-agent", "redo 恢复 agent 改动");
    }

    #[tokio::test]
    async fn redo_empty_stack_returns_error() {
        let (_tmp, project_dir, _logger) = setup().await;
        let engine = UndoEngine::new(project_dir);
        let r = engine.redo("never-undone").await;
        assert!(r.is_err());
    }

    // ─── history / timeline ───────────────────────────────────

    #[tokio::test]
    async fn history_returns_recent_entries_descending() {
        let (tmp, project_dir, logger) = setup().await;
        for i in 0..5 {
            write_and_log(tmp.path(), &logger, &format!("h{i}.txt"), b"x").await;
        }
        let engine = UndoEngine::new(project_dir);
        let h = engine.history(Some("sess-x"), 10).unwrap();
        assert_eq!(h.len(), 5);
        // 倒序：seq 5 在前
        assert_eq!(h[0].seq, 5);
        assert_eq!(h[4].seq, 1);
    }

    #[tokio::test]
    async fn timeline_merges_multiple_sessions() {
        let tmp = TempDir::new().unwrap();
        let project_dir = tmp.path().to_path_buf();
        let logger_a = Arc::new(UndoLogger::new_at(project_dir.clone(), "sess-A".into()).unwrap());
        let logger_b = Arc::new(UndoLogger::new_at(project_dir.clone(), "sess-B".into()).unwrap());

        write_and_log(tmp.path(), &logger_a, "a1.txt", b"x").await;
        write_and_log(tmp.path(), &logger_b, "b1.txt", b"x").await;
        write_and_log(tmp.path(), &logger_a, "a2.txt", b"x").await;

        let engine = UndoEngine::new(project_dir);
        let tl = engine.timeline(DateTime::<Utc>::from_timestamp(0, 0).unwrap()).unwrap();
        assert_eq!(tl.len(), 3);
        // 时间倒序：a2 最新在前；后续根据 timestamp 排序
        // 因为顺序写，a2 一定 > b1 > a1
        assert_eq!(tl[0].path, tmp.path().join("a2.txt").to_string_lossy());
        assert_eq!(tl.last().unwrap().path, tmp.path().join("a1.txt").to_string_lossy());
    }

    // ─── 跨 session find_latest ───────────────────────────────

    // ─── Phase 7 清理 hook ────────────────────────────────────

    /// 设老 mtime — Rust 1.75+ 原生 API，无需第三方 crate
    fn set_mtime_old(path: &std::path::Path, secs_ago: u64) {
        use std::fs::FileTimes;
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(secs_ago);
        let f = std::fs::OpenOptions::new().write(true).open(path)
            .or_else(|_| std::fs::File::open(path))
            .unwrap();
        let times = FileTimes::new().set_modified(old);
        f.set_times(times).ok();
    }

    #[tokio::test]
    async fn cleanup_stale_removes_old_sessions() {
        let tmp = TempDir::new().unwrap();
        let project_dir = tmp.path().to_path_buf();

        // sess-old：写一条然后把 log mtime 改老
        let logger_old = Arc::new(UndoLogger::new_at(project_dir.clone(), "sess-old".into()).unwrap());
        write_and_log(tmp.path(), &logger_old, "old.txt", b"x").await;
        // sess-recent：写一条不动 mtime（活跃）
        let logger_recent = Arc::new(UndoLogger::new_at(project_dir.clone(), "sess-recent".into()).unwrap());
        write_and_log(tmp.path(), &logger_recent, "recent.txt", b"y").await;

        // 把 sess-old 的 log.jsonl mtime 改到 30 天前
        let old_log = super::super::paths::session_log_path(&project_dir, "sess-old");
        set_mtime_old(&old_log, 30 * 24 * 3600);

        // cleanup threshold = 7 天
        let engine = UndoEngine::new(project_dir.clone());
        let cleaned = engine.cleanup_stale(chrono::Duration::days(7)).await.unwrap();
        assert_eq!(cleaned, 1, "应清理 1 个过期 session");

        // sess-old 目录应消失
        assert!(!super::super::paths::session_undo_dir(&project_dir, "sess-old").exists());
        // sess-recent 目录应保留
        assert!(super::super::paths::session_undo_dir(&project_dir, "sess-recent").exists());
    }

    #[tokio::test]
    async fn cleanup_stale_no_op_when_all_active() {
        let tmp = TempDir::new().unwrap();
        let project_dir = tmp.path().to_path_buf();
        let logger = Arc::new(UndoLogger::new_at(project_dir.clone(), "sess-fresh".into()).unwrap());
        write_and_log(tmp.path(), &logger, "fresh.txt", b"x").await;

        let engine = UndoEngine::new(project_dir.clone());
        let cleaned = engine.cleanup_stale(chrono::Duration::days(7)).await.unwrap();
        assert_eq!(cleaned, 0);
        assert!(super::super::paths::session_undo_dir(&project_dir, "sess-fresh").exists());
    }

    #[tokio::test]
    async fn cleanup_stale_skips_shared_dir() {
        let tmp = TempDir::new().unwrap();
        let project_dir = tmp.path().to_path_buf();
        // 创建 _shared 目录（即使没改 mtime，list_sessions 也应排除它）
        let shared = super::super::paths::project_shared_dir(&project_dir);
        std::fs::create_dir_all(&shared).unwrap();

        let engine = UndoEngine::new(project_dir.clone());
        let cleaned = engine.cleanup_stale(chrono::Duration::days(7)).await.unwrap();
        assert_eq!(cleaned, 0, "_shared 目录应被 list_sessions 排除");
        assert!(shared.exists());
    }

    #[tokio::test]
    async fn cleanup_stale_with_no_sessions_returns_zero() {
        let tmp = TempDir::new().unwrap();
        let project_dir = tmp.path().to_path_buf();
        let engine = UndoEngine::new(project_dir);
        let cleaned = engine.cleanup_stale(chrono::Duration::days(7)).await.unwrap();
        assert_eq!(cleaned, 0);
    }

    #[tokio::test]
    async fn undo_last_with_no_session_id_finds_global_latest() {
        let tmp = TempDir::new().unwrap();
        let project_dir = tmp.path().to_path_buf();
        let logger_a = Arc::new(UndoLogger::new_at(project_dir.clone(), "sess-A".into()).unwrap());
        let logger_b = Arc::new(UndoLogger::new_at(project_dir.clone(), "sess-B".into()).unwrap());

        write_and_log(tmp.path(), &logger_a, "x.txt", b"1").await;
        // sess-B 时间稍晚（毫秒级延迟）
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let path_b = write_and_log(tmp.path(), &logger_b, "y.txt", b"2").await;

        let engine = UndoEngine::new(project_dir);
        let r = engine.undo_last(None).await.unwrap();
        // 应选 sess-B 最新
        assert_eq!(r.session_id, "sess-B");
        assert!(!path_b.exists());
    }
}
