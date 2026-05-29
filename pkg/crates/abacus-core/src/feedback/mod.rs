//! Feedback 子系统（P3-B4）
//!
//! ## 模块
//! - `trajectory`: MT-GRPO 轨迹收集器（Zeng et al. 2024）
//!
//! ## 使用时机
//! - 每次 turn 结束时自动收集（TurnPipeline::post_process）
//! - 导出为 JSONL 供离线 MT-GRPO 训练脚本消费
//! - 未来可扩展到在线 PPO 训练

pub mod trajectory;
