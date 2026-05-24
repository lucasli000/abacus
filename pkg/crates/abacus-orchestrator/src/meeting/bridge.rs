//! # MeetingEngineAdapter — CoreLoop 桥接
//!
//! ## 场景
//! AgentMeeting 模式下，将 MeetingSession 与 CoreLoop 连接起来:
//! 1. route_input() → 路由到 Specialist
//! 2. MeetingPromptAssembler → 组装 prompt
//! 3. CoreLoop.process_turn() → LLM 推理
//! 4. MeetingHarnessProvider.post_check() → 校验结论
//! 5. MeetingSession.process_opinion() → 记录
//!
//! ## 依赖链
//! ```text
//! abacus-core::core (CoreLoop, SessionState)
//! crate::meeting::* (session / assembler / harness / router)
//!   └── crate::meeting::bridge ← 本文件
//! ```
//!
//! ## 引用关系
//! - 持有 `MeetingSession`
//! - 接收外部 `CoreLoop` 和 `SessionState`（不创建）
//!
//! ## 边界
//! - `process_turn` 需要 `CoreLoop` 外部传入（注入模式）
//! - 仅 `Running` 状态可执行 process_turn
//! - Escalate 路由选第⼀个候选人直接推理

use tokio::sync::RwLock;
use abacus_core::core::CoreLoop;
use abacus_core::core::SessionState;
use crate::meeting::core::{MeetingError, MeetingStatus};
use crate::meeting::session::MeetingSession;
use crate::meeting::assembler::MeetingPromptAssembler;
use crate::meeting::harness::MeetingHarnessProvider;
use crate::meeting::router::{RoutingDecision, RoutingMode};
use crate::specialist::{SpecialistId, SpecialistOpinion};

pub struct MeetingTurnResult {
    pub meeting_id: String,
    pub target_specialist: SpecialistId,
    pub opinion: Option<SpecialistOpinion>,
    pub engine_output: String,
}

pub struct MeetingEngineAdapter {
    pub session: MeetingSession,
}

impl MeetingEngineAdapter {
    pub fn new(session: MeetingSession) -> Self {
        Self { session }
    }

    // ─── 核心流程 ─────────────────────────────────────
    // process_turn → route → assemble → core_loop → post_check → record

    /// 执行一次会议推理轮次
    ///
    /// ## 流程
    /// 1. 校验会议状态 (Running)
    /// 2. route_input() → RoutingDecision
    /// 3. 按 Direct/Escalate 执行:
    ///    a. pre_check
    ///    b. assemble prompt
    ///    c. CoreLoop.process_turn()
    ///    d. extract SpecialistOpinion
    ///    e. post_check
    ///    f. process_opinion() 记录
    ///
    /// ## 边界
    /// - NoMatch 直接返回错误
    /// - opinion.confidence 为简化估算值（0.8 Direct / 0.7 Escalate）
    pub async fn process_turn(
        &mut self,
        input: &str,
        core: &CoreLoop,
        session_state: &RwLock<SessionState>,
    ) -> Result<MeetingTurnResult, MeetingError> {
        self.process_turn_cancellable(input, core, session_state, None).await
    }

