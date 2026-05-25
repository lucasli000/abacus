//! FileWatcher — 基于 stat 轮询的文件变化检测器
//!
//! ## 设计选择
//! 使用 tokio::fs::metadata 轮询（而非 notify crate），原因：
//! 1. 零新依赖（复用 tokio）
//! 2. 跨平台一致性（无 inotify/FSEvents 差异）
//! 3. 与 CronScheduler 的 polling 模式统一
//! 4. V0.2 场景监控文件少（<20），2s 轮询完全足够
//!
//! ## 引用关系
//! - 被 `JobRunner` 持有，每个 tick 调用 `poll()`
//! - 变化事件转化为 `TriggerEvent` 送入 AutoEngine
//!
//! ## 生命周期
//! - `add_watch()` 注册路径
//! - `remove_watch()` 移除
//! - `poll()` 返回本轮变化的路径列表（调用方负责调用频率）
//! - 随 JobRunner 创建/销毁

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// 单个监控条目
#[derive(Debug, Clone)]
pub struct WatchEntry {
    /// 监控路径（文件或目录）
    pub path: PathBuf,
    /// 关联的 pipeline_id（变化时触发）
    pub pipeline_id: String,
    /// 可选标签
    pub label: String,
    /// 上次已知的 mtime
    last_mtime: Option<SystemTime>,
}

/// 文件变化事件
#[derive(Debug, Clone)]
pub struct FileChangeEvent {
    pub path: PathBuf,
    pub pipeline_id: String,
    /// 变化类型
    pub kind: FileChangeKind,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FileChangeKind {
    /// 文件内容修改（mtime 变化）
    Modified,
    /// 文件新出现（之前不存在）
    Created,
    /// 文件消失
    Deleted,
}

/// Stat-based 文件轮询监控器
///
/// 不开后台线程——调用方（JobRunner）以固定间隔调用 `poll()`
pub struct FileWatcher {
    entries: Vec<WatchEntry>,
    /// path → 上次 mtime 缓存（用于跨 poll 周期对比）
    mtime_cache: HashMap<PathBuf, Option<SystemTime>>,
}

impl FileWatcher {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            mtime_cache: HashMap::new(),
        }
    }

    /// 注册一个监控路径
    pub fn add_watch(&mut self, path: impl AsRef<Path>, pipeline_id: String, label: String) {
        let path = path.as_ref().to_path_buf();
        self.entries.push(WatchEntry {
            path: path.clone(),
            pipeline_id,
            label,
            last_mtime: None,
        });
        self.mtime_cache.insert(path, None);
    }

    /// 移除指定 pipeline 的所有监控
    pub fn remove_watch(&mut self, pipeline_id: &str) {
        self.entries.retain(|e| {
            if e.pipeline_id == pipeline_id {
                self.mtime_cache.remove(&e.path);
                false
            } else {
                true
            }
        });
    }

    /// 监控条目列表（只读）
    pub fn entries(&self) -> &[WatchEntry] {
        &self.entries
    }

    /// 轮询所有监控路径，返回本轮发生变化的事件列表
    ///
    /// 使用 std::fs::metadata（同步），对于 <20 个文件耗时 <1ms
    pub fn poll(&mut self) -> Vec<FileChangeEvent> {
        let mut events = Vec::new();

        for entry in &mut self.entries {
            let current_mtime = std::fs::metadata(&entry.path)
                .ok()
                .and_then(|m| m.modified().ok());

            let cached = self.mtime_cache.get(&entry.path).copied().flatten();

            let event_kind = match (cached, current_mtime) {
                (None, Some(_)) => {
                    // 首次轮询或文件刚出现
                    if entry.last_mtime.is_some() {
                        // 之前存在过缓存 → 说明是重新出现
                        Some(FileChangeKind::Created)
                    } else {
                        // 首次 poll，不触发事件（基线建立）
                        None
                    }
                }
                (Some(_prev), None) => Some(FileChangeKind::Deleted),
                (Some(prev), Some(curr)) if prev != curr => Some(FileChangeKind::Modified),
                _ => None,
            };

            // 更新缓存
            entry.last_mtime = current_mtime;
            self.mtime_cache.insert(entry.path.clone(), current_mtime);

            if let Some(kind) = event_kind {
                events.push(FileChangeEvent {
                    path: entry.path.clone(),
                    pipeline_id: entry.pipeline_id.clone(),
                    kind,
                });
            }
        }

        events
    }

    /// 重置所有基线（不触发事件重新建立 mtime 缓存）
    pub fn reset_baselines(&mut self) {
        for entry in &mut self.entries {
            let mtime = std::fs::metadata(&entry.path)
                .ok()
                .and_then(|m| m.modified().ok());
            entry.last_mtime = mtime;
            self.mtime_cache.insert(entry.path.clone(), mtime);
        }
    }
}

impl Default for FileWatcher {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_baseline_no_event() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.txt");
        std::fs::write(&file, "hello").unwrap();

        let mut w = FileWatcher::new();
        w.add_watch(&file, "p1".into(), "test file".into());

        // 第一次 poll 建立基线，不应触发事件
        let events = w.poll();
        assert!(events.is_empty());
    }

    #[test]
    fn test_modify_detected() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.txt");
        std::fs::write(&file, "hello").unwrap();

        let mut w = FileWatcher::new();
        w.add_watch(&file, "p1".into(), "test file".into());
        w.poll(); // baseline

        // 修改文件
        std::thread::sleep(std::time::Duration::from_millis(50));
        let mut f = std::fs::OpenOptions::new().write(true).open(&file).unwrap();
        f.write_all(b"world").unwrap();
        drop(f);

        let events = w.poll();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, FileChangeKind::Modified);
        assert_eq!(events[0].pipeline_id, "p1");
    }

    #[test]
    fn test_delete_detected() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.txt");
        std::fs::write(&file, "hello").unwrap();

        let mut w = FileWatcher::new();
        w.add_watch(&file, "p1".into(), "test".into());
        w.poll(); // baseline

        std::fs::remove_file(&file).unwrap();
        let events = w.poll();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, FileChangeKind::Deleted);
    }
}
