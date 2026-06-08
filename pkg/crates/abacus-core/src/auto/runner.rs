//! JobRunner — 统一自动化调度器
//!
//! ## 架构
//! ```text
//! JobRunner::spawn()
//!   └─ tokio::spawn 后台 task
//!       └─ loop { tick() → sleep(interval) }
//!             ├─ CronScheduler.tick() → due pipelines
//!             ├─ FileWatcher.poll() → change events → fire triggers
//!             └─ update AutoHealth snapshot → push to TUI channel
//! ```
//!
//! ## 引用关系
//! - 持有 `AutoEngine`（Arc）：调度 pipeline 执行
//! - 持有 `FileWatcher`：文件监控
//! - 通过 `health_tx` 推送 `AutoHealth` 给 TUI
//!
//! ## 生命周期
//! - `spawn()` 启动后台 tokio task，返回 `RunnerHandle`
//! - `RunnerHandle.shutdown()` 发信号停止 loop
//! - 后台 task 退出后自动清理（无 detached 资源）
//!
//! ## 副作用
//! - tokio::spawn 创建后台 task（通过 RunnerHandle::shutdown_tx 控制销毁）
//! - 通过 health_tx mpsc 推送状态快照

use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch, RwLock};

use super::health::{AutoHealth, JobKind, JobState, JobStatus};
use super::watcher::{FileWatcher, FileChangeEvent};
use super::{AutoEngine, TriggerEvent};

/// Runner 配置
#[derive(Debug, Clone)]
pub struct RunnerConfig {
    /// Cron 检查间隔（默认 60s）
    pub cron_interval: Duration,
    /// FileWatcher 轮询间隔（默认 3s）
    pub watch_interval: Duration,
    /// Health 快照推送间隔（默认 1s）
    pub health_push_interval: Duration,
}

impl Default for RunnerConfig {
    fn default() -> Self {
        Self {
            cron_interval: Duration::from_secs(60),
            watch_interval: Duration::from_secs(3),
            health_push_interval: Duration::from_secs(1),
        }
    }
}

/// Runner 后台任务句柄
///
/// 生命周期：调用 shutdown() 或 drop 后，后台 task 在下次 tick 收到信号退出
pub struct RunnerHandle {
    shutdown_tx: watch::Sender<bool>,
}

impl RunnerHandle {
    /// 优雅停止 Runner 后台任务
    pub fn shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
    }
}

impl Drop for RunnerHandle {
    fn drop(&mut self) {
        let _ = self.shutdown_tx.send(true);
    }
}

/// 统一调度器
pub struct JobRunner {
    engine: Arc<AutoEngine>,
    watcher: Arc<RwLock<FileWatcher>>,
    config: RunnerConfig,
    start_time: Instant,
    /// 每个 job 的运行统计（pipeline_id → stats）
    job_stats: Arc<RwLock<std::collections::HashMap<String, JobRunStats>>>,
}

/// 单任务运行时统计
#[derive(Debug, Clone, Default)]
struct JobRunStats {
    run_count: u64,
    fail_count: u64,
    last_run: Option<Instant>,
    last_duration_ms: Option<u64>,
    state: JobState,
}

impl Default for JobState {
    fn default() -> Self {
        JobState::Idle
    }
}

impl JobRunner {
    pub fn new(engine: Arc<AutoEngine>, config: RunnerConfig) -> Self {
        Self {
            engine,
            watcher: Arc::new(RwLock::new(FileWatcher::new())),
            config,
            start_time: Instant::now(),
            job_stats: Arc::new(RwLock::new(std::collections::HashMap::new())),
        }
    }

    /// 获取 FileWatcher 的写锁引用（用于外部注册监控路径）
    pub fn watcher(&self) -> &Arc<RwLock<FileWatcher>> {
        &self.watcher
    }

    /// 启动后台调度循环
    ///
    /// 返回：
    /// - `RunnerHandle`：用于停止后台任务
    /// - `mpsc::Receiver<AutoHealth>`：TUI 侧消费健康快照
    ///
    /// 副作用：tokio::spawn 一个后台 task
    pub fn spawn(self) -> (RunnerHandle, mpsc::Receiver<AutoHealth>) {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (health_tx, health_rx) = mpsc::channel(4);

        let engine = self.engine;
        let watcher = self.watcher;
        let config = self.config;
        let start_time = self.start_time;
        let job_stats = self.job_stats;

        tokio::spawn(async move {
            run_loop(engine, watcher, config, start_time, job_stats, shutdown_rx, health_tx).await;
        });

        (RunnerHandle { shutdown_tx }, health_rx)
    }