    /// P2 修复：带外部 cancel token 的 process_turn。
    /// token 为 None 时行为与 process_turn 等价。token cancel 时让 in-flight
    /// LLM 请求与 90s 超时一起失败（取最先发生的）。
    pub async fn process_turn_cancellable(
        &mut self,
        input: &str,
        core: &CoreLoop,
        session_state: &RwLock<SessionState>,
        cancel: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<MeetingTurnResult, MeetingError> {
        if self.session.status != MeetingStatus::Running {
            return Err(MeetingError::Other("会议未启动或已结束".into()));
        }

        let decision = self.session.route_input(input);

        match decision {
            RoutingDecision::Direct(sp_id, mode) => {
                let sp = self.session.participants.get(&sp_id.0)
                    .ok_or_else(|| MeetingError::NotParticipant(sp_id.0.clone()))?;
                MeetingHarnessProvider::pre_check(input, sp)?;
                let sp_clone = sp.clone();
                let prompt = MeetingPromptAssembler::assemble_specialist_prompt(
                    &self.session.topic,
                    &self.session.participants,
                    &self.session.context_pool,
                    &sp_clone,
                    &mode,
                );
                // V0.2: 使用 specialist 偏好模型（如有）
                // Phase 4 + Gap B 修复（thinking refactor）：specialist YAML engagement.thinking 注入 RequestContext，
                // 让研究员 high / 文案 low 等 per-specialist thinking 指令走进 LlmRequest.thinking_intent。
                // 引用关系：sp_clone.specialty.engagement.parse_thinking_intent() 定义于 specialist/mod.rs:111。
                let mut req_ctx = if let Some(ref model) = sp_clone.preferred_model {
                    abacus_core::core::RequestContext::with_model(model)
                } else {
                    abacus_core::core::RequestContext::default()
                };
                req_ctx.thinking_intent = sp_clone.specialty.engagement.parse_thinking_intent();
                // 单参与者超时: 90 秒防止会议死锁；同时接外部 cancel
                let participant_timeout = std::time::Duration::from_secs(600);
                let engine_result = run_specialist(
                    core, &prompt, session_state, req_ctx, participant_timeout, cancel.clone(),
                ).await
                    .map_err(|e| MeetingError::Other(format!(
                        "specialist '{}': {}", sp_id.0, e
                    )))?;
                let opinion = SpecialistOpinion {
                    specialist_id: sp_id.clone(),
                    turn: self.session.context_pool.turn_count() + 1,
                    conclusion: engine_result.response.clone(),
                    confidence: 0.8,
                    reasoning_summary: String::new(),
                    tool_evidence: vec![],
                    suggestions: vec![],
                    requires_attention: vec![],
                    auto_approve: true,
                    host_review_required: false,
                };
                let sp = self.session.participants.get(&sp_id.0)
                    .ok_or_else(|| MeetingError::NotParticipant(sp_id.0.clone()))?;
                MeetingHarnessProvider::post_check(&opinion, sp)?;
                self.session.process_opinion(opinion.clone())?;
                Ok(MeetingTurnResult {
                    meeting_id: self.session.id.clone(),
                    target_specialist: sp_id,
                    opinion: Some(opinion),
                    engine_output: engine_result.response,
                })
            }
            RoutingDecision::Escalate(candidates) => {
                let top = candidates.first()
                    .ok_or_else(|| MeetingError::Other("无候选 Specialist".into()))?;
                let sp_id = &top.0;
                let sp = self.session.participants.get(&sp_id.0)
                    .ok_or_else(|| MeetingError::NotParticipant(sp_id.0.clone()))?;
                MeetingHarnessProvider::pre_check(input, sp)?;
                let sp_clone = sp.clone();
                let prompt = MeetingPromptAssembler::assemble_specialist_prompt(
                    &self.session.topic,
                    &self.session.participants,
                    &self.session.context_pool,
                    &sp_clone,
                    &RoutingMode::Fresh,
                );
                // V0.2: Escalate 路径使用 specialist 偏好模型
                // Phase 4 + Gap B 修复：同 routing 路径一致注入 thinking_intent。
                let mut req_ctx = if let Some(ref model) = sp_clone.preferred_model {
                    abacus_core::core::RequestContext::with_model(model)
                } else {
                    abacus_core::core::RequestContext::default()
                };
                req_ctx.thinking_intent = sp_clone.specialty.engagement.parse_thinking_intent();
                let participant_timeout = std::time::Duration::from_secs(600);
                let engine_result = run_specialist(
                    core, &prompt, session_state, req_ctx, participant_timeout, cancel.clone(),
                ).await
                    .map_err(|e| MeetingError::Other(format!(
                        "specialist '{}' (escalated): {}", sp_id.0, e
                    )))?;
                let opinion = SpecialistOpinion {
                    specialist_id: sp_id.clone(),
                    turn: self.session.context_pool.turn_count() + 1,
                    conclusion: engine_result.response.clone(),
                    confidence: 0.7,
                    reasoning_summary: String::new(),
                    tool_evidence: vec![],
                    suggestions: vec![],
                    requires_attention: vec![],
                    auto_approve: true,
                    host_review_required: false,
                };
                let sp = self.session.participants.get(&sp_id.0)
                    .ok_or_else(|| MeetingError::NotParticipant(sp_id.0.clone()))?;
                MeetingHarnessProvider::post_check(&opinion, sp)?;
                self.session.process_opinion(opinion.clone())?;
                Ok(MeetingTurnResult {
                    meeting_id: self.session.id.clone(),
                    target_specialist: sp_id.clone(),
                    opinion: Some(opinion),
                    engine_output: engine_result.response,
                })
            }
            RoutingDecision::NoMatch { suggestion, .. } => {
                Err(MeetingError::Other(suggestion))
            }
        }
    }
}

/// 运行单个 specialist：内部 90s 超时 + 外部 cancel token 二选一最先发生者退出。
///
/// P2 修复：把原本散落在两个分支的 `tokio::time::timeout` + `core.process` 模式
/// 统一封装；同时支持外部 cancel（drop = HTTP 请求 cancel，tokio runtime 保证）。
async fn run_specialist(
    core: &CoreLoop,
    prompt: &str,
    session_state: &RwLock<SessionState>,
    req_ctx: abacus_core::core::RequestContext,
    timeout: std::time::Duration,
    cancel: Option<tokio_util::sync::CancellationToken>,
) -> Result<abacus_core::core::TurnResult, String> {
    let work = core.process(prompt, session_state, req_ctx);
    let timed = tokio::time::timeout(timeout, work);
    let result = match cancel {
        Some(token) => tokio::select! {
            r = timed => r,
            _ = token.cancelled() => return Err("cancelled by caller".into()),
        },
        None => timed.await,
    };
    match result {
        Ok(Ok(r)) => Ok(r),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err(format!("timed out after {}s", timeout.as_secs())),
    }
}
