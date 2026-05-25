//! AutoHealth — 自动化模块健康状态快照
//!
//! ## 引用关系
//! - 被 `JobRunner::health()` 生成
//! - 被 TUI `AppState.auto_health` 持有，供 `render_dashboard_auto()` 渲染
//!
//! ## 生命周期
//! 每次 JobRunner tick 后重新生成（Copy 语义，无堆分配路径可选）

use std::time::Instant;

/// 单个自动化任务的状态快照
#[derive(Debug, Clone)]
pub struct JobStatus {
    /// 任务 ID（对应 Pipeline ID 或自定义标签）
    pub id: String,
    /// 人可读的简短描述
    pub label: String,
    /// 任务类型
    pub kind: JobKind,
    /// 当前状态
    pub state: JobState,
    /// 上次触发时间
    pub last_run: Option<Instant>,
    /// 上次运行耗时 ms
    pub last_duration_ms: Option<u64>,
    /// 累计执行次数
    pub run_count: u64,
    /// 累计失败次数
    pub fail_count: u64,
}

/// 任务类型标记
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum JobKind {
    /// Cron 定时任务
    Cron,
    /// 文件监听
    Watch,
    /// 手动/事件触发
    Event,
}

/// 任务当前状态
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum JobState {
    /// 空闲等待下次触发
    Idle,
    /// 正在执行
    Running,
    /// 最后一次执行失败
    Failed,
    /// 已暂停
    Paused,
}

/// 自动化模块整体健康快照
///
/// 由 JobRunner 每 tick 生成，通过 mpsc 推送给 TUI
#[derive(Debug, Clone)]
pub struct AutoHealth {
    /// 所有注册任务的状态
    pub jobs: Vec<JobStatus>,
    /// Runner 是否活跃
    pub runner_active: bool,
    /// Runner 已运行时长
    pub uptime: std::time::Duration,
    /// 待审阅事件数（需要 LLM 或用户介入）
    pub pending_reviews: u32,
}

impl Default for AutoHealth {
    fn default() -> Self {
        Self {
            jobs: Vec::new(),
            runner_active: false,
            uptime: std::time::Duration::ZERO,
            pending_reviews: 0,
        }
    }
}

impl AutoHealth {
    /// 活跃任务数
    pub fn active_count(&self) -> usize {
        self.jobs.iter().filter(|j| j.state == JobState::Running).count()
    }

    /// 失败任务数
    pub fn failed_count(&self) -> usize {
        self.jobs.iter().filter(|j| j.state == JobState::Failed).count()
    }

    /// 总任务数
    pub fn total_count(&self) -> usize {
        self.jobs.len()
    }
}

impl std::fmt::Display for JobKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JobKind::Cron => write!(f, "cron"),
            JobKind::Watch => write!(f, "watch"),
            JobKind::Event => write!(f, "event"),
        }
    }
}

impl std::fmt::Display for JobState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JobState::Idle => write!(f, "idle"),
            JobState::Running => write!(f, "running"),
            JobState::Failed => write!(f, "failed"),
            JobState::Paused => write!(f, "paused"),
        }
    }
}
