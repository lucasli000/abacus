//! # MeetingRouter — 三级路由
//!
//! ## 场景
//! 用户输入 → 路由决策，三种匹配方式:
//! 1. @mention 精确匹配 (id / name / tags)
//! 2. 语义评分降级匹配
//! 3. 无匹配 → NoMatch + 建议
//!
//! ## 依赖链
//! ```text
//! crate::specialist (SpecialistId, SpecialistRegistry, Specialty)
//!   └── crate::meeting::router ← 本文件
//! ```
//!
//! ## 引用关系
//! - `MeetingRouter` 被 `MeetingSession` 持有
//! - `RoutingDecision` 被 `MeetingEngineAdapter` 消费
//!
//! ## 边界
//! - semantic_score 基于词袋匹配，非 embedding（v0.1，接口预留替换）
//! - 置信度 < 0.2 的 Specialist 不返回
//! - @mention 精确匹配优先于语义匹配

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::sync::Arc;
use crate::specialist::{SpecialistId, SpecialistInstance, SpecialistRegistry, Specialty};

/// 路由模式 — 决定 Specialist 的参与方式
#[derive(Debug, Clone, PartialEq)]
pub enum RoutingMode {
    /// 首次参与
    Fresh,
    /// 基于历史推理继续
    FollowUp,
    /// 所有专家同时分析
    Broadcast,
}

/// 路由决策结果
#[derive(Debug, Clone)]
pub enum RoutingDecision {
    Direct(SpecialistId, RoutingMode),
    Escalate(Vec<(SpecialistId, f64)>),
    NoMatch { input: String, suggestion: String },
}

pub struct MeetingRouter {
    registry: Arc<SpecialistRegistry>,
}

impl MeetingRouter {
    pub fn new(registry: Arc<SpecialistRegistry>) -> Self {
        Self { registry }
    }

    /// 三级 @mention 匹配
    ///
    /// ## 匹配顺序
    /// 1. registration.id (精确)
    /// 2. registration.name (精确)
    /// 3. registration.tags (精确)
    ///
    /// ## 返回
    /// `Some((reg_id, mention_text))` 或 `None`
    pub fn parse_mention(&self, input: &str) -> Option<(String, String)> {
        let registrations = self.registry.list_registrations();
        let tokens: Vec<&str> = input.split_whitespace().collect();
        for token in &tokens {
            if !token.starts_with('@') { continue; }
            let mention = &token[1..];
            for reg in &registrations {
                if reg.id == mention || reg.name == mention {
                    return Some((reg.id.clone(), mention.to_string()));
                }
                for tag in &reg.tags {
                    if tag == mention {
                        return Some((reg.id.clone(), mention.to_string()));
                    }
                }
            }
        }
        None
    }

    /// 词袋语义评分 (v0.1)
    ///
    /// ## 评分规则
    /// - 每个 hint_tag 匹配: +0.3
    /// - domain 匹配: +0.4
    /// - 封顶 1.0
    fn semantic_score(&self, input: &str, specialty: &Specialty) -> f64 {
        let lower = input.to_lowercase();
        let mut score = 0.0f64;
        for tag in &specialty.hint_tags {
            if lower.contains(&tag.to_lowercase()) {
                score += 0.3;
            }
        }
        if lower.contains(&specialty.domain.to_lowercase()) {
            score += 0.4;
        }
        score.min(1.0)
    }

