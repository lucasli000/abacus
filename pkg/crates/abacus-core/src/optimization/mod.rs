//! Optimization 子系统（P2-B1）
//!
//! ## 模块
//! - `opro`: OPRO 风格 Prompt 优化器（Yang et al. 2023）
//!
//! ## 使用时机
//! - 由 AutoEngine CronScheduler 定期触发（每 6 小时）
//! - 用户命令 `/optimize-prompt <skill_id>` 手动触发
//! - 候选写入 pending_review 状态，等待用户确认

pub mod opro;