    /// 生成当前健康快照（同步版，用于非 Runner 场景下的状态查询）
    pub async fn health_snapshot(&self) -> AutoHealth {
        build_health(&self.engine, &self.watcher, &self.job_stats, self.start_time).await
    }
}

/// 后台主循环
async fn run_loop(
    engine: Arc<AutoEngine>,
    watcher: Arc<RwLock<FileWatcher>>,
    config: RunnerConfig,
    start_time: Instant,
    job_stats: Arc<RwLock<std::collections::HashMap<String, JobRunStats>>>,
    mut shutdown_rx: watch::Receiver<bool>,
    health_tx: mpsc::Sender<AutoHealth>,
) {
    let mut cron_interval = tokio::time::interval(config.cron_interval);
    let mut watch_interval = tokio::time::interval(config.watch_interval);
    let mut health_interval = tokio::time::interval(config.health_push_interval);

    // 首次 tick 立即执行（跳过 initial delay）
    cron_interval.tick().await;
    watch_interval.tick().await;
    health_interval.tick().await;

    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    tracing::info!("JobRunner shutting down");
                    break;
                }
            }
            _ = cron_interval.tick() => {
                // Cron tick：检查到期任务并执行
                let due = engine.cron.lock().await.tick();
                for pipeline_id in due {
                    execute_pipeline(&engine, &job_stats, &pipeline_id).await;
                }
            }
            _ = watch_interval.tick() => {
                // FileWatcher poll：检测文件变化
                let events = {
                    let mut w = watcher.write().await;
                    w.poll()
                };
                for event in events {
                    handle_file_event(&engine, &job_stats, event).await;
                }
            }
            _ = health_interval.tick() => {
                // 推送健康快照给 TUI
                let health = build_health(&engine, &watcher, &job_stats, start_time).await;
                // 非阻塞发送（TUI 来不及消费则丢弃旧的）
                let _ = health_tx.try_send(health);
            }
        }
    }
}

/// 执行单个 pipeline 并更新统计
async fn execute_pipeline(
    engine: &AutoEngine,
    job_stats: &Arc<RwLock<std::collections::HashMap<String, JobRunStats>>>,
    pipeline_id: &str,
) {
    // 标记 Running
    {
        let mut stats = job_stats.write().await;
        let entry = stats.entry(pipeline_id.to_string()).or_default();
        entry.state = JobState::Running;
    }

    let start = Instant::now();
    let result = engine.fire_pipeline(pipeline_id).await;
    let duration = start.elapsed();

    // 更新统计
    let mut stats = job_stats.write().await;
    let entry = stats.entry(pipeline_id.to_string()).or_default();
    entry.run_count += 1;
    entry.last_run = Some(Instant::now());
    entry.last_duration_ms = Some(duration.as_millis() as u64);

    if let Some(ref r) = result {
        if matches!(r.state, super::pipeline::PipelineState::Completed) {
            entry.state = JobState::Idle;
        } else {
            entry.state = JobState::Failed;
            entry.fail_count += 1;
        }
    } else {
        // pipeline 未找到
        entry.state = JobState::Failed;
        entry.fail_count += 1;
        tracing::warn!(pipeline_id, "pipeline not found during auto-execution");
    }
}

/// 处理文件变化事件：触发对应 pipeline
async fn handle_file_event(
    engine: &AutoEngine,
    job_stats: &Arc<RwLock<std::collections::HashMap<String, JobRunStats>>>,
    event: FileChangeEvent,
) {
    tracing::debug!(path = %event.path.display(), kind = ?event.kind, "file change detected");
    // 触发对应 pipeline
    execute_pipeline(engine, job_stats, &event.pipeline_id).await;
    // 同时通过 trigger 系统广播事件
    let trigger_event = TriggerEvent {
        name: format!("file:{}", event.kind as u8),
        payload: Some(serde_json::json!({
            "path": event.path.to_string_lossy(),
            "pipeline_id": event.pipeline_id,
        })),
    };
    engine.fire(&trigger_event).await;
}

