//! # MeetingHarnessProvider — 输入/输出校验
//!
//! ## 场景
//! AgentMeeting 中在 Specialist 推理前后执行校验:
//! - pre_check: 输入有效性、发言次数、工具调用上限
//! - post_check: 结论非空、置信度阈值、反模式匹配
//!
//! ## 依赖链
//! ```text
//! crate::meeting::core (MeetingError)
//! crate::specialist (SpecialistInstance, SpecialistOpinion)
//!   └── crate::meeting::harness ← 本文件
//! ```
//!
//! ## 边界
//! - 反模式检查按行分割，逐行`String::contains`匹配（非语义匹配）
//! - tool_call 上限检查基于总调用次数（v0.1 简化），跨 think 不重置

use crate::meeting::core::MeetingError;
use crate::specialist::{SpecialistInstance, SpecialistOpinion};

pub struct MeetingHarnessProvider;

impl MeetingHarnessProvider {
    pub fn pre_check(
        input: &str,
        specialist: &SpecialistInstance,
    ) -> Result<(), MeetingError> {
        if input.trim().is_empty() {
            return Err(MeetingError::Other("输入不能为空".into()));
        }
        if input.len() > 100_000 {
            return Err(MeetingError::InputTooLong);
        }
        if specialist.exceeded_speech_limit(&specialist.specialty.engagement) {
            return Err(MeetingError::SpeechLimitExceeded(specialist.name.clone()));
        }
        if specialist.tool_calls.len() as u32 >= specialist.specialty.engagement.max_tool_calls_per_think {
            return Err(MeetingError::ToolCallLimitExceeded);
        }
        Ok(())
    }

    pub fn post_check(
        opinion: &SpecialistOpinion,
        specialist: &SpecialistInstance,
    ) -> Result<(), MeetingError> {
        if opinion.conclusion.trim().is_empty() {
            return Err(MeetingError::Other("推理结论为空".into()));
        }
        let min_conf = specialist.specialty.engagement.min_confidence;
        if opinion.confidence < min_conf {
            return Err(MeetingError::LowConfidence(opinion.confidence, min_conf));
        }
        let anti = &specialist.specialty.anti_pattern;
        if !anti.is_empty() {
            for line in anti.lines() {
                let line = line.trim();
                if !line.is_empty() && opinion.conclusion.contains(line) {
                    return Err(MeetingError::AntiPatternViolation);
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::specialist::{EngagementLimit, Specialty, SpecialistId, SpecialistOpinion, SpecialistStatus, ToolCallRecord};
    use crate::team::AgentRole;

    fn make_specialist(tool_calls: u32) -> SpecialistInstance {
        SpecialistInstance {
            id: SpecialistId("sp-coder".into()),
            name: "Coder".into(),
            avatar: None,
            role: AgentRole::Member,
            specialty: Specialty {
                domain: "test".into(),
                description: "".into(),
                key_capabilities: vec![],
                hint_tags: vec![],
                expert_ref: None,
                guide_strategy: "".into(),
                anti_pattern: "unsafe".into(),
                knowledge_mounts: vec![],
                engagement: EngagementLimit {
                    max_speeches_per_round: 3,
                    min_confidence: 0.5,
                    max_tool_calls_per_think: 5,
                    ..Default::default()
                },
            },
            status: SpecialistStatus::Listening,
            current_turn: 0,
            speeches_count: 1,
            thinking: vec![],
            tool_calls: (0..tool_calls).map(|i| ToolCallRecord {
                tool_id: format!("tool_{}", i),
                arguments: serde_json::Value::Null,
                status: crate::specialist::ToolCallStatus::Success,
                result: None,
            }).collect(),
            preferred_model: None,
        }
    }

    #[test]
    fn test_pre_check_empty_input() {
        let sp = make_specialist(0);
        assert!(MeetingHarnessProvider::pre_check("", &sp).is_err());
        assert!(MeetingHarnessProvider::pre_check("   ", &sp).is_err());
    }

    #[test]
    fn test_pre_check_input_too_long() {
        let sp = make_specialist(0);
        let long = "a".repeat(100_001);
        let result = MeetingHarnessProvider::pre_check(&long, &sp);
        assert!(matches!(result, Err(MeetingError::InputTooLong)));
    }

    #[test]
    fn test_pre_check_speech_limit() {
        let mut sp = make_specialist(0);
        sp.speeches_count = 3;
        let result = MeetingHarnessProvider::pre_check("hello", &sp);
        assert!(matches!(result, Err(MeetingError::SpeechLimitExceeded(_))));
    }

    #[test]
    fn test_pre_check_tool_call_limit() {
        let sp = make_specialist(5);
        let result = MeetingHarnessProvider::pre_check("hello", &sp);
        assert!(matches!(result, Err(MeetingError::ToolCallLimitExceeded)));
    }

    #[test]
    fn test_pre_check_valid() {
        let sp = make_specialist(0);
        assert!(MeetingHarnessProvider::pre_check("hello", &sp).is_ok());
    }

    #[test]
    fn test_post_check_empty_conclusion() {
        let sp = make_specialist(0);
        let opinion = SpecialistOpinion {
            specialist_id: SpecialistId("sp-coder".into()),
            turn: 1, conclusion: "".into(), confidence: 0.9,
            reasoning_summary: "".into(), tool_evidence: vec![],
            suggestions: vec![], requires_attention: vec![],
            auto_approve: true, host_review_required: false,
        };
        assert!(MeetingHarnessProvider::post_check(&opinion, &sp).is_err());
    }

    #[test]
    fn test_post_check_low_confidence() {
        let sp = make_specialist(0);
        let opinion = SpecialistOpinion {
            specialist_id: SpecialistId("sp-coder".into()),
            turn: 1, conclusion: "ok".into(), confidence: 0.3,
            reasoning_summary: "".into(), tool_evidence: vec![],
            suggestions: vec![], requires_attention: vec![],
            auto_approve: true, host_review_required: false,
        };
        let result = MeetingHarnessProvider::post_check(&opinion, &sp);
        assert!(matches!(result, Err(MeetingError::LowConfidence(_, _))));
    }

    #[test]
    fn test_post_check_anti_pattern() {
        let sp = make_specialist(0);
        let opinion = SpecialistOpinion {
            specialist_id: SpecialistId("sp-coder".into()),
            turn: 1, conclusion: "建议使用 unsafe 代码优化".into(), confidence: 0.9,
            reasoning_summary: "".into(), tool_evidence: vec![],
            suggestions: vec![], requires_attention: vec![],
            auto_approve: true, host_review_required: false,
        };
        let result = MeetingHarnessProvider::post_check(&opinion, &sp);
        assert!(matches!(result, Err(MeetingError::AntiPatternViolation)));
    }

    #[test]
    fn test_post_check_valid() {
        let sp = make_specialist(0);
        let opinion = SpecialistOpinion {
            specialist_id: SpecialistId("sp-coder".into()),
            turn: 1, conclusion: "代码质量良好".into(), confidence: 0.9,
            reasoning_summary: "".into(), tool_evidence: vec![],
            suggestions: vec![], requires_attention: vec![],
            auto_approve: true, host_review_required: false,
        };
        assert!(MeetingHarnessProvider::post_check(&opinion, &sp).is_ok());
    }
}
