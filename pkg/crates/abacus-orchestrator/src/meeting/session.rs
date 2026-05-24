//! # MeetingSession — 会议编排核心
//!
//! ## 场景
//! AgentMeeting (Mode 3) 的核心编排器，管理:
//! - Specialist 生命周期（邀请/移除）
//! - 状态机流转（启动/暂停/完成/取消）
//! - 输入路由 + 推理结论处理
//!
//! ## 依赖链
//! ```text
//! crate::team (AgentRole)
//! crate::specialist (SpecialistRegistry, SpecialistInstance, SpecialistOpinion)
//!   └── crate::meeting (ContextPool, MeetingRouter, MeetingEvent)
//!         └── crate::meeting::session ← 本文件
//! ```
//!
//! ## 引用关系
//! - `MeetingSession` 被 `MeetingEngineAdapter` 持有（`crate::meeting::bridge`）
//! - 持有 `ContextPool` / `MeetingRouter` / `event_tx`
//!
//! ## 边界
//! - participants 以 `SpecialistId.0` (String) 为 key
//! - invite 在 capacity 检查之后才 create_instance（防泄露）
//! - remove 同步清理 registry（防泄露）

use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::broadcast;
use crate::team::AgentRole;
use crate::meeting::core::{MeetingError, MeetingEvent, MeetingStatus};
use crate::meeting::context::{ContextPool, TimelineEntry};
use crate::meeting::router::{MeetingRouter, RoutingDecision};
use crate::specialist::{SpecialistId, SpecialistInstance, SpecialistOpinion, SpecialistRegistry, SpecialistStatus};

pub struct MeetingSession {
    pub id: String,
    pub topic: String,
    pub status: MeetingStatus,
    pub host_id: Option<SpecialistId>,
    pub participants: BTreeMap<String, SpecialistInstance>,
    pub context_pool: ContextPool,
    pub router: MeetingRouter,
    pub registry: Arc<SpecialistRegistry>,
    pub event_tx: broadcast::Sender<MeetingEvent>,
    pub max_participants: u32,
}

impl MeetingSession {
    // ─── 生命周期 ─────────────────────────────────────
    // new → invite → start → running → (pause → resume)* → complete/cancel

    pub fn new(
        id: String,
        topic: String,
        registry: Arc<SpecialistRegistry>,
        event_tx: broadcast::Sender<MeetingEvent>,
    ) -> Self {
        let router = MeetingRouter::new(registry.clone());
        Self {
            id,
            topic,
            status: MeetingStatus::Initializing,
            host_id: None,
            participants: BTreeMap::new(),
            context_pool: ContextPool::new(),
            router,
            registry,
            event_tx,
            max_participants: 8,
        }
    }

    /// 邀请 Specialist 加入
    ///
    /// ## 流程
    /// 1. 校验 capacity
    /// 2. registry.create_instance() 创建运行时实例
    /// 3. 首个加入者自动成为主持人
    /// 4. 加入 participants
    /// 5. emit ParticipantJoined 事件
    pub async fn invite(&mut self, reg_id: &str, role: AgentRole) -> Result<(), MeetingError> {
        if self.participants.len() as u32 >= self.max_participants {
            return Err(MeetingError::MaxParticipants(self.max_participants));
        }
        let instance = self.registry.create_instance(reg_id, role).await
            .map_err(|e| MeetingError::Other(e.to_string()))?;
        let sp_id = instance.id.clone();
        let name = instance.name.clone();
        let is_first = self.participants.is_empty();
        self.participants.insert(sp_id.0.clone(), instance);
        if is_first {
            self.host_id = Some(sp_id.clone());
            let _ = self.event_tx.send(MeetingEvent::MeetingStatusChange {
                old_status: self.status.clone(),
                new_status: self.status.clone(),
            });
        }
        let _ = self.event_tx.send(MeetingEvent::ParticipantJoined {
            specialist_id: sp_id,
            name,
        });
        Ok(())
    }

