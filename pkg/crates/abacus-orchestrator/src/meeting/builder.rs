//! # MeetingSessionBuilder — 会议会话构建器
//!
//! ## 场景
//! 为用户提供简单的入口创建 Meeting 会话，隐藏 13 个文件的使用细节。
//!
//! ## 配置优先级
//! 1. `with_config_file(path)` — 从 YAML 文件加载
//! 2. `with_config(cfg)` — 直接传入配置对象
//! 3. 均未提供 → 空注册表（需通过 `with_specialist()` 手动指定）
//!
//! ## 使用示例
//! ```rust,ignore
//! // 方式 A: YAML 配置文件
//! let mut h = MeetingSessionBuilder::new("架构评审")
//!     .with_config_file("examples/meeting-code-review.yaml")
//!     .build().await?;
//!
//! // 方式 B: CLI 指定专家 (registry 为空时自动创建 GenericExpert)
//! let mut h = MeetingSessionBuilder::new("讨论")
//!     .with_specialist("coder")
//!     .with_specialist("reviewer")
//!     .build().await?;
//! ```
//!
//! ## 边界
//! - 不创建或持有 CoreLoop — 调用方自行管理引擎
//! - config 和 specialist_ids 会合并：config 中的注册 + CLI 指定的邀请

use tokio::sync::{broadcast, RwLock};
use crate::meeting::core::{MeetingError, MeetingEvent, MeetingStatus};
use crate::meeting::session::MeetingSession;
use crate::meeting::bridge::{MeetingEngineAdapter, MeetingTurnResult};
use crate::meeting::minutes::{MeetingMinutes, MeetingMinutesGenerator};
use crate::meeting::config::AbacusOrchestratorConfig;
use crate::specialist::{EngagementLimit, SpecialistRegistration, SpecialistRegistry};
use crate::team::AgentRole;

pub struct MeetingSessionBuilder {
    topic: String,
    specialists: Vec<(String, AgentRole)>, // (id, role)
    config: Option<AbacusOrchestratorConfig>,
}

impl MeetingSessionBuilder {
    pub fn new(topic: impl Into<String>) -> Self {
        Self {
            topic: topic.into(),
            specialists: Vec::new(),
            config: None,
        }
    }

    /// 通过 CLI 指定要邀请的专家 ID（默认 Member 角色）
    pub fn with_specialist(mut self, id: &str) -> Self {
        self.specialists.push((id.to_string(), AgentRole::Member));
        self
    }

    /// 指定专家 ID 和角色
    pub fn with_specialist_role(mut self, id: &str, role: AgentRole) -> Self {
        self.specialists.push((id.to_string(), role));
        self
    }

    /// 从 YAML 文件加载配置（含 meeting 参数 + specialist 注册列表）
    pub fn with_config_file(mut self, path: &str) -> Self {
        match AbacusOrchestratorConfig::from_file(path) {
            Ok(cfg) => self.config = Some(cfg),
            Err(e) => tracing::warn!("加载配置失败: {} (使用回退)", e),
        }
        self
    }

    /// 直接注入配置对象
    pub fn with_config(mut self, config: AbacusOrchestratorConfig) -> Self {
        self.config = Some(config);
        self
    }

