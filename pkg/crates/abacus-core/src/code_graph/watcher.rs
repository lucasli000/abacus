//! FileWatcher 管理器 — 长 daemon 模式增量索引触发
//!
//! ## 职责
//! 监听文件系统变更事件，经去抖（debounce）后批量触发 `Indexer::index_files()`。
//! 仅在长 daemon 模式（如 LSP backend / background agent）启用。
//!
//! ## 依赖 (external)
//! - `notify` v7: 跨平台文件系统事件监听（macOS: kqueue）
//! - `tokio`: 异步 runtime（watch channel + spawn）
//!
//! ## 依赖 (internal)
//! - `Indexer::index_files()`: 增量索引入口
//! - `Language::from_extension()`: 文件扩展名过滤
//!
//! ## 引用关系
//! - 被 `CodeGraphManager::enable_watcher()` 创建和启动
//! - 被 `CodeGraphManager::stop_watcher()` 优雅停止
//!
//! ## 生命周期
//! - 创建：`CodeGraphManager::enable_watcher(debounce_ms)` 时
//! - 激活：`start(workspace)` 后 background task 开始监听
//! - 销毁：`stop()` 发送 shutdown 信号 → background task 退出 → watcher drop

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use notify::Watcher;
use tokio::sync::RwLock;

use super::indexer::Indexer;
use super::lang::Language;

// ─── 忽略列表 ─────────────────────────────────────────────────────────────────

/// 应忽略的目录名列表
///
/// 与 `indexer.rs::is_hidden_or_ignored()` 保持一致。
/// 如果新增忽略项，两处必须同步更新。
const IGNORED_DIRS: &[&str] = &[
    "target",
    "node_modules",
    "__pycache__",
    "vendor",
    "build",
    "dist",
    ".git",
    ".hg",
    ".svn",
];

// ─── FileWatcherManager ──────────────────────────────────────────────────────

/// 文件监听管理器
///
/// ## 线程安全
/// 所有字段通过 `Arc` / `RwLock` / `watch channel` 实现跨 task 共享。
///
/// ## 去抖策略
/// 文件变更事件高频发生（IDE 保存、格式化、git checkout 等）。
/// 使用时间窗口去抖：在 `debounce` 时间内累积所有变更路径，
/// 窗口结束后一次性提交给 Indexer，避免重复索引。
///
/// ## 错误隔离
/// Indexer 调用失败不会终止 watcher——记录错误后继续监听。
pub struct FileWatcherManager {
    /// 索引引擎引用（用于触发增量索引）
    indexer: Arc<Indexer>,
    /// 去抖间隔（推荐 500ms-2000ms）
    debounce: Duration,
    /// 停止信号发送端
    ///
    /// 发送 `true` 时 background task 退出。
    /// `RwLock` 包装因为 `start()` 需要写入初始化，后续只读。
    shutdown_tx: RwLock<Option<tokio::sync::watch::Sender<bool>>>,
    /// 停止信号接收端（background task 持有 clone）
    shutdown_rx: RwLock<Option<tokio::sync::watch::Receiver<bool>>>,
    /// 当前是否正在运行
    running: RwLock<bool>,
}

impl FileWatcherManager {
    /// 创建 FileWatcherManager
    ///
    /// ## 参数
    /// - `indexer`: 共享的 Indexer 实例
    /// - `debounce`: 去抖时间窗口（推荐 500ms ~ 2000ms）
    ///
    /// ## 注意
    /// 创建后不会自动开始监听，需调用 `start()` 启动。
    pub fn new(indexer: Arc<Indexer>, debounce: Duration) -> Self {
        let (tx, rx) = tokio::sync::watch::channel(false);
        Self {
            indexer,
            debounce,
            shutdown_tx: RwLock::new(Some(tx)),
            shutdown_rx: RwLock::new(Some(rx)),
            running: RwLock::new(false),
        }
    }

    /// 启动文件监听
    ///
    /// ## 行为
    /// 1. 创建 `notify::RecommendedWatcher`（macOS 使用 kqueue）
    /// 2. 递归监听 `workspace` 目录
    /// 3. 在 tokio background task 中处理事件：
    ///    - 过滤：只关注受支持语言的文件
    ///    - 去抖：累积变更路径，debounce 时间窗口结束后批量提交
    ///    - 索引：调用 `indexer.index_files()` 处理变更
    ///
    /// ## 错误
    /// - `notify` 初始化失败（权限/路径不存在）
    /// - 已经在运行时重复调用
    ///
    /// ## 幂等性
    /// 重复调用返回错误，不会创建多个 watcher。
    pub async fn start(&self, workspace: &Path) -> Result<(), String> {
        // 防止重复启动
        {
            let running = self.running.read().await;
            if *running {
                return Err("FileWatcher is already running".into());
            }
        }

        let workspace = workspace.to_path_buf();
        let indexer = self.indexer.clone();
        let debounce = self.debounce;

        // 取出 shutdown receiver（只能取一次）
        let shutdown_rx = {
            let mut rx_guard = self.shutdown_rx.write().await;
            rx_guard.take().ok_or_else(|| "shutdown receiver already consumed".to_string())?
        };

        // 创建 event channel（notify → background task）
        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel::<notify::Event>();

        // 初始化 notify watcher
        //
        // notify v7 API: RecommendedWatcher + Config + event handler closure
        let watcher = {
            let tx = event_tx.clone();
            notify::RecommendedWatcher::new(
                move |result: Result<notify::Event, notify::Error>| {
                    if let Ok(event) = result {
                        // 非阻塞发送，channel 满时丢弃（极端情况下可接受丢失）
                        let _ = tx.send(event);
                    }
                },
                notify::Config::default(),
            )
            .map_err(|e| format!("Failed to create file watcher: {e}"))?
        };

        // 递归监听 workspace
        {
            let mut watcher = watcher;
            watcher
                .watch(&workspace, notify::RecursiveMode::Recursive)
                .map_err(|e| format!("Failed to watch workspace: {e}"))?;

            // 将 watcher 移入 background task（防止 drop）
            self.spawn_background_task(watcher, event_rx, shutdown_rx, indexer, debounce, workspace)
                .await;
        }

        // 标记为运行中
        *self.running.write().await = true;

        Ok(())
    }