    /// 当前主持人 ID
    pub fn host(&self) -> Option<&SpecialistId> {
        self.host_id.as_ref()
    }

    /// 是否主持人
    pub fn is_host(&self, sp_id: &SpecialistId) -> bool {
        self.host_id.as_ref() == Some(sp_id)
    }

    /// 主持人分发问题给指定专家
    pub fn host_assign(&self, target: &str, question: &str) -> (String, String) {
        let prefix = if self.is_host_by_id(target) {
            "(主持人自我分配) ".to_string()
        } else {
            format!("(主持人指派 @{}) ", target)
        };
        (target.to_string(), format!("{}{}", prefix, question))
    }

    /// 获取摘要：主持人获得所有意见的汇总
    pub fn host_summary(&self) -> &[TimelineEntry] {
        self.context_pool.recent(10)
    }

    fn is_host_by_id(&self, id: &str) -> bool {
        self.host_id.as_ref().is_some_and(|h| h.0 == id)
    }

    /// 移除 Specialist
    ///
    /// ## 流程
    /// 1. 从 participants 移除
    /// 2. registry.remove_instance() 清理
    /// 3. emit ParticipantLeft 事件
    pub async fn remove(&mut self, sp_id: &SpecialistId) -> Result<(), MeetingError> {
        let instance = self.participants.remove(&sp_id.0)
            .ok_or_else(|| MeetingError::NotParticipant(sp_id.0.clone()))?;
        self.registry.remove_instance(sp_id).await;
        let _ = self.event_tx.send(MeetingEvent::ParticipantLeft {
            specialist_id: sp_id.clone(),
            name: instance.name,
        });
        Ok(())
    }

    // ─── 运行 ─────────────────────────────────────────

    pub fn start(&mut self) -> Result<(), MeetingError> {
        let old = self.status.clone();
        if !self.status.can_transition_to(&MeetingStatus::Running) {
            return Err(MeetingError::Other(format!("无法从 {:?} 启动会议", self.status)));
        }
        self.status = MeetingStatus::Running;
        let _ = self.event_tx.send(MeetingEvent::MeetingStatusChange {
            old_status: old,
            new_status: MeetingStatus::Running,
        });
        Ok(())
    }

    pub fn pause(&mut self) -> Result<(), MeetingError> {
        let old = self.status.clone();
        if !self.status.can_transition_to(&MeetingStatus::Paused) {
            return Err(MeetingError::Other(format!("无法从 {:?} 暂停会议", self.status)));
        }
        self.status = MeetingStatus::Paused;
        let _ = self.event_tx.send(MeetingEvent::MeetingStatusChange {
            old_status: old,
            new_status: MeetingStatus::Paused,
        });
        Ok(())
    }

    pub fn complete(&mut self) -> Result<(), MeetingError> {
        let old = self.status.clone();
        if !self.status.can_transition_to(&MeetingStatus::Completed) {
            return Err(MeetingError::Other(format!("无法从 {:?} 结束会议", self.status)));
        }
        self.status = MeetingStatus::Completed;
        let _ = self.event_tx.send(MeetingEvent::MeetingStatusChange {
            old_status: old,
            new_status: MeetingStatus::Completed,
        });
        Ok(())
    }

    pub fn cancel(&mut self) -> Result<(), MeetingError> {
        let old = self.status.clone();
        if !self.status.can_transition_to(&MeetingStatus::Cancelled) {
            return Err(MeetingError::Other(format!("无法从 {:?} 取消会议", self.status)));
        }
        self.status = MeetingStatus::Cancelled;
        let _ = self.event_tx.send(MeetingEvent::MeetingStatusChange {
            old_status: old,
            new_status: MeetingStatus::Cancelled,
        });
        Ok(())
    }

    // ─── 路由与推理 ────────────────────────────────────

