//! # Meeting 事件/状态/错误
//!
//! ## 场景
//! - 定义 AgentMeeting (Mode 3) 的事件模型、状态机和错误类型
//! - `MeetingEvent` 是 Dashboard 层的唯⼀数据入口，broadcast 给所有消费者
//!
//! ## 依赖链
//! ```text
//! crate::specialist (SpecialistId, SpecialistOpinion, SpecialistStatus)
//!   └── crate::meeting::core ← 本文件
//! ```
//!
//! ## 引用关系
//! - `MeetingStatus` 被 `MeetingSession` 持有（生命周期管理）
//! - `MeetingEvent` 通过 broadcast::Sender → Dashboard 消费
//! - `MeetingError` 被所有 meeting 模块引用
//!
//! ## 边界
//! - Error 事件不包含详情字段（避免敏感信息泄露）
//! - broadcast channel 满时丢弃最旧事件

use serde::{Deserialize, Serialize};
use thiserror::Error;
use crate::specialist::{SpecialistId, SpecialistOpinion, SpecialistStatus};

#[derive(Error, Debug, Clone, PartialEq)]
pub enum MeetingError {
    #[error("会议 {0} 未找到")]
    NotFound(String),
    #[error("Specialist {0} 不在与会者中")]
    NotParticipant(String),
    #[error("已达最大参与人数 ({0})")]
    MaxParticipants(u32),
    #[error("输入过长 (超过 100K 字符)")]
    InputTooLong,
    #[error("{0} 已超过发言上限")]
    SpeechLimitExceeded(String),
    #[error("置信度 {0} 低于最低要求 {1}")]
    LowConfidence(f64, f64),
    #[error("结论包含反模式内容")]
    AntiPatternViolation,
    #[error("工具调用次数超限")]
    ToolCallLimitExceeded,
    #[error("{0}")]
    Other(String),
}

/// 会议生命周期状态
///
/// ## 场景
/// MeetingSession 的状态，控制哪些操作允许执行
///
/// ## 流转
/// ```text
/// Initializing → Inviting → Running ⇄ Paused
///                     ↘          ↘
///                    Cancelled   Completed
/// ```
///
/// ## 边界
/// - Completed/Cancelled 为终结态，不可再转换
/// - 只有 Running/Inviting 时 is_active() = true
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum MeetingStatus {
    Initializing, Inviting, Running, Paused, Completed, Cancelled,
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

/// Dashboard 事件源
///
/// ## 场景
/// MeetingSession 每次状态变化/工具调用/推理完成时广播
///
/// ## 消费方
/// - Dashboard UI (实时展示)
/// - 日志系统 (审计追踪)
/// - 主持人通知 (Host notification)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MeetingEvent {
    SpecialistThinking { specialist_id: SpecialistId, turn: u32, step_index: u32, thought: String },
    SpecialistOpinionReady { specialist_id: SpecialistId, opinion: SpecialistOpinion },
    SpecialistStatusChange { specialist_id: SpecialistId, old_status: SpecialistStatus, new_status: SpecialistStatus },
    MeetingStatusChange { old_status: MeetingStatus, new_status: MeetingStatus },
    ParticipantJoined { specialist_id: SpecialistId, name: String },
    ParticipantLeft { specialist_id: SpecialistId, name: String },
    ToolCallStarted { specialist_id: SpecialistId, tool_id: String, arguments: serde_json::Value },
    ToolCallCompleted { specialist_id: SpecialistId, tool_id: String, success: bool, latency_ms: u64 },
    Info { message: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_status_transitions_valid() {
        assert!(MeetingStatus::Initializing.can_transition_to(&MeetingStatus::Inviting));
        assert!(MeetingStatus::Inviting.can_transition_to(&MeetingStatus::Running));
        assert!(MeetingStatus::Running.can_transition_to(&MeetingStatus::Paused));
        assert!(MeetingStatus::Running.can_transition_to(&MeetingStatus::Completed));
        assert!(MeetingStatus::Paused.can_transition_to(&MeetingStatus::Running));
        assert!(MeetingStatus::Paused.can_transition_to(&MeetingStatus::Cancelled));
        assert!(MeetingStatus::Inviting.can_transition_to(&MeetingStatus::Cancelled));
    }

    #[test]
    fn test_status_transitions_invalid() {
        assert!(!MeetingStatus::Initializing.can_transition_to(&MeetingStatus::Running));
        assert!(!MeetingStatus::Initializing.can_transition_to(&MeetingStatus::Completed));
        assert!(!MeetingStatus::Completed.can_transition_to(&MeetingStatus::Running));
        assert!(!MeetingStatus::Cancelled.can_transition_to(&MeetingStatus::Running));
    }

    #[test]
    fn test_is_active() {
        assert!(MeetingStatus::Inviting.is_active());
        assert!(MeetingStatus::Running.is_active());
        assert!(!MeetingStatus::Paused.is_active());
        assert!(!MeetingStatus::Completed.is_active());
        assert!(!MeetingStatus::Cancelled.is_active());
        assert!(!MeetingStatus::Initializing.is_active());
    }
}