    /// 全量匹配（降序，≥0.2）
    pub fn match_specialists(&self, input: &str) -> Vec<(String, f64)> {
        let mut r: Vec<(String, f64)> = self.registry.list_registrations().iter()
            .map(|reg| (reg.id.clone(), self.semantic_score(input, &reg.to_specialty())))
            .filter(|(_, s)| *s >= 0.2)
            .collect();
        r.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(Ordering::Equal));
        r
    }

    /// 分析输入 → 路由决策
    ///
    /// ## 流程
    /// 1. @mention → Direct(实际 participant ID, FollowUp/Fresh)
    /// 2. 语义匹配 → Direct(最高分 participant) / Escalate(多候选)
    /// 3. 无匹配 → NoMatch
    pub fn analyze_context(&self, input: &str, participants: &BTreeMap<String, SpecialistInstance>) -> RoutingDecision {
        if let Some((reg_id, _)) = self.parse_mention(input) {
            let prefix = format!("sp-{}", reg_id);
            let actual_id = participants.keys()
                .find(|k| k.starts_with(&prefix))
                .cloned()
                .unwrap_or(prefix);
            let mode = if participants.contains_key(&actual_id) {
                RoutingMode::FollowUp
            } else {
                RoutingMode::Fresh
            };
            return RoutingDecision::Direct(SpecialistId(actual_id), mode);
        }
        let prefix_match = |reg_id: &str| -> bool {
            participants.keys().any(|k| k.starts_with(&format!("sp-{}", reg_id)))
        };
        let all_matched = self.match_specialists(input);
        let mut participant_scores: Vec<(String, f64)> = all_matched.into_iter()
            .filter(|(id, _)| prefix_match(id))
            .filter(|(_, s)| *s >= 0.3)
            .collect();
        participant_scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(Ordering::Equal));
        match participant_scores.len() {
            0 => RoutingDecision::NoMatch {
                input: input.into(),
                suggestion: "尝试 @coder 或 @reviewer 指定专家".into(),
            },
            1 => RoutingDecision::Direct(SpecialistId(format!("sp-{}", participant_scores[0].0)), RoutingMode::FollowUp),
            _ => RoutingDecision::Escalate(
                participant_scores.into_iter()
                    .map(|(id, s)| {
                        let prefix = format!("sp-{}", id);
                        let actual = participants.keys()
                            .find(|k| k.starts_with(&prefix))
                            .cloned()
                            .unwrap_or(prefix);
                        (SpecialistId(actual), s)
                    })
                    .collect()
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::specialist::{EngagementLimit, SpecialistRegistration};
    use crate::team::AgentRole;

    fn make_registry() -> Arc<SpecialistRegistry> {
        let mut reg = SpecialistRegistry::new();
        reg.register(SpecialistRegistration {
            id: "coder".into(), domain: "coding".into(), name: "Coder".into(),
            role: AgentRole::Member, model: "test".into(),
            guide_strategy: "".into(), anti_pattern: "".into(),
            capabilities: vec!["code".into()], tags: vec!["代码".into(), "编程".into()],
            allowed_tools: vec![], engagement: EngagementLimit::default(),
        }).unwrap();
        reg.register(SpecialistRegistration {
            id: "reviewer".into(), domain: "review".into(), name: "Reviewer".into(),
            role: AgentRole::Advisor, model: "test".into(),
            guide_strategy: "".into(), anti_pattern: "".into(),
            capabilities: vec!["review".into()], tags: vec!["审查".into()],
            allowed_tools: vec![], engagement: EngagementLimit::default(),
        }).unwrap();
        Arc::new(reg)
    }

    fn make_participants(registry: &SpecialistRegistry) -> BTreeMap<String, SpecialistInstance> {
        let mut map = BTreeMap::new();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let coder = rt.block_on(registry.create_instance("coder", AgentRole::Member)).unwrap();
        let reviewer = rt.block_on(registry.create_instance("reviewer", AgentRole::Advisor)).unwrap();
        map.insert(coder.id.0.clone(), coder);
        map.insert(reviewer.id.0.clone(), reviewer);
        map
    }

    #[test]
    fn test_parse_mention_by_id() {
        let registry = make_registry();
        let router = MeetingRouter::new(registry);
        let result = router.parse_mention("请 @coder 看看这段代码");
        assert!(result.is_some());
        assert_eq!(result.unwrap().0, "coder");
    }

    #[test]
    fn test_parse_mention_by_name() {
        let registry = make_registry();
        let router = MeetingRouter::new(registry);
        let result = router.parse_mention("请 @Coder 看看");
        assert!(result.is_some());
        assert_eq!(result.unwrap().0, "coder");
    }

    #[test]
    fn test_parse_mention_by_tag() {
        let registry = make_registry();
        let router = MeetingRouter::new(registry);
        let result = router.parse_mention("关于 @编程 的问题");
        assert!(result.is_some());
        assert_eq!(result.unwrap().0, "coder");
    }

    #[test]
    fn test_parse_mention_no_match() {
        let registry = make_registry();
        let router = MeetingRouter::new(registry);
        let result = router.parse_mention("没有 at anyone");
        assert!(result.is_none());
    }

    #[test]
    fn test_analyze_context_direct_mention() {
        let registry = make_registry();
        let router = MeetingRouter::new(registry.clone());
        let participants = make_participants(&registry);
        let decision = router.analyze_context("请 @coder 分析", &participants);
        match decision {
            RoutingDecision::Direct(id, mode) => {
                assert!(id.0.contains("coder"));
                assert_eq!(mode, RoutingMode::FollowUp);
            }
            _ => panic!("expected Direct"),
        }
    }

    #[test]
    fn test_analyze_context_no_match() {
        let registry = make_registry();
        let router = MeetingRouter::new(registry.clone());
        let participants = make_participants(&registry);
        let decision = router.analyze_context("今天的天气怎么样", &participants);
        assert!(matches!(decision, RoutingDecision::NoMatch { .. }));
    }

    #[test]
    fn test_match_specialists_returns_sorted() {
        let registry = make_registry();
        let router = MeetingRouter::new(registry);
        let results = router.match_specialists("写一段代码");
        assert!(!results.is_empty());
        assert_eq!(results[0].0, "coder");
    }
}