    /// 构建会议会话
    ///
    /// ## 流程
    /// 1. 收集所有注册（config + fallback）
    /// 2. 创建 broadcast 事件通道
    /// 3. 创建 MeetingSession
    /// 4. 邀请 Specialist
    pub async fn build(self) -> Result<MeetingSessionHandle, MeetingError> {
        let mut all_registrations: Vec<SpecialistRegistration> = Vec::new();

        if let Some(cfg) = &self.config {
            all_registrations.extend(cfg.specialists.clone());
        }

        for (sp_id, role) in &self.specialists {
            let already = all_registrations.iter().any(|r| r.id == *sp_id);
            if !already {
                all_registrations.push(SpecialistRegistration {
                    id: sp_id.clone(),
                    domain: "general".into(),
                    name: sp_id.clone(),
                    role: role.clone(),
                    model: "deepseek-v4-flash".into(),
                    guide_strategy: format!("You are a domain expert in {}. Analyze the question from your perspective.", sp_id),
                    anti_pattern: "".into(),
                    capabilities: vec![],
                    tags: vec![sp_id.clone()],
                    allowed_tools: vec![],
                    engagement: EngagementLimit::default(),
                });
            }
        }

        let mut registry = SpecialistRegistry::new();
        for reg in &all_registrations {
            registry.register(reg.clone())
                .map_err(|e| MeetingError::Other(e.to_string()))?;
        }

        let (event_tx, event_rx) = broadcast::channel(64);

        let mut session = MeetingSession::new(
            format!("mtg_{}", chrono::Utc::now().timestamp_millis()),
            self.topic,
            std::sync::Arc::new(registry),
            event_tx.clone(),
        );

        if let Some(cfg) = &self.config {
            session.max_participants = cfg.meeting.max_participants;
        }

        for (sp_id, role) in &self.specialists {
            session.invite(sp_id, role.clone()).await?;
        }

        // V28.7: 邀请完成后显式推进到 Inviting 状态——后续 start() 调用才能合法
        // 转到 Running（Initializing → Running 的单跳被状态机拒绝，必须经 Inviting）
        // 引用关系：MeetingStatus::can_transition_to 规则；状态机定义见 core.rs:69-77
        if !session.participants.is_empty() && session.status == MeetingStatus::Initializing {
            session.status = MeetingStatus::Inviting;
        }

        Ok(MeetingSessionHandle {
            adapter: MeetingEngineAdapter::new(session),
            event_tx,
            event_rx,
        })
    }
}

pub struct MeetingSessionHandle {
    adapter: MeetingEngineAdapter,
    event_tx: broadcast::Sender<MeetingEvent>,
    event_rx: broadcast::Receiver<MeetingEvent>,
}

impl MeetingSessionHandle {
    pub fn start(&mut self) -> Result<(), MeetingError> {
        self.adapter.session.start()
    }

    pub fn pause(&mut self) -> Result<(), MeetingError> {
        self.adapter.session.pause()
    }

    pub fn complete(&mut self) -> Result<(), MeetingError> {
        self.adapter.session.complete()
    }

    pub fn cancel(&mut self) -> Result<(), MeetingError> {
        self.adapter.session.cancel()
    }

    pub async fn process(
        &mut self,
        input: &str,
        core: &abacus_core::core::CoreLoop,
        session_state: &RwLock<abacus_core::core::SessionState>,
    ) -> Result<MeetingTurnResult, MeetingError> {
        if self.adapter.session.status != MeetingStatus::Running {
            return Err(MeetingError::Other("会议未启动，请先调用 start()".into()));
        }
        self.adapter.process_turn(input, core, session_state).await
    }

    /// P2: 带外部 cancel token 的 process（server timeout/客户端断开时唤醒）
    pub async fn process_cancellable(
        &mut self,
        input: &str,
        core: &abacus_core::core::CoreLoop,
        session_state: &RwLock<abacus_core::core::SessionState>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<MeetingTurnResult, MeetingError> {
        if self.adapter.session.status != MeetingStatus::Running {
            return Err(MeetingError::Other("会议未启动，请先调用 start()".into()));
        }
        self.adapter.process_turn_cancellable(input, core, session_state, Some(cancel)).await
    }

    pub fn route_debug(&self, input: &str) -> crate::meeting::router::RoutingDecision {
        self.adapter.session.route_input(input)
    }

    pub fn generate_minutes(&self) -> MeetingMinutes {
        let participants: Vec<String> = self.adapter.session.participants
            .values().map(|sp| sp.name.clone()).collect();
        MeetingMinutesGenerator::generate(
            &self.adapter.session.id,
            &self.adapter.session.topic,
            &participants,
            self.adapter.session.context_pool.all(),
            &[],
        )
    }

    pub fn subscribe(&self) -> broadcast::Receiver<MeetingEvent> {
        self.event_tx.subscribe()
    }

    pub fn try_recv_event(&mut self) -> Result<MeetingEvent, tokio::sync::broadcast::error::TryRecvError> {
        self.event_rx.try_recv()
    }

    pub fn session(&self) -> &MeetingSession {
        &self.adapter.session
    }

    pub fn session_mut(&mut self) -> &mut MeetingSession {
        &mut self.adapter.session
    }
}