/// 构建健康快照
async fn build_health(
    engine: &AutoEngine,
    watcher: &Arc<RwLock<FileWatcher>>,
    job_stats: &Arc<RwLock<std::collections::HashMap<String, JobRunStats>>>,
    start_time: Instant,
) -> AutoHealth {
    let stats = job_stats.read().await;
    let mut jobs = Vec::new();

    // Cron 任务
    for (expr, pipeline_id) in engine.cron.lock().await.entries() {
        let s = stats.get(&pipeline_id);
        jobs.push(JobStatus {
            id: pipeline_id.clone(),
            label: format!("{} [{}]", pipeline_id, expr),
            kind: JobKind::Cron,
            state: s.map(|s| s.state).unwrap_or(JobState::Idle),
            last_run: s.and_then(|s| s.last_run),
            last_duration_ms: s.and_then(|s| s.last_duration_ms),
            run_count: s.map(|s| s.run_count).unwrap_or(0),
            fail_count: s.map(|s| s.fail_count).unwrap_or(0),
        });
    }

    // Watch 任务
    let w = watcher.read().await;
    for entry in w.entries() {
        let s = stats.get(&entry.pipeline_id);
        jobs.push(JobStatus {
            id: entry.pipeline_id.clone(),
            label: entry.label.clone(),
            kind: JobKind::Watch,
            state: s.map(|s| s.state).unwrap_or(JobState::Idle),
            last_run: s.and_then(|s| s.last_run),
            last_duration_ms: s.and_then(|s| s.last_duration_ms),
            run_count: s.map(|s| s.run_count).unwrap_or(0),
            fail_count: s.map(|s| s.fail_count).unwrap_or(0),
        });
    }

    // Event triggers
    let triggers = engine.triggers.read().await;
    for trigger in triggers.iter() {
        // 只添加不在 cron/watch 中的 trigger
        if !jobs.iter().any(|j| j.id == trigger.pipeline_id) {
            let s = stats.get(&trigger.pipeline_id);
            jobs.push(JobStatus {
                id: trigger.pipeline_id.clone(),
                label: format!("{} → {}", trigger.event_name, trigger.pipeline_id),
                kind: JobKind::Event,
                state: s.map(|s| s.state).unwrap_or(JobState::Idle),
                last_run: s.and_then(|s| s.last_run),
                last_duration_ms: s.and_then(|s| s.last_duration_ms),
                run_count: s.map(|s| s.run_count).unwrap_or(0),
                fail_count: s.map(|s| s.fail_count).unwrap_or(0),
            });
        }
    }

    AutoHealth {
        jobs,
        runner_active: true,
        uptime: start_time.elapsed(),
        pending_reviews: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auto::pipeline::Pipeline;

    #[tokio::test]
    async fn test_runner_spawn_and_shutdown() {
        let engine = Arc::new(AutoEngine::new());
        let runner = JobRunner::new(engine, RunnerConfig {
            cron_interval: Duration::from_millis(100),
            watch_interval: Duration::from_millis(100),
            health_push_interval: Duration::from_millis(50),
            ..Default::default()
        });
        let (handle, mut health_rx) = runner.spawn();

        // 应该在 ~50ms 内收到首个 health push
        let health = tokio::time::timeout(Duration::from_millis(200), health_rx.recv()).await;
        assert!(health.is_ok());
        let h = health.unwrap().unwrap();
        assert!(h.runner_active);
        assert_eq!(h.jobs.len(), 0);

        handle.shutdown();
        // 等待后台任务退出
        tokio::time::sleep(Duration::from_millis(150)).await;
    }

    #[tokio::test]
    async fn test_health_snapshot() {
        let engine = Arc::new(AutoEngine::new());
        // 注册一个 pipeline + cron
        let p = Pipeline::new("daily-report");
        engine.register_pipeline(p).await;
        engine.cron.lock().await.add("0 9 * * *", "daily-report".to_string());

        let runner = JobRunner::new(engine, RunnerConfig::default());
        let health = runner.health_snapshot().await;
        assert_eq!(health.jobs.len(), 1);
        assert_eq!(health.jobs[0].kind, JobKind::Cron);
        assert_eq!(health.jobs[0].state, JobState::Idle);
    }
}
