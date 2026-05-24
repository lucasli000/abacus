//! # MeetingEventLayer — MagChain 中间件
//!
//! ## 场景
//! AgentMeeting 中，MagChain 中间件拦截 Specialist 的每一次工具调用，
//! 通过 broadcast channel 发送 `MeetingEvent` 到 Dashboard。
//!
//! ## 依赖链
//! ```text
//! abacus-core::mag_chain (Middleware trait)
//! abacus-types (KernelError, ToolId, ToolOutput)
//! crate::meeting::core (MeetingEvent)
//!   └── crate::meeting::event_layer ← 本文件
//! ```
//!
//! ## 引用关系
//! - 实现 `Middleware` trait，可注册到 CoreLoop 的 MagChain
//! - 通过 event_tx 将事件广播出去
//!
//! ## 边界
//! - send 失败静默丢弃（broadcast channel 满）
//!
//! ## 注册示例
//! ```rust,ignore
//! use tokio::sync::broadcast;
//! use std::sync::Arc;
//! use abacus_core::core::CoreLoop;
//! use abacus_orchestrator::meeting::event_layer::MeetingEventLayer;
//! use abacus_orchestrator::specialist::SpecialistId;
//!
//! // 1. 创建事件总线
//! let (tx, _rx) = broadcast::channel(64);
//!
//! // 2. 为每个 Specialist 创建 EventLayer
//! let sp_id = SpecialistId("sp-coder".into());
//! let layer = Arc::new(MeetingEventLayer::new(tx, sp_id));
//!
//! // 3. 注册到 CoreLoop 的 MagChain
//! // core_loop.mag_chain_mut().add(layer);
//! ```

use async_trait::async_trait;
use abacus_core::mag_chain::Middleware;
use abacus_types::{engine::ToolOutput, KernelError, ToolId};
use serde_json::Value;
use tokio::sync::broadcast;
use crate::meeting::core::MeetingEvent;
use crate::specialist::SpecialistId;

pub struct MeetingEventLayer {
    event_tx: broadcast::Sender<MeetingEvent>,
    specialist_id: SpecialistId,
}

impl MeetingEventLayer {
    pub fn new(
        event_tx: broadcast::Sender<MeetingEvent>,
        specialist_id: SpecialistId,
    ) -> Self {
        Self { event_tx, specialist_id }
    }
}

#[async_trait]
impl Middleware for MeetingEventLayer {
    fn name(&self) -> &str {
        "meeting_event_layer"
    }

    async fn before_execute(
        &self,
        tool_id: &ToolId,
        params: &Value,
    ) -> Result<(), KernelError> {
        let _ = self.event_tx.send(MeetingEvent::ToolCallStarted {
            specialist_id: self.specialist_id.clone(),
            tool_id: tool_id.0.clone(),
            arguments: params.clone(),
        });
        Ok(())
    }

    async fn after_execute(
        &self,
        tool_id: &ToolId,
        output: &mut ToolOutput,
    ) -> Result<(), KernelError> {
        let _ = self.event_tx.send(MeetingEvent::ToolCallCompleted {
            specialist_id: self.specialist_id.clone(),
            tool_id: tool_id.0.clone(),
            success: output.success,
            latency_ms: output.latency_ms,
        });
        Ok(())
    }
}
