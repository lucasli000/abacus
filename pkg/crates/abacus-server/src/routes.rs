use std::sync::Arc;

use axum::{
    extract::{State, Path},
    response::Json,
    routing::{get, post, delete},
    Router,
    http::StatusCode,
};
use serde::{Deserialize, Serialize};

use abacus_orchestrator::team::TeamBuilder;
use abacus_orchestrator::specialist::SpecialistRegistration;
use abacus_types::progressive::UserResponse;
use crate::server::AppState;

pub fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        // Core
        .route("/api/v1/health", get(health_handler))
        .route("/api/v1/chat", post(chat_handler))
        .route("/api/v1/chat/stream", post(chat_stream_handler))
        .route("/api/v1/chat/continue", post(continue_handler))
        // Sessions
        .route("/api/v1/sessions", get(list_sessions))
        .route("/api/v1/sessions/{id}", delete(delete_session))
        // Skills
        .route("/api/v1/skills", get(list_skills))
        // Models
        .route("/api/v1/models", get(list_models))
        // Config
        .route("/api/v1/config", get(show_config))
        // Teams
        .route("/api/v1/teams", get(list_teams).post(create_team))
        .route("/api/v1/teams/{id}", get(team_detail).delete(delete_team))
        // Meetings
        .route("/api/v1/meetings", get(list_meetings).post(create_meeting))
        .route("/api/v1/meetings/{id}", get(meeting_detail).delete(delete_meeting))
        .route("/api/v1/meetings/{id}/ask", post(meeting_ask))
        // Specialists
        .route("/api/v1/specialists", get(list_specialists).post(register_specialist))
        // Metrics
        .route("/metrics", get(metrics_handler))
        .with_state(state)
}

// ─── Shared Types ───────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct ErrorResponse { pub error: String }

#[derive(Debug, Deserialize)]
pub struct ChatRequest {
    pub message: String,
    pub session_id: Option<String>,
    /// Phase 4：per-request 思考意图覆盖（接受字符串：off/adaptive/low/medium/high/max/xhigh/minimal/<整数>）
    /// 缺省 → 走全局 core.thinking 配置
    #[serde(default)]
    pub thinking: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ChatResponse {
    pub response: String,
    pub session_id: String,
    pub tool_calls: usize,
}

#[derive(Debug, Serialize)]
pub struct SessionInfo {
    pub session_id: String,
    pub turn_count: u32,
}

// ─── Health ─────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub status: String,
    pub version: String,
    pub session_count: usize,
    pub team_count: usize,
    pub model_count: usize,
}

async fn health_handler(State(state): State<Arc<AppState>>) -> Json<HealthResponse> {
    let session_count = state.sessions.list().await.len();
    let team_count = state.team_manager.list().await.len();
    let model_count = state.core_loop.list_models().await.len();
    Json(HealthResponse {
        status: "ok".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        session_count,
        team_count,
        model_count,
    })
}

// ─── Chat ───────────────────────────────────────────────────────────────────

