use std::sync::Arc;
use tokio::sync::{RwLock, Semaphore};
use abacus_core::core::{CoreLoop, SessionState, RequestContext};
use crate::meeting::builder::{MeetingSessionBuilder, MeetingSessionHandle};
use crate::meeting::core::{MeetingError, MeetingStatus};
use crate::meeting::bridge::MeetingTurnResult;
use crate::meeting::assembler::MeetingPromptAssembler;
use crate::meeting::harness::MeetingHarnessProvider;
use crate::meeting::router::RoutingMode;
use crate::specialist::SpecialistOpinion;
use crate::team::AgentRole;

/// Configuration for a specialist participant in live meeting mode.
///
/// ## V36-1 通道选择说明
/// `system_prompt` 在 Meeting 中目前**不直接走 RequestContext.system_prompt_override**，
/// 因为 Meeting prompt 是"会议主题/其他 specialist 意见/context_pool 快照"等动态上下文的
/// 拼接（见 `meeting::assembler::MeetingPromptAssembler`），不是稳定角色 prompt——
/// cache 命中率低，走 user message 拼接更适合。
///
/// 若未来某 specialist 的 system_prompt 完全独立于会议状态（纯角色定义），
/// 可考虑在 bridge.rs 处把它注入 `req_ctx.system_prompt_override`，
/// 让该 specialist 的角色段进入可缓存 system 段。
/// Meeting 专家配置
///
/// ## 传输类型
/// - 本地: 通过 CoreLoop.process_turn() 执行（LLM 推理）
/// - 外部: 通过 MCP/HTTP 调用外部 Agent（transport 非 None）
#[derive(Debug, Clone)]
pub struct SpecialistConfig {
    pub id: String,
    pub name: String,
    pub model: String,
    pub system_prompt: String,
    pub role: AgentRole,
    /// 外部 Agent 传输配置（None = 本地 LLM 专家）
    #[allow(dead_code)]
    pub transport: Option<AgentTransport>,
}

/// 外部 Agent 传输配置
#[derive(Debug, Clone)]
pub struct AgentTransport {
    /// 传输类型: "mcp" | "http"
    pub transport_type: String,
    /// 端点地址
    pub endpoint: String,
}

impl SpecialistConfig {
    /// 创建本地 LLM 专家
    pub fn local(id: &str, name: &str, model: &str, system_prompt: &str, role: AgentRole) -> Self {
        Self {
            id: id.to_string(),
            name: name.to_string(),
            model: model.to_string(),
            system_prompt: system_prompt.to_string(),
            role,
            transport: None,
        }
    }

    /// 创建外部 Agent 专家
    pub fn external(id: &str, name: &str, endpoint: &str, transport_type: &str, role: AgentRole) -> Self {
        Self {
            id: id.to_string(),
            name: name.to_string(),
            model: String::new(),
            system_prompt: String::new(),
            role,
            transport: Some(AgentTransport {
                transport_type: transport_type.to_string(),
                endpoint: endpoint.to_string(),
            }),
        }
    }

    /// 是否为外部 Agent
    pub fn is_external(&self) -> bool {
        self.transport.is_some()
    }
}

/// Orchestrates a live meeting with real LLM-powered specialists.
///
/// Shares `Arc<CoreLoop>` with the server/CLI — no exclusive ownership required.
pub struct MeetingManager {
    core: Arc<CoreLoop>,
    session_state: Arc<RwLock<SessionState>>,
    handle: Option<MeetingSessionHandle>,
    topic: String,
    specialist_configs: Vec<SpecialistConfig>,
    max_concurrent: usize,
    max_rounds: u32,
}

impl MeetingManager {
    pub fn new(
        core: Arc<CoreLoop>,
        session_state: Arc<RwLock<SessionState>>,
        topic: String,
    ) -> Self {
        Self {
            core,
            session_state,
            handle: None,
            topic,
            specialist_configs: Vec::new(),
            max_concurrent: 4,
            max_rounds: 1,
        }
    }

    pub fn with_max_concurrent(mut self, n: usize) -> Self {
        self.max_concurrent = n.max(1);
        self
    }

    pub fn with_max_rounds(mut self, n: u32) -> Self {
        self.max_rounds = n.max(1);
        self
    }

    pub fn add_specialist(&mut self, config: SpecialistConfig) {
        self.specialist_configs.push(config);
    }

