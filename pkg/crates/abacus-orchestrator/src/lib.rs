pub mod team;
pub mod subagent;
pub mod plan;
pub mod specialist;
pub mod meeting;

#[allow(ambiguous_glob_reexports)]
pub use team::*;
pub use subagent::*;
pub use plan::*;
pub use specialist::*;
#[allow(ambiguous_glob_reexports)]
pub use meeting::*;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn test_team_manager_register_session() {
        let mgr = team::TeamManager::new();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let session = team::TeamBuilder::new("team_1", "build feature").build();
        let arc = rt.block_on(mgr.register(session));
        assert_eq!(arc.team_id, "team_1");
        let found = rt.block_on(mgr.get("team_1"));
        assert!(found.is_some());
    }

    #[test]
    fn test_subagent_dispatch() {
        let dispatcher = subagent::SubAgentDispatcher::new();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let boundary = subagent::SubAgentBoundary {
            max_steps: 10,
            max_tokens: 4096,
            max_duration: std::time::Duration::from_secs(60),
            allowed_tools: vec![abacus_types::ToolId("fs_read".into())],
            forbidden_tools: vec![],
            context_scope: subagent::ContextScope::Isolated,
            allow_nesting: false,
            max_nesting_depth: 1,
            progressive_gate_scope: abacus_types::progressive::GateScope::TeamInExecution,
        };
        let ctx = subagent::SubAgentContext {
            parent_session_id: "sess_1".into(),
            inherited_keys: vec![],
            task_description: "test task".into(),
            nesting_depth: 0,
        };
        let instance = rt.block_on(dispatcher.create(boundary, ctx));
        assert!(instance.id.starts_with("sa_"));
    }

    #[test]
    fn test_plan_executor_step() {
        use plan::*;
        let executor = PlanExecutor::new();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let step = PlanStep {
            id: "step_1".into(),
            kind: StepKind::ToolCall { tool_id: "fs_read".into(), params: serde_json::json!({}) },
            description: "read file".into(),
            depends_on: vec![],
            status: StepStatus::Pending,
            result: None,
            retries: 0,
            max_retries: 1,
        };
        let result = rt.block_on(executor.execute_step(&step));
        assert!(result.is_err());
        assert!(format!("{}", result.unwrap_err()).contains("no services injected"));
    }

    // ─── Meeting 集成测试 ─────────────────────────────────────
    // 完整流程: route → assemble → pre_check → (simulate) → post_check → process_opinion

    #[test]
    fn test_meeting_full_flow() {
        use tokio::sync::broadcast;
        use specialist::{EngagementLimit, SpecialistRegistration, SpecialistId, SpecialistOpinion, SpecialistStatus};
        use meeting::*;
        use team::AgentRole;

        let rt = tokio::runtime::Runtime::new().unwrap();
        let (session, _rx) = rt.block_on(async {
            let mut reg = SpecialistRegistry::new();
            reg.register(SpecialistRegistration {
                id: "coder".into(), domain: "coding".into(), name: "Coder".into(),
                role: AgentRole::Member, model: "test".into(),
                guide_strategy: "代码实现专家".into(), anti_pattern: "unsafe".into(),
                capabilities: vec!["code".into()], tags: vec!["代码".into()],
                allowed_tools: vec![], engagement: EngagementLimit::default(),
            }).unwrap();
            let registry = Arc::new(reg);
            let (tx, rx) = broadcast::channel(16);
            let mut s = MeetingSession::new("mtg_int".into(), "代码审查".into(), registry.clone(), tx);
            s.invite("coder", AgentRole::Member).await.unwrap();
            assert_eq!(s.participant_count(), 1);
            s.status = MeetingStatus::Inviting;
            s.start().unwrap();
            (s, rx)
        });

        // 1. Route: @mention → Direct
        let decision = session.route_input("请 @coder 审查这段代码");
        let (sp_id, mode) = match &decision {
            RoutingDecision::Direct(id, mode) => (id.clone(), mode.clone()),
            _ => panic!("expected Direct, got {:?}", decision),
        };
        assert_eq!(mode, RoutingMode::FollowUp);
        assert!(sp_id.0.contains("coder"));

        // 2. Assemble prompt
        let sp = session.participants.get(&sp_id.0).unwrap();
        let prompt = MeetingPromptAssembler::assemble_specialist_prompt(
            &session.topic,
            &session.participants,
            &session.context_pool,
            sp,
            &mode,
        );
        assert!(prompt.contains("Coder"));
        assert!(prompt.contains("代码实现专家"));

        // 3. Pre-check
        assert!(MeetingHarnessProvider::pre_check("审查代码", sp).is_ok());

        // 4. Simulate opinion generation
        let opinion = SpecialistOpinion {
            specialist_id: sp_id.clone(),
            turn: 1,
            conclusion: "代码质量良好，建议优化错误处理".into(),
            confidence: 0.85,
            reasoning_summary: "审查了错误处理和性能".into(),
            tool_evidence: vec![],
            suggestions: vec!["添加错误处理".into()],
            requires_attention: vec![],
            auto_approve: true,
            host_review_required: false,
        };

        // 5. Post-check
        // Need mutable access for post_check. Drop immutable borrow first.
        let sp_name = sp.name.clone();
        let sp_eng = sp.specialty.engagement.clone();
        let sp_anti = sp.specialty.anti_pattern.clone();
        let _ = sp;
        assert!(MeetingHarnessProvider::post_check(&opinion, {
            // reconstruct minimal view for post_check
            &specialist::SpecialistInstance {
                id: sp_id.clone(),
                name: sp_name,
                avatar: None,
                role: AgentRole::Member,
                specialty: specialist::Specialty {
                    domain: "coding".into(), description: "".into(),
                    key_capabilities: vec![], hint_tags: vec![],
                    expert_ref: None,
                    guide_strategy: "".into(), anti_pattern: sp_anti,
                    knowledge_mounts: vec![],
                    engagement: sp_eng,
                },
                status: SpecialistStatus::Thinking,
                current_turn: 0, speeches_count: 0,
                thinking: vec![], tool_calls: vec![],
                preferred_model: None,
            }
        }).is_ok());

        // 6. Process opinion
        let (mut session, _rx) = rt.block_on(async {
            let mut reg = SpecialistRegistry::new();
            reg.register(SpecialistRegistration {
                id: "coder".into(), domain: "coding".into(), name: "Coder".into(),
                role: AgentRole::Member, model: "test".into(),
                guide_strategy: "".into(), anti_pattern: "".into(),
                capabilities: vec![], tags: vec![],
                allowed_tools: vec![], engagement: EngagementLimit::default(),
            }).unwrap();
            let registry = Arc::new(reg);
            let (tx, rx) = broadcast::channel(16);
            let mut s = MeetingSession::new("mtg_int2".into(), "t".into(), registry.clone(), tx);
            s.invite("coder", AgentRole::Member).await.unwrap();
            let sp_id = SpecialistId(s.participants.keys().next().unwrap().clone());
            let sp = s.participants.get_mut(&sp_id.0).unwrap();
            sp.status = SpecialistStatus::Thinking;
            let _ = sp;
            s.status = MeetingStatus::Running;
            (s, rx)
        });
        let sp_id = SpecialistId(session.participants.keys().next().unwrap().clone());
        let opinion = SpecialistOpinion {
            specialist_id: sp_id.clone(),
            turn: 1, conclusion: "结论".into(), confidence: 0.9,
            reasoning_summary: "".into(), tool_evidence: vec![],
            suggestions: vec![], requires_attention: vec![],
            auto_approve: true, host_review_required: false,
        };
        session.process_opinion(opinion).unwrap();

        // 7. Verify: timeline + status
        assert_eq!(session.context_pool.turn_count(), 1);
        let sp = session.participants.get(&sp_id.0).unwrap();
        assert_eq!(sp.status, SpecialistStatus::Speaking);
    }

    #[test]
    fn test_meeting_route_no_match() {
        use meeting::*;
        use specialist::{EngagementLimit, SpecialistRegistration, SpecialistRegistry};
        use team::AgentRole;
        use std::sync::Arc;

        let rt = tokio::runtime::Runtime::new().unwrap();
        let (session, _) = rt.block_on(async {
            let mut reg = SpecialistRegistry::new();
            reg.register(SpecialistRegistration {
                id: "coder".into(), domain: "coding".into(), name: "Coder".into(),
                role: AgentRole::Member, model: "test".into(),
                guide_strategy: "".into(), anti_pattern: "".into(),
                capabilities: vec![], tags: vec![],
                allowed_tools: vec![], engagement: EngagementLimit::default(),
            }).unwrap();
            let registry = Arc::new(reg);
            let (tx, _) = tokio::sync::broadcast::channel(16);
            let mut s = MeetingSession::new("mtg_nm".into(), "t".into(), registry.clone(), tx);
            s.invite("coder", AgentRole::Member).await.unwrap();
            (s, ())
        });

        let decision = session.route_input("今天天气怎么样");
        // V41 设计意图：NoMatch 降级为 Broadcast——把 input 推给所有已邀请专家，
        // 让 host 综合判断；只有当 participants 为空时才回 NoMatch。
        // 此场景已邀请 "coder"，故预期 Escalate（包含该专家）。
        assert!(matches!(decision, RoutingDecision::Escalate(_)),
            "V41：无匹配但有参与者时降级为 Escalate，got {:?}", decision);
    }
}