    /// 优雅停止文件监听
    ///
    /// ## 行为
    /// 1. 发送 shutdown 信号
    /// 2. Background task 收到信号后退出循环
    /// 3. `notify::Watcher` 随 task 结束而 drop（停止底层 kqueue/inotify）
    ///
    /// ## 幂等性
    /// 多次调用安全——已停止后再调用无副作用。
    pub async fn stop(&self) {
        let tx_guard = self.shutdown_tx.read().await;
        if let Some(ref tx) = *tx_guard {
            let _ = tx.send(true);
        }
        *self.running.write().await = false;
    }

    /// 查询当前运行状态
    pub async fn is_running(&self) -> bool {
        *self.running.read().await
    }

    // ─── 内部方法 ─────────────────────────────────────────────────────────────

    /// 启动后台事件处理 task
    ///
    /// ## 事件处理循环
    /// ```text
    /// loop {
    ///     select! {
    ///         event => 累积到 pending set
    ///         debounce_tick => 如果 pending 非空，执行索引
    ///         shutdown => break
    ///     }
    /// }
    /// ```
    async fn spawn_background_task(
        &self,
        watcher: notify::RecommendedWatcher,
        mut event_rx: tokio::sync::mpsc::UnboundedReceiver<notify::Event>,
        mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
        indexer: Arc<Indexer>,
        debounce: Duration,
        workspace: PathBuf,
    ) {
        tokio::spawn(async move {
            // 持有 watcher 防止 drop（drop 会停止底层监听）
            let _watcher = watcher;

            // 去抖累积集合
            let mut pending: HashSet<PathBuf> = HashSet::new();
            // 去抖定时器（初始无 deadline）
            let mut debounce_deadline: Option<tokio::time::Instant> = None;

            loop {
                // 计算 sleep 时间
                let sleep_future = match debounce_deadline {
                    Some(deadline) => tokio::time::sleep_until(deadline),
                    None => tokio::time::sleep(Duration::from_secs(86400)), // 无限等待
                };

                tokio::select! {
                    // 优先检查 shutdown
                    _ = shutdown_rx.changed() => {
                        if *shutdown_rx.borrow() {
                            tracing::info!("[CodeGraph/Watcher] shutdown signal received, exiting");
                            break;
                        }
                    }

                    // 接收文件事件
                    Some(event) = event_rx.recv() => {
                        let dominated_paths = Self::extract_relevant_paths(&event, &workspace);
                        if !dominated_paths.is_empty() {
                            pending.extend(dominated_paths);
                            // 重置/设置去抖 deadline
                            debounce_deadline = Some(tokio::time::Instant::now() + debounce);
                        }
                    }

                    // 去抖窗口到期 → 执行索引
                    _ = sleep_future, if debounce_deadline.is_some() => {
                        if !pending.is_empty() {
                            let files: Vec<PathBuf> = pending.drain().collect();
                            tracing::debug!(
                                "[CodeGraph/Watcher] debounce fired, indexing {} files",
                                files.len()
                            );

                            // 索引失败不终止 watcher
                            match indexer.index_files(&files).await {
                                Ok(report) => {
                                    tracing::info!(
                                        "[CodeGraph/Watcher] incremental index done: \
                                         {} indexed, {} failed, {}ms",
                                        report.indexed,
                                        report.failed,
                                        report.duration_ms
                                    );
                                }
                                Err(e) => {
                                    tracing::error!(
                                        "[CodeGraph/Watcher] index_files error: {e}"
                                    );
                                }
                            }
                        }
                        debounce_deadline = None;
                    }
                }
            }
        });
    }

    /// 从 notify 事件中提取与代码相关的路径
    ///
    /// ## 过滤规则
    /// 1. 只处理 Create / Modify / Remove 事件（忽略 Access / Other）
    /// 2. 只处理受支持语言的文件扩展名
    /// 3. 忽略隐藏文件和目录
    /// 4. 忽略 IGNORED_DIRS 中列出的目录
    fn extract_relevant_paths(event: &notify::Event, _workspace: &Path) -> Vec<PathBuf> {
        use notify::EventKind;

        // 只关注文件内容变更事件
        match &event.kind {
            EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_) => {}
            _ => return Vec::new(),
        }