async fn chat_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ChatRequest>,
) -> Result<Json<ChatResponse>, (StatusCode, Json<ErrorResponse>)> {
    let session_id = req.session_id
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("sess_{}", uuid::Uuid::new_v4()));
    let (session, is_new) = state.sessions.get_or_create(&session_id).await;
    if is_new {
        let guard = session.read().await;
        state.core_loop.register_session_context_tools(&guard).await;
    }
    // 自适应超时：按 LLM 状态计算实际等待上限，只在极端情况（挂死）触发
    let (turn_count, timeout_secs) = {
        let s = session.read().await;
        let cfg = state.core_loop.config();
        let secs = crate::server::adaptive_timeout_secs(
            &cfg.default_model.0,
            cfg.thinking_intent.is_some(),
            cfg.default_max_tokens,
            s.turn_count,
            state.request_timeout_secs, // ceiling
        );
        (s.turn_count, secs)
    };
    tracing::debug!(
        timeout_secs, turn_count,
        model = %state.core_loop.config().default_model.0,
        thinking = state.core_loop.config().thinking_intent.is_some(),
        "adaptive timeout computed for chat"
    );
    // P2: 创建 cancel token；timeout 触发时 cancel，让 pipeline 内部 in-flight
    // reqwest 立即中断（drop = cancel，tokio runtime 保证）
    let cancel_token = tokio_util::sync::CancellationToken::new();
    let t0 = std::time::Instant::now();
    let token_for_pipeline = cancel_token.clone();

    // Phase 4：解析 per-request thinking 字段
    let mut req_ctx = abacus_core::RequestContext::default();
    if let Some(ref s) = req.thinking {
        req_ctx.thinking_intent = abacus_types::ThinkingIntent::from_str_loose(s);
    }
    let work = state.core_loop.process_turn_cancellable_with_context(
        &req.message, &session, req_ctx, token_for_pipeline);
    let outcome = tokio::time::timeout(Duration::from_secs(timeout_secs), work).await;
    let latency_ms = t0.elapsed().as_millis() as u64;

    match outcome {
        Ok(Ok(result)) => {
            state.metrics.record_request(
                latency_ms, true, false,
                result.stats.prompt_tokens, result.stats.completion_tokens, result.stats.cached_tokens,
                result.tool_outputs.len() as u64,
            );
            Ok(Json(ChatResponse {
                response: result.response,
                session_id: result.session_id,
                tool_calls: result.tool_outputs.len(),
            }))
        }
        Ok(Err(e)) => {
            state.metrics.record_request(latency_ms, false, false, 0, 0, 0, 0);
            tracing::error!(error = %e, "chat handler error");
            Err((StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse { error: e.user_message() })))
        },
        Err(_) => {
            // 关键：cancel token 让 in-flight LLM 请求立即终止，避免泄漏
            cancel_token.cancel();
            state.metrics.record_request(latency_ms, false, true, 0, 0, 0, 0);
            Err((StatusCode::GATEWAY_TIMEOUT, Json(ErrorResponse {
                error: format!(
                    "request timed out after {}s (model={}, thinking={}, turn={})",
                    timeout_secs,
                    state.core_loop.config().default_model.0,
                    state.core_loop.config().thinking_intent.is_some(),
                    turn_count,
                ),
            })))
        },
    }
}

// ─── Sessions ───────────────────────────────────────────────────────────────

async fn list_sessions(State(state): State<Arc<AppState>>) -> Json<Vec<SessionInfo>> {
    let tuples = state.sessions.list().await;
    Json(tuples.into_iter().map(|(session_id, turn_count)| SessionInfo { session_id, turn_count }).collect())
}

async fn delete_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    if state.sessions.remove(&id).await {
        Ok(Json(serde_json::json!({"deleted": id})))
    } else {
        Err((StatusCode::NOT_FOUND, Json(ErrorResponse { error: "session not found".into() })))
    }
}

// ─── Skills / Models / Config ───────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct SkillListResponse {
    pub skills: Vec<SkillInfo>,
    pub count: usize,
}

#[derive(Debug, Serialize)]
pub struct SkillInfo {
    pub id: String,
    pub version: String,
}

async fn list_skills(State(state): State<Arc<AppState>>) -> Json<SkillListResponse> {
    let skills = state.core_loop.list_skills().await;
    let infos: Vec<SkillInfo> = skills.into_iter().map(|s| SkillInfo {
        id: s.id.0, version: s.version,
    }).collect();
    let count = infos.len();
    Json(SkillListResponse { skills: infos, count })
}

/// `/api/v1/models` 端点 — 优先实时 discover（5s timeout），失败回退 cache，
/// 再失败回退 supported_models() 静态列表。
///
/// 查询参数：`?source=cache` 显式跳过实时拉取，仅读 cache。
async fn list_models(State(state): State<Arc<AppState>>) -> Json<Vec<String>> {
    use std::time::Duration;
    use abacus_core::llm::ModelCache;

    // 实时拉取（30s timeout）
    let live = tokio::time::timeout(
        Duration::from_secs(30),
        state.core_loop.discover_all_models(),
    ).await;

    let mut models: Vec<String> = match live {
        Ok(map) if !map.is_empty() => {
            map.values().flat_map(|v| v.iter().cloned()).collect()
        }
        _ => {
            // 实时失败 → 读 cache
            match ModelCache::load(&ModelCache::default_path()) {
                Ok(Some(cache)) => cache.all_models(),
                _ => state.core_loop.list_models().await, // cache 也没有 → 静态
            }
        }
    };
    models.sort();
    models.dedup();
    Json(models)
}

