//! Auto 自动化引擎 — Pipeline 执行器 + Cron 调度 + 事件触发
//!
//! ## 场景
//! 定时/事件驱动的自动化工作流执行。Pipeline 是串行步骤序列，
//! 每步可以是工具调用、条件判断或子 Pipeline。
//!
//! ## 架构
//! ```text
//! Trigger.fire() → Pipeline.run() → Step[0] → Step[1] → ... → Step[N]
//!                                          ↓
//!                                     Condition → skip/branch
//! Cron.schedule() → 定时触发 Trigger
//! ```
//!
//! ## 边界
//! - Pipeline 当前为内存执行，无持久化状态（后续对接 SQLite）
//! - Cron 调度最小间隔 1 分钟
//! - Trigger 支持事件名匹配和定时两种模式

mod pipeline;
mod cron;
mod trigger;
pub mod store;

pub use pipeline::{Pipeline, Step, StepKind, StepResult, PipelineState};
pub use cron::CronScheduler;
pub use trigger::{Trigger, TriggerEvent};
pub use store::{AutoStore, PipelineRunRecord};

use std::sync::Arc;
use tokio::sync::RwLock;

/// Auto 引擎聚合入口
///
/// ## Task #81：SQLite 持久化
/// `store` 为 None → 内存模式（无运行历史）；
/// `store` 为 Some → 每次 fire/fire_pipeline 写入 `pipeline_runs` 表。
/// Pipeline 定义本身留内存（重启丢失），与 Skill/LSP 一致。
pub struct AutoEngine {
    pub pipelines: RwLock<Vec<Pipeline>>,
    pub cron: CronScheduler,
    pub triggers: RwLock<Vec<Trigger>>,
    /// 可选 SQLite 持久化层（None → 不写历史）
    pub store: Option<Arc<AutoStore>>,
}

impl AutoEngine {
    pub fn new() -> Self {
        Self {
            pipelines: RwLock::new(Vec::new()),
            cron: CronScheduler::new(),
            triggers: RwLock::new(Vec::new()),
            store: None,
        }
    }

    /// 带 SQLite 持久化的构造（生产部署用）
    ///
    /// 引用关系：store Arc 与 AutoEngine 同生命周期；其他模块不应直接持 store
    /// （通过 AutoEngine.store 访问）。
    pub fn with_store(store: Arc<AutoStore>) -> Self {
        Self {
            pipelines: RwLock::new(Vec::new()),
            cron: CronScheduler::new(),
            triggers: RwLock::new(Vec::new()),
            store: Some(store),
        }
    }

    /// 注册一个 Pipeline
    pub async fn register_pipeline(&self, pipeline: Pipeline) {
        self.pipelines.write().await.push(pipeline);
    }

    /// 注册一个定时任务
    pub async fn register_cron(&mut self, expr: &str, pipeline_id: &str) {
        self.cron.add(expr, pipeline_id.to_string());
    }

    /// 注册一个事件触发器
    pub async fn register_trigger(&self, trigger: Trigger) {
        self.triggers.write().await.push(trigger);
    }

    /// 触发事件：匹配的 Trigger 启动对应的 Pipeline，并按需持久化运行历史
    pub async fn fire(&self, event: &TriggerEvent) -> Vec<String> {
        let mut started = Vec::new();
        let triggers = self.triggers.read().await;
        let pipelines = self.pipelines.read().await;
        for trigger in triggers.iter().filter(|t| t.matches(event)) {
            if let Some(pipeline) = pipelines.iter().find(|p| p.id == trigger.pipeline_id) {
                let started_at = chrono::Utc::now().timestamp();
                let result = pipeline.run().await;
                let ended_at = chrono::Utc::now().timestamp();
                // Task #81：写入持久化层（失败仅 warn，不影响触发返回）
                if let Some(ref store) = self.store {
                    if let Err(e) = store.record_run(&result, started_at, ended_at).await {
                        tracing::warn!(error = %e, pipeline = %pipeline.id, "auto-store record_run failed");
                    }
                }
                started.push(format!("{}: {:?}", pipeline.id, result.state));
            }
        }
        started
    }

    /// 直接执行某 pipeline（独立于 trigger 系统，用于手动触发）
    pub async fn fire_pipeline(&self, pipeline_id: &str) -> Option<pipeline::PipelineRunResult> {
        let pipelines = self.pipelines.read().await;
        let pipeline = pipelines.iter().find(|p| p.id == pipeline_id)?;
        let started_at = chrono::Utc::now().timestamp();
        let result = pipeline.run().await;
        let ended_at = chrono::Utc::now().timestamp();
        if let Some(ref store) = self.store {
            if let Err(e) = store.record_run(&result, started_at, ended_at).await {
                tracing::warn!(error = %e, pipeline = %pipeline_id, "auto-store record_run failed");
            }
        }
        Some(result)
    }
}

impl Default for AutoEngine {
    fn default() -> Self { Self::new() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auto::pipeline::StepKind;

    #[tokio::test]
    async fn test_pipeline_run() {
        let mut pipeline = Pipeline::new("test-pipe");
        pipeline.add_step(Step::new("echo", StepKind::Script("echo hello".into())));
        let result = pipeline.run().await;
        assert_eq!(result.state, PipelineState::Completed);
        assert!(result.results.is_empty() || result.results.len() == 1);
    }

    #[tokio::test]
    async fn test_trigger_match() {
        let t = Trigger::new("data_updated", "pipe-1");
        let event = TriggerEvent { name: "data_updated".into(), payload: None };
        assert!(t.matches(&event));

        let wrong = TriggerEvent { name: "other".into(), payload: None };
        assert!(!t.matches(&wrong));
    }
}