    pub async fn build(&mut self) -> Result<(), MeetingError> {
        let mut builder = MeetingSessionBuilder::new(&self.topic);
        for sp in &self.specialist_configs {
            builder = builder.with_specialist_role(&sp.id, sp.role.clone());
        }
        let handle = builder.build().await?;
        self.handle = Some(handle);
        Ok(())
    }

    /// Run meeting with concurrent specialist execution.
    ///
    /// Phase 1 — Host distributes task to each specialist (sequential, prompt assembly)
    /// Phase 2 — All specialists run concurrently (semaphore-limited LLM calls)
    /// Phase 3 — Host collects and summarizes results (sequential opinion recording)
    ///
    /// Each specialist uses its own configured model.
    /// Single model = all specialists share (fallback).
    pub async fn run_all(&mut self) -> Result<Vec<MeetingTurnResult>, MeetingError> {
        let handle = self.handle.as_mut().ok_or_else(|| {
            MeetingError::Other("MeetingManager: build() must be called before run_all()".into())
        })?;

        handle.session_mut().status = MeetingStatus::Inviting;
        handle.start()?;

        let meeting_id = handle.session().id.clone();
        let n = self.specialist_configs.len();
        let sem = Arc::new(Semaphore::new(self.max_concurrent));

        // Phase 1: Host distributes tasks — build per-specialist prompts
        let mut inputs = Vec::with_capacity(n);
        let host_id = handle.session().host().cloned();
        for sp in &self.specialist_configs {
            let sp_instance = handle.session().participants.get(&sp.id)
                .ok_or_else(|| MeetingError::NotParticipant(sp.id.clone()))?;
            let (target, question) = if host_id.as_ref().is_some_and(|h| h.0 == sp.id) {
                (sp.id.clone(), format!("作为主持人，分析会议主题并协调讨论: {}", self.topic))
            } else {
                handle.session().host_assign(&sp.id, &format!("分析会议主题: {}", self.topic))
            };
            let prompt = MeetingPromptAssembler::assemble_specialist_prompt(
                &handle.session().topic,
                &handle.session().participants,
                &handle.session().context_pool,
                sp_instance,
                &RoutingMode::Fresh,
            );
            let final_prompt = format!("{}\n\n{}", question, prompt);
            inputs.push((sp.clone(), target, final_prompt));
        }

        // Phase 2: Concurrent execution with semaphore
        // 故障隔离：单个 specialist panic 不中止整个 meeting（fail-partial 策略）
        // 内部 LLM 专家: core.process() + RequestContext::fast()
        // 外部 Agent 专家: MCP/HTTP 调用（transport 非 None）
        let core = self.core.clone();
        let session = self.session_state.clone();
        let mut handles = Vec::with_capacity(n);
        for (sp, _target, prompt) in inputs {
            let sem = sem.clone();
            let c = core.clone();
            let s = session.clone();
            handles.push(tokio::spawn(async move {
                let _permit = match sem.acquire().await {
                    Ok(p) => p,
                    Err(_) => return (sp, None, Some("semaphore closed before acquire".into())),
                };
                const SPECIALIST_TIMEOUT_SECS: u64 = 90;

                // 外部 Agent 专家：通过 MCP/HTTP 调用
                if let Some(ref transport) = sp.transport {
                    let agent_id = sp.id.clone();
                    let transport = transport.clone();
                    let work = call_external_specialist(&transport, &prompt);
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(SPECIALIST_TIMEOUT_SECS),
                        work,
                    ).await {
                        Ok(Ok(response)) => (sp, Some(response), None),
                        Ok(Err(e)) => (sp, None, Some(e)),
                        Err(_) => (sp, None, Some(format!(
                            "external agent '{}' timed out after {}s", agent_id, SPECIALIST_TIMEOUT_SECS
                        ))),
                    }
                } else {
                    // 内部 LLM 专家：core.process()
                    let work = c.process(&prompt, &s, RequestContext::fast());
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(SPECIALIST_TIMEOUT_SECS),
                        work,
                    ).await {
                        Ok(Ok(result)) => (sp, Some(result.response), None),
                        Ok(Err(e)) => (sp, None, Some(e.to_string())),
                        Err(_) => {
                            let id = sp.id.clone();
                            (sp, None, Some(format!(
                                "specialist '{}' timed out after {}s", id, SPECIALIST_TIMEOUT_SECS
                            )))
                        }
                    }
                }
            }));
        }

        // Phase 3: Collect results（fail-partial：记录失败，继续收集其余结果）
        let mut raw_results = Vec::with_capacity(n);
        let mut partial_errors: Vec<String> = Vec::new();
        for h in handles {
            match h.await {
                Ok((sp, Some(response), _)) => raw_results.push((sp, response)),
                Ok((_sp, _, Some(err))) => {
                    // 单个 specialist 失败：记录但继续（fail-partial）
                    tracing::warn!(error = %err, "specialist failed, continuing with remaining");
                    partial_errors.push(err);
                }
                Err(join_err) => {
                    // tokio task 级别失败（极罕见，任务被外部 abort）
                    let msg = format!("specialist task aborted: {join_err}");
                    tracing::error!(%msg, "meeting specialist task aborted");
                    partial_errors.push(msg);
                }
                _ => unreachable!(),
            }
        }

        // 全部 specialist 都失败则返回错误
        if raw_results.is_empty() && !partial_errors.is_empty() {
            return Err(MeetingError::Other(format!(
                "all specialists failed: {}",
                partial_errors.join("; ")
            )));
        }

        // Phase 4: Host processes opinions sequentially + generates summary
        let mut results = Vec::with_capacity(n);
        for (sp, response) in raw_results {
            let sp_instance = handle.session().participants.get(&sp.id)
                .ok_or_else(|| MeetingError::NotParticipant(sp.id.clone()))?;

            let is_host = host_id.as_ref().is_some_and(|h| h.0 == sp.id);
            let opinion = SpecialistOpinion {
                specialist_id: crate::specialist::SpecialistId(sp.id.clone()),
                turn: handle.session().context_pool.turn_count() + 1,
                conclusion: response.clone(),
                confidence: if is_host { 0.9 } else { 0.8 },
                reasoning_summary: String::new(),
                tool_evidence: vec![],
                suggestions: vec![],
                requires_attention: vec![],
                auto_approve: true,
                host_review_required: false,
            };

            MeetingHarnessProvider::post_check(&opinion, sp_instance)?;
            let sp_id = crate::specialist::SpecialistId(sp.id.clone());
            handle.session_mut().process_opinion(opinion.clone())?;

            results.push(MeetingTurnResult {
                meeting_id: meeting_id.clone(),
                target_specialist: sp_id,
                opinion: Some(opinion),
                engine_output: response,
                needs_clarify: false,
            });
        }

        // Host summary
        if host_id.is_some() {
            let summary = handle.session().host_summary();
            let summary_text: Vec<String> = summary.iter()
                .map(|e| format!("[{}] {}", e.speaker.0, e.conclusion))
                .collect();
            tracing::info!(?summary_text, "Meeting host summary");
        }

        handle.complete()?;
        Ok(results)
    }

    pub fn handle(&self) -> Option<&MeetingSessionHandle> {
        self.handle.as_ref()
    }

    pub fn handle_mut(&mut self) -> Option<&mut MeetingSessionHandle> {
        self.handle.as_mut()
    }
}