    pub fn route_input(&self, input: &str) -> RoutingDecision {
        self.router.analyze_context(input, &self.participants)
    }

    /// 处理 Specialist 推理结论
    ///
    /// ## 流程
    /// 1. 校验 participant
    /// 2. 更新状态 → Speaking
    /// 3. 写入 ContextPool timeline
    /// 4. emit SpecialistOpinionReady 事件
    pub fn process_opinion(&mut self, opinion: SpecialistOpinion) -> Result<(), MeetingError> {
        let sp_id = &opinion.specialist_id;
        let sp = self.participants.get_mut(&sp_id.0)
            .ok_or_else(|| MeetingError::NotParticipant(sp_id.0.clone()))?;
        let _ = sp.try_transition_to(SpecialistStatus::Speaking);
        self.context_pool.add_turn(TimelineEntry {
            turn: self.context_pool.turn_count() + 1,
            speaker: sp_id.clone(),
            conclusion: opinion.conclusion.clone(),
            confidence: opinion.confidence,
        });
        let _ = self.event_tx.send(MeetingEvent::SpecialistOpinionReady {
            specialist_id: sp_id.clone(),
            opinion,
        });
        Ok(())
    }

    pub fn participant_count(&self) -> usize {
        self.participants.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::specialist::{EngagementLimit, SpecialistRegistration};
    use tokio::sync::broadcast;

    fn make_registry() -> Arc<SpecialistRegistry> {
        let mut reg = SpecialistRegistry::new();
        reg.register(SpecialistRegistration {
            id: "coder".into(), domain: "coding".into(), name: "Coder".into(),
            role: AgentRole::Member, model: "test".into(),
            guide_strategy: "".into(), anti_pattern: "".into(),
            capabilities: vec![], tags: vec![],
            allowed_tools: vec![], engagement: EngagementLimit::default(),
        }).unwrap();
        reg.register(SpecialistRegistration {
            id: "reviewer".into(), domain: "review".into(), name: "Reviewer".into(),
            role: AgentRole::Advisor, model: "test".into(),
            guide_strategy: "".into(), anti_pattern: "".into(),
            capabilities: vec![], tags: vec![],
            allowed_tools: vec![], engagement: EngagementLimit::default(),
        }).unwrap();
        Arc::new(reg)
    }

    fn make_session() -> (MeetingSession, broadcast::Receiver<MeetingEvent>) {
        let registry = make_registry();
        let (tx, rx) = broadcast::channel(16);
        let session = MeetingSession::new("mtg_test".into(), "test topic".into(), registry, tx);
        (session, rx)
    }

    #[tokio::test]
    async fn test_invite_and_count() {
        let (mut session, _) = make_session();
        assert_eq!(session.participant_count(), 0);
        session.invite("coder", AgentRole::Member).await.unwrap();
        assert_eq!(session.participant_count(), 1);
    }

    #[tokio::test]
    async fn test_invite_max_capacity() {
        let (mut session, _) = make_session();
        session.max_participants = 1;
        session.invite("coder", AgentRole::Member).await.unwrap();
        let result = session.invite("reviewer", AgentRole::Advisor).await;
        assert!(matches!(result, Err(MeetingError::MaxParticipants(1))));
    }

    #[tokio::test]
    async fn test_remove_success() {
        let (mut session, _) = make_session();
        session.invite("coder", AgentRole::Member).await.unwrap();
        let sp_id = session.participants.keys().next().cloned().unwrap();
        let sp_id = SpecialistId(sp_id);
        session.remove(&sp_id).await.unwrap();
        assert_eq!(session.participant_count(), 0);
    }

    #[tokio::test]
    async fn test_remove_not_participant() {
        let (mut session, _) = make_session();
        let result = session.remove(&SpecialistId("sp-nonexistent".into())).await;
        assert!(matches!(result, Err(MeetingError::NotParticipant(_))));
    }

    #[tokio::test]
    async fn test_lifecycle_start() {
        let (mut session, _) = make_session();
        assert_eq!(session.status, MeetingStatus::Initializing);
        session.status = MeetingStatus::Inviting;
        session.start().unwrap();
        assert_eq!(session.status, MeetingStatus::Running);
    }

    #[test]
    fn test_lifecycle_pause_complete_cancel() {
        let (mut session, _) = make_session();
        session.status = MeetingStatus::Running;
        session.pause().unwrap();
        assert_eq!(session.status, MeetingStatus::Paused);
        session.start().unwrap();
        assert_eq!(session.status, MeetingStatus::Running);
        session.complete().unwrap();
        assert_eq!(session.status, MeetingStatus::Completed);
    }

    #[test]
    fn test_lifecycle_invalid_transition() {
        let (mut session, _) = make_session();
        assert!(session.start().is_err());
        assert!(session.complete().is_err());
    }

    #[test]
    fn test_process_opinion_updates_status() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (mut session, _) = rt.block_on(async {
            let registry = make_registry();
            let (tx, _) = broadcast::channel(16);
            let mut s = MeetingSession::new("mtg_test".into(), "t".into(), registry, tx);
            s.invite("coder", AgentRole::Member).await.unwrap();
            (s, ())
        });
        let sp_id = SpecialistId(session.participants.keys().next().cloned().unwrap());
        let sp = session.participants.get_mut(&sp_id.0).unwrap();
        sp.status = SpecialistStatus::Thinking;
        let _ = sp;
        let opinion = SpecialistOpinion {
            specialist_id: sp_id.clone(),
            turn: 1,
            conclusion: "code looks good".into(),
            confidence: 0.9,
            reasoning_summary: "".into(),
            tool_evidence: vec![],
            suggestions: vec![],
            requires_attention: vec![],
            auto_approve: true,
            host_review_required: false,
        };
        session.process_opinion(opinion).unwrap();
        let sp = session.participants.get(&sp_id.0).unwrap();
        assert_eq!(sp.status, SpecialistStatus::Speaking);
        assert_eq!(session.context_pool.turn_count(), 1);
    }