#[derive(Debug, Serialize)]
pub struct ConfigResponse {
    pub summary: String,
}

async fn show_config(State(state): State<Arc<AppState>>) -> Json<ConfigResponse> {
    Json(ConfigResponse { summary: state.config_manager.summary() })
}

// ─── Teams ──────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct CreateTeamRequest { pub team_id: String, pub goal: String }

#[derive(Debug, Serialize)]
pub struct TeamResponse { pub team_id: String, pub status: String }

#[derive(Debug, Serialize)]
pub struct TeamDetailResponse {
    pub team_id: String,
    pub status: String,
}

async fn create_team(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateTeamRequest>,
) -> Result<Json<TeamResponse>, (StatusCode, Json<ErrorResponse>)> {
    let session = TeamBuilder::new(&req.team_id, &req.goal).build();
    state.team_manager.register(session).await;
    Ok(Json(TeamResponse { team_id: req.team_id, status: "created".into() }))
}

async fn list_teams(State(state): State<Arc<AppState>>) -> Json<Vec<String>> {
    Json(state.team_manager.list().await)
}

async fn team_detail(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<TeamDetailResponse>, (StatusCode, Json<ErrorResponse>)> {
    let team = state.team_manager.get(&id).await
        .ok_or_else(|| (StatusCode::NOT_FOUND, Json(ErrorResponse { error: "team not found".into() })))?;
    let status = format!("{:?}", team.status().await);
    Ok(Json(TeamDetailResponse { team_id: id, status }))
}

async fn delete_team(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    if state.team_manager.remove(&id).await {
        Ok(Json(serde_json::json!({"deleted": id})))
    } else {
        Err((StatusCode::NOT_FOUND, Json(ErrorResponse { error: "team not found".into() })))
    }
}

// ─── Meetings ───────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Clone)]
pub struct MeetingSummary {
    pub id: String,
    pub topic: String,
    pub participant_count: usize,
    pub turn_count: u32,
}

#[derive(Debug, Deserialize)]
pub struct CreateMeetingRequest {
    pub id: String,
    pub topic: String,
    #[serde(default)]
    pub specialist_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct AskMeetingRequest {
    pub message: String,
}

// ─── Meetings: P1 接通 L3 MeetingSession ────────────────────────────────
// 改动语义：之前是 in-memory stub；现在通过 AppState.meetings (MeetingStore) 接通
// L3 已实现的 MeetingSessionBuilder + MeetingSessionHandle 全链路。

async fn create_meeting(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateMeetingRequest>,
) -> Result<Json<MeetingSummary>, (StatusCode, Json<ErrorResponse>)> {
    use abacus_orchestrator::meeting::MeetingSessionBuilder;

    let mut builder = MeetingSessionBuilder::new(&req.topic);
    for sp_id in &req.specialist_ids {
        builder = builder.with_specialist(sp_id);
    }

    let handle = builder.build().await.map_err(|e| {
        (StatusCode::INTERNAL_SERVER_ERROR,
         Json(ErrorResponse { error: format!("build meeting failed: {}", e) }))
    })?;

    let participant_count = handle.session().participants.len();
    state.meetings.register(req.id.clone(), handle).await;

    Ok(Json(MeetingSummary {
        id: req.id,
        topic: req.topic,
        participant_count,
        turn_count: 0,
    }))
}

async fn list_meetings(State(state): State<Arc<AppState>>) -> Json<Vec<MeetingSummary>> {
    let ids = state.meetings.list().await;
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        if let Some(h) = state.meetings.get(&id).await {
            let g = h.read().await;
            let s = g.session();
            out.push(MeetingSummary {
                id: s.id.clone(),
                topic: s.topic.clone(),
                participant_count: s.participants.len(),
                turn_count: s.context_pool.turn_count(),
            });
        }
    }
    Json(out)
}

async fn meeting_detail(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<MeetingSummary>, (StatusCode, Json<ErrorResponse>)> {
    let h = state.meetings.get(&id).await.ok_or_else(|| {
        (StatusCode::NOT_FOUND,
         Json(ErrorResponse { error: format!("meeting {id} not found") }))
    })?;
    let g = h.read().await;
    let s = g.session();
    Ok(Json(MeetingSummary {
        id: s.id.clone(),
        topic: s.topic.clone(),
        participant_count: s.participants.len(),
        turn_count: s.context_pool.turn_count(),
    }))
}

async fn delete_meeting(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    if state.meetings.remove(&id).await {
        Ok(Json(serde_json::json!({"deleted": id})))
    } else {
        Err((StatusCode::NOT_FOUND,
             Json(ErrorResponse { error: format!("meeting {id} not found") })))
    }
}

async fn meeting_ask(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<AskMeetingRequest>,
) -> Result<Json<ChatResponse>, (StatusCode, Json<ErrorResponse>)> {
    use abacus_orchestrator::meeting::MeetingStatus;

    let h = state.meetings.get(&id).await.ok_or_else(|| {
        (StatusCode::NOT_FOUND,
         Json(ErrorResponse { error: format!("meeting {id} not found") }))
    })?;

    // 单独 SessionState 池：每个 meeting 复用一个稳定的 server-side session
    let session_id = format!("mtg_{}", id);
    let (session_state, is_new) = state.sessions.get_or_create(&session_id).await;
    if is_new {
        let guard = session_state.read().await;
        state.core_loop.register_session_context_tools(&guard).await;
    }

    // bridge.rs 内部已含 90s 单参与者 timeout；此处用 server ceiling 作为外层兜底
    let timeout_secs = state.request_timeout_secs;
    let mut handle_guard = h.write().await;

    // 状态机：未启动则 start（Initializing → Inviting → Running）
    if handle_guard.session().status == MeetingStatus::Initializing {
        let _ = handle_guard.session_mut().status.clone();
        // session.invite 已在 build 时完成；这里直接尝试 start
        if let Err(e) = handle_guard.start() {
            // start 要求 Inviting；如果直接 Initializing，先转到 Inviting
            handle_guard.session_mut().status = MeetingStatus::Inviting;
            if let Err(e2) = handle_guard.start() {
                return Err((StatusCode::INTERNAL_SERVER_ERROR,
                           Json(ErrorResponse { error: format!("start meeting: {} / {}", e, e2) })));
            }
        }
    }

    // P2: cancel token + 外层 ceiling 双保险——timeout 触发时 cancel 让 in-flight LLM 立即终止
    let cancel_token = tokio_util::sync::CancellationToken::new();
    let token_for_pipeline = cancel_token.clone();
    let t0 = std::time::Instant::now();
    let outcome = tokio::time::timeout(
        Duration::from_secs(timeout_secs),
        handle_guard.process_cancellable(&req.message, &state.core_loop, &session_state, token_for_pipeline),
    ).await;
    let latency_ms = t0.elapsed().as_millis() as u64;

    match outcome {
        Ok(Ok(result)) => {
            // Meeting 路径暂未暴露 token stats（bridge MeetingTurnResult 不含）
            state.metrics.record_request(latency_ms, true, false, 0, 0, 0, 0);
            Ok(Json(ChatResponse {
                response: result.engine_output,
                session_id,
                tool_calls: 0,
            }))
        }
        Ok(Err(e)) => {
            state.metrics.record_request(latency_ms, false, false, 0, 0, 0, 0);
            tracing::error!(error = %e, "meeting ask handler error");
            Err((StatusCode::INTERNAL_SERVER_ERROR,
                 Json(ErrorResponse { error: e.to_string() })))
        }
        Err(_) => {
            cancel_token.cancel();
            state.metrics.record_request(latency_ms, false, true, 0, 0, 0, 0);
            Err((StatusCode::GATEWAY_TIMEOUT,
                 Json(ErrorResponse {
                     error: format!("meeting ask timed out after {}s", timeout_secs),
                 })))
        }
    }
}

// ─── Specialists ─────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct SpecialistInfo {
    pub id: String,
    pub name: String,
    pub domain: String,
    pub capabilities: Vec<String>,
    pub tags: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct RegisterSpecialistRequest {
    pub id: String,
    pub name: String,
    pub domain: String,
    /// 可选；缺省时从 ConfigManager.core.default_model 读取（M5 修复）
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub guide_strategy: String,
    #[serde(default)]
    pub anti_pattern: String,
}

async fn list_specialists(State(state): State<Arc<AppState>>) -> Json<Vec<SpecialistInfo>> {
    let reg = state.specialist_registry.read().await;
    let infos: Vec<SpecialistInfo> = reg.list_registrations().into_iter().map(|r| SpecialistInfo {
        id: r.id.clone(), name: r.name.clone(), domain: r.domain.clone(),
        capabilities: r.capabilities.clone(), tags: r.tags.clone(),
    }).collect();
    Json(infos)
}

async fn register_specialist(
    State(state): State<Arc<AppState>>,
    Json(req): Json<RegisterSpecialistRequest>,
) -> Result<Json<SpecialistInfo>, (StatusCode, Json<ErrorResponse>)> {
    // M5 修复：模型优先级 = req.model > ConfigManager.core.default_model > 内置 fallback
    let model = req.model.clone().unwrap_or_else(|| {
        state.config_manager.get_str("core.default_model")
            .map(String::from)
            .unwrap_or_else(|| "deepseek-v4-flash".into())
    });
    let registration = SpecialistRegistration {
        id: req.id.clone(),
        name: req.name.clone(),
        domain: req.domain.clone(),
        role: abacus_orchestrator::team::AgentRole::Member,
        model,
        guide_strategy: if req.guide_strategy.is_empty() { format!("{}专家", req.domain) } else { req.guide_strategy },
        anti_pattern: req.anti_pattern,
        capabilities: req.capabilities.clone(),
        tags: req.tags.clone(),
        allowed_tools: vec![],
        engagement: Default::default(),
    };
    state.specialist_registry.write().await.register(registration).map_err(|e| {
        (StatusCode::BAD_REQUEST, Json(ErrorResponse { error: e.to_string() }))
    })?;
    Ok(Json(SpecialistInfo {
        id: req.id, name: req.name, domain: req.domain,
        capabilities: req.capabilities, tags: req.tags,
    }))
}

// ─── SSE Stream ─────────────────────────────────────────────────────────

use axum::response::sse::{Event, KeepAlive, Sse};
use futures_util::stream::Stream;
use std::time::Duration;

async fn chat_stream_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ChatRequest>,
) -> Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>> {
    let session_id = req.session_id
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("sess_{}", uuid::Uuid::new_v4()));
    let (session, is_new) = state.sessions.get_or_create(&session_id).await;
    if is_new {
        let guard = session.read().await;
        state.core_loop.register_session_context_tools(&guard).await;
    }
    // stream_requests_total: SSE 连接建立时计数（无法用 record_request 因为流式无确定结束时刻）
    state.metrics.stream_requests_total.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let core = state.core_loop.clone();
    let metrics = state.metrics.clone();
    // P2 修复 (C4)：cancel token 用 RAII 守护——stream 结束（含客户端断开）时
    // _drop_guard 析构会触发 cancel，让 spawn 的 pipeline 任务在 phase boundary
    // 退出，并通过 complete_cancellable 中断 in-flight reqwest，避免孤儿任务。
    let cancel_token = tokio_util::sync::CancellationToken::new();
    let drop_guard = cancel_token.clone().drop_guard();
    let stream = async_stream::stream! {
        // 把 drop_guard move 进 stream 闭包：stream drop（客户端断连）时 token 自动 cancel
        let _drop_guard = drop_guard;
        yield Ok(Event::default().event("session").data(session_id.clone()));

        // V0.2: 真实流式 — 通过 stream channel 逐 chunk 推送 SSE events
        let (stream_tx, mut stream_rx) = tokio::sync::mpsc::unbounded_channel::<abacus_core::llm::stream::StreamChunk>();

        let core_clone = core.clone();
        let session_clone = session.clone();
        let message = req.message.clone();
        let token_for_pipeline = cancel_token.clone();
        let handle = tokio::spawn(async move {
            core_clone.process_turn_streaming_cancellable(
                &message, &session_clone, stream_tx, token_for_pipeline,
            ).await
        });

        // 实时转发 streaming chunks 为 SSE events
        while let Some(chunk) = stream_rx.recv().await {
            use abacus_core::llm::stream::StreamChunk;
            match chunk {
                StreamChunk::TextDelta(t) => {
                    yield Ok(Event::default().event("content_delta").data(t));
                }
                StreamChunk::Thinking(t) => {
                    yield Ok(Event::default().event("thinking_delta").data(t));
                }
                StreamChunk::ToolStart { name } => {
                    yield Ok(Event::default().event("tool_start").data(name));
                }
                StreamChunk::ToolEnd { name, success, duration_ms } => {
                    let data = serde_json::json!({"name":name,"success":success,"duration_ms":duration_ms}).to_string();
                    yield Ok(Event::default().event("tool_end").data(data));
                }
                StreamChunk::Complete(stats) => {
                    let done_data = serde_json::json!({"tokens":stats.total_tokens,"latency_ms":stats.latency_ms,"turn":stats.turn_number}).to_string();
                    yield Ok(Event::default().event("done").data(done_data));
                }
                StreamChunk::Error(e) => {
                    yield Ok(Event::default().event("error").data(e));
                }
                StreamChunk::ConfirmRequired(req) => {
                    // V28：MCIP 实时授权请求转 SSE 事件，前端弹窗收集决策
                    let data = serde_json::json!({
                        "tool_id": req.tool_id,
                        "reason": req.reason,
                        "nonce": req.nonce,
                        "kind": format!("{:?}", req.kind),
                        "params_preview": req.params_preview,
                    }).to_string();
                    yield Ok(Event::default().event("confirm_required").data(data));
                }
                // V29.11: ToolArgs/ToolOutput 是 TUI 专用 chunk（diff/trace 渲染）
                // SSE 客户端不消费——静默丢弃
                StreamChunk::ToolArgs { .. } | StreamChunk::ToolOutput { .. } => {}
                // Iteration/Compress 是 CoreLoop 内部生命周期信号，SSE 转为轻量状态事件
                StreamChunk::IterationStart { iteration } => {
                    yield Ok(Event::default().event("iteration_start").data(iteration.to_string()));
                }
                StreamChunk::CompressStart => {
                    yield Ok(Event::default().event("compress_start").data(""));
                }
                StreamChunk::CompressEnd { messages_compressed, tokens_saved } => {
                    let data = serde_json::json!({"messages_compressed": messages_compressed, "tokens_saved": tokens_saved}).to_string();
                    yield Ok(Event::default().event("compress_end").data(data));
                }
                StreamChunk::RetryProgress { attempt, max_attempts, reason } => {
                    let data = serde_json::json!({"attempt": attempt, "max_attempts": max_attempts, "reason": reason}).to_string();
                    yield Ok(Event::default().event("retry_progress").data(data));
                }
                StreamChunk::TeamProgress { phase, tasks } => {
                    let agents: Vec<serde_json::Value> = tasks.iter().map(|t| {
                        serde_json::json!({
                            "id": t.id,
                            "title": t.title,
                            "status": t.status,
                            "output_preview": t.output_preview,
                        })
                    }).collect();
                    let data = serde_json::json!({"phase": phase, "agents": agents}).to_string();
                    yield Ok(Event::default().event("team_progress").data(data));
                }
            }
        }

        // 等待 pipeline 完成，获取最终结果（用于 tool outputs + metrics）
        match handle.await {
            Ok(Ok(result)) => {
                // record_stream_complete: 不写延迟分桶，避免 latency=0 污染直方图
                metrics.record_stream_complete(
                    true,
                    result.stats.prompt_tokens,
                    result.stats.completion_tokens,
                    result.stats.cached_tokens,
                    result.tool_outputs.len() as u64,
                );
                for o in &result.tool_outputs {
                    let data = serde_json::json!({"tool":o.tool_id.0,"success":o.success,"output":o.output,"latency_ms":o.latency_ms}).to_string();
                    yield Ok(Event::default().event("tool_call").data(data));
                }
                // Final complete content (for clients that don't assemble deltas)
                yield Ok(Event::default().event("content").data(result.response));
            }
            Ok(Err(e)) => {
                metrics.record_stream_complete(false, 0, 0, 0, 0);
                tracing::error!(error = %e, "chat stream error");
                yield Ok(Event::default().event("error").data(e.user_message()));
            }
            Err(e) => {
                metrics.record_stream_complete(false, 0, 0, 0, 0);
                yield Ok(Event::default().event("error").data(format!("internal: {}", e)));
            }
        }
    };
    Sse::new(stream).keep_alive(KeepAlive::default().interval(Duration::from_secs(30)))
}