/// 调用外部 Agent 专家
///
/// 通过 MCP 协议向外部 Agent 发送咨询请求，获取专家意见。
/// 当前实现：HTTP POST /invoke（占位，Phase C 完善 MCP 协议调用）
async fn call_external_specialist(
    transport: &AgentTransport,
    prompt: &str,
) -> Result<String, String> {
    match transport.transport_type.as_str() {
        "http" => {
            // HTTP REST 调用
            let client = reqwest::Client::new();
            let resp = client
                .post(format!("{}/invoke", transport.endpoint))
                .json(&serde_json::json!({
                    "prompt": prompt,
                    "timeout_secs": 90,
                }))
                .timeout(std::time::Duration::from_secs(90))
                .send()
                .await
                .map_err(|e| format!("HTTP call failed: {}", e))?;

            let body: serde_json::Value = resp.json().await
                .map_err(|e| format!("HTTP response parse failed: {}", e))?;

            body.get("response")
                .and_then(|v| v.as_str())
                .map(String::from)
                .ok_or_else(|| "Missing 'response' field in agent response".into())
        }
        "mcp" => {
            // MCP 协议调用（复用 McpClient）
            // TODO: Phase C 实现完整的 MCP 协议调用
            Err("MCP transport not yet implemented for meeting specialists".into())
        }
        other => Err(format!("Unsupported transport type: {}", other)),
    }
}
