//! # Meeting 事件/状态/错误定义
//!
//! ## 场景
//! AgentMeeting (Mode 3) 的事件源和状态机。
//! MeetingEvent 是 Dashboard 层的唯一数据入口，广播给所有消费者。
//!
//! ## 依赖链
//! ```text
//! crate::specialist (SpecialistOpinion, ThinkingStep, ToolCallRecord)
//!   └── crate::meeting::event ← 本文件
//! ```
//!
//! ## 引用关系
//! - `MeetingStatus` 被 `MeetingSession` 持有
//! - `MeetingEvent` 通过 broadcast::Sender → Dashboard 消费
//!
//! ## 边界
//! - Error 事件不包含详情字段（避免敏感信息泄露）
//! - broadcast channel 满时丢弃最旧事件

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::specialist::{SpecialistId, SpecialistOpinion, SpecialistStatus};

// ─── 错误类型 ───────────────────────────────────────────────────────────

#[derive(Error, Debug, Clone, PartialEq)]
pub enum MeetingError {
    #[error("会议 {0} 未找到")]
    NotFound(String),

    #[error("Specialist {0} 不在与会者中")]
    NotParticipant(String),

    #[error("Specialist {0} 当前状态不可执行操作")]
    InvalidState(String),

    #[error("已达最大参与人数 ({max})")]
    MaxParticipants(u32),

    #[error("{0}")]
    Other(String),
}

// ─── MeetingStatus ──────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum MeetingStatus {
    Initializing,
    Inviting,
    Running,
    Paused,
    Completed,
    Cancelled,
}

impl MeetingStatus {
    pub fn can_transition_to(&self, next: &MeetingStatus) -> bool {
        use MeetingStatus::*;
        matches!((self, next),
            (Initializing, Inviting) |
            (Inviting, Running) | (Inviting, Cancelled) |
            (Running, Paused) | (Running, Completed) |
            (Paused, Running) | (Paused, Cancelled)
        )
    }

    pub fn is_active(&self) -> bool {
        matches!(self, MeetingStatus::Running | MeetingStatus::Inviting)
    }
}

// ─── MeetingEvent — Dashboard 事件源 ───────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MeetingEvent {
    SpecialistThinking {
        specialist_id: SpecialistId,
        turn: u32,
        step_index: u32,
        thought: String,
    },
    SpecialistOpinionReady {
        specialist_id: SpecialistId,
        opinion: SpecialistOpinion,
    },
    SpecialistStatusChange {
        specialist_id: SpecialistId,
        old_status: SpecialistStatus,
        new_status: SpecialistStatus,
    },
    MeetingStatusChange {
        old_status: MeetingStatus,
        new_status: MeetingStatus,
    },
    ParticipantJoined {
        specialist_id: SpecialistId,
        name: String,
    },
    ParticipantLeft {
        specialist_id: SpecialistId,
        name: String,
    },
    Info {
        message: String,
    },
}