    #[test]
    fn test_process_opinion_not_participant() {
        let (mut session, _) = make_session();
        let opinion = SpecialistOpinion {
            specialist_id: SpecialistId("sp-unknown".into()),
            turn: 1,
            conclusion: "x".into(),
            confidence: 0.9,
            reasoning_summary: "".into(),
            tool_evidence: vec![],
            suggestions: vec![],
            requires_attention: vec![],
            auto_approve: true,
            host_review_required: false,
        };
        assert!(session.process_opinion(opinion).is_err());
    }

    #[test]
    fn test_event_emission_on_opinion() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (mut session, mut rx) = rt.block_on(async {
            let registry = make_registry();
            let (tx, mut rx) = broadcast::channel(16);
            let mut s = MeetingSession::new("mtg_test".into(), "t".into(), registry, tx);
            s.invite("coder", AgentRole::Member).await.unwrap();
            // drain all initial events (MeetingStatusChange + ParticipantJoined)
            while rx.try_recv().is_ok() {}
            (s, rx)
        });
        let sp_id = SpecialistId(session.participants.keys().next().cloned().unwrap());
        let sp = session.participants.get_mut(&sp_id.0).unwrap();
        sp.status = SpecialistStatus::Thinking;
        let _ = sp;
        let opinion = SpecialistOpinion {
            specialist_id: sp_id.clone(),
            turn: 1, conclusion: "ok".into(), confidence: 0.9,
            reasoning_summary: "".into(), tool_evidence: vec![],
            suggestions: vec![], requires_attention: vec![],
            auto_approve: true, host_review_required: false,
        };
        session.process_opinion(opinion).unwrap();
        let event = rx.try_recv();
        assert!(matches!(event, Ok(MeetingEvent::SpecialistOpinionReady { .. })));
    }
}