        event
            .paths
            .iter()
            .filter(|path| {
                // 必须是文件（有扩展名）
                let ext = match path.extension().and_then(|e| e.to_str()) {
                    Some(ext) => ext,
                    None => return false,
                };

                // 扩展名必须是受支持的语言
                if Language::from_extension(ext).is_none() {
                    return false;
                }

                // 检查路径中是否含有应忽略的组件
                !Self::path_contains_ignored(path)
            })
            .cloned()
            .collect()
    }

    /// 检查路径中是否包含应忽略的目录组件
    ///
    /// 遍历路径的每个 component，检查是否匹配 IGNORED_DIRS
    /// 或以 '.' 开头（隐藏文件/目录）。
    fn path_contains_ignored(path: &Path) -> bool {
        for component in path.components() {
            if let std::path::Component::Normal(name) = component {
                let name_str = name.to_string_lossy();
                // 隐藏文件/目录
                if name_str.starts_with('.') {
                    return true;
                }
                // 已知忽略目录
                if IGNORED_DIRS.contains(&name_str.as_ref()) {
                    return true;
                }
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_path_contains_ignored_hidden() {
        let path = PathBuf::from("/project/.git/config");
        assert!(FileWatcherManager::path_contains_ignored(&path));
    }

    #[test]
    fn test_path_contains_ignored_target() {
        let path = PathBuf::from("/project/target/debug/main.rs");
        assert!(FileWatcherManager::path_contains_ignored(&path));
    }

    #[test]
    fn test_path_contains_ignored_node_modules() {
        let path = PathBuf::from("/project/node_modules/foo/index.ts");
        assert!(FileWatcherManager::path_contains_ignored(&path));
    }

    #[test]
    fn test_path_not_ignored_normal() {
        let path = PathBuf::from("/project/src/main.rs");
        assert!(!FileWatcherManager::path_contains_ignored(&path));
    }

    #[test]
    fn test_path_not_ignored_nested_src() {
        let path = PathBuf::from("/project/crates/core/src/lib.rs");
        assert!(!FileWatcherManager::path_contains_ignored(&path));
    }

    #[test]
    fn test_extract_relevant_paths_create_event() {
        let event = notify::Event {
            kind: notify::EventKind::Create(notify::event::CreateKind::File),
            paths: vec![
                PathBuf::from("/project/src/main.rs"),
                PathBuf::from("/project/src/lib.ts"),
                PathBuf::from("/project/readme.md"),      // unsupported ext
                PathBuf::from("/project/.git/HEAD"),       // hidden dir
                PathBuf::from("/project/target/foo.rs"),   // ignored dir
            ],
            attrs: Default::default(),
        };
        let workspace = PathBuf::from("/project");
        let paths = FileWatcherManager::extract_relevant_paths(&event, &workspace);

        assert_eq!(paths.len(), 2);
        assert!(paths.contains(&PathBuf::from("/project/src/main.rs")));
        assert!(paths.contains(&PathBuf::from("/project/src/lib.ts")));
    }

    #[test]
    fn test_extract_relevant_paths_access_event_ignored() {
        let event = notify::Event {
            kind: notify::EventKind::Access(notify::event::AccessKind::Read),
            paths: vec![PathBuf::from("/project/src/main.rs")],
            attrs: Default::default(),
        };
        let workspace = PathBuf::from("/project");
        let paths = FileWatcherManager::extract_relevant_paths(&event, &workspace);
        assert!(paths.is_empty());
    }

    #[tokio::test]
    async fn test_new_creates_stopped_instance() {
        // 验证初始状态
        let indexer = create_mock_indexer();
        let watcher = FileWatcherManager::new(indexer, Duration::from_millis(100));
        assert!(!watcher.is_running().await);
    }

    #[tokio::test]
    async fn test_stop_idempotent() {
        let indexer = create_mock_indexer();
        let watcher = FileWatcherManager::new(indexer, Duration::from_millis(100));
        // stop before start — should not panic
        watcher.stop().await;
        watcher.stop().await;
        assert!(!watcher.is_running().await);
    }

    /// 创建一个用于测试的 mock Indexer
    ///
    /// 注意：这只是为了构造 FileWatcherManager 实例进行单元测试，
    /// 实际索引功能需要数据库连接。集成测试中应使用真实 Indexer。
    fn create_mock_indexer() -> Arc<Indexer> {
        use std::collections::HashMap;
        use rusqlite::Connection;
        use tokio::sync::Mutex;

        // In-memory DB + minimal setup for Indexer construction
        let conn = Connection::open_in_memory().unwrap();
        super::super::schema::ensure_tables(&conn).unwrap();
        let db = Arc::new(Mutex::new(conn));
        let analyzers: HashMap<Language, Box<dyn super::super::lang::LanguageAnalyzer>> =
            HashMap::new();
        Arc::new(Indexer::new(db, Arc::new(analyzers), PathBuf::from("/tmp")))
    }
}
