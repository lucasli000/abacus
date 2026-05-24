//! # meeting — AgentMeeting (Mode 3)
//!
//! ## 场景
//! Abacus 三模式之一 — "专家会诊"模式:
//! - Mode 1: 单 Agent 多步骤 (CoreLoop.process_turn)
//! - Mode 2: 主 Agent 指派 SubAgent (TeamSession + SubAgentDispatcher)
//! - Mode 3: 专家斗法 (MeetingSession + Specialist)  ← 本模块
//!
//! 适用: 方案评审、多领域决策、架构审查、代码审查会议
//!
//! ## 架构
//! ```text
//! meeting/
//!   core.rs       — MeetingError/Status/Event
//!   context.rs    — ContextPool 三层上下文
//!   router.rs     — MeetingRouter 三级路由
//!   session.rs    — MeetingSession 核心编排
//!   minutes.rs    — MeetingMinutesGenerator
//!   assembler.rs  — MeetingPromptAssembler 提示词组装
//!   harness.rs    — MeetingHarnessProvider 输入/输出校验
//!   event_layer.rs— MagChain 中间件 (tool call → event)
//!   defaults.rs   — 默认 Specialist 注册 (Coder/Reviewer/Architect)
//!   config.rs     — YAML 配置加载
//!   bridge.rs     — CoreLoop 桥接适配器
//! ```
//!
//! ## 依赖链
//! ```text
//! abacus-core (CoreLoop, MagChain)
//!   └── crate::specialist (SpecialistInstance, SpecialistRegistry)
//!         └── crate::meeting ← 本模块
//! ```
//!
//! ## 边界
//! - 最多 8 个 Specialist 同时参与 (configurable)
//! - timeline > 200 轮触发压缩
//! - Specialist 通过 @mention 或语义匹配路由

pub mod core;
pub mod context;
pub mod router;
pub mod session;
pub mod minutes;
pub mod assembler;
pub mod harness;
pub mod event_layer;
pub mod defaults;
pub mod config;
pub mod bridge;
pub mod builder;
pub mod manager;
pub mod store;

pub use core::*;
pub use context::*;
pub use router::*;
pub use session::*;
pub use minutes::*;
pub use assembler::*;
pub use harness::*;
pub use event_layer::*;
pub use defaults::*;
pub use config::*;
pub use bridge::*;
pub use manager::*;
pub use builder::*;
pub use store::*;
