//! # MeetingMinutes — 会议纪要生成
//!
//! ## 场景
//! AgentMeeting 结束后自动生成结构化纪要，包含结论和未决项
//!
//! ## 依赖链
//! ```text
//! crate::specialist (SpecialistOpinion)
//!   └── crate::meeting::minutes ← 本文件
//! ```
//!
//! ## 边界
//! - 置信度 ≥ 0.5 归入结论
//! - 置信度 < 0.3 归入未决项

use crate::specialist::SpecialistOpinion;
use crate::meeting::context::TimelineEntry;

#[derive(Debug, Clone)]
pub struct MeetingMinutes {
    pub meeting_id: String,
    pub topic: String,
    pub total_turns: u32,
    pub participants: Vec<String>,
    pub conclusions: Vec<String>,
    pub unresolved: Vec<String>,
}

pub struct MeetingMinutesGenerator;

impl MeetingMinutesGenerator {
    pub fn generate(
        meeting_id: &str,
        topic: &str,
        participants: &[String],
        timeline: &[TimelineEntry],
        opinions: &[SpecialistOpinion],
    ) -> MeetingMinutes {
        let total_turns = timeline.len() as u32;

        let mut conclusions: Vec<String> = opinions.iter()
            .filter(|o| o.confidence >= 0.5)
            .map(|o| format!("[{}] {} (置信度: {:.1})", o.specialist_id.0, o.conclusion, o.confidence))
            .collect();

        for entry in timeline {
            conclusions.push(format!("轮次{} [{}]: {}", entry.turn, entry.speaker.0, entry.conclusion));
        }

        let unresolved: Vec<String> = opinions.iter()
            .filter(|o| o.confidence < 0.3)
            .map(|o| format!("[{}] {}", o.specialist_id.0, o.conclusion))
            .collect();

        MeetingMinutes {
            meeting_id: meeting_id.to_string(),
            topic: topic.to_string(),
            total_turns,
            participants: participants.to_vec(),
            conclusions,
            unresolved,
        }
    }
}