// ─── Progressive Continuation ───────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ContinueRequest { pub session_id: String, pub responses: Vec<ContinueItem> }

#[derive(Debug, Deserialize)]
pub struct ContinueItem { pub id: u32, pub kind: String, #[serde(default)] pub value: Option<String> }

fn to_user_response(item: &ContinueItem) -> Option<UserResponse> {
    match item.kind.as_str() {
        "confirmed" => Some(UserResponse::Confirmed),
        "corrected" => item.value.clone().map(UserResponse::Corrected),
        "chosen" => item.value.clone().map(UserResponse::Chosen),
        "skipped" => Some(UserResponse::Skipped),
        "supplemented" => item.value.clone().map(UserResponse::Supplemented),
        _ => None,
    }
}

async fn continue_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ContinueRequest>,
) -> Result<Json<ChatResponse>, (StatusCode, Json<ErrorResponse>)> {
    let session = state.sessions.get(&req.session_id).await
        .ok_or_else(|| (StatusCode::NOT_FOUND, Json(ErrorResponse { error: "session not found".into() })))?;
    let responses: Vec<(u32, UserResponse)> = req.responses.into_iter()
        .filter_map(|item| to_user_response(&item).map(|r| (item.id, r))).collect();
    if responses.is_empty() {
        return Err((StatusCode::BAD_REQUEST, Json(ErrorResponse { error: "no valid responses".into() })));
    }
    // continuation 通常是 gated phase 2，耗时与 phase 1 相当，同样自适应
    let (turn_count, timeout_secs) = {
        let s = session.read().await;
        let cfg = state.core_loop.config();
        let secs = crate::server::adaptive_timeout_secs(
            &cfg.default_model.0,
            cfg.thinking_intent.is_some(),
            cfg.default_max_tokens,
            s.turn_count,
            state.request_timeout_secs,
        );
        (s.turn_count, secs)
    };
    // P2: cancel token 中断 in-flight LLM
    let cancel_token = tokio_util::sync::CancellationToken::new();
    let token_for_pipeline = cancel_token.clone();
    let t0 = std::time::Instant::now();
    let outcome = tokio::time::timeout(
        Duration::from_secs(timeout_secs),
        state.core_loop.process_turn_continuation_cancellable(responses, &session, token_for_pipeline),
    ).await;
    let latency_ms = t0.elapsed().as_millis() as u64;
    match outcome {
        Ok(Ok(result)) => {
            state.metrics.record_request(
                latency_ms, true, false,
                result.stats.prompt_tokens, result.stats.completion_tokens, result.stats.cached_tokens,
                result.tool_outputs.len() as u64,
            );
            Ok(Json(ChatResponse {
                response: result.response, session_id: req.session_id,
                tool_calls: result.tool_outputs.len(),
            }))
        }
        Ok(Err(e)) => {
            state.metrics.record_request(latency_ms, false, false, 0, 0, 0, 0);
            tracing::error!(error = %e, "continue handler error");
            Err((StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse { error: e.user_message() })))
        },
        Err(_) => {
            cancel_token.cancel();
            state.metrics.record_request(latency_ms, false, true, 0, 0, 0, 0);
            Err((StatusCode::GATEWAY_TIMEOUT, Json(ErrorResponse {
                error: format!(
                    "continuation timed out after {}s (turn={})",
                    timeout_secs, turn_count,
                ),
            })))
        },
    }
}

// ─── Metrics ────────────────────────────────────────────────────────────────

async fn metrics_handler(State(state): State<Arc<AppState>>) -> String {
    let session_count = state.sessions.list().await.len();
    let team_count = state.team_manager.list().await.len();
    state.metrics.render_prometheus(session_count, team_count)
}
