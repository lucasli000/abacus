use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use abacus_types::{
    KernelError, ModelId, ToolId, ToolOutput, TurnStats,
};
use abacus_types::progressive::OutputAction;
use abacus_types::progressive::UserResponse;
use serde_json::Value;
use tokio::sync::RwLock;

use crate::core::context::SessionSnapshot;
use crate::core::inertia;
use crate::core::preflight::PreflightReport;
use crate::core::progressive_inject::build_progressive_prompt;
use crate::core::silent_router::SilentRouter;
use crate::core::task_analyzer::TaskAnalyzer;
use crate::llm::{
    LlmProvider, LlmRequest, Message, MessageContent, MessageRole,
    ToolDefinition,
};
use crate::mcip::McipDecision;
use crate::skill::SkillCandidate;

use crate::core::fallible::MutexExt;

use super::CoreLoop;
use super::SessionState;
use super::TurnResult;

/// V35-1: 按需在 LlmRequest.messages 末尾追加 prefix=true 的 assistant message
///
/// ## 引用关系
/// - 由 RequestContext.prefix_assistant_content 字段触发
/// - 调用点：execute_loop 主循环 + continue_gated 单次 turn（每 turn 重新注入）
/// - 模型能力查询：abacus_types::lookup_model(model_id).supports_prefix_completion
///
/// ## 何时跳过（任一即跳过）
/// - prefix 为 None 或空串
/// - model_id 在 ModelRegistry 中未登记（lookup_model 返回 None）
/// - 模型不支持 prefix completion（如 Anthropic / OpenAI 标准模型）
///
/// ## 副作用
/// - 仅修改传入的 messages local var（不写入 session.messages，请求级隔离）
/// - 多 turn 场景每 turn 重新注入；不会累积
///
/// ## 设计意图
/// 让 Planner agent 等场景能强制 LLM 输出格式（如以 ```json\n[ 起头），
/// 使 V34-2 的 JSON 解析从依赖 prompt 软约束升级到协议层硬约束
fn maybe_inject_prefix_message(
    messages: &mut Vec<Message>,
    model_id: &str,
    prefix: Option<&str>,
) {
    let Some(content) = prefix else { return; };
    if content.is_empty() { return; }
    let Some(info) = abacus_types::lookup_model(model_id) else { return; };
    if !info.supports_prefix_completion { return; }
    messages.push(Message {
        role: MessageRole::Assistant,
        content: Some(MessageContent::Text(content.to_string())),
        name: None,
        tool_calls: None,
        tool_call_id: None,
        reasoning_content: None,
        prefix: true,
    });
}

// Phase 5/6 拆出到 post.rs（inherent impl 跨文件分布）
mod post;
pub mod dedup;

/// Mutable state accumulated during a single turn pipeline.
struct TurnContext {
    turn_number: u32,
    total_tool_calls: u32,
    all_tool_outputs: Vec<ToolOutput>,
    start_time: std::time::Instant,
    final_response: String,
    prompt_tokens: u64,
    completion_tokens: u64,
    cached_tokens: u64,
    /// V30：思考 tokens 累加（completion_tokens 子集；信息透明用）
    thinking_tokens: u64,
    matched_skills: Vec<SkillCandidate>,
    enriched_system: String,
    /// 分段 system prompt（供 Anthropic 等支持多 block 的 provider 使用）
    system_segments: Vec<crate::llm::provider::SystemSegment>,
    tool_defs: Vec<ToolDefinition>,
    provider_id: String,
    provider: Arc<dyn LlmProvider>,
    classification: crate::core::task_analyzer::TaskClassification,
    inertia_warning: Option<inertia::InertiaSignal>,
    /// MCIP NeedsConfirm 收集器：本 turn 被拦截的工具授权请求
    /// 写入：Phase 4 工具分发 `NeedsConfirm` 时收集
    /// 消费：最终写入 TurnResult.pending_confirmations
    pending_confirmations: Vec<crate::mcip::McipConfirmRequest>,
    /// Complexity-driven ThinkingIntent（L1 后；config.thinking_intent 显式设置时不生效）
    /// 引用：analyze_complexity() → map_complexity_to_thinking() → execute_loop LlmRequest.thinking_intent
    /// 生命周期：setup() 写入 → execute_loop() 消费
    complexity_thinking: Option<abacus_types::ThinkingIntent>,
    /// V30: premature stop retry counter — 防止 LLM 工具失败后过早放弃
    /// 引用：execute_loop 中检测 "工具全失败 + 短文本输出" 时递增
    /// 上限：3 次（避免无限续写循环）
    premature_stop_retries: u32,
    /// V39: Tool-in-text fallback retry counter — 防止 LLM 把工具名写进文本而非发起 tool_call
    /// 引用：execute_loop 中 tool_calls 为空 + 文本含已注册工具名时递增
    /// 上限：1 次（纠正一次足够，再失败说明模型不支持 tool calling）
    tool_text_fallback_retries: u32,
    /// Complexity-driven temperature（优先级：req_ctx.temperature > 此值 > config.default_temperature）
    /// 引用：analyze_complexity() → task_temperature() → execute_loop LlmRequest.temperature
    /// 生命周期：setup() 写入 → execute_loop() 消费
    complexity_temperature: Option<f64>,
    /// Phase 4 KV cache 修复：动态 preamble 缓冲（注入到 latest user message 顶部）
    ///
    /// ## 引用关系
    /// 写入：setup() 阶段 ICL Primer 检索结果写入此字段（替代 push_dynamic 到 sys_out）
    /// 读取：execute_loop / handle_model_escalation 构建 LlmRequest 时复制到 user_message_preamble
    ///
    /// ## 设计意图
    /// ICL 等"本轮检索素材"放 system 会破前缀 cache。改放到 latest user message 顶部，
    /// system+history 全 stable，仅 last user 携带 preamble（永不缓存），不破 cache。
    user_message_preamble: Option<String>,
    /// Error-recovery: LLM provider 连续失败计数
    /// 引用：execute_loop 中 provider 错误分支——retry <= 2 时注入错误并 continue，否则 graceful break
    /// 生命周期：每次 provider 成功后重置为 0；连续失败时递增
    provider_retries: u32,
    /// Error-recovery: 全局恢复尝试计数（防止无限恢复循环）
    /// 引用：execute_loop 中每次走恢复路径（非 break/return）时递增
    /// 上限：5 次——超过后 graceful break，避免死循环
    recovery_attempts: u32,
    /// Error-recovery: 工具调用耗尽标记
    /// 引用：turn_max_tool_calls 或 safety guard 触发后设为 true
    /// 效果：下次构建 LlmRequest 时传空 tools 数组，强制 LLM 仅产出文本总结
    tools_exhausted: bool,
    /// 80% 工具配额 warning 是否已发出（确保单轮只触发一次）
    tool_warning_emitted: bool,
    /// 2026-05-27: 时长预算感知——turn 开始时刻
    turn_started_at: std::time::Instant,
    /// 时长 warning 是否已发出（确保单轮只触发一次）
    time_warning_emitted: bool,
    /// 2026-06-02: 动态超时——根据任务难度估算的 LLM 请求超时秒数
    /// 替代硬编码 300s：简单任务 60s，复杂推理+多工具任务可达 600s
    /// 写入：setup() 阶段根据 thinking 模式和工具数量估算
    /// 读取：execute_loop / handle_model_escalation 中构建 timeout
    dynamic_timeout_secs: u64,
    /// V41: 安全分类器连续拦截计数
    /// 引用：工具执行前 ToolActionClassifier 返回 NeedsConfirm/Deny 时递增
    /// 效果：达到 3 次时 force_confirm_all=true（后续所有工具调用走确认通道）
    /// 生命周期：turn 级别，每轮重置
    consecutive_blocks: u32,
    /// V41 Step 3: 连续拦截降级标记
    /// true 时所有后续工具调用强制走 NeedsConfirm，跳过 user_grant 和 MCIP Allowed
    /// 引用：execute_loop tool dispatch 在 MCIP 决策前检查
    /// 激活：consecutive_blocks >= 3
    /// 销毁：随 TurnContext（turn 结束）
    force_confirm_all: bool,
}

/// Encapsulates the full lifecycle of a single conversational turn.
///
/// ## Phases
/// 1. Setup — safety checks, turn numbering, context window, analysis, preflight
/// 2. Prompt building — system prompt assembly + progressive injection + model escalation
/// 3. Tool & provider resolution — Silent Router, tool definitions, provider lookup
/// 4. Execution loop — LLM completion, tool execution, model escalation, progressive gate
/// 5. Post-processing — compression, MapAnalyzer, injector, effectiveness, skills, cooldown, deduction
/// 6. Inertia detection — RetryWithNudge
/// 7. Persistence & result assembly
pub struct TurnPipeline<'a> {
    core: &'a CoreLoop,
    input: &'a str,
    session: &'a RwLock<SessionState>,
    req_ctx: super::RequestContext,
    /// Cancellation token — checked between phases; when cancelled, turn aborts gracefully.
    /// None = not cancellable (default for backward compat).
    ///
    /// P2 修复：从 `Option<Arc<AtomicBool>>` 升级到 `CancellationToken`，
    /// 让 provider 层可以用 `tokio::select!` 与 LLM 请求竞速取消，避免
    /// timeout 后 in-flight reqwest 仍然完成的资源泄漏。
    cancel: Option<tokio_util::sync::CancellationToken>,
    /// V0.2: Streaming output channel. When Some, pipeline uses stream_complete()
    /// and forwards chunks in real-time. When None, uses blocking complete().
    stream_tx: Option<tokio::sync::mpsc::UnboundedSender<crate::llm::stream::StreamChunk>>,
}

impl<'a> TurnPipeline<'a> {
    /// 向后兼容构造（RequestContext::default）
    pub fn new(core: &'a CoreLoop, input: &'a str, session: &'a RwLock<SessionState>) -> Self {
        Self { core, input, session, req_ctx: super::RequestContext::default(), cancel: None, stream_tx: None }
    }

    /// 带 RequestContext 构造（新入口）
    pub fn with_context(core: &'a CoreLoop, input: &'a str, session: &'a RwLock<SessionState>, ctx: super::RequestContext) -> Self {
        Self { core, input, session, req_ctx: ctx, cancel: None, stream_tx: None }
    }

    /// 带取消令牌构造（供 TUI/Server 取消长时间 turn）。
    /// 调用 `token.cancel()` 将在下一个 phase boundary 中止 turn，
    /// 同时 provider 层 `complete_cancellable` 会用 select! 中断 in-flight 请求。
    pub fn with_cancel(mut self, token: tokio_util::sync::CancellationToken) -> Self {
        self.cancel = Some(token);
        self
    }

    /// V0.2: 启用流式输出。Pipeline 将通过 stream_tx 实时推送 StreamChunk。
    pub fn with_stream(mut self, tx: tokio::sync::mpsc::UnboundedSender<crate::llm::stream::StreamChunk>) -> Self {
        self.stream_tx = Some(tx);
        self
    }

    /// Check if turn has been cancelled.
    fn is_cancelled(&self) -> bool {
        self.cancel.as_ref().map(|t| t.is_cancelled()).unwrap_or(false)
    }

    /// 返回当前 cancel token 的克隆（供 provider 层 select!）
    fn cancel_token(&self) -> Option<tokio_util::sync::CancellationToken> {
        self.cancel.clone()
    }

    /// 解析当前 turn 的 ThinkingIntent（session-sticky 锁定）
    ///
    /// ## 引用关系
    /// 调用：`run::execute_loop` 主路径、`continue_gated`、`handle_model_escalation` 三处共用真相源
    ///
    /// ## 决策优先级
    /// 1. `CoreConfig.thinking_intent` 显式 → 永远赢（用户配置优先级最高）
    /// 2. `SessionState.thinking_decision` 已锁定 → 复用首轮决策
    /// 3. 首轮：用 `complexity_fallback` 决定，写入 session 锁定
    ///
    /// ## 锁定动机（B 方案 KV cache 修复）
    /// 跨 turn toggle thinking on/off 会让 DeepSeek `build_messages` 改写**所有历史 assistant
    /// 消息**的 `reasoning_content` 字段存在性（V15 协议要求一致性）→ 整段 history bytes shift
    /// → prefix cache miss。锁定后协议层字段稳定，cache 命中率显著提升。
    ///
    /// ## 取舍
    /// 后续 turn 即使 input 复杂度变化，仍用首轮 thinking effort。质量略受影响但 session 内一致；
    /// 显式配置可绕过（用户掌控）。
    async fn resolve_thinking_config_sticky(
        &self,
        complexity_fallback: Option<abacus_types::ThinkingIntent>,
    ) -> Option<abacus_types::ThinkingIntent> {
        // 1. explicit config 永远赢
        if let Some(intent) = self.core.build_thinking_intent() {
            return Some(intent);
        }
        // 2 + 3. 查 / 写 session sticky（write lock 串行化首轮决策，避免并发竞争）
        // C-2 fix: 先取出 Arc clone，释放 session read guard，再单独 await 内层写锁
        // 防止 session.read() guard 跨 await 持有，与 session.write() 路径形成 livelock
        let decision_lock = {
            let session = self.session.read().await;
            std::sync::Arc::clone(&session.thinking_decision)
        }; // session read guard 在此释放
        let mut decision_guard = decision_lock.write().await;
        if let Some(ref locked) = *decision_guard {
            return locked.clone();
        }
        *decision_guard = Some(complexity_fallback.clone());
        complexity_fallback
    }

    /// Task #94：只读 sticky thinking decision（不写入）
    ///
    /// ## 用途
    /// progressive gate / continue_gated 等"非主决策"路径用此读取——避免污染首轮 sticky。
    /// 真正写入 sticky 的应该是 execute_loop（持有 complexity_thinking 的主路径）。
    ///
    /// ## 引用关系
    /// 与 resolve_thinking_config_sticky 协同：
    /// - **写入端**：execute_loop / handle_model_escalation 调 resolve_*（持 complexity 信号）
    /// - **只读端**：setup（continue_gated 路径）调本方法（无 complexity 信号）
    ///
    /// 返回 None 表示 sticky 未锁定 + 无 explicit config。
    async fn read_thinking_sticky_only(&self) -> Option<abacus_types::ThinkingIntent> {
        if let Some(intent) = self.core.build_thinking_intent() {
            return Some(intent);
        }
        // C-2 fix: 先取 Arc clone 再释放 session guard，避免嵌套持锁跨 await
        let decision_lock = {
            let session = self.session.read().await;
            std::sync::Arc::clone(&session.thinking_decision)
        };
        let guard = decision_lock.read().await;
        match guard.as_ref() {
            Some(locked) => locked.clone(),
            None => None,
        }
    }

    #[tracing::instrument(skip_all, fields(turn_input_len = self.input.len()))]
    pub async fn run(self) -> Result<TurnResult, KernelError> {
        // ─── Phase 1: Setup ─────────────────────────────────────────
        let mut ctx = self.setup().await?;

        // Hook: TurnStart（Setup 通过后触发）
        {
            let sid = self.session.read().await.session_id.clone();
            self.core.emit_pipeline_event(crate::mag_chain::PipelineEvent::TurnStart {
                input: self.input.to_string(),
                session_id: sid,
            }).await?;
        }

        if self.is_cancelled() {
            return Err(KernelError::Other("turn cancelled by user".into()));
        }

        // ─── Phase 4: Execution loop ────────────────────────────────
        let gated = self.execute_loop(&mut ctx).await?;
        if let Some(result) = gated {
            return Ok(result);
        }

        if self.is_cancelled() {
            return Err(KernelError::Other("turn cancelled by user".into()));
        }

        // ─── Phase 5: Post-processing ───────────────────────────────
        self.post_process(&mut ctx).await;
        // Hook: PostProcess
        let _ = self.core.emit_pipeline_event(crate::mag_chain::PipelineEvent::PostProcess).await;

        // ─── Phase 6: Inertia detection（可跳过） ────────────────────
        if !self.req_ctx.skip_inertia {
            self.detect_inertia(&mut ctx).await;
        }

        // ─── Phase 7: Persist & build result ────────────────────────
        self.persist_and_build_result(ctx).await
    }

    /// Phase 2 continuation: after gated turn, user confirms and LLM continues.
    pub async fn continue_gated(self, responses: Vec<(u32, UserResponse)>) -> Result<TurnResult, KernelError> {
        // Inject confirmation results into progressive controller
        {
            let s = self.session.read().await;
            let mut ctrl = s.progressive.write().await;
            ctrl.on_confirmation(responses);
        }

        // Build continuation system prompt
        // Phase 5：matched_skills 不再被 system_prompt 消费，删除其求值
        let default_report = PreflightReport::default();
        let mut enriched_system = self.core.build_system_prompt("", self.session, &default_report).await;
        // V35-2: continue_gated 路径同步注入 system prompt 覆盖（与 execute_loop 对齐）
        if let Some(override_prompt) = self.req_ctx.system_prompt_override.as_deref() {
            if !override_prompt.is_empty() {
                enriched_system.push_str("\n\n");
                enriched_system.push_str(override_prompt);
            }
        }

        // Progressive continuation injection
        {
            let s = self.session.read().await;
            let ctrl = s.progressive.read().await;
            if let Some((_, prompt_text)) = build_progressive_prompt(ctrl.current_state(), ctrl.current_strategy()) {
                enriched_system.push_str("\n\n");
                enriched_system.push_str(&prompt_text);
            }
        }

        // Resolve provider and call LLM
        let (provider_id, provider) = self.core.resolve_provider().await?;
        // V35-1: messages 改为 mut，便于按需追加 prefix=true assistant message
        let mut messages = {
            let s = self.session.read().await;
            let guard = s.messages.read().await;
            guard.clone()
        };

        // Phase β-D：从 session.task_kind_locked 读取已锁定的 task_kind label，用于工具路由过滤
        // Phase γ-I：从 session.turn_count 取 current turn，用于 frequency pruning
        let (task_kind_label, current_turn) = {
            let s = self.session.read().await;
            let locked = s.task_kind_locked.read().await;
            (locked.as_ref().map(|k| k.label().to_string()), Some(s.turn_count as u64))
        };
        // Phase γ-Palace-C：每 N turn 同步行为宫殿信号到 effectiveness（降级失败工具）
        // 跳过：interval=None / palace 未启用 / 首轮（turn=0 时同步无意义）
        if let Some(interval) = self.core.config.palace_sync_interval_turns {
            if let Some(turn) = current_turn {
                if turn > 0 && interval > 0 && turn % interval as u64 == 0 {
                    // 段 L2：传真实 turn，让 K4 probation 机制启动（demoted_at=turn）
                    let demoted = self.core.sync_from_palace_at(turn).await;
                    if demoted > 0 {
                        tracing::info!(turn, demoted, "palace sync: tools demoted");
                    }
                }
            }
        }
        let tool_defs = self.core.build_tool_definitions_for(
            task_kind_label.as_deref(),
            current_turn,
        ).await;
        let start_time = std::time::Instant::now();

        // V35-1: continue_gated 路径同步注入 prefix message（与 execute_loop 对齐）
        // 优先级与 execute_loop 一致：req_ctx.model > model_override > default
        let cg_model = self.req_ctx.model.clone()
            .or(self.core.get_model_override().await)
            .unwrap_or_else(|| self.core.config.default_model.clone());
        maybe_inject_prefix_message(
            &mut messages,
            &cg_model.0,
            self.req_ctx.prefix_assistant_content.as_deref(),
        );

        let req = LlmRequest {
            model: cg_model,
            messages,
            system: Some(enriched_system),
            system_segments: Vec::new(),
            tools: tool_defs,
            temperature: Some(self.core.config.default_temperature),
            max_tokens: Some(self.core.config.model_spec.as_ref()
                .map(|s| s.max_output_tokens)
                .unwrap_or(self.core.config.default_max_tokens)),
            top_p: None, stop: Vec::new(), stream: false,
            // L1 + Task #94: thinking_intent 单通道——continue_gated 路径只读 sticky
            // 避免在此处用 None 写入 sticky 污染首轮决策（execute_loop 才是主写入点）
            thinking_intent: self.req_ctx.thinking_intent.clone()
                .or(self.read_thinking_sticky_only().await),
            cache_config: Some(crate::llm::prompt_cache::PromptCacheConfig::default()), // 修复: continue_gated 路径开启缓存
            extra_body: HashMap::new(),
            // continue_gated 路径无 ICL 检索，preamble=None
            user_message_preamble: None,
        };

        // 5-minute hard timeout 安全网（provider 层有 120s 请求超时，此处防 misconfigure 或挂起）
        let timeout_secs = self.core.threshold_u64("turn_timeout");
        let provider_timeout = std::time::Duration::from_secs(timeout_secs);
        let response = match tokio::time::timeout(provider_timeout, provider.complete_cancellable(req, self.cancel_token())).await {
            Ok(result) => result?,
            Err(_elapsed) => {
                if let Some(ref stx) = self.stream_tx {
                    let _ = stx.send(crate::llm::stream::StreamChunk::Error(
                        format!("⚠️ request timed out after {}s", timeout_secs)
                    ));
                }
                return Err(KernelError::Provider(format!("request timeout: {}s", timeout_secs)));
            }
        };
        let final_response = super::extract_text(&response.message);

        // Store in session
        {
            let s = self.session.read().await;
            let mut msgs = s.messages.write().await;
            msgs.push(response.message.clone());
        }

        // Finalize progressive controller
        {
            let s = self.session.read().await;
            let mut ctrl = s.progressive.write().await;
            ctrl.finalize(response.usage.total_tokens);
        }

        let turn_number = {
            let mut s = self.session.write().await;
            s.turn_count += 1;
            // V41: 每 turn 开始时重置 LLM 申请的超时延长（防止跨 turn 累积）
            s.timeout_extension_secs = 0;
            s.turn_count
        };
        // W4 (Task #102)：把当前 turn 推送到 ContextManager，让 compress / evict_by_importance
        //   等不直接持有 turn 上下文的路径能从单一真相源读 turn。
        self.core.context_manager.set_current_turn(turn_number);

        let session_id = { let s = self.session.read().await; s.session_id.clone() };
        let progressive_state = {
            let s = self.session.read().await;
            let ctrl = s.progressive.read().await;
            if ctrl.is_passthrough() { None } else { Some(ctrl.current_state().clone()) }
        };

        Ok(TurnResult {
            response: final_response,
            stats: {
                let ctx_window = self.core.context_manager.window.read().await;
                TurnStats {
                    turn_number, tool_calls: 0,
                    provider_id,
                    model_id: self.core.config.default_model.0.clone(),
                    prompt_tokens: response.usage.prompt_tokens,
                    completion_tokens: response.usage.completion_tokens,
                    cached_tokens: response.usage.cached_tokens,
                    total_tokens: response.usage.total_tokens,
                    thinking_tokens: response.usage.thinking_tokens,
                    latency_ms: start_time.elapsed().as_millis() as u64,
                    skills_matched: vec![],
                    context_tokens: Some(ctx_window.current_tokens as u64),
                    context_max: Some(ctx_window.max_tokens as u64),
                    model_limit: Some(ctx_window.model_limit as u64),
                }
            },
            tool_outputs: vec![],
            matched_skills: vec![],
            session_id,
            progressive_state,
            inertia_warning: None,
            pending_confirmations: Vec::new(),
        })
    }

    // ─── Phase 1: Setup ─────────────────────────────────────────────────────

    async fn setup(&self) -> Result<TurnContext, KernelError> {
        // Safety check: input length
        self.core.safety_guard.check_input_length(self.input)
            .map_err(|e| KernelError::Other(e.to_string()))?;

        // Phase Ctx-A: pressure shed pending 检查——若上一轮 pressure_monitor 报警，
        // 这里立即触发 auto_compress 让 prefix 字节回到稳定区间
        // 修复：仅在实际压缩发生时才发 CompressStart（避免每轮空弹 toast）
        if self.core.context_manager.take_shed_pending() {
            let s = self.session.read().await;
            let mut msgs = s.messages.write().await;
            let compressed = self.core.context_manager.auto_compress_messages(&mut msgs).await;
            if !compressed.is_empty() {
                let tokens_saved: usize = compressed.iter()
                    .map(|c| c.original_tokens.saturating_sub(c.compressed_tokens))
                    .sum();
                tracing::info!("pressure shed: compressed {} messages, freed ~{} tok", compressed.len(), tokens_saved);

                // 通知 TUI（显式状态变更）
                if let Some(ref stx) = self.stream_tx {
                    let _ = stx.send(crate::llm::stream::StreamChunk::CompressStart);
                    let _ = stx.send(crate::llm::stream::StreamChunk::CompressEnd {
                        messages_compressed: compressed.len(),
                        tokens_saved,
                    });
                }

                // 注入 system message 通知 LLM 上下文已被压缩
                // LLM 在下次看到此消息时能感知历史被摘要化，避免引用已压缩细节
                msgs.push(crate::llm::Message {
                    role: crate::llm::MessageRole::System,
                    content: Some(crate::llm::MessageContent::Text(format!(
                        "[Context auto-compressed] {} messages summarized, ~{} tokens freed. \
                         Use `messages_recover` tool with recover_id if you need original details. \
                         Use `context_status` tool to check current usage.",
                        compressed.len(), tokens_saved
                    ))),
                    name: None,
                    tool_calls: None,
                    tool_call_id: None,
                    reasoning_content: None,
                    prefix: false,
                });
            }
            // 无压缩时静默——shed 标记已消费，下轮不会重复触发
        }

        // Copy session messages to session-level context_messages
        {
            let s = self.session.read().await;
            let msgs = s.messages.read().await;
            *s.context_messages.write().await = msgs.clone();
        }

        let turn_number = { let s = self.session.read().await; s.turn_count + 1 };

        // V43.2: context_window 跟随当前活跃模型（per-model + per-provider）
        // 优先级链：req_ctx.model > model_override(/model) > default_model
        // Spec 查询：qualified_specs (provider+model) > specs (model) > default
        {
            let model_override = self.core.get_model_override().await;
            let effective_model = self.req_ctx.model.clone()
                .or(model_override)
                .unwrap_or_else(|| self.core.config.default_model.clone());
            // 解析 provider_id 用于 qualified spec lookup
            let provider_id = self.core.resolve_provider_id_for_model(&effective_model.0).await;
            let spec = if let Some(ref catalog) = self.core.config.model_catalog {
                if let Some(ref pid) = provider_id {
                    catalog.lookup_qualified(pid, &effective_model)
                } else {
                    catalog.lookup_or_default(&effective_model)
                }
            } else if let Some(ref s) = self.core.config.model_spec {
                std::sync::Arc::new(s.clone())
            } else {
                std::sync::Arc::new(abacus_types::ModelSpec::default())
            };
            let ratio = self.core.config.context_window_ratio.clamp(0.1, 1.0);
            let available = ((spec.context_window as f64 * ratio) as usize).max(128_000);
            self.core.context_manager.set_window(available, spec.context_window);
        }

        // Progressive Output: complexity analysis + strategy decision（RequestContext 可跳过）
        let complexity_profile = TaskAnalyzer::analyze_complexity(self.input);
        let classification = TaskAnalyzer::classify(self.input);
        if !self.req_ctx.skip_progressive {
            let s = self.session.read().await;
            let mut ctrl = s.progressive.write().await;
            ctrl.begin_with_task_type(&complexity_profile, classification.kind.label());
        }

        // Preflight dual-track self-review（RequestContext 可跳过）
        use crate::core::preflight::PreflightChecker;
        // analyze_complexity 驱动三条并行路径：① SelfReview 关注点 ② ThinkingConfig 推导 ③ temperature 推导
        // skip_preflight=true 时 complexity 仍计算（下面的 ThinkingConfig/temperature 推导判断需要它）
        let complexity = TaskAnalyzer::analyze_complexity(self.input);
        let mut preflight_tokens: (u64, u64) = (0, 0);
        let preflight = if self.req_ctx.skip_preflight {
            PreflightReport::default()
        } else {
            let rule_report = PreflightChecker::check_with_patterns(
                self.input, &classification.kind, Some(&complexity),
                Some(&self.core.config.policy.preflight.destructive_patterns),
            );
            if rule_report.is_safe() {
                rule_report
            } else {
                let (llm_report, pt, ct) = self.core.llm_self_review(self.input, &classification.kind).await;
                preflight_tokens = (pt, ct);
                PreflightReport::merge(rule_report, llm_report)
            }
        };

        let matched_skills = {
            let engine = self.core.skill_engine.read().await;
            engine.evaluate(self.input, None)
        };
        // ─── 统一构建 text + segments（一次调用，消除发散）─────────────────────
        // 使用 SystemPromptOutput::push_dynamic() 同步追加动态内容到 text 和 segments，
        // 确保 Anthropic multi-block cache 路径与非 Anthropic 路径行为一致。
        let mut sys_out = self.core.build_system_output(
            self.input, self.session, &preflight
        ).await;

        // V44: Progressive prompt → preamble（每轮变化，不破 prefix cache）
        {
            let ctx_pressure = {
                let w = self.core.context_manager.window.read().await;
                let pct = w.usage_pct();
                let trigger = w.compression_trigger_pct as f64;
                if pct >= 95.0 { "CRITICAL" }
                else if pct >= trigger { "ELEVATED" }
                else { "OK" }
            };
            if matches!(ctx_pressure, "ELEVATED" | "CRITICAL") {
                sys_out.push_preamble(&format!(
                    "[Progressive Override] Context pressure={ctx_pressure}. \
                     Skip staged/gated review — output key conclusions directly and concisely. \
                     Do NOT produce large structured documents this turn."
                ));
            } else {
                let s = self.session.read().await;
                let ctrl = s.progressive.read().await;
                if let Some((_priority, prompt_text)) = build_progressive_prompt(
                    ctrl.current_state(), ctrl.current_strategy(),
                ) {
                    sys_out.push_preamble(&prompt_text);
                }
            }
        }

        // Model Self-Escalation prompt（flash models only）
        // 内容固定（不含变量），保留在 system prompt 中（不破 cache，同 model 内字节稳定）
        if self.core.config.default_model.0.contains("flash") {
            sys_out.push_dynamic("[Model Routing]\n\
                If this request requires deep multi-step reasoning, complex architecture analysis, \
                security auditing, or tasks where accuracy is critical over speed:\n\
                1. Output `[ESCALATE]` as your FIRST line\n\
                2. Then provide your preliminary analysis (key observations, identified structure, initial conclusions)\n\
                3. A stronger model will continue from your analysis, verify it, and produce the final response\n\
                For all other requests, proceed normally without [ESCALATE].");
        }

        // V44: EpistemicGuard declaration → preamble（累积变化，不破 prefix cache）
        if let Some(declaration) = self.core.epistemic_guard.declaration_if_needed().await {
            sys_out.push_preamble(&declaration);
        }

        // ─── SM-2 到期复习注入（已删除）──────────────────────────────────
        // Phase 1 KV cache 评审：SM-2 是边缘功能，多数 session 无关；不应默认 push 注入到
        // system 破坏 cache 前缀。如需启用，应作为独立工具让 LLM 主动调（memory_palace.review_due）。
        // memory_palace 数据结构保留，工具入口由 Phase 5 evaluation 决定。

        // ─── ICL Primer：KB 精准检索 ──────────────────────────────────────────
        // Phase 4 KV cache 修复：从 push_dynamic（破 system 前缀）→ 写入 user_message_preamble
        //
        // ## 旧实现问题
        // ICL query = format!("{} {}", task_kind, input[..80])，input-driven 每轮变 → KB hits 不同
        // → push_dynamic 注入 system 末尾 → 字节变化破后续 cache → 每轮 cache miss 整段 history
        //
        // ## 新实现
        // 命中的 KB context 写入 user_message_preamble，由 provider build_messages 拼到 latest user
        // 顶部。语义 framing 为"本轮自动检索的 KB 素材"——这本就是 user 视角的 RAG 结果，不是 system 权威。
        // user message 永不 cache，所以 preamble 字节变化只影响本轮，不破前缀 cache。
        //
        // ## 删除 active_knowledge 兜底
        // 之前 `if !icl_injected` 分支再次注入 active_knowledge——但 active_knowledge 已通过
        // injector_segments 在 Layer 180 注入过了，这是 duplicate。删除冗余注入。
        let user_message_preamble: Option<String> = if complexity.score > 0.50 {
            if let Some(ref store) = self.core.knowledge_store {
                let q = format!("{} {}", classification.kind.label(),
                                &self.input[..self.input.len().min(80)]);
                match store.query(&q, 3, None).await {
                    Ok(results) => {
                        let hits: Vec<_> = results.iter()
                            .filter(|r| r.score >= 0.35)
                            .collect();
                        if !hits.is_empty() {
                            let mut icl_text = String::from("## [Retrieved KB Context — auto-injected for this turn]\n");
                            for hit in &hits {
                                if !hit.heading_path.is_empty() {
                                    icl_text.push_str(&format!("### {}\n", hit.heading_path));
                                }
                                icl_text.push_str(&hit.content);
                                icl_text.push('\n');
                            }
                            Some(icl_text)
                        } else { None }
                    }
                    Err(_) => None,
                }
            } else { None }
        } else { None };

        // V44: 合并 dynamic_preamble (awareness/progressive/epistemic) + ICL primer
        // 两者都写入 user_message_preamble → latest user message 顶部（不破 prefix cache）
        let user_message_preamble: Option<String> = {
            let mut parts: Vec<String> = Vec::new();
            // 动态遥测（awareness + focus + progressive + epistemic）
            if !sys_out.dynamic_preamble.is_empty() {
                parts.push(sys_out.dynamic_preamble);
            }
            // ICL Primer（KB 检索结果）
            if let Some(icl) = user_message_preamble {
                parts.push(icl);
            }
            if parts.is_empty() { None } else { Some(parts.join("\n\n")) }
        };

        let enriched_system = sys_out.text;
        let dynamic_blocks = sys_out.segments.len().saturating_sub(1);
        let system_segments = sys_out.segments;

        // Hook: PromptBuilt（system prompt 组装完成，adapter 应用前）
        let _ = self.core.emit_pipeline_event(crate::mag_chain::PipelineEvent::PromptBuilt {
            system_len: enriched_system.len(),
            dynamic_blocks,
        }).await;

        let (provider_id, provider) = self.core.resolve_provider().await?;

        // PromptAdapter: provider 确定后应用最优格式（Anthropic XML / OpenAI Markdown / DeepSeek 中文）
        // 仅转换 enriched_system（text）；system_segments 为 Anthropic 内部结构，不需要额外转换
        // let mut: DecayRouter 在后面还会追加 hint
        let mut enriched_system = {
            let adapter = self.core.get_adapter(&provider_id).await;
            let adapted = adapter.apply(&enriched_system);
            if adapted != enriched_system {
                tracing::debug!(
                    provider = %provider_id,
                    adapter = adapter.provider_id(),
                    style = ?adapter.style(),
                    "prompt adapter applied"
                );
            }
            adapted
        };

        // Add user message
        {
            let s = self.session.read().await;
            let mut msgs = s.messages.write().await;
            msgs.push(Message {
                role: MessageRole::User,
                content: Some(MessageContent::Text(self.input.to_string())),
                name: None, tool_calls: None, tool_call_id: None, reasoning_content: None, prefix: false,
            });
        }

        // Silent Router: tool ordering optimization
        let tool_defs = if self.core.config.silent_router_enabled
            && !SilentRouter::should_skip(self.input)
        {
            let experience = { self.core.effectiveness.read().await.experience_signal() };
            let session_tools: Vec<ToolId> = {
                let s = self.session.read().await;
                let map = s.interaction_map.read().await;
                map.recent_tools(5)
            };
            // Phase γ-Palace-E：从行为宫殿取上一个工具的关联推荐
            //
            // recommend_next_tools 用 last_tool 作为锚，沿 bridge 关系链返回历史关联工具列表。
            // last_tool 取 session_tools 末尾——session 内最近一次调用。
            let palace_recs: Vec<(String, f64)> = if let Some(ref palace) = self.core.memory_palace {
                if let Some(last) = session_tools.last() {
                    let p = palace.read().await;
                    p.recommend_next_tools(&last.0).await
                } else {
                    Vec::new()
                }
            } else {
                Vec::new()
            };
            let route_hint = self.core.silent_router.route(
                self.input, &experience, &matched_skills, &session_tools, &palace_recs
            );
            // Phase β-D + γ-I：路由过滤 + frequency pruning
            let (task_kind_label_inner, current_turn_inner) = {
                let s = self.session.read().await;
                let locked = s.task_kind_locked.read().await;
                (locked.as_ref().map(|k| k.label().to_string()), Some(s.turn_count as u64))
            };
            let mut defs = self.core.build_tool_definitions_for(
                task_kind_label_inner.as_deref(),
                current_turn_inner,
            ).await;
            if let Some(priority_order) = route_hint {
                defs.sort_by_key(|td| {
                    priority_order.iter().position(|t| t.0 == td.function.name)
                        .map(|p| p as i64)
                        .unwrap_or(1000)
                });
            }
            defs
        } else {
            // Phase β-D + γ-I
            let (task_kind_label_inner, current_turn_inner) = {
                let s = self.session.read().await;
                let locked = s.task_kind_locked.read().await;
                (locked.as_ref().map(|k| k.label().to_string()), Some(s.turn_count as u64))
            };
            self.core.build_tool_definitions_for(
                task_kind_label_inner.as_deref(),
                current_turn_inner,
            ).await
        };

        // SilentRouter 返回不可变绑定，此处改为可变以支持 DecayRouter 二次排序
        let mut tool_defs = tool_defs;

        // ─── DecayRouter：快衰查询提升 web 工具 + 注入衰减提示 ───────────────
        // skip_decay_router=true 时跳过（Meeting specialist、内部结构化调用）
        // 设计目标是直接用户交互，对 allowed_tools=[] 的内部调用没有意义
        //
        // V29.13 段3b：把 hint 包装成 "[Hook: MagChain.DecayRouter]" 格式标识，
        // 让 LLM 明确感知到这是 Pipeline Hook 介入而非普通指令。同时声明可用的
        // magchain_status 工具，让 LLM 知道有"主动查询 hook 状态"的能力。
        if !self.req_ctx.skip_decay_router {
            use crate::mag_chain::DecayRouter;
            let decay_tier = DecayRouter::classify(self.input);
            let promoted = DecayRouter::tools_to_promote(decay_tier);
            if !promoted.is_empty() {
                tool_defs.sort_by_key(|td| {
                    promoted.iter()
                        .position(|p| *p == td.function.name.as_str())
                        .map(|i| i as i64)
                        .unwrap_or(1000)
                });
            }
            if let Some(hint) = DecayRouter::prompt_hint(decay_tier) {
                enriched_system.push_str("\n\n[Hook: MagChain.DecayRouter] ");
                enriched_system.push_str(hint);
                enriched_system.push_str("\n（提示来源：Pipeline Hook System。本 hint 由 DecayRouter 根据用户输入信号词自动注入。如需查看完整 hook 状态/violations 计数，调用 `magchain_status` 工具。）");
            }
        }

        // V35-2: 角色 system prompt 覆盖（Planner/Specialist 等场景）
        // 引用关系：req_ctx.system_prompt_override（cli/api/mod.rs::send_planner_message_streaming 设置）
        // 顺序：append-after，叠加在所有动态 hint 之后；不替换基座 system
        // 设计意图：让角色化调用走独立 system 段，KV cache 友好 + 用户消息不被污染
        if let Some(override_prompt) = self.req_ctx.system_prompt_override.as_deref() {
            if !override_prompt.is_empty() {
                enriched_system.push_str("\n\n");
                enriched_system.push_str(override_prompt);
            }
        }

        // Record opportunities for all visible tools
        {
            let mut eff = self.core.effectiveness.write().await;
            for td in &tool_defs {
                eff.record_opportunity(&ToolId(td.function.name.clone()));
            }
        }

        // ─── Complexity → ThinkingConfig + temperature 推导 ─────────────────────
        // ThinkingConfig 优先级：config.thinking_config（显式）> complexity 推导 > None
        // temperature 优先级：req_ctx.temperature（外部）> complexity 推导 > config 默认
        // skip_preflight=true 时仍计算，fast mode 不需要 SelfReview 但仍益于准确的生成参数
        let complexity_thinking = map_complexity_to_thinking(&complexity);
        let complexity_temperature = Some(task_temperature(&classification.kind, &complexity));

        // 动态超时：在 complexity_thinking 被 move 前估算
        // V41: 动态超时——综合 thinking 深度 + ComplexityProfile + 工具数量
        //
        // ## 计算公式
        // base = config.turn_provider_timeout_secs (clamp 60-600)
        // + thinking 深度加成 (0-180s)
        // + 复杂度加成: score * 120s (0-120s)
        // + 工具数量加成: tool_count * 10s (上限 100s)
        // 总上限: 900s (15 分钟，防止无限等待)
        //
        // ## Per-call timeout（V43: 从 turn-level 改为 per-LLM-call）
        //
        // 设计变更：之前 dynamic_timeout_secs 覆盖整个 turn（含多次工具执行），
        // 导致长 agentic 循环（10+ tool calls）中后期 LLM 调用被 turn deadline 截断。
        //
        // 新语义：dynamic_timeout_secs = 单次 LLM API 调用的最大等待时间。
        // 每次 LLM 调用独立计时，工具执行时间不算入 LLM 超时。
        //
        // ## 引用关系
        // - 消费: execute_loop 中每次 provider.complete_cancellable() 的 tokio::time::timeout
        // - 数据源: thinking level（主因子，决定 LLM 单次推理上限）
        let estimated_timeout = {
            // V43.1: 统一 per-call timeout 为 600s
            // 原因：thinking 模型（deepseek-v4-flash 等）推理时间不可预测，
            // Medium 任务也可能需要 5+ 分钟。300s 导致频繁误杀。
            // 无 thinking 的纯 completion 也给足余量（避免大上下文慢响应被截）。
            let t: u64 = if let Some(ref ti) = complexity_thinking {
                match ti {
                    abacus_types::ThinkingIntent::Off => 300, // 无思考：300s 足够
                    _ => 600, // 有思考：统一 600s
                }
            } else {
                300 // 无 thinking intent（纯 completion）
            };
            t.min(self.core.threshold_u64("turn_timeout"))
        };

        Ok(TurnContext {
            turn_number,
            total_tool_calls: 0,
            all_tool_outputs: Vec::new(),
            start_time: std::time::Instant::now(),
            final_response: String::new(),
            prompt_tokens: preflight_tokens.0,
            completion_tokens: preflight_tokens.1,
            cached_tokens: 0,
            thinking_tokens: 0,
            matched_skills,
            enriched_system,
            system_segments,
            tool_defs,
            provider_id,
            provider,
            classification,
            inertia_warning: None,
            pending_confirmations: Vec::new(),
            premature_stop_retries: 0,
            tool_text_fallback_retries: 0,
            complexity_thinking,
            complexity_temperature,
            user_message_preamble, // Phase 4：setup() 阶段已构建（ICL 检索结果）
            provider_retries: 0,
            recovery_attempts: 0,
            tools_exhausted: false,
            tool_warning_emitted: false,
            turn_started_at: std::time::Instant::now(),
            time_warning_emitted: false,
            dynamic_timeout_secs: estimated_timeout,
            consecutive_blocks: 0,
            force_confirm_all: false,
        })
    }

    // ─── Phase 4: Execution Loop ────────────────────────────────────────────

    /// Returns `Some(TurnResult)` if the turn gated (early return),
    /// `None` if normal completion.
    /// Sanitize tool protocol invariants in message history.
    ///
    /// Handles two cases to prevent API errors:
    /// 1. **Dangling tool_calls**: assistant message has tool_calls but not all
    ///    matching tool responses exist after it → remove the assistant message.
    /// 2. **Orphaned tool responses**: tool message whose tool_call_id doesn't match
    ///    any preceding assistant's tool_calls → remove the orphan.
    ///
    /// Case 2 occurs when context compression removes the assistant (with tool_calls)
    /// but preserves the tool response, or when a proactive tool injection lacks
    /// a synthetic assistant turn.
    ///
    /// ## 引用关系
    /// - 调用点：execute_loop 每次构建 LlmRequest.messages 前
    /// - 消费方：所有 OpenAI-compatible providers (DeepSeek, OpenAI, etc.)
    fn sanitize_dangling_tool_calls(msgs: &mut Vec<Message>) {
        if msgs.is_empty() { return; }

        // ── Case 1: dangling assistant tool_calls without matching responses ──
        // 2026-05-27 修复：扫描全部（不仅最后一个），因为 force_discard 压缩可能在中间留下孤儿。
        // 从后向前扫描，移除时 index 不影响后续遍历。
        let mut indices_to_remove: Vec<usize> = Vec::new();
        for idx in 0..msgs.len() {
            if msgs[idx].role != MessageRole::Assistant { continue; }
            let tool_calls = match msgs[idx].tool_calls.as_ref() {
                Some(tc) if !tc.is_empty() => tc,
                _ => continue,
            };
            let expected_ids: std::collections::HashSet<&str> = tool_calls.iter()
                .map(|tc| tc.id.as_str())
                .collect();
            // 向后搜索 tool responses（直到下一个 user/assistant 消息或末尾）
            let found_ids: std::collections::HashSet<&str> = msgs[idx + 1..].iter()
                .take_while(|m| m.role == MessageRole::Tool || m.role == MessageRole::Assistant)
                .filter(|m| m.role == MessageRole::Tool)
                .filter_map(|m| m.tool_call_id.as_deref())
                .collect();
            if !expected_ids.is_subset(&found_ids) {
                tracing::warn!(
                    idx,
                    missing = ?(expected_ids.difference(&found_ids).collect::<Vec<_>>()),
                    "removing dangling tool_calls message from history"
                );
                indices_to_remove.push(idx);
            }
        }
        // 从后往前移除，保持 index 稳定
        for idx in indices_to_remove.into_iter().rev() {
            msgs.remove(idx);
        }

        // ── Case 2: orphaned tool responses without preceding assistant tool_calls ──
        // Collect all tool_call IDs declared by assistant messages (owned to avoid borrow conflict).
        let declared_ids: std::collections::HashSet<String> = msgs.iter()
            .filter(|m| m.role == MessageRole::Assistant)
            .filter_map(|m| m.tool_calls.as_ref())
            .flatten()
            .map(|tc| tc.id.clone())
            .collect();

        let before_len = msgs.len();
        msgs.retain(|m| {
            if m.role != MessageRole::Tool { return true; }
            match m.tool_call_id.as_deref() {
                Some(id) => declared_ids.contains(id),
                // tool message without tool_call_id is malformed — remove
                None => false,
            }
        });
        let removed = before_len - msgs.len();
        if removed > 0 {
            tracing::warn!(
                removed,
                "removed orphaned tool response messages (no matching assistant tool_calls)"
            );
        }

        // ── Case 3: merge consecutive same-role messages ──
        // OpenAI-compatible APIs require alternating user/assistant roles.
        // Consecutive user messages can arise from:
        //   - auto_compress inserting role=user summaries adjacent to real user messages
        //   - model escalation path pushing continuation prompts
        // Fix: merge consecutive user (or assistant) messages by joining content with \n\n.
        // ── Case 3: merge consecutive same-role messages ──
        // OpenAI-compatible APIs require alternating user/assistant roles.
        // Consecutive user messages can arise from auto_compress (role=user summaries)
        // or model escalation (continuation prompts). Also skip System role (must be first).
        if msgs.len() >= 2 {
            let mut merged: Vec<Message> = Vec::with_capacity(msgs.len());
            for msg in msgs.drain(..) {
                if let Some(last) = merged.last_mut() {
                    if last.role == msg.role
                        && last.role != MessageRole::Tool
                        && last.role != MessageRole::System
                        && last.tool_calls.is_none()
                        && msg.tool_calls.is_none()
                    {
                        // Extract text content, preserving MultiPart by serializing to text
                        let extract_text = |c: MessageContent| -> String {
                            match c {
                                MessageContent::Text(t) => t,
                                MessageContent::MultiPart(parts) => {
                                    // Preserve multipart by extracting text parts
                                    parts.iter().filter_map(|p| {
                                        if let crate::llm::ContentPart::Text { text } = p {
                                            Some(text.as_str())
                                        } else { None }
                                    }).collect::<Vec<_>>().join("\n")
                                }
                            }
                        };
                        let existing = last.content.take().map(extract_text).unwrap_or_default();
                        let incoming = msg.content.map(extract_text).unwrap_or_default();
                        last.content = Some(MessageContent::Text(
                            format!("{}\n\n{}", existing, incoming)
                        ));
                        continue;
                    }
                }
                merged.push(msg);
            }
            *msgs = merged;
        }

        // ── Case 4: 预防性 content=None 修复 ──
        // 2026-05-28: assistant message 无 tool_calls 时 content 不能为 null/None
        // （OpenAI/DeepSeek 均拒绝）。有 tool_calls 时 content=null 合法。
        // 预防性修复：填充空字符串（不等 400 后再补）。
        for m in msgs.iter_mut() {
            if m.role == MessageRole::Assistant
                && m.content.is_none()
                && m.tool_calls.as_ref().map(|tc| tc.is_empty()).unwrap_or(true)
            {
                m.content = Some(MessageContent::Text(String::new()));
            }
        }
    }

    /// 根据 400 错误 body 中的线索尝试修复消息序列。
    /// 返回 true 表示已做出修改（应重试），false 表示无法修复。
    ///
    /// ## 修复策略
    /// 根据错误消息关键词匹配不同修复动作：
    /// - tool protocol violations → 重跑 sanitize
    /// - role alternation violations → 合并连续同角色
    /// - reasoning_content issues → 剥离所有 reasoning_content
    /// - empty content → 填充占位文本
    ///
    /// ## 引用关系
    /// - 调用点：execute_loop 中 provider 400 响应后
    /// - 依赖：sanitize_dangling_tool_calls (Case 1-3)
    /// V39: 尝试从消息 content 中解析 XML 格式的工具调用（DeepSeek 有时输出 <tool_calls> XML）
    ///
    /// 支持两种 XML 格式：
    /// 1. `<tool_call><name>X</name><arguments>{...}</arguments></tool_call>`（原有格式）
    /// 2. `<tool_calls><tool_call>{"name":"X","arguments":{...}}</tool_call></tool_calls>`（JSON 内嵌格式）
    ///
    /// 引用关系：execute_loop 在 structured tool_calls 为空时调用（兜底防御层）
    /// 返回解析出的 ToolCall 列表；解析失败或无 XML 时返回空 vec。
    fn try_parse_xml_tool_calls(message: &Message) -> Vec<crate::llm::ToolCall> {
        let text = match &message.content {
            Some(MessageContent::Text(t)) => t,
            _ => return Vec::new(),
        };
        // 检测 <tool_call> 或 <function_call> 或 <tool_calls> 格式
        if !text.contains("<tool_call") && !text.contains("<function_call") {
            return Vec::new();
        }

        // ── 路径 1：regex 提取 <name>/<arguments> 子标签格式 ──
        static RE_NAME: LazyLock<regex::Regex> = LazyLock::new(|| {
            regex::Regex::new(r"<name>\s*([^<]+)\s*</name>").unwrap()
        });
        static RE_ARGS: LazyLock<regex::Regex> = LazyLock::new(|| {
            regex::Regex::new(r"<arguments>\s*([\s\S]*?)\s*</arguments>").unwrap()
        });
        let mut calls = Vec::new();
        {
            let rn = &*RE_NAME;
            let ra = &*RE_ARGS;
            let names: Vec<&str> = rn.captures_iter(text).filter_map(|c| c.get(1).map(|m| m.as_str())).collect();
            let args: Vec<&str> = ra.captures_iter(text).filter_map(|c| c.get(1).map(|m| m.as_str())).collect();
            for (i, name) in names.iter().enumerate() {
                let arg_str = args.get(i).unwrap_or(&"{}");
                calls.push(crate::llm::ToolCall {
                    id: format!("xml_tc_{}", i),
                    type_: "function".into(),
                    function: crate::llm::ToolFunction {
                        name: name.trim().to_string(),
                        arguments: arg_str.to_string(),
                    },
                });
            }
        }

        // ── 路径 2：JSON 内嵌格式 <tool_calls><tool_call>{JSON}</tool_call></tool_calls> ──
        // 仅当路径 1 无结果时尝试（避免重复解析）
        if calls.is_empty() {
            if let Some(start) = text.find("<tool_calls>") {
                if let Some(end) = text.find("</tool_calls>") {
                    let block = &text[start + "<tool_calls>".len()..end];
                    for segment in block.split("<tool_call>") {
                        let segment = segment.trim();
                        if segment.is_empty() {
                            continue;
                        }
                        let content = segment
                            .strip_suffix("</tool_call>")
                            .unwrap_or(segment)
                            .trim();
                        if content.is_empty() {
                            continue;
                        }
                        // 解析 JSON: {"name": "xxx", "arguments": {...}}
                        if let Ok(val) = serde_json::from_str::<serde_json::Value>(content) {
                            let name = val
                                .get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let args = val
                                .get("arguments")
                                .map(|v| serde_json::to_string(v).unwrap_or_default())
                                .unwrap_or_default();
                            if !name.is_empty() {
                                calls.push(crate::llm::ToolCall {
                                    id: format!("xml_tc_{}", calls.len()),
                                    type_: "function".into(),
                                    function: crate::llm::ToolFunction {
                                        name,
                                        arguments: args,
                                    },
                                });
                            }
                        }
                    }
                }
            }
        }

        if !calls.is_empty() {
            tracing::info!(count = calls.len(), "Parsed XML tool calls from response text");
        }
        calls
    }

    fn try_repair_messages(msgs: &mut Vec<Message>, error_body: &str) -> bool {
        let body_lower = error_body.to_lowercase();
        let mut repaired = false;
        let before_len = msgs.len();

        // Strategy 1: tool protocol violations
        if body_lower.contains("tool") {
            Self::sanitize_dangling_tool_calls(msgs);
            repaired |= msgs.len() != before_len;
        }

        // Strategy 2: role alternation violations — always run sanitize (includes merge)
        if body_lower.contains("role") || body_lower.contains("alternate")
            || body_lower.contains("consecutive") || body_lower.contains("preceding")
        {
            let pre = msgs.len();
            Self::sanitize_dangling_tool_calls(msgs);
            repaired |= msgs.len() != pre;
        }

        // Strategy 3: reasoning_content / thinking field issues
        if body_lower.contains("reasoning_content") || body_lower.contains("thinking") {
            for m in msgs.iter_mut() {
                if m.role == MessageRole::Assistant && m.reasoning_content.is_some() {
                    m.reasoning_content = None;
                    repaired = true;
                }
            }
        }

        // Strategy 4: empty/null content issues
        if body_lower.contains("content") && (body_lower.contains("required")
            || body_lower.contains("empty") || body_lower.contains("null"))
        {
            for m in msgs.iter_mut() {
                if m.content.is_none() && m.role != MessageRole::Tool {
                    m.content = Some(MessageContent::Text("(empty)".into()));
                    repaired = true;
                }
            }
        }

        // Strategy 5: catch-all — if no specific match but it's a 400, run full sanitize
        if !repaired {
            let pre = msgs.len();
            Self::sanitize_dangling_tool_calls(msgs);
            repaired = msgs.len() != pre;
        }

        if repaired {
            tracing::info!(strategies_applied = true, "try_repair_messages modified {} messages", before_len - msgs.len());
        }
        repaired
    }

    async fn execute_loop(&self, ctx: &mut TurnContext) -> Result<Option<TurnResult>, KernelError> {
        // 单轮不设工具总量上限——工具限制统一由 SafetyGuard session 级控制（默认 500）

        for loop_iter in 0..self.core.config.thresholds.turn_max_iterations {
            // Fix 6: Cancel check at top of each loop iteration
            if self.is_cancelled() {
                if let Some(ref stx) = self.stream_tx {
                    let _ = stx.send(crate::llm::stream::StreamChunk::Error("Turn cancelled".into()));
                }
                break;
            }
            // V38: 迭代边界信号——TUI 收到后清空 streaming_thinking，避免跨迭代累积
            if let Some(ref stx) = self.stream_tx {
                let _ = stx.send(crate::llm::stream::StreamChunk::IterationStart {
                    iteration: loop_iter as u32,
                });
            }
            // V35-1: messages 改为 mut，便于按需追加 prefix=true 的 assistant message
            let mut messages = {
                let s = self.session.read().await;
                let mut msgs = s.messages.write().await;
                // Sanitize: remove dangling assistant tool_calls without matching tool responses.
                // This prevents "tool_calls must be followed by tool messages" API errors
                // when a previous turn was interrupted mid-execution.
                Self::sanitize_dangling_tool_calls(&mut msgs);
                msgs.clone()
            };

            let total = self.core.context_manager.estimate_total_tokens(&messages).await;
            {
                let mut w = self.core.context_manager.window.write().await;
                w.current_tokens = total;
            }

            // ThinkingConfig 优先级：config（显式）> session-sticky（首轮锁定）> complexity 推导 > None
            // 详见 resolve_thinking_config_sticky 实现注释（B 方案 KV cache 修复）
            let thinking = self.resolve_thinking_config_sticky(ctx.complexity_thinking.clone()).await;
            // RequestContext 覆盖：model / temperature / max_tokens
            // model 优先级：req_ctx（per-request 显式）> escalated_model（Task #96 sticky）> default
            // 引用关系：escalated_model 在 handle_model_escalation 升级成功后写入；
            // 后续所有 turn 优先用此 model 直到 session 结束——避免 cache 池来回切换
            // C-3 fix: 拆分两步，防止临时 session guard 跨 await 持有
            // 1) 独立作用域取出 Arc、释放 session guard
            let escalated_model_lock = {
                let s = self.session.read().await;
                std::sync::Arc::clone(&s.escalated_model)
            }; // session guard 在此释放
            // 2) 单独 await 内层锁，不再持有 session guard
            let escalated = escalated_model_lock.read().await.clone();
            // 用户 /model 命令设置的运行时覆盖（应高于 escalated 优先级）
            // 不把 model_override 放进 req_ctx.model（那是 per-request），而是作为 session 级默认
            // 优先级：req_ctx.model（显式单请求覆盖）> model_override（用户 /model 设置）> escalated > default
            let model_override = self.core.get_model_override().await;
            let effective_model = self.req_ctx.model.clone()
                .or(model_override)
                .or(escalated)
                .unwrap_or_else(|| self.core.config.default_model.clone());
            let effective_temperature = self.req_ctx.temperature
                .or(ctx.complexity_temperature)
                .unwrap_or(self.core.config.default_temperature);
            // V40: 动态自适应 max_tokens — 确保 prompt + max_tokens ≤ context_window
            // 优先级：req_ctx 显式覆盖 > model_spec.max_output_tokens > config.default_max_tokens
            // 策略：min(configured_max, context_remaining - safety_margin)
            // safety_margin = 1024：留余量给 token 估算误差 + 系统开销
            // 下限 clamp 2048：即使 context 紧张也保证最低输出空间（触发压缩而非静默截断）
            let model_max_output = self.core.config.model_spec.as_ref()
                .map(|s| s.max_output_tokens)
                .unwrap_or(self.core.config.default_max_tokens);
            let configured_max = self.req_ctx.max_tokens
                .unwrap_or(model_max_output);
            let effective_max_tokens = {
                let w = self.core.context_manager.window.read().await;
                let context_remaining = w.max_tokens.saturating_sub(total).saturating_sub(1024);
                let adaptive = (configured_max as usize).min(context_remaining).max(2048);
                adaptive as u32
            };

            // V35-1: 注入 prefix completion message（仅当 ctx 有 prefix 字段且 model 支持）
            // 引用关系：RequestContext.prefix_assistant_content（cli/api/mod.rs::send_planner_message_streaming 设置）
            // 副作用：仅修改 local messages，不污染 session.messages
            // 多 turn 守卫：仅 loop_iter==0 首轮注入；后续 tool-call response 轮不再注入
            //   (避免每 turn 都 "```json\n[" 让模型误以为要继续写 JSON 而非自由 reasoning)
            if loop_iter == 0 {
                maybe_inject_prefix_message(
                    &mut messages,
                    &effective_model.0,
                    self.req_ctx.prefix_assistant_content.as_deref(),
                );
            }

            let req = LlmRequest {
                model: effective_model,
                messages,
                system: Some(ctx.enriched_system.clone()),
                // KV 缓存优化: 多段 system prompt，稳定段标记 cacheable
                // Anthropic provider 优先使用 system_segments 实现 block 级缓存
                system_segments: ctx.system_segments.clone(),
                // Error-recovery: tools_exhausted 时传空数组，强制 LLM 仅产出文本总结
                tools: if ctx.tools_exhausted { Vec::new() } else { ctx.tool_defs.clone() },
                temperature: Some(effective_temperature),
                max_tokens: Some(effective_max_tokens),
                top_p: None, stop: Vec::new(), stream: false,
                // L1: thinking_intent 单通道——per-request override 优先，否则用 sticky 决策
                thinking_intent: self.req_ctx.thinking_intent.clone()
                    .or_else(|| thinking.clone()),
                cache_config: Some(crate::llm::prompt_cache::PromptCacheConfig::default()),
                extra_body: HashMap::new(),
                // Phase 4：主路径 LlmRequest 携带 ctx.user_message_preamble（pipeline 在 setup 阶段写入）
                user_message_preamble: ctx.user_message_preamble.clone(),
            };

            // V38: 始终走 streaming path（有 stream_tx 时）
            // stream_complete 返回完整 LlmResponse（含 tool_calls），同时实时 forward
            // thinking/text chunks 到 TUI。不再因为有 tools 就降级到 blocking（那会导致
            // thinking 和消息输出"等完才弹出"，违背实时流式体验）。
            let use_streaming = self.stream_tx.is_some();

            // 执行 LLM 请求（streaming 或 blocking），400 错误时尝试自动修复并重试一次
            let provider_result: Result<crate::llm::LlmResponse, KernelError> = if use_streaming {
                let stream_tx = self.stream_tx.as_ref().unwrap();
                let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
                let provider = ctx.provider.clone();
                let req_clone = req.clone();
                let stx = stream_tx.clone();

                // 在后台运行流式请求，同时转发 chunks 到 TUI
                let handle = tokio::spawn(async move {
                    provider.stream_complete(req_clone, event_tx).await
                });

                // 转发 StreamEvent → StreamChunk（实时推送到 TUI）
                // 同时在本线累积 structured tool_calls（id → (name, args_buf)）
                // stream_complete 返回的 LlmResponse.tool_calls 可能为 None（流式路径不组装）
                // 此处拦截 ToolCallArgDelta 并在 Done 后注入到最终 response
                let mut stream_tool_buf: std::collections::HashMap<
                    String, // call id
                    (String, String), // (name, accumulated_args)
                > = std::collections::HashMap::new();

                // 2026-05-28: 追踪已发送到 TUI 的文本内容（用于流中断重试时通知 TUI 清除部分内容）
                let mut streamed_text_acc = String::new();

                // V43: per-call streaming timeout（每次 LLM 调用独立计时）
                // +60s 余量：streaming 响应已经在传输中，给足缓冲
                let stream_total_timeout = std::time::Duration::from_secs(
                    ctx.dynamic_timeout_secs.saturating_add(60)
                );
                let stream_deadline = tokio::time::Instant::now() + stream_total_timeout;

                loop {
                    let event = {
                        let recv_fut = event_rx.recv();
                        let timeout_fut = tokio::time::sleep_until(stream_deadline);
                        if let Some(ref ct) = self.cancel {
                            tokio::select! {
                                evt = recv_fut => evt,
                                _ = ct.cancelled() => {
                                    handle.abort();
                                    break;
                                }
                                _ = timeout_fut => {
                                    // P0-1: streaming 总超时触发
                                    handle.abort();
                                    if let Some(ref stx2) = self.stream_tx {
                                        let _ = stx2.send(crate::llm::stream::StreamChunk::Error(
                                            format!("⚠️ streaming timeout: {}s (no complete response)", stream_total_timeout.as_secs())
                                        ));
                                    }
                                    tracing::warn!(timeout_secs = stream_total_timeout.as_secs(), "streaming total timeout hit");
                                    break;
                                }
                            }
                        } else {
                            // P0-3: cancel=None 时也有 fallback timeout（防永久阻塞）
                            tokio::select! {
                                evt = recv_fut => evt,
                                _ = timeout_fut => {
                                    handle.abort();
                                    if let Some(ref stx2) = self.stream_tx {
                                        let _ = stx2.send(crate::llm::stream::StreamChunk::Error(
                                            format!("⚠️ streaming timeout: {}s", stream_total_timeout.as_secs())
                                        ));
                                    }
                                    break;
                                }
                            }
                        }
                    };
                    let Some(event) = event else { break };
                    match event {
                        crate::llm::stream::StreamEvent::TextDelta(t) => {
                            streamed_text_acc.push_str(&t);
                            let _ = stx.send(crate::llm::stream::StreamChunk::TextDelta(t));
                        }
                        crate::llm::stream::StreamEvent::ThinkingDelta(t) => {
                            let _ = stx.send(crate::llm::stream::StreamChunk::Thinking(t));
                        }
                        crate::llm::stream::StreamEvent::ToolCallStart { id, name } => {
                            let _ = stx.send(crate::llm::stream::StreamChunk::ToolStart { name: name.clone() });
                            stream_tool_buf.insert(id, (name, String::new()));
                        }
                        crate::llm::stream::StreamEvent::ToolCallArgDelta { id, delta } => {
                            if let Some((_name, args)) = stream_tool_buf.get_mut(&id) {
                                args.push_str(&delta);
                            }
                        }
                        crate::llm::stream::StreamEvent::Error(msg) => {
                            let _ = stx.send(crate::llm::stream::StreamChunk::Error(msg));
                        }
                        crate::llm::stream::StreamEvent::Done => break,
                        _ => {}
                    }
                }

                let stream_result = handle.await
                    .map_err(|e| KernelError::Other(format!("stream task panicked: {e}")))
                    .and_then(|r| r);

                // 2026-05-28: 流中断时通知 TUI 清除已渲染的部分内容
                // 如果 stream 返回错误且已有内容发送到 TUI，发 StreamRetryReset
                // 让 TUI 知道之前的内容即将因重试被重新生成
                match stream_result {
                    Ok(mut response) => {
                        // 如果 streaming 收集到了 structured tool_calls 且 response 中为空，注入组装结果
                        if !stream_tool_buf.is_empty() && response.message.tool_calls.as_ref().map_or(true, |v| v.is_empty()) {
                            let assembled: Vec<crate::llm::provider::ToolCall> = stream_tool_buf
                                .into_iter()
                                .map(|(id, (name, arguments))| crate::llm::provider::ToolCall {
                                    id,
                                    type_: "function".into(),
                                    function: crate::llm::provider::ToolFunction { name, arguments },
                                })
                                .collect();
                            tracing::debug!(n = assembled.len(), "streaming: assembled tool_calls from SSE deltas");
                            // 2026-05-28: 组装完成后立即发 ToolArgs，让 TUI Running 状态就能显示路径
                            // （不等 dispatch 阶段再发——消除 ToolStart 和 ToolArgs 之间的时间差）
                            for tc in &assembled {
                                let _ = stx.send(crate::llm::stream::StreamChunk::ToolArgs {
                                    name: tc.function.name.clone(),
                                    args_json: tc.function.arguments.clone(),
                                });
                            }
                            response.message.tool_calls = Some(assembled);
                        }
                        Ok(response)
                    }
                    Err(e) => {
                        if !streamed_text_acc.is_empty() {
                            let _ = stx.send(crate::llm::stream::StreamChunk::StreamRetryReset {
                                partial_text: streamed_text_acc,
                            });
                        }
                        Err(e)
                    }
                }
            } else {
                // Blocking path: 有 tools 或未启用 streaming
                // V41: 叠加 LLM 主动申请的 timeout extension
                let extension = {
                    let s = self.session.read().await;
                    s.timeout_extension_secs
                };
                let effective_timeout = ctx.dynamic_timeout_secs.saturating_add(extension).min(900);
                let provider_timeout = std::time::Duration::from_secs(effective_timeout);
                match tokio::time::timeout(provider_timeout, ctx.provider.complete_cancellable(req.clone(), self.cancel_token())).await {
                    Ok(result) => result,
                    Err(_elapsed) => {
                        if let Some(ref stx) = self.stream_tx {
                            let _ = stx.send(crate::llm::stream::StreamChunk::Error(
                                format!("LLM request timed out after {}s", ctx.dynamic_timeout_secs)
                            ));
                        }
                        Err(KernelError::Provider(format!("request timeout: {}s", ctx.dynamic_timeout_secs)))
                    }
                }
            };

            // ── 400 Auto-Repair: 检测到 400 时尝试修复消息序列并重试一次 ──
            let response = match provider_result {
                Ok(resp) => resp,
                Err(KernelError::ApiError { status: 400, ref body }) => {
                    let body_lower = body.to_lowercase();
                    // ── Strategy 0: Context length exceeded → 压缩后重试 ──
                    // Anthropic: "prompt is too long", OpenAI: "maximum context length",
                    // DeepSeek/generic: "token limit", "too many tokens"
                    let is_context_overflow = body_lower.contains("prompt is too long")
                        || body_lower.contains("input is too long")
                        || body_lower.contains("maximum context length")
                        || body_lower.contains("too many tokens")
                        || body_lower.contains("context_length_exceeded")
                        || body_lower.contains("request too large")
                        || body_lower.contains("tokens exceeds");
                    if is_context_overflow {
                        tracing::warn!("Context overflow detected, triggering emergency compression");
                        let compressed = {
                            let s = self.session.read().await;
                            let mut msgs = s.messages.write().await;
                            self.core.context_manager.auto_compress_messages(&mut msgs).await
                        };
                        if !compressed.is_empty() {
                            if let Some(ref stx) = self.stream_tx {
                                let _ = stx.send(crate::llm::stream::StreamChunk::CompressStart);
                                let tokens_saved: usize = compressed.iter()
                                    .map(|c| c.original_tokens.saturating_sub(c.compressed_tokens))
                                    .sum();
                                let _ = stx.send(crate::llm::stream::StreamChunk::CompressEnd {
                                    messages_compressed: compressed.len(),
                                    tokens_saved,
                                });
                            }
                        }
                        if !compressed.is_empty() {
                            // 压缩成功，重建消息并重试
                            ctx.recovery_attempts += 1;
                            if ctx.recovery_attempts > self.core.config.thresholds.turn_max_recovery {
                                ctx.final_response = "[System] 上下文压缩多次后仍超限，请精简对话或重新开始。".into();
                                break;
                            }
                            continue;
                        }
                        // 压缩后仍无法缩减 → fall through to normal repair
                    }
                    // 尝试根据错误信息修复消息
                    let mut retry_messages = {
                        let s = self.session.read().await;
                        let guard = s.messages.read().await;
                        guard.clone()
                    };
                    let repaired = Self::try_repair_messages(&mut retry_messages, body);
                    if repaired {
                        tracing::warn!(
                            error_body = %body.chars().take(200).collect::<String>(),
                            "400 detected, attempting message repair and retry"
                        );
                        // 写回修复后的消息
                        {
                            let s = self.session.read().await;
                            *s.messages.write().await = retry_messages.clone();
                        }
                        // 重建 request 并重试
                        let retry_req = LlmRequest {
                            model: req.model.clone(),
                            messages: retry_messages,
                            system: req.system.clone(),
                            system_segments: req.system_segments.clone(),
                            tools: req.tools.clone(),
                            temperature: req.temperature,
                            max_tokens: req.max_tokens,
                            top_p: req.top_p,
                            stop: req.stop.clone(),
                            stream: req.stream,
                            thinking_intent: req.thinking_intent.clone(),
                            cache_config: req.cache_config.clone(),
                            extra_body: req.extra_body.clone(),
                            user_message_preamble: req.user_message_preamble.clone(),
                        };
                        match ctx.provider.complete_cancellable(retry_req, self.cancel_token()).await {
                            Ok(resp) => resp,
                            Err(retry_err) => {
                                // Error-recovery: 400 repair retry also failed
                                let err_desc = sanitize_provider_error(&retry_err);
                                if let Some(ref stx) = self.stream_tx {
                                    let _ = stx.send(crate::llm::stream::StreamChunk::Error(
                                        format!("LLM retry after repair also failed: {}", err_desc)
                                    ));
                                }
                                ctx.recovery_attempts += 1;
                                if ctx.recovery_attempts > self.core.config.thresholds.turn_max_recovery {
                                    ctx.final_response = "[System] 多次恢复尝试失败，请重新描述你的需求。".into();
                                    break;
                                }
                                {
                                    let s = self.session.read().await;
                                    let mut msgs = s.messages.write().await;
                                    msgs.push(Message {
                                        role: MessageRole::User,
                                        content: Some(MessageContent::Text(format!(
                                            "[Abacus 系统约束] 修复后 LLM 请求仍失败：{}。\n\
约束与指引：\n\
① 简化当前工具调用参数，避免复杂嵌套结构\n\
② 将任务拆解为更小的独立步骤逐步执行\n\
③ 若仍无法继续，停止工具序列并向用户说明情况",
                                            err_desc
                                        ))),
                                        name: None, tool_calls: None, tool_call_id: None,
                                        reasoning_content: None, prefix: false,
                                    });
                                }
                                continue;
                            }
                        }
                    } else {
                        // Error-recovery: 400 无法自动修复 → 注入错误上下文让 LLM 适应
                        let err_desc = body.chars().take(200).collect::<String>();
                        if let Some(ref stx) = self.stream_tx {
                            let _ = stx.send(crate::llm::stream::StreamChunk::Error(
                                format!("LLM API error (400): {}", err_desc)
                            ));
                        }
                        ctx.recovery_attempts += 1;
                        if ctx.recovery_attempts > self.core.config.thresholds.turn_max_recovery {
                            ctx.final_response = "[System] 多次恢复尝试失败，请重新描述你的需求。".into();
                            break;
                        }
                        {
                            let s = self.session.read().await;
                            let mut msgs = s.messages.write().await;
                            msgs.push(Message {
                                role: MessageRole::User,
                                content: Some(MessageContent::Text(format!(
                                    "[Abacus 系统约束] API 返回 400 错误：{}。\n\
约束与指引：\n\
① 检查工具调用的参数格式和字段类型是否符合 schema\n\
② 减少单次请求的 token 消耗（缩短 system prompt 或 context）\n\
③ 若错误持续，终止工具序列并向用户报告具体错误信息",
                                    err_desc
                                ))),
                                name: None, tool_calls: None, tool_call_id: None,
                                reasoning_content: None, prefix: false,
                            });
                        }
                        continue;
                    }
                }
                Err(e) => {
                    let err_str = e.to_string();
                    let is_net = is_network_error(&err_str);

                    ctx.provider_retries += 1;
                    if ctx.provider_retries <= 2 {
                        // 网络错误 vs API 错误用不同前缀，TUI 据此分类显示
                        let chunk_msg = if is_net {
                            format!("NETWORK_ERROR:retrying {}/{}: 网络连接失败，自动重试中...",
                                ctx.provider_retries, 3)
                        } else {
                            format!("Provider error (retrying {}/3): {}", ctx.provider_retries, e)
                        };
                        if let Some(ref stx) = self.stream_tx {
                            let _ = stx.send(crate::llm::stream::StreamChunk::Error(chunk_msg));
                        }
                        ctx.recovery_attempts += 1;
                        if ctx.recovery_attempts > self.core.config.thresholds.turn_max_recovery {
                            ctx.final_response = "[System] 多次恢复尝试失败，请重新描述你的需求。".into();
                            break;
                        }
                        // 网络错误不注入 messages（LLM 没收到请求，注入无意义）
                        if !is_net {
                            let s = self.session.read().await;
                            let mut msgs = s.messages.write().await;
                            msgs.push(Message {
                                role: MessageRole::User,
                                content: Some(MessageContent::Text(format!(
                                    "[Abacus 系统通知] LLM 服务暂时不可用（第 {}/3 次重试）：{}。\n当前任务上下文已保留，系统将自动重试——无需重复操作。",
                                    ctx.provider_retries, sanitize_provider_error(&e)
                                ))),
                                name: None, tool_calls: None, tool_call_id: None,
                                reasoning_content: None, prefix: false,
                            });
                        }
                        continue;
                    } else {
                        // 3 次全失败 — 区分网络错误给用户明确指引
                        let final_err = if is_net {
                            "NETWORK_ERROR:FATAL:网络连接失败，请检查网络后重试".to_string()
                        } else {
                            format!("🛑 LLM 服务不可达（3 次重试失败）: {}", e)
                        };
                        if let Some(ref stx) = self.stream_tx {
                            let _ = stx.send(crate::llm::stream::StreamChunk::Error(final_err));
                        }
                        return Err(KernelError::Provider(format!(
                            "Provider unavailable after 3 attempts: {}", e
                        )));
                    }
                }
            };

            // Error-recovery: provider 成功响应 → 重置重试计数
            ctx.provider_retries = 0;

            ctx.prompt_tokens += response.usage.prompt_tokens;
            ctx.completion_tokens += response.usage.completion_tokens;
            ctx.cached_tokens += response.usage.cached_tokens;
            ctx.thinking_tokens += response.usage.thinking_tokens;
            // cross-session: emit LlmComplete event（半截子修复——之前定义但 0 处触发）
            // 引用：mag_chain::PipelineEvent::LlmComplete 字段：loop_iter, completion_tokens
            // 让 hook 能追踪 LLM 的多轮调用（max_turns_per_request 内每轮一次）
            // 失败处理：emit 错误吞掉（hook 失败不阻塞 LLM 主路径）
            let _ = self.core.emit_pipeline_event(crate::mag_chain::PipelineEvent::LlmComplete {
                loop_iter: loop_iter as usize,
                completion_tokens: response.usage.completion_tokens,
            }).await;
            // Task #95：累积 session 级 cache telemetry（每轮 LLM 调用都累加）
            // 引用关系：CoreLoop::cache_stats / audit_optimizations 读取此聚合
            // 副作用：单 RwLock 写——若 LLM 高频并发调用同 session 不应有竞争
            {
                let s = self.session.read().await;
                let mut tele = s.cache_telemetry.write().await;
                tele.total_input_tokens += response.usage.prompt_tokens;
                tele.total_cached_tokens += response.usage.cached_tokens;
                tele.total_cache_creation_tokens += response.usage.cache_creation_tokens;
            }
            let tool_calls = response.message.tool_calls.clone().unwrap_or_default();

            // V40: 多格式文本工具调用解析（XML / JSON / Markdown fenced）
            // 引用：crate::llm::text_tool_parser — 支持 function_calls XML、裸 JSON、代码块
            // 降级：text_tool_parser 未命中时再用旧 try_parse_xml_tool_calls（向后兼容）
            // clean_text: 去除 XML 标记后的纯文本，写入 session 历史防止消息中断
            let mut clean_text = String::new(); // 当文本工具调用解析成功时赋値
            let tool_calls = if tool_calls.is_empty() {
                let raw_text = super::extract_text(&response.message);
                let (parsed_clean, text_parsed) =
                    crate::llm::text_tool_parser::extract_text_tool_calls(&raw_text);
                if !text_parsed.is_empty() {
                    clean_text = parsed_clean;
                    text_parsed
                } else {
                    Self::try_parse_xml_tool_calls(&response.message)
                }
            } else {
                tool_calls
            };

            // session 历史写入：文本工具调用时用 clean_text，防止 XML 标记残留
            {
                let s = self.session.read().await;
                let mut msgs = s.messages.write().await;
                let mut msg_to_push = response.message.clone();
                if !tool_calls.is_empty() && !clean_text.is_empty() {
                    msg_to_push.content =
                        Some(crate::llm::provider::MessageContent::Text(clean_text.clone()));
                    msg_to_push.tool_calls = Some(tool_calls.clone());
                }
                msgs.push(msg_to_push);
            }

            if tool_calls.is_empty() {
                let text = super::extract_text(&response.message);

                // V42: 空回复强制重试——LLM 不应空回复
                // 条件：text 为空 + 有工具输出 + 重试 < 1（防止无限循环）
                // 动作：注入 system message 要求生成总结
                if text.is_empty() && !ctx.all_tool_outputs.is_empty() && ctx.recovery_attempts < 1 {
                    ctx.recovery_attempts += 1;
                    tracing::warn!("LLM returned empty response after tool calls — requesting summary");
                    {
                        let s = self.session.read().await;
                        let mut msgs = s.messages.write().await;
                        msgs.push(Message {
                            role: MessageRole::System,
                            content: Some(MessageContent::Text(
                                "[Abacus] You completed tool calls but did not provide a response to the user. \
                                 Please summarize what you found/did and respond to the user's original request."
                                    .into()
                            )),
                            name: None, tool_calls: None, tool_call_id: None,
                            reasoning_content: None, prefix: false,
                        });
                    }
                    continue; // 重新调用 LLM
                }

                // ── V39: Tool-Call-in-Text Detection ──────────────────────────────
                // DeepSeek 等模型偶尔会在文本中写出工具名/命令而非发起 structured tool_call。
                // 检测条件：response text 包含已注册工具名 + tool_calls 为空 + 重试 < 1
                // 机制：注入纠正提示，让 LLM 用 function calling 而非文本输出来调工具。
                // tools_exhausted=true 时跳过：LLM 已知晓工具耗尽，注入纠错只会加剧混乱
                if ctx.tool_text_fallback_retries < 1 && !text.is_empty() && !ctx.tools_exhausted {
                    // 跳过描述性文本中的工具名提及（LLM 在解释计划而非调用）
                    let text_lower = text.to_lowercase();
                    // 跳过描述性语境：精确短语避免过度匹配
                    // “可以”过宽（中文回复几乎都含此词）——改用具体动词短语
                    let skip_patterns = ["i'll use", "i can use", "let me", "using ", "let's",
                                         "you can use", "you could use", "call ", "should call",
                                         "我来使用", "让我使用", "可以使用", "我会使用",
                                         "你可以使用", "我需要使用", "调用", "执行",
                                         "必须先", "然后才能", "再响应用户"];
                    if !skip_patterns.iter().any(|p| text_lower.contains(p)) {
                        let registered_names: Vec<String> = self.core.registry
                            .tool_names()
                            .await;
                        if let Some(tool_name) = registered_names.iter().find(|name| {
                            name.len() >= 4 && text_lower.contains(name.as_str())
                        }) {
                        // 额外检查：工具名在反引号内 → 描述性引用，跳过
                        let in_backticks = {
                            let name_pos = text_lower.find(tool_name.as_str());
                            name_pos.map_or(false, |pos| {
                                let before = &text[..pos];
                                let after = &text[pos + tool_name.len()..];
                                // 检查前后是否有反引号或代码块标记
                                before.ends_with('`') || before.contains("``") ||
                                after.starts_with('`') || after.contains("``") ||
                                // 检查是否在行内代码格式中
                                before.contains("`") && after.contains("`")
                            })
                        };
                        if in_backticks {
                            tracing::debug!(
                                tool = %tool_name,
                                "tool name in backtick-quoted text — skipping false positive"
                            );
                            // 不注入纠正消息，继续正常处理
                        } else {
                        ctx.tool_text_fallback_retries += 1;
                        tracing::warn!(
                            tool = %tool_name,
                            "LLM wrote tool name in text without function call — injecting correction"
                        );
                        // 修复：不再发假工具 ToolStart/ToolEnd，改用 processing_phase 更新显示纠正状态
                        if let Some(ref stx) = self.stream_tx {
                            let _ = stx.send(crate::llm::stream::StreamChunk::IterationStart {
                                iteration: ctx.tool_text_fallback_retries,
                            });
                        }
                        {
                            let s = self.session.read().await;
                            let mut msgs = s.messages.write().await;
                            // Abacus 内部指令：System 角色避免 LLM 文字确认回复
                            msgs.push(Message {
                                role: MessageRole::System,
                                content: Some(MessageContent::Text(
                                    format!(
                                        "[Abacus] 你在文本中提到了工具名 \"{}\"，但没有通过 function calling 调用它。\
                                         \n如果你确实要调用此工具，请使用结构化 tool_call（不要写在文本里）。\
                                         \n如果你只是正常推理中提到了工具名（并非要调用），请忽略此提示，继续你的回答。",
                                        tool_name
                                    ).into()
                                )),
                                name: None, tool_calls: None, tool_call_id: None,
                                reasoning_content: None, prefix: false,
                            });
                        }
                        continue; // 重试本轮
                        } // end else: in_backticks
                    }
                }
                } // if ctx.tool_text_fallback_retries < 1

                // V30: 检测 LLM 任务未完成即停止（premature stop）
                // 条件：本轮有工具失败 + LLM 输出了短文本（< 200 chars）+ 之前已有工具调用
                // 机制：注入续写提示，让 LLM 继续尝试或显式声明阻塞
                // 修复1：tools_exhausted=true 时跳过（工具已关，LLM 第一次输出文本就是正常总结）
                // 修复2：只检查最近 1 次工具调用结果，而非最近 5 次（历史失败不应影响当前判断）
                if ctx.total_tool_calls > 0 && loop_iter > 0 && !ctx.tools_exhausted {
                    let has_recent_failures = ctx.all_tool_outputs.last()
                        .map(|o| !o.success)
                        .unwrap_or(false);

                    if has_recent_failures {
                        // 检查 LLM 是否显式声明了状态（遵守 [Explicit Declaration]）
                        let has_declaration = text.contains("[Blocked]")
                            || text.contains("[Stuck]")
                            || text.contains("[Need Input]")
                            || text.contains("[Partial]");

                        let retry_count = ctx.premature_stop_retries;

                        // 短文本 + 无声明 + 未用完重试配额 → 注入提醒
                        let stop_chars = self.core.config.policy.thresholds.premature_stop_chars;
                        let max_retries = self.core.config.policy.thresholds.premature_stop_max_retries;
                        if !has_declaration && text.len() < stop_chars && retry_count < max_retries {
                            ctx.premature_stop_retries += 1;
                            tracing::warn!(
                                retry = retry_count + 1,
                                text_len = text.len(),
                                "LLM stopped without explicit declaration after tool failures"
                            );
                            // 用户感知：通知 TUI policy 拦截生效（IterationStart，不产生虚假工具条目）
                            if let Some(ref stx) = self.stream_tx {
                                let _ = stx.send(crate::llm::stream::StreamChunk::IterationStart {
                                    iteration: retry_count + 1,
                                });
                            }
                            // 构建携带失败上下文的续写提示
                            // has_recent_failures=true → last() 一定存在且 success=false
                            let hint_msg = {
                                let last = ctx.all_tool_outputs.last().unwrap();
                                let failed_tool = &last.tool_id.0;
                                let fk = last.failure_kind.as_deref().unwrap_or("Unknown");
                                let err_text = last.output.get("error")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("(no detail)");
                                // 安全截断（避免 multi-byte 边界 panic）
                                let err_short: String = err_text.chars().take(200).collect();
                                format!(
                                    "Tool `{failed_tool}` failed ({fk}): {err_short}\n\
                                     You MUST either:\n\
                                     1. Try alternative approaches to complete the task, OR\n\
                                     2. Explicitly state what happened using [Blocked], [Stuck], [Need Input], or [Partial] prefix.\n\
                                     Do not stop silently."
                                )
                            };
                            {
                                let s = self.session.read().await;
                                let mut msgs = s.messages.write().await;
                                // Abacus 内部指令：System 角色避免 LLM 文字确认回复
                                msgs.push(Message {
                                    role: MessageRole::System,
                                    content: Some(MessageContent::Text(hint_msg)),
                                    name: None, tool_calls: None, tool_call_id: None,
                                    reasoning_content: None, prefix: false,
                                });
                            }
                            continue;
                        }
                        // 有声明 → 允许停下（LLM 已告知用户问题所在）
                    }
                }

                // V38: 检测 max_tokens 截断 — finish_reason=="length" 表示输出被切断
                // 自动追加续写请求（一次），让 LLM 从断点继续
                if response.finish_reason == "length" && !text.is_empty() {
                    tracing::warn!(text_len = text.len(), "response truncated by max_tokens, requesting continuation");
                    // 通知用户输出被截断、正在续写
                    if let Some(ref stx) = self.stream_tx {
                        let _ = stx.send(crate::llm::stream::StreamChunk::TextDelta(
                            "\n[...output truncated, continuing...]\n".into()
                        ));
                    }
                    // 把截断的响应存为 assistant message（已在上方 push 了）
                    // 追加一条 user 续写提示
                    {
                        let s = self.session.read().await;
                        let mut msgs = s.messages.write().await;
                        // Abacus 内部续写指令：System 角色
                        msgs.push(Message {
                            role: MessageRole::System,
                            content: Some(MessageContent::Text("[Abacus] Output was truncated. Continue directly from where you left off without any preamble.".into())),
                            name: None, tool_calls: None, tool_call_id: None, reasoning_content: None, prefix: false,
                        });
                    }
                    // 继续循环（下一次迭代会发新请求）
                    continue;
                }

                // Content policy filter — LLM response blocked
                if response.finish_reason == "content_filter" {
                    if let Some(ref stx) = self.stream_tx {
                        let _ = stx.send(crate::llm::stream::StreamChunk::Error(
                            "Response blocked by content safety policy".into()
                        ));
                    }
                    ctx.final_response = "[Content filtered by safety policy]".into();
                    break;
                }

                // Model Self-Escalation (flash → pro)
                if let Some(result) = self.handle_model_escalation(ctx, &text).await? {
                    return Ok(Some(result));
                }

                // Progressive Output gate check
                let action = {
                    let s = self.session.read().await;
                    let mut ctrl = s.progressive.write().await;
                    ctrl.on_output_chunk(&text)
                };
                match action {
                    OutputAction::Gate => {
                        return Ok(Some(self.build_gated_result(ctx, text).await));
                    }
                    OutputAction::Buffer => {
                        ctx.final_response = text;
                        break;
                    }
                    OutputAction::Forward => {
                        ctx.final_response = text;
                        break;
                    }
                }
            }

            // 单轮工具调用限制（per-turn，由 SafetyGuard.max_total_tool_calls 控制）
            // Session 级不设限——用户可持续多轮对话不受累积约束
            let batch_size = tool_calls.len() as u32;
            ctx.total_tool_calls += batch_size;
            let tool_limit = self.core.config.thresholds.turn_max_tool_calls;
            let threshold_75 = (tool_limit as f64 * 0.75) as u32;
            let threshold_95 = (tool_limit as f64 * 0.95) as u32;

            // ── 75% Warning：提示 LLM 调整策略（仅一次）──
            if !ctx.tool_warning_emitted && ctx.total_tool_calls >= threshold_75 {
                ctx.tool_warning_emitted = true;
                let s = self.session.read().await;
                let mut msgs = s.messages.write().await;
                msgs.push(Message {
                    role: MessageRole::System,
                    content: Some(MessageContent::Text(format!(
                        "[Abacus 资源提示] 工具调用: {}/{}（75%），剩余 {} 次。\
                         优先高价值操作；若即将超限请停止工具调用并总结当前进展。",
                        ctx.total_tool_calls, tool_limit, tool_limit - ctx.total_tool_calls
                    ))),
                    name: None, tool_calls: None, tool_call_id: None,
                    reasoning_content: None, prefix: false,
                });
            }

            // ── 时长预算感知 ──
            // V43: 改用 turn-level 时间预算（max_turn_timeout_secs），而非 per-call timeout
            // 因为 agentic 循环中多次 tool call + LLM 调用总时间自然累加
            if !ctx.time_warning_emitted {
                let elapsed = ctx.turn_started_at.elapsed();
                let turn_budget_secs = self.core.threshold_u64("max_turn_timeout");
                let budget = std::time::Duration::from_secs(turn_budget_secs);
                if elapsed > budget.mul_f64(0.6) {
                    ctx.time_warning_emitted = true;
                    let remaining_secs = budget.saturating_sub(elapsed).as_secs();
                    let s = self.session.read().await;
                    let mut msgs = s.messages.write().await;
                    msgs.push(Message {
                        role: MessageRole::System,
                        content: Some(MessageContent::Text(format!(
                            "[Abacus 资源提示] 本轮已运行 {}s，剩余约 {}s。\
                             若即将超限，请立即停止工具调用并输出当前进展总结（已完成/未完成/建议下一步）。",
                            elapsed.as_secs(), remaining_secs
                        ))),
                        name: None, tool_calls: None, tool_call_id: None,
                        reasoning_content: None, prefix: false,
                    });
                }
            }

            // ── 95% 强制压缩：压缩上下文释放空间，LLM 继续工作（不终止）──
            if ctx.total_tool_calls >= threshold_95 && !ctx.tools_exhausted {
                ctx.tools_exhausted = true; // 防止重复触发
                if let Some(ref stx) = self.stream_tx {
                    let _ = stx.send(crate::llm::stream::StreamChunk::CompressStart);
                }
                let compressed = {
                    let s = self.session.read().await;
                    let mut msgs = s.messages.write().await;
                    self.core.context_manager.auto_compress_messages(&mut msgs).await
                };
                if let Some(ref stx) = self.stream_tx {
                    let tokens_saved: usize = compressed.iter()
                        .map(|c| c.original_tokens.saturating_sub(c.compressed_tokens))
                        .sum();
                    let _ = stx.send(crate::llm::stream::StreamChunk::CompressEnd {
                        messages_compressed: compressed.len(),
                        tokens_saved,
                    });
                }
                // 注入压缩通知——让 LLM 知道历史已压缩，调整引用策略
                {
                    let s = self.session.read().await;
                    let mut msgs = s.messages.write().await;
                    msgs.push(Message {
                        role: MessageRole::System,
                        content: Some(MessageContent::Text(format!(
                            "[Abacus 资源提示] 工具调用已达 95%（{}/{}），上下文已压缩。\
                             立即停止新工具调用，输出进展总结（已完成/未完成/建议下一步）。",
                            ctx.total_tool_calls, tool_limit
                        ))),
                        name: None, tool_calls: None, tool_call_id: None,
                        reasoning_content: None, prefix: false,
                    });
                }
                // 重置 tools_exhausted 让 LLM 继续调用工具（压缩释放了空间）
                ctx.tools_exhausted = false;
            }

            // ── 100% 硬上限：SafetyGuard 检查 ──
            if let Err(e) = self.core.safety_guard.check_tool_call_count(ctx.total_tool_calls) {
                let err_msg = e.to_string();
                if let Some(ref stx) = self.stream_tx {
                    let _ = stx.send(crate::llm::stream::StreamChunk::Error(
                        format!("Turn tool limit reached ({}): {}", ctx.total_tool_calls, err_msg)
                    ));
                }
                ctx.tools_exhausted = true;
                // 系统提示覆盖：明确告知 LLM 工具通道已关，防止其继续尝试调用工具而导致混乱
                // 注入到 enriched_system 末尾，确保下次 LlmRequest 就带这个信息
                ctx.enriched_system.push_str(
                    "\n\n[SYSTEM OVERRIDE] Tool calling is now DISABLED. \
                     The function calling channel is closed. \
                     Do NOT attempt any tool calls or write tool names in text. \
                     Provide your final answer in plain text only."
                );
                {
                    let s = self.session.read().await;
                    let mut msgs = s.messages.write().await;
                    msgs.push(Message {
                        role: MessageRole::System,
                        content: Some(MessageContent::Text(format!(
                            "[Abacus] Tool budget exhausted ({} calls). Tool calling disabled. \
                             Output your final summary now.",
                            ctx.total_tool_calls
                        ))),
                        name: None, tool_calls: None, tool_call_id: None,
                        reasoning_content: None, prefix: false,
                    });
                }
                continue;
            }

            let user_role = { let s = self.session.read().await; s.user_role };

            // ── P0-C1: 并行预执行（idempotent 纯读工具）──────────────────────
            //
            // ## 设计原理
            // 对满足以下条件的工具，在主串行循环前并发预执行 registry.execute()：
            //   1. 工具名前缀属于已知纯读工具集（kb_* / db_read* / fs_read* 等）
            //   2. 不是特殊工具（session.* / interaction.* / bash_exec 等会修改状态）
            //   3. 同轮 ≥2 个此类工具才触发并行（单个工具无并行收益）
            //
            // ## 安全保证
            // - 仅执行 mag_chain.before → registry.execute → wrap → mag_chain.after
            // - 主循环仍按顺序发送 ToolStart/ToolArgs/ToolOutput/ToolEnd streaming 事件
            // - 结果按 tool_call_id 索引，主循环命中后直接使用，跳过 registry.execute 调用
            // - 如果预执行失败，主循环走正常串行路径（预结果不存在时降级）
            //
            // ## 引用关系
            // - 触发方：每次 LLM 返回多个 tool_calls 时
            // - 消费方：下方主 for 循环（命中 parallel_results 则提前 Ok 返回）
            // - 生命周期：仅当前 iteration（局部变量，loop 结束即 drop）
            let parallel_results: std::collections::HashMap<String, ToolOutput> = {
                // 过滤可并行工具（保守白名单：kb/db_read/fs 纯读）
                let parallelizable: Vec<&crate::llm::ToolCall> = tool_calls.iter().filter(|tc| {
                    let n = tc.function.name.as_str();
                    !n.starts_with("session_") && !n.starts_with("interaction_")
                        && n != "bash_exec"
                        && !n.starts_with("subagent_") && !n.starts_with("skill_")
                        && (n.starts_with("kb_") || n.starts_with("db_read_")
                            || n == "db_info" || n == "db_list_tables" || n == "db_table_schema"
                            || n.starts_with("fs_read") || n.starts_with("fs_search")
                            || n == "fs_glob" || n == "fs_tree" || n == "fs_info"
                            || n.starts_with("file_kb_") || n.starts_with("file_retrieval_"))
                }).collect();

                if parallelizable.len() >= 2 {
                    // 构建共享 ExecutionContext（纯读，无写锁）
                    let exec_ctx = {
                        let s = self.session.read().await;
                        crate::tool::ExecutionContext {
                            session_id: s.session_id.clone(),
                            filengine: s.filengine_session.clone(),
                            turn_number: ctx.turn_number,
                            bash_default_timeout: self.core.config.policy.thresholds.bash_default_timeout,
                            bash_max_timeout: self.core.config.policy.thresholds.bash_max_timeout,
                            tool_default_timeout: self.core.config.policy.thresholds.tool_default_timeout,
                            role_caps: Arc::clone(&s.role_caps),
                        }
                    };
                    // 并发执行（futures_util::future::join_all 保序）
                    let futs: Vec<_> = parallelizable.iter().map(|tc| {
                        let tid = ToolId(tc.function.name.clone());
                        let params: Value = serde_json::from_str(&tc.function.arguments).unwrap_or(Value::Null);
                        let call_id = tc.id.clone();
                        let ctx_c = exec_ctx.clone();
                        let core = &self.core;
                        async move {
                            let r: Result<ToolOutput, KernelError> = async {
                                core.mag_chain.read().await.before(&tid, &params).await?;
                                let mut out = core.registry.execute(&tid, params, &ctx_c).await?;
                                out = core.mcip_gateway.wrap_output(out);
                                core.mag_chain.read().await.after(&tid, &mut out).await?;
                                Ok(out)
                            }.await;
                            (call_id, r)
                        }
                    }).collect();
                    let joined = futures_util::future::join_all(futs).await;
                    joined.into_iter()
                        .filter_map(|(id, r)| r.ok().map(|o| (id, o)))
                        .collect()
                } else {
                    std::collections::HashMap::new()
                }
            };
            // ── 并行预执行结束 ────────────────────────────────────────────────

            // V41: ToolAgent 批次检测 — 如果所有 tool_calls 匹配某个 ToolAgent，
            // 则走批量隔离执行（不逐个发 ToolStart/ToolEnd），只推送一条 ToolAgentResult
            let tool_ids_for_match: Vec<&str> = tool_calls.iter()
                .map(|tc| tc.function.name.as_str())
                .collect();
            // V41: 先尝试全量匹配；失败则部分匹配（拆分为 agent 路径 + 普通路径）
            let toolagent_match = self.core.subagent_registry.match_batch(&tool_ids_for_match);
            let partial_match = if toolagent_match.is_none() {
                self.core.subagent_registry.match_partial(&tool_ids_for_match)
            } else { None };

            if let Some(agent_def) = toolagent_match {
                // ── ToolAgent 批量执行路径 ──
                let agent_id = agent_def.id.clone();
                let agent_icon = agent_def.icon.clone();
                let agent_name = agent_def.name.clone();
                let mut batch_outputs: Vec<ToolOutput> = Vec::new();
                let mut batch_details: Vec<String> = Vec::new();

                // 预构建 exec_ctx（避免在循环内反复获取 session read lock）
                let batch_exec_ctx = {
                    let s = self.session.read().await;
                    crate::tool::ExecutionContext {
                        session_id: s.session_id.clone(),
                        filengine: s.filengine_session.clone(),
                        turn_number: ctx.turn_number,
                        bash_default_timeout: self.core.config.policy.thresholds.bash_default_timeout,
                        bash_max_timeout: self.core.config.policy.thresholds.bash_max_timeout,
                        tool_default_timeout: self.core.config.policy.thresholds.tool_default_timeout,
                        role_caps: Arc::clone(&s.role_caps),
                    }
                };

                for tc in &tool_calls {
                    // P1 修复：batch 执行中检查取消（避免用户 Esc 后仍执行剩余工具）
                    if self.is_cancelled() {
                        let tool_id = ToolId(tc.function.name.clone());
                        batch_details.push(format!("⏹ {} → cancelled", tool_id.0));
                        batch_outputs.push(ToolOutput {
                            tool_id,
                            success: false,
                            output: serde_json::json!({"error": "cancelled by user"}),
                            latency_ms: 0,
                            failure_kind: Some("Cancelled".into()),
                            try_instead: Vec::new(),
                        });
                        continue;
                    }
                    let tool_id = ToolId(tc.function.name.clone());
                    let params: Value = serde_json::from_str(&tc.function.arguments)
                        .unwrap_or(Value::Null);
                    match self.core.registry.execute(&tool_id, params, &batch_exec_ctx).await {
                        Ok(output) => {
                            let preview: String = output.output.to_string().chars().take(120).collect();
                            let status = if output.success { "✓" } else { "✗" };
                            batch_details.push(format!("{} {} → {}", status, tool_id.0, preview));
                            batch_outputs.push(output);
                        }
                        Err(e) => {
                            batch_details.push(format!("✗ {} → error: {}", tool_id.0, e));
                            batch_outputs.push(ToolOutput {
                                tool_id: tool_id.clone(),
                                success: false,
                                output: serde_json::json!({"error": e.to_string()}),
                                latency_ms: 0,
                                failure_kind: Some("ExecutionError".into()),
                                try_instead: Vec::new(),
                            });
                        }
                    }
                    // P0 修复：不再在此递增——L1911 已统一计数 batch_size
                    // 旧代码 ctx.total_tool_calls += 1 导致双计 → 预算提前耗尽
                }

                // 汇总摘要：首个成功输出的前 80 字符
                let summary: String = batch_outputs.iter()
                    .find(|o| o.success)
                    .map(|o| o.output.to_string().chars().take(80).collect())
                    .unwrap_or_else(|| "执行完成".into());

                // 推送 ToolAgentResult 到 TUI（替代多个 ToolStart/ToolEnd）
                if let Some(ref stx) = self.stream_tx {
                    let _ = stx.send(crate::llm::stream::StreamChunk::ToolAgentResult {
                        agent_id: agent_id.clone(),
                        icon: agent_icon,
                        name: agent_name,
                        call_count: batch_outputs.len(),
                        summary,
                        details: batch_details,
                    });
                }

                // 构建 tool results 消息返回给 LLM
                // V41: summarize_results=true 时压缩为一条摘要（省 token）
                let summarize = agent_def.summarize_results;
                let mut batch_tool_msgs: Vec<Message> = Vec::new();

                if summarize {
                    // 摘要模式：每个 tool_call_id 仍需有对应 tool_result（LLM 协议要求）
                    // 内容经结构化提取后返回有效 JSON（不盲目截断）
                    //
                    // 重要：不能合并为 1 条！DeepSeek/OpenAI 要求 tool_calls 与 tool_results 1:1 匹配
                    for (i, tc) in tool_calls.iter().enumerate() {
                        if let Some(output) = batch_outputs.get(i) {
                            ctx.all_tool_outputs.push(output.clone());
                            // 结构化摘要：保持 JSON 合法性，按工具类型提取关键信息
                            let summarized = summarize_tool_output(
                                &tc.function.name, &output.output, output.success
                            );
                            batch_tool_msgs.push(Message {
                                role: MessageRole::Tool,
                                content: Some(MessageContent::Text(summarized)),
                                name: Some(tc.function.name.clone()),
                                tool_calls: None,
                                tool_call_id: Some(tc.id.clone()),
                                reasoning_content: None,
                                prefix: false,
                            });
                        }
                    }
                } else {
                    // 完整模式：每条 tool_result 独立返回（推理精度最高）
                    for (i, tc) in tool_calls.iter().enumerate() {
                        if let Some(output) = batch_outputs.get(i) {
                            let content_json = serde_json::to_string(&output.output).unwrap_or_default();
                            batch_tool_msgs.push(Message {
                                role: MessageRole::Tool,
                                content: Some(MessageContent::Text(content_json)),
                                name: Some(tc.function.name.clone()),
                                tool_calls: None,
                                tool_call_id: Some(tc.id.clone()),
                                reasoning_content: None,
                                prefix: false,
                            });
                            ctx.all_tool_outputs.push(output.clone());
                        }
                    }
                }

                // 写入 session 历史（仅 tool results——assistant 消息已在 L1713 写入）
                // P0 修复：删除 response.message.clone() 的重复 push
                // 引用关系：L1713 无条件写入 assistant msg（含 tool_calls 字段），此处仅追加 tool results
                {
                    let s = self.session.read().await;
                    let mut msgs = s.messages.write().await;
                    msgs.extend(batch_tool_msgs);
                }

                continue; // 跳过下方的逐个 dispatch 循环，进入下一轮 LLM 调用
            }

            // V41: 部分匹配——拆分 agent 路径 vs 普通路径
            // matched_indices 的 tool_calls 由 ToolAgent 批量执行，unmatched 走普通 dispatch
            let partial_handled_indices: std::collections::HashSet<usize> = if let Some((agent_def, matched_idx, _unmatched_idx)) = partial_match {
                let agent_id = agent_def.id.clone();
                let agent_icon = agent_def.icon.clone();
                let agent_name = agent_def.name.clone();
                let summarize = agent_def.summarize_results;
                let mut batch_outputs: Vec<ToolOutput> = Vec::new();
                let mut batch_details: Vec<String> = Vec::new();

                let batch_exec_ctx = {
                    let s = self.session.read().await;
                    crate::tool::ExecutionContext {
                        session_id: s.session_id.clone(),
                        filengine: s.filengine_session.clone(),
                        turn_number: ctx.turn_number,
                        bash_default_timeout: self.core.config.policy.thresholds.bash_default_timeout,
                        bash_max_timeout: self.core.config.policy.thresholds.bash_max_timeout,
                        tool_default_timeout: self.core.config.policy.thresholds.tool_default_timeout,
                        role_caps: Arc::clone(&s.role_caps),
                    }
                };

                for &idx in &matched_idx {
                    if self.is_cancelled() { break; }
                    let tc = &tool_calls[idx];
                    let tool_id = ToolId(tc.function.name.clone());
                    let params: Value = serde_json::from_str(&tc.function.arguments).unwrap_or(Value::Null);
                    match self.core.registry.execute(&tool_id, params, &batch_exec_ctx).await {
                        Ok(output) => {
                            let preview: String = output.output.to_string().chars().take(80).collect();
                            batch_details.push(format!("✓ {} → {}", tool_id.0, preview));
                            batch_outputs.push(output);
                        }
                        Err(e) => {
                            batch_details.push(format!("✗ {} → {}", tool_id.0, e));
                            batch_outputs.push(ToolOutput {
                                tool_id, success: false,
                                output: serde_json::json!({"error": e.to_string()}),
                                latency_ms: 0, failure_kind: Some("ExecutionError".into()),
                                try_instead: Vec::new(),
                            });
                        }
                    }
                }

                // 推送 ToolAgentResult
                if let Some(ref stx) = self.stream_tx {
                    let summary: String = batch_outputs.iter()
                        .find(|o| o.success)
                        .map(|o| o.output.to_string().chars().take(80).collect())
                        .unwrap_or_else(|| "部分执行".into());
                    let _ = stx.send(crate::llm::stream::StreamChunk::ToolAgentResult {
                        agent_id: agent_id.clone(), icon: agent_icon, name: agent_name,
                        call_count: batch_outputs.len(), summary, details: batch_details,
                    });
                }

                // 写入 tool results 到 session
                {
                    let s = self.session.read().await;
                    let mut msgs = s.messages.write().await;
                    for (batch_i, &orig_idx) in matched_idx.iter().enumerate() {
                        let tc = &tool_calls[orig_idx];
                        if let Some(output) = batch_outputs.get(batch_i) {
                            ctx.all_tool_outputs.push(output.clone());
                            let content = if summarize {
                                summarize_tool_output(&tc.function.name, &output.output, output.success)
                            } else {
                                serde_json::to_string(&output.output).unwrap_or_default()
                            };
                            msgs.push(Message {
                                role: MessageRole::Tool,
                                content: Some(MessageContent::Text(content)),
                                name: Some(tc.function.name.clone()),
                                tool_calls: None,
                                tool_call_id: Some(tc.id.clone()),
                                reasoning_content: None, prefix: false,
                            });
                        }
                    }
                }

                matched_idx.into_iter().collect()
            } else {
                std::collections::HashSet::new()
            };

            let mut tool_results: Vec<Message> = Vec::new();
            for (tc_idx, tc) in tool_calls.iter().enumerate() {
                // V41: 跳过已被 partial ToolAgent 处理的 tool_call
                if partial_handled_indices.contains(&tc_idx) {
                    continue;
                }
                // 单一命名约定：schema.name == ToolId.0 == LLM 调用名（全部 _ 形态）。
                // 注册时已保证 LLM 协议字符集合规，dispatch 直接构造 ToolId，
                // 不再做 O(N) 反查（旧 V21 resolve_sanitized_id 已删除）。
                let tool_id = ToolId(tc.function.name.clone());

                // V29.9: per-tool ToolStart — 发 LLM-sanitized name(与 streaming 路径协议一致)
                //   引用关系: TUI run.rs:574 消费 ToolStart, push 到 streaming_tools(name 作为匹配键)
                //   生命周期: 与 ToolEnd(下方 dispatch 解析后)配对, 中间态 status=Running
                if let Some(ref stx) = self.stream_tx {
                    let _ = stx.send(crate::llm::stream::StreamChunk::ToolStart {
                        name: tc.function.name.clone(),
                    });
                }
                let params: Value = serde_json::from_str(&tc.function.arguments).unwrap_or_else(|e| {
                    tracing::warn!(
                        "failed to parse tool arguments JSON: {e}, raw: [REDACTED] ({} bytes)",
                        tc.function.arguments.len()
                    );
                    Value::Null
                });
                // V29.11: 紧跟 ToolStart 发 ToolArgs — 让 TUI trace 拿到输入参数(fs_edit diff 需要)
                if let Some(ref stx) = self.stream_tx {
                    let _ = stx.send(crate::llm::stream::StreamChunk::ToolArgs {
                        name: tc.function.name.clone(),
                        args_json: tc.function.arguments.clone(),
                    });
                }

                // 工具名归一化：LLM 调用时用的是消毒后的名字（点 → 下划线）
                // 例：`session.set_focus` 在 build_tool_definitions 中消毒为 `session_set_focus`
                // 所有 session.*/interaction.* 检查需要同时匹配两种形式
                let raw_name = tool_id.0.as_str();

                // P0-C1: 并行预执行命中检测
                // parallel_results 已在循环前并发执行完毕，命中则此工具的 execute 已完成
                // 主循环仍走完整路径（streaming/telemetry/session push），仅 execute 阶段跳过
                let parallel_hit: Option<ToolOutput> = parallel_results.get(&tc.id).cloned();

                // W2 (Task #100)：dedup 早退路径——仅 idempotent=true 工具走 cache
                //
                // 引用：CoreLoop.tool_result_dedup（Option，None 时跳过整段）
                // 生命周期：dispatch 前查询；命中则把 cached output 当 Ok 包装跳过所有 dispatch 分支
                // 副作用：命中仍会走下方 push/telemetry/stream/record_tool_invocation 同等链路，
                //   仅省去真实 IO/Provider 调用——LLM 端看到的 ToolMessage 与原始路径一致
                //
                // 两个变量同时承载：
                //   `dedup_key` —— 本次调用的 dedup 哈希键，dispatch 后写回时复用（避免 params 被 move 后无法 borrow）
                //   `dedup_hit` —— lookup 结果，None 表示需要走真正 dispatch
                let (dedup_key, dedup_hit): (
                    Option<crate::core::pipeline::dedup::DedupKey>,
                    Option<ToolOutput>,
                ) = if let Some(dedup) = &self.core.tool_result_dedup {
                    let is_idempotent = self.core.registry
                        .get(&tool_id).await
                        .map(|h| h.schema.idempotent)
                        .unwrap_or(false);
                    if is_idempotent {
                        let key = crate::core::pipeline::dedup::DedupKey::new(&tool_id, &params);
                        let hit = dedup.lookup(&tool_id, &params);
                        (Some(key), hit)
                    } else {
                        (None, None)
                    }
                } else {
                    (None, None)
                };

                let output_result = if let Some(pre_out) = parallel_hit {
                    // P0-C1: 并行预执行结果——直接使用，跳过 execute 路径
                    // 仅 idempotent 纯读工具命中；streaming/telemetry/session push 仍在主循环正常进行
                    Ok::<ToolOutput, KernelError>(pre_out)
                } else if let Some(cached) = dedup_hit.clone() {
                    Ok::<ToolOutput, KernelError>(cached)
                } else if raw_name == "session_request_permission" {
                    // LLM 主动申请权限入口：向用户展示授权对话框
                    // LLM 可调用此工具说明“我需要运行 xxx ，请授权”，而不是直接尝试调用被拦截的工具
                    let target_tool = params.get("tool_id").and_then(|v| v.as_str()).unwrap_or("");
                    let reason = params.get("reason").and_then(|v| v.as_str())
                        .unwrap_or("LLM requested permission to use this tool");
                    if !target_tool.is_empty() {
                        ctx.pending_confirmations.push(crate::mcip::McipConfirmRequest {
                            tool_id: target_tool.to_string(),
                            reason: reason.to_string(),
                            kind: crate::mcip::McipConfirmKind::McipPolicy,
                            params_preview: None,
                            nonce: String::new(), // LLM 主动申请路径不走 channel await
                            suggested_action: None, // LLM 主动申请：系统无法预判，由用户决策
                        });
                    }
                    Ok(ToolOutput {
                        tool_id: tool_id.clone(),
                        success: !target_tool.is_empty(),
                        output: if target_tool.is_empty() {
                            serde_json::json!({"error": "missing required parameter 'tool_id'"})
                        } else {
                            serde_json::json!({
                                "status": "permission_request_submitted",
                                "tool": target_tool,
                                "message": "Permission request submitted to user. Wait for their response before attempting to call the tool again."
                            })
                        },
                        latency_ms: 0,
                        failure_kind: if target_tool.is_empty() {
                            Some("BusinessError".into())
                        } else { None },
                        try_instead: Vec::new(),
                    })
                } else if raw_name.starts_with("interaction_") || raw_name.starts_with("session_") || raw_name.starts_with("magchain_") || raw_name.starts_with("cross_session_") || raw_name == "messages_recover" || raw_name == "tool_compass" {
                    // V29.13 段3a：magchain_* 工具走同一 inline dispatch（无外部依赖，CoreLoop 内部状态）
                    // cross-session: cross_session_* + messages_recover 也走 inline——
                    //   都需要访问 CoreLoop 的 memory_palace / context_manager
                    // 段 J2: tool_compass 也走 inline（依赖 CoreLoop.cluster_registry）
                    self.core.handle_interaction_tool(&tool_id, &params, self.session).await
                } else {
                    // V41: ToolActionClassifier 安全预检（在 MCIP 之前）
                    // 决策优先级：hard_deny → soft_deny → allow_rules → MCIP → user_grant
                    use crate::core::action_classifier::ClassifyResult;
                    let safety_result = self.core.action_classifier.classify(tool_id.0.as_str(), &params);
                    if let ClassifyResult::Deny(ref reason) = safety_result {
                        ctx.consecutive_blocks += 1;
                        Ok(ToolOutput {
                            tool_id: tool_id.clone(),
                            success: false,
                            output: serde_json::json!({"error": format!("Safety denied: {}", reason)}),
                            latency_ms: 0,
                            failure_kind: Some("SafetyDenied".into()),
                            try_instead: Vec::new(),
                        })
                    } else {

                    // 授权检查：优先于session 永久授权和本 turn 单次授权，匹配则跳过 MCIP 策略
                    // V41 Step 3: force_confirm_all 激活后，忽略 user_grant（强制确认所有操作）
                    let is_user_granted = if ctx.force_confirm_all {
                        false
                    } else {
                        let s = self.session.read().await;
                        let grants = s.mcip_grants.read().unwrap_or_else(|p| p.into_inner());
                        grants.contains(tool_id.0.as_str())
                    } || (!ctx.force_confirm_all && self.req_ctx.mcip_once_grants.contains(tool_id.0.as_str()));

                    // V41: safety NeedsConfirm 升级为 MCIP NeedsConfirm（复用已有确认通道）
                    let decision = if ctx.force_confirm_all && !is_user_granted {
                        // 降级模式：所有工具强制确认
                        McipDecision::NeedsConfirm("[降级] 连续拦截后所有操作需确认".into())
                    } else if is_user_granted {
                        McipDecision::Allowed
                    } else if let ClassifyResult::NeedsConfirm(reason) = &safety_result {
                        ctx.consecutive_blocks += 1;
                        // 连续 3 次拦截 → 降级执行策略（V41 Step 3）
                        // 行为：force_confirm_all=true，后续所有工具调用强制走 NeedsConfirm
                        // 引用关系：下方 MCIP 决策前检查 ctx.force_confirm_all
                        if ctx.consecutive_blocks >= 3 && !ctx.force_confirm_all {
                            ctx.force_confirm_all = true;
                            tracing::warn!("连续 {} 次安全拦截，降级到逐步确认模式", ctx.consecutive_blocks);
                            if let Some(ref stx) = self.stream_tx {
                                let _ = stx.send(crate::llm::stream::StreamChunk::TextDelta(
                                    "\n⚠️ 连续多次操作被安全策略拦截，已切换到逐步确认模式\n".into()
                                ));
                            }
                        }
                        McipDecision::NeedsConfirm(reason.clone())
                    } else {
                        self.core.mcip_gateway.check(&tool_id, &params, user_role)
                    };

                    // V30: Bash command-level classification (finer than tool-level MCIP)
                    // 即使 bash_exec 工具级别已授权，仍检查具体命令的安全等级
                    // 依赖：classify_bash_command() from tool::builtin::filengine
                    // 生命周期：per-invocation 分类，无持久状态
                    //
                    // 2026-05-28: bash_policy 执行从工具层上移到此处（pipeline 是唯一门控点）
                    // ReadOnly: 非 Allow 命令 → NeedsConfirm（用户可授权覆盖）
                    // DevTools: 保持 classify 原始决策
                    // Full: Dangerous 降级为 NeedsConfirm
                    let decision = if matches!(decision, McipDecision::Allowed)
                        && tool_id.0.as_str() == "bash_exec"
                    {
                        let cmd_str = params.get("command")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let raw_decision = crate::tool::builtin::filengine::classify_bash_command(cmd_str);

                        // 应用 bash_policy
                        let bash_policy = {
                            let s = self.session.read().await;
                            let fs = s.filengine_session.read().await;
                            fs.bash_policy
                        };
                        use crate::tool::builtin::filengine::BashDecision;
                        use abacus_types::BashPolicyLevel;
                        let policy_decision = match bash_policy {
                            BashPolicyLevel::ReadOnly => {
                                if matches!(raw_decision, BashDecision::Allow) {
                                    raw_decision
                                } else {
                                    BashDecision::NeedsConfirm(
                                        "bash_policy=ReadOnly: 此命令需要确认".into())
                                }
                            }
                            BashPolicyLevel::DevTools => raw_decision,
                            BashPolicyLevel::Full => {
                                if let BashDecision::Dangerous(reason) = raw_decision {
                                    BashDecision::NeedsConfirm(format!("[full-policy] {}", reason))
                                } else {
                                    raw_decision
                                }
                            }
                        };

                        match policy_decision {
                            BashDecision::Allow => decision,
                            BashDecision::NeedsConfirm(reason)
                            | BashDecision::Dangerous(reason) => {
                                // 检查此命令是否已在本 session 被用户单次授权过
                                let cmd_prefix: String = cmd_str.split_whitespace()
                                    .take(2).collect::<Vec<_>>().join(" ");
                                let bash_granted = {
                                    let s = self.session.read().await;
                                    let grants = s.mcip_grants.read().unwrap_or_else(|p| p.into_inner());
                                    grants.contains(&format!("bash:{}", cmd_prefix))
                                };
                                if bash_granted {
                                    McipDecision::Allowed
                                } else {
                                    McipDecision::NeedsConfirm(
                                        format!("[bash] {}", reason))
                                }
                            }
                        }
                    } else {
                        decision
                    };

                    match decision {
                        McipDecision::Denied(reason) => Ok(ToolOutput {
                            tool_id: tool_id.clone(),
                            success: false,
                            output: serde_json::json!({"error": format!("MCIP blocked: {}", reason)}),
                            latency_ms: 0,
                            failure_kind: Some("MCIPBlocked".into()),
                            try_instead: Vec::new(),
                        }),
                        McipDecision::NeedsConfirm(reason) => {
                            // V28：实时暂停-继续模型替代旧"伪 fail + grant_and_rerun"
                            //   1. 创建 oneshot channel + nonce
                            //   2. sender 存进 session.mcip_confirm_channels[nonce]
                            //   3. 通过 stream_tx 实时通知 UI（UI 弹窗）
                            //   4. await receiver——挂起 dispatch 等用户决策
                            //   5. true → 走真 execute 路径；false → Denied
                            // C-4 fix: 用单调递增 AtomicU64 替代时间戳 nonce
                            // 防止同批次出现相同 tool_id 时纳秒级码表碰撞导致 confirm channel 泡漏
                            static MCIP_NONCE_COUNTER: std::sync::atomic::AtomicU64 =
                                std::sync::atomic::AtomicU64::new(0);
                            let nonce = format!("{}_{}", tool_id.0,
                                MCIP_NONCE_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
                            let (tx_one, rx_one) = tokio::sync::oneshot::channel::<bool>();
                            {
                                let s = self.session.read().await;
                                s.mcip_confirm_channels.lock_or_recover().insert(nonce.clone(), tx_one);
                            }
                            // V30: bash 命令预览——让用户在弹窗中看到具体命令
                            let preview = serde_json::to_string(&params)
                                .ok()
                                .map(|s| s.chars().take(120).collect::<String>());

                            // 系统+LLM 建议评估：根据工具语义 + reason + schema 计算 suggested_action
                            // 让 TUI 始终弹窗但可以基于建议调整超时行为
                            let suggested_action: Option<bool> = {
                                let tool_lower = tool_id.0.to_lowercase();
                                // 破坏性标记：reason 或工具名含危险关键词
                                let is_destructive = reason.starts_with("[destructive]")
                                    || reason.contains("⚠")
                                    || reason.contains("[bash]")
                                    || ["rm ", "delete", "drop", "destroy", "format", "truncate"]
                                        .iter().any(|k| reason.to_lowercase().contains(k));
                                // 安全模式：工具为只读/查询/幂等
                                let safe_prefixes = ["read", "search", "list", "get", "ls",
                                    "tree", "grep", "info", "query", "find", "fetch", "lookup"];
                                let is_safe_pattern = safe_prefixes.iter().any(|k| {
                                    tool_lower == *k
                                        || tool_lower.ends_with(&format!("_{k}"))
                                        || tool_lower.starts_with(&format!("{k}_"))
                                });
                                // schema.idempotent = true 视为安全
                                let is_idempotent = self.core.registry.get(&tool_id).await
                                    .map(|h| h.schema.idempotent).unwrap_or(false);

                                if is_destructive {
                                    Some(false)  // 系统建议拒绝
                                } else if is_safe_pattern || is_idempotent {
                                    Some(true)   // 系统建议允许（仅低风险工具）
                                } else {
                                    None         // 需用户决策
                                }
                            };

                            let confirm_req = crate::mcip::McipConfirmRequest {
                                tool_id: tool_id.0.clone(),
                                reason: reason.clone(),
                                kind: crate::mcip::McipConfirmKind::McipPolicy,
                                params_preview: preview,
                                nonce: nonce.clone(),
                                suggested_action,
                            };
                            ctx.pending_confirmations.push(confirm_req.clone());
                            // V28：实时推送给 UI（流式路径）
                            if let Some(ref stx) = self.stream_tx {
                                let _ = stx.send(crate::llm::stream::StreamChunk::ConfirmRequired(confirm_req));
                            }
                            self.core.mcip_gateway.log_decision(crate::mcip::McipDecisionRecord {
                                tool_id: tool_id.0.clone(),
                                decision: "needs_confirm".into(),
                                reason: reason.clone(),
                                timestamp_ms: std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH).unwrap_or_default()
                                    .as_millis() as u64,
                            }).await;
                            // BLOCK-6 fix: 等待用户决策时加入 cancel_token 竞争，Esc 可立即中断
                            // 非流式路径（无 stream_tx）维持原有 MCIP 行为（超时自动判定）
                            let is_dangerous = reason.contains("[bash]") && reason.contains("⚠");
                            let timeout_dur = std::time::Duration::from_secs(
                                self.core.config.policy.thresholds.confirm_timeout_secs
                            );
                            let wait_result = if let Some(ref ct) = self.cancel {
                                tokio::select! {
                                    r = tokio::time::timeout(timeout_dur, rx_one) => r.map(|inner| inner.unwrap_or(false)).ok(),
                                    _ = ct.cancelled() => {
                                        tracing::debug!(tool = %tool_id.0, "MCIP confirm cancelled by user");
                                        Some(false)
                                    }
                                }
                            } else {
                                tokio::time::timeout(timeout_dur, rx_one).await
                                    .map(|inner| inner.unwrap_or(false)).ok()
                            };
                            let approved = match wait_result {
                                Some(v) => v,
                                None => {
                                    // 超时：通知 TUI
                                    tracing::warn!(
                                        tool = %tool_id.0,
                                        dangerous = is_dangerous,
                                        "confirm timeout — pipeline-side fallback"
                                    );
                                    if let Some(ref stx) = self.stream_tx {
                                        let _ = stx.send(crate::llm::stream::StreamChunk::AuthResult {
                                            tool: format!("超时自动:{}", &tool_id.0),
                                            approved: !is_dangerous,
                                        });
                                    }
                                    !is_dangerous
                                }
                            };
                            if approved {
                                // 继续性增强：授权后向 TUI 发送 Toast，让用户知道 pipeline 继续执行
                                // 不用假工具 ToolStart（会卡在 Running 状态）
                                if let Some(ref stx) = self.stream_tx {
                                    let _ = stx.send(crate::llm::stream::StreamChunk::AuthResult {
                                        tool: tool_id.0.clone(),
                                        approved: true,
                                    });
                                }
                                // V30: bash 命令级 session grant——同类命令本 session 不再弹窗
                                // 格式 "bash:{cmd} {subcmd}"，如 "bash:git push"
                                if tool_id.0.as_str() == "bash_exec" {
                                    if let Some(cmd_str) = params.get("command").and_then(|v| v.as_str()) {
                                        let prefix: String = cmd_str.split_whitespace()
                                            .take(2).collect::<Vec<_>>().join(" ");
                                        let grant_key = format!("bash:{}", prefix);
                                        let s = self.session.read().await;
                                        s.mcip_grants.write().unwrap_or_else(|p| p.into_inner()).insert(grant_key);
                                    }
                                }
                                // 走真 execute 路径（mag_chain.before → registry.execute → wrap → after）
                                // 使用 async block 捕获错误以确保 ToolEnd 在 Fix 5 outer match 中发送
                                let exec_result: Result<ToolOutput, KernelError> = async {
                                    self.core.mag_chain.read().await.before(&tool_id, &params).await?;
                                    let exec_ctx = {
                                        let s = self.session.read().await;
                                        crate::tool::ExecutionContext {
                                            session_id: s.session_id.clone(),
                                            filengine: s.filengine_session.clone(),
                                            turn_number: ctx.turn_number,
                                            bash_default_timeout: self.core.config.policy.thresholds.bash_default_timeout,
                                            bash_max_timeout: self.core.config.policy.thresholds.bash_max_timeout,
                                            tool_default_timeout: self.core.config.policy.thresholds.tool_default_timeout,
                                            role_caps: Arc::clone(&s.role_caps),
                                        }
                                    };
                                    let mut output = self.core.registry.execute(&tool_id, params.clone(), &exec_ctx).await?;
                                    output = self.core.mcip_gateway.wrap_output(output);
                                    self.core.mag_chain.read().await.after(&tool_id, &mut output).await?;
                                    Ok(output)
                                }.await;
                                exec_result
                            } else {
                                // 破坏性操作被拒绝
                                // 修复：不再强制终止 pipeline，而是返回失败结果 + 让 LLM 知道操作被拒绝
                                // 设计原则：任务连续性 > 强制中断；LLM 应有机会换根安全方案或向用户说明原因
                                let is_destructive_denied = reason.contains("⚠")
                                    || reason.contains("dangerous")
                                    || reason.contains("system-dangerous");
                                if is_destructive_denied {
                                    if let Some(ref stx) = self.stream_tx {
                                        let _ = stx.send(crate::llm::stream::StreamChunk::AuthResult {
                                            tool: tool_id.0.clone(),
                                            approved: false,
                                        });
                                    }
                                    tracing::warn!(
                                        tool = %tool_id.0,
                                        reason = %reason,
                                        "destructive op denied — returning failure to LLM for recovery"
                                    );
                                    // 返回失败 ToolOutput，LLM 会看到拒绝原因并换方案
                                }
                                // 非破坏性拒绝 → 返回错误让 LLM 换方案（不终止）
                                Ok(ToolOutput {
                                    tool_id: tool_id.clone(),
                                    success: false,
                                    output: serde_json::json!({
                                        "error": if is_destructive_denied {
                                            format!("Operation '{}' was DENIED by user as unsafe. Do NOT retry this operation. Explain to the user why this cannot be done safely, or suggest a safer alternative.", tool_id.0)
                                        } else {
                                            "denied".to_string()
                                        },
                                        "do_not_retry": true,
                                    }),
                                    latency_ms: 0,
                                    failure_kind: Some("Authorization".into()),
                                    try_instead: Vec::new(),
                                })
                            }
                        }
                        McipDecision::Allowed => {
                            // 破坏性操作检查：工具 schema confirm_required=true 且未授权
                            let needs_tool_confirm = !is_user_granted && {
                                self.core.registry.get(&tool_id).await
                                    .and_then(|h| h.schema.security)
                                    .map(|s| s.confirm_required)
                                    .unwrap_or(false)
                            };
                            if needs_tool_confirm {
                                // 参数预览：取前 120 字符让用户判断操作内容
                                let preview = serde_json::to_string(&params)
                                    .ok()
                                    .map(|s| s.chars().take(120).collect::<String>());
                                ctx.pending_confirmations.push(crate::mcip::McipConfirmRequest {
                                    tool_id: tool_id.0.clone(),
                                    reason: "This tool performs potentially destructive or irreversible operations.".into(),
                                    kind: crate::mcip::McipConfirmKind::DestructiveOp,
                                    params_preview: preview,
                                    nonce: String::new(), // DestructiveOp 路径暂不走 channel（保留旧 ToolOutput 模式）
                                    suggested_action: Some(false), // 破坏性操作：系统建议拒绝
                                });
                                Ok(ToolOutput {
                                    tool_id: tool_id.clone(),
                                    success: false,
                                    output: serde_json::json!({
                                        "status": "confirmation_required",
                                        "tool": tool_id.0,
                                        "message": format!(
                                            "Tool '{}' is flagged as a potentially destructive operation. \
                                             User confirmation required before execution.",
                                            tool_id.0
                                        )
                                    }),
                                    latency_ms: 0,
                                    failure_kind: Some("DestructiveOp".into()),
                                    try_instead: Vec::new(),
                                })
                            } else {
                                // 正常执行：read lock 足够（before/after 均取 &self）
                                // 使用闭包捕获错误以确保 ToolEnd 在 Fix 5 outer match 中发送
                                let exec_result: Result<ToolOutput, KernelError> = async {
                                    self.core.mag_chain.read().await.before(&tool_id, &params).await?;
                                    // 构建 per-request ExecutionContext（携带当前 session 的 filengine 状态）
                                    // 持有 Arc 克隆，不阻塞 session write lock
                                    let exec_ctx = {
                                        let s = self.session.read().await;
                                        crate::tool::ExecutionContext {
                                            session_id: s.session_id.clone(),
                                            filengine: s.filengine_session.clone(),
                                            turn_number: ctx.turn_number,
                                            bash_default_timeout: self.core.config.policy.thresholds.bash_default_timeout,
                                            bash_max_timeout: self.core.config.policy.thresholds.bash_max_timeout,
                                            tool_default_timeout: self.core.config.policy.thresholds.tool_default_timeout,
                                            role_caps: Arc::clone(&s.role_caps),
                                        }
                                    };
                                    // W2 (Task #100): clone params 因下游 dedup.record 还需要原值
                                    let mut output = self.core.registry.execute(&tool_id, params.clone(), &exec_ctx).await?;
                                    output = self.core.mcip_gateway.wrap_output(output);
                                    self.core.mag_chain.read().await.after(&tool_id, &mut output).await?;
                                    Ok(output)
                                }.await;
                                exec_result
                            }
                        }
                    }
                    } // close V41 safety else block
                };
                // tool_end_sent 标记：应对 Err 路径在内部已发 ToolEnd 的情况，避免行 2249 次发
                let mut tool_end_sent = false;
                let mut output = match output_result {
                    Ok(o) => o,
                    Err(e) => {
                        // Error-recovery: tool dispatch failure → synthesize failed ToolOutput
                        // instead of terminating. LLM sees the error via Tool message and adapts.
                        let fk = "DispatchError".to_string();
                        if let Some(ref stx) = self.stream_tx {
                            let _ = stx.send(crate::llm::stream::StreamChunk::ToolEnd {
                                name: tc.function.name.clone(),
                                success: false,
                                duration_ms: 0,
                                failure_kind: Some(fk.clone()),
                            });
                            // 不发 StreamChunk::Error（会中断 streaming 和设置 Ready）
                            // TUI 通过 ToolBlocked 和 failure_kind 感知错误
                        }
                        tool_end_sent = true;
                        // Synthesize a failed output so the tool loop continues
                        ToolOutput {
                            tool_id: tool_id.clone(),
                            success: false,
                            output: serde_json::json!({
                                "error": format!("Tool dispatch error: {}", sanitize_provider_error(&e)),
                                "recoverable": true
                            }),
                            latency_ms: 0,
                            failure_kind: Some("DispatchError".into()),
                            try_instead: Vec::new(),
                        }
                    }
                };
                // V29.11: ToolOutput — 工具返回内容(让 TUI trace 展开时显示 output)
                if let Some(ref stx) = self.stream_tx {
                    let output_str = serde_json::to_string(&output.output).unwrap_or_default();
                    // 截断保护：>4KB output 不走 channel（避免 buffer 膨胀）；TUI 从 EngineResponse 兜底拿
                    if output_str.len() <= 4096 {
                        let _ = stx.send(crate::llm::stream::StreamChunk::ToolOutput {
                            name: tc.function.name.clone(),
                            output_json: output_str,
                        });
                    }
                }
                // V29.9: per-tool ToolEnd — 与上方 ToolStart 配对(同名), success/duration_ms
                //   反映 dispatch 真实结果. UI run.rs:599 消费此事件按 name 反查 streaming_tools
                //   并更新 trace_events 状态(Running → Success/Failed).
                //   注: 用 tc.function.name(LLM-sanitized) 而非 tool_id.0(resolved real),
                //   保持与 ToolStart 同键, 避免 UI 端反查 mismatch.
                //   tool_end_sent: Err 路径已在行 2211 发送 ToolEnd，此处跳过避免重复
                if !tool_end_sent {
                if let Some(ref stx) = self.stream_tx {
                    let _ = stx.send(crate::llm::stream::StreamChunk::ToolEnd {
                        name: tc.function.name.clone(),
                        success: output.success,
                        duration_ms: output.latency_ms,
                        failure_kind: output.failure_kind.clone(),
                    });
                    // 环境阻塞时发出 ToolBlocked（LLM + 前端双向感知）
                    if let Some(ref fk) = output.failure_kind {
                        if !output.success && fk != "BusinessError" {
                            let _ = stx.send(crate::llm::stream::StreamChunk::ToolBlocked {
                                tool_id: tc.function.name.clone(),
                                kind: fk.clone(),
                                message: format!("Tool '{}' blocked: {}", tc.function.name, fk),
                                recoverable: fk != "Unauthorized" && fk != "DestructiveOp",
                            });
                        }
                    }
                }
                } // if !tool_end_sent
                ctx.all_tool_outputs.push(output.clone());
                // W2 (Task #100)：写入 dedup 池——仅在 idempotent && success && 非缓存命中时
                //
                // 引用：CoreLoop.tool_result_dedup（与上方 dedup_hit 同源）
                // 生命周期：本次 dispatch 真实结果 → warm tier promote
                // 不写入条件：
                //   ① dedup 关闭（None）→ 跳过
                //   ② 已经是缓存命中（dedup_hit.is_some()）→ 不重复刷新（避免 LRU 顺序无谓抖动）
                //   ③ 工具非幂等 → 缓存正确性会破坏
                //   ④ 失败结果 → 失败原因可能临时（cooldown/auth），下一次重试有意义
                if dedup_hit.is_none() {
                    if let (Some(dedup), Some(key)) = (&self.core.tool_result_dedup, dedup_key) {
                        if output.success {
                            // dedup_key 仅在 idempotent 路径生成；此处使用预计算 key
                            // 避免重复 canonicalize+hash（lookup 阶段已算过）
                            dedup.record_with_key(key, &output);
                        }
                    }
                }
                // Task #98 (F): per-tool token telemetry
                //
                // 引用：CacheTelemetry::record_tool_result（src/core/mod.rs）
                // 生命周期：execute_loop 每次工具产出 → CacheTelemetry 累加 → audit_report 消费 top_tools_by_tokens
                //
                // 不区分成功/失败：失败的大返回（如 stack trace、tool error message）一样吃 token，
                // 也是优化目标。bytes/4 ≈ tokens 用作粗估，与 LLM provider 实测的差距由 audit 后续
                // 用 cache_telemetry.total_input_tokens 校准。
                {
                    let bytes = serde_json::to_string(&output.output)
                        .map(|s| s.len())
                        .unwrap_or(0);
                    if bytes > 0 {
                        let s = self.session.read().await;
                        let mut tele = s.cache_telemetry.write().await;
                        tele.record_tool_result(&tool_id, bytes);
                    }
                }
                // Phase γ-I：记录工具调用 turn（仅成功路径，失败不计入新鲜度）
                if output.success {
                    let turn = {
                        let s = self.session.read().await;
                        s.turn_count as u64
                    };
                    self.core.record_tool_invocation(&tool_id, turn).await;
                }
                // Phase γ-E：大结果摘要化——超过阈值则把 output 替换为 truncated summary，
                // 完整原始内容存入 CoreLoop.result_store 让 LLM 通过 result.expand 按需取回。
                //
                // ## 跳过情形
                // result.expand 自身的输出不再二次截断（避免无限循环）；失败结果不截断（给 LLM 完整诊断信息）。
                if tool_id.0 != "result_expand" && output.success {
                    use crate::tool::builtin::result::{
                        compute_result_id, build_truncated_summary, RESULT_TRUNCATE_THRESHOLD,
                    };
                    let serialized = serde_json::to_string(&output.output).unwrap_or_default();
                    // Phase γ-Palace-D：高 expand_rate 工具阈值翻倍
                    //
                    // 查询 palace 看 expanded:{tool} / tool_call:{tool} 比率：
                    // - 比率 > 0.5 → 该工具结果常被 expand → 阈值翻倍（避免反复截断 expand 来回）
                    // - 否则按默认阈值
                    let mut effective_threshold = RESULT_TRUNCATE_THRESHOLD;
                    if let Some(ref palace) = self.core.memory_palace {
                        let p = palace.read().await;
                        let snap = p.behavior.snapshot().await;
                        let calls = snap.get(&format!("tool_call:{}", tool_id.0))
                            .map(|m| m.frequency).unwrap_or(0);
                        let expands = snap.get(&format!("expanded:{}", tool_id.0))
                            .map(|m| m.frequency).unwrap_or(0);
                        if calls >= 3 && expands as f64 / calls as f64 > 0.5 {
                            effective_threshold = RESULT_TRUNCATE_THRESHOLD * 2;
                        }
                    }
                    if serialized.len() > effective_threshold {
                        let session_id = {
                            let s = self.session.read().await;
                            s.session_id.clone()
                        };
                        let result_id = compute_result_id(&session_id, &tool_id.0, &serialized);
                        // 把完整原始 output + source_tool_id 存入 CoreLoop.result_store
                        self.core.result_store.write().await.insert(
                            result_id.clone(),
                            (output.output.clone(), tool_id.0.clone()),
                        );
                        // LLM 看到的 output 替换为摘要
                        output.output = build_truncated_summary(&result_id, &output.output, serialized.len());
                        // Phase Ctx-A：同步 result_store 子系统占用——遍历当前 store 累计 token 字节
                        let store_total: usize = self.core.result_store.read().await
                            .values()
                            .map(|(v, _)| serde_json::to_string(v).map(|s| s.len() / 3).unwrap_or(0))
                            .sum();
                        self.core.context_manager
                            .set_subsystem_usage("result_store", store_total).await;
                    }
                }
                // Phase α-B/H: 把 failure_kind 和 try_instead 注入 content JSON
                //
                // ## 设计
                // ToolOutput.failure_kind / try_instead 是结构化失败元数据。
                // 之前 LLM 只能看到 output.output JSON 文本——靠字符串模式判断失败原因。
                // 现在显式注入：成功时不加；失败时把 failure_kind 拼到 output 顶层，
                // try_instead 非空时也拼，让 LLM 通过 JSON 字段精确判断而非文案匹配。
                let content_json = if output.failure_kind.is_some() || !output.try_instead.is_empty() {
                    let mut wrapped = match output.output.clone() {
                        Value::Object(map) => Value::Object(map),
                        other => serde_json::json!({ "result": other }),
                    };
                    if let Value::Object(ref mut m) = wrapped {
                        if let Some(ref kind) = output.failure_kind {
                            m.insert("failure_kind".into(), Value::String(kind.clone()));
                        }
                        if !output.try_instead.is_empty() {
                            m.insert("try_instead".into(),
                                Value::Array(output.try_instead.iter()
                                    .map(|s| Value::String(s.clone())).collect()));
                        }
                    }
                    serde_json::to_string(&wrapped).unwrap_or_default()
                } else {
                    serde_json::to_string(&output.output).unwrap_or_default()
                };
                tool_results.push(Message {
                    role: MessageRole::Tool,
                    content: Some(MessageContent::Text(content_json)),
                    name: Some(tc.function.name.clone()),
                    tool_calls: None,
                    tool_call_id: Some(tc.id.clone()),
                    reasoning_content: None,
                    prefix: false,
                });
            }
            {
                let s = self.session.read().await;
                let mut msgs = s.messages.write().await;
                msgs.extend(tool_results);
            }

            // ── Mid-turn user signal injection ──────────────────────────────
            // 用户在 LLM 工作期间发送了消息 → 注入让 LLM 感知。
            //
            // ## 2026-05-27 根因修复
            // 旧设计用 role=User 注入，但 assistant(tool_calls) → user → assistant
            // 会在某些 API (OpenAI) 触发 400（tool_calls 必须紧跟 tool responses）。
            // 改为 role=System 注入：
            //   1. System 不参与 tool_calls/tool 配对校验，不破坏协议
            //   2. LLM 仍能感知内容（System 消息优先级高于 User）
            //   3. 明确指令：不中断当前工具序列，完成后再处理
            //
            // ## 死锁预防
            // mid_turn_signals lock 在独立 scope 内获取并 drain，
            // 然后释放后再获取 messages write lock，避免嵌套锁。
            {
                let user_signals: Vec<String> = {
                    let s = self.session.read().await;
                    let mut signals = s.mid_turn_signals.lock().await;
                    signals.drain(..).collect()
                };
                if !user_signals.is_empty() {
                    let combined = user_signals.join("\n");
                    let s = self.session.read().await;
                    let mut msgs = s.messages.write().await;
                    msgs.push(Message {
                        role: MessageRole::System,
                        content: Some(MessageContent::Text(format!(
                            "[Abacus 用户插入] 用户发来新消息：\n{}\n\n\
                             约束：不要中断正在进行的工具调用序列。\
                             完成当前步骤后，在下次文本输出中回应用户消息。\
                             若用户要求停止，完成当前工具调用后立即停止并总结。",
                            combined
                        ))),
                        name: None,
                        tool_calls: None,
                        tool_call_id: None,
                        reasoning_content: None,
                        prefix: false,
                    });
                    // 通知 stream 层（TUI 可据此显示 "用户消息已注入" 事件）
                    if let Some(ref stx) = self.stream_tx {
                        let _ = stx.send(crate::llm::stream::StreamChunk::TextDelta(String::new()));
                    }
                }
            }

            // Fix 3: Warn user when approaching loop limit (80% threshold)
            let limit = self.core.config.thresholds.turn_max_iterations;
            if loop_iter >= (limit * 80 / 100) && loop_iter < limit {
                if let Some(ref stx) = self.stream_tx {
                    let _ = stx.send(crate::llm::stream::StreamChunk::Error(
                        format!("Approaching iteration limit ({}/{})", loop_iter + 1, limit)
                    ));
                }
            }
        }

        Ok(None)
    }

    // ─── Model Escalation sub-phase ─────────────────────────────────────────

    async fn handle_model_escalation(
        &self,
        ctx: &mut TurnContext,
        text: &str,
    ) -> Result<Option<TurnResult>, KernelError> {
        if !text.starts_with("[ESCALATE]") && !text.starts_with("[escalate]") {
            return Ok(None);
        }
        let escalate_to = "deepseek-v4-pro";
        let current_model = &self.core.config.default_model.0;
        if current_model.contains("pro") {
            return Ok(None);
        }

        // Task #96：Budget check —— 防止 cache 振荡
        // 引用关系：SessionState.escalation_count 跨 turn 持续；CoreConfig.max_escalations 阈值
        // 副作用：达到 budget 时丢弃 [ESCALATE] 标签，让 flash 答复直接返回（虽次优但稳定）
        let max_esc = self.core.config.max_escalations;
        let current_count = {
            let s = self.session.read().await;
            s.escalation_count.load(std::sync::atomic::Ordering::Relaxed)
        };
        if current_count >= max_esc {
            tracing::warn!(
                session_escalations = current_count, max = max_esc,
                "escalation budget exhausted, suppressing [ESCALATE] tag and using flash response"
            );
            return Ok(None);
        }

        // Task #97：ROI cost gate —— 升级前评估代价/收益
        //
        // ## 设计动机
        // 升级一次 ≈ 5000+ tokens prefix 在 pro cache 池冷启动。如果 session 短到
        // pro cache 没机会复用（用户问完即走），升级=纯亏。
        //
        // ## 启发式信号
        // - **input_short**：当前输入 < 80 字节 → 大概率是简短问题，flash 足够
        // - **session_warm**：session 已积累 ≥ 4 条消息（含 user/assistant 来回）
        //   → 已建立 cache，且后续 turn 大概率继续，升级 ROI 高
        // - **first_input**：session 仍是首轮（messages.len ≤ 2，仅当前 user + 待响应）
        //
        // ## Gate 触发条件
        // input_short && first_input → 跳过升级
        // 理由：用户首次问且问得短 → 大概率 flash 能答；硬升级无法摊销 cache cost
        //
        // ## 引用关系
        // 仅由 handle_model_escalation 内部使用；budget check 通过后再走此 gate。
        let input_short = self.input.len() < 80;
        let session_msg_count = {
            let s = self.session.read().await;
            let msgs = s.messages.read().await;
            msgs.len()
        };
        let first_input = session_msg_count <= 2;
        if input_short && first_input {
            tracing::info!(
                input_len = self.input.len(),
                session_msgs = session_msg_count,
                "skipping escalation — short input on first turn (no cache to amortize)"
            );
            return Ok(None);
        }

        let flash_analysis = text
            .trim_start_matches("[ESCALATE]")
            .trim_start_matches("[escalate]")
            .trim()
            .to_string();

        let continuation_prompt = if flash_analysis.is_empty() {
            "The previous model determined this request requires deeper reasoning. \
             Please process the user's request with full analytical depth."
                .to_string()
        } else {
            format!(
                "The fast model produced this preliminary analysis:\n\
                 ---\n{}\n---\n\
                 Continue from this analysis: verify its correctness, deepen the reasoning, \
                 and produce the final complete response. Fix any errors in the preliminary analysis.",
                flash_analysis
            )
        };

        // Replace flash's [ESCALATE] message
        {
            let s = self.session.read().await;
            let mut msgs = s.messages.write().await;
            msgs.pop();
            if !flash_analysis.is_empty() {
                msgs.push(Message {
                    role: MessageRole::Assistant,
                    content: Some(MessageContent::Text(
                        format!("[preliminary analysis]\n{}", flash_analysis)
                    )),
                    name: None, tool_calls: None, tool_call_id: None, reasoning_content: None, prefix: false,
                });
            }
            msgs.push(Message {
                role: MessageRole::User,
                content: Some(MessageContent::Text(continuation_prompt)),
                name: None, tool_calls: None, tool_call_id: None, reasoning_content: None, prefix: false,
            });
        }

        // V35-1: escalation 路径同样注入 prefix（保证升级模型也遵守同一格式约束）
        let mut messages = {
            let s = self.session.read().await;
            let msgs = s.messages.read().await;
            msgs.clone()
        };
        maybe_inject_prefix_message(
            &mut messages,
            escalate_to,
            self.req_ctx.prefix_assistant_content.as_deref(),
        );
        let escalated_req = LlmRequest {
            model: ModelId(escalate_to.to_string()),
            messages,
            system: Some(ctx.enriched_system.clone()),
            system_segments: ctx.system_segments.clone(),
            tools: ctx.tool_defs.clone(),
            temperature: Some(self.core.config.default_temperature),
            max_tokens: Some(self.core.config.model_spec.as_ref()
                .map(|s| s.max_output_tokens)
                .unwrap_or(self.core.config.default_max_tokens)),
            top_p: None, stop: Vec::new(), stream: false,
            // L1: thinking_intent 单通道——per-request 覆盖优先，否则 escalation 沿用 sticky 决策
            thinking_intent: self.req_ctx.thinking_intent.clone()
                .or(self.resolve_thinking_config_sticky(ctx.complexity_thinking.clone()).await),
            cache_config: Some(crate::llm::prompt_cache::PromptCacheConfig::default()), // 修复: model escalation 路径开启缓存
            extra_body: Default::default(),
            // Escalation 沿用 ctx.user_message_preamble
            user_message_preamble: ctx.user_message_preamble.clone(),
        };

        // 5-minute hard timeout 安全网（escalation 路径同样受保护）
        let provider_timeout = std::time::Duration::from_secs(self.core.threshold_u64("turn_timeout"));
        let escalation_result = match tokio::time::timeout(provider_timeout, ctx.provider.complete_cancellable(escalated_req, self.cancel_token())).await {
            Ok(result) => result,
            Err(_elapsed) => {
                let secs = provider_timeout.as_secs();
                if let Some(ref stx) = self.stream_tx {
                    let _ = stx.send(crate::llm::stream::StreamChunk::Error(
                        format!("LLM escalation request timed out after {}s", secs)
                    ));
                }
                Err(KernelError::Provider(format!("escalation request timeout: {}s", secs)))
            }
        };
        match escalation_result {
            Ok(escalated_resp) => {
                let escalated_text = super::extract_text(&escalated_resp.message);
                ctx.prompt_tokens += escalated_resp.usage.prompt_tokens;
                ctx.completion_tokens += escalated_resp.usage.completion_tokens;
                ctx.thinking_tokens += escalated_resp.usage.thinking_tokens;
                // Task #95：累积 cache telemetry（含 escalation 路径）
                {
                    let s = self.session.read().await;
                    let mut tele = s.cache_telemetry.write().await;
                    tele.total_input_tokens += escalated_resp.usage.prompt_tokens;
                    tele.total_cached_tokens += escalated_resp.usage.cached_tokens;
                    tele.total_cache_creation_tokens += escalated_resp.usage.cache_creation_tokens;
                    tele.model_switches += 1;  // escalate 视为 1 次 model 切换
                }
                // Task #96：升级成功——累计 count + 锁定 sticky model
                // 引用关系：后续 turn 的 build_request 检查 escalated_model 优先于 default_model
                {
                    let s = self.session.read().await;
                    s.escalation_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    *s.escalated_model.write().await = Some(abacus_types::ModelId(escalate_to.to_string()));
                }
                // 发送 ModelEscalation 事件（前端 + LLM 双向感知）
                if let Some(ref stx) = self.stream_tx {
                    let _ = stx.send(crate::llm::stream::StreamChunk::ModelEscalation {
                        from_model: current_model.to_string(),
                        to_model: escalate_to.to_string(),
                        reason: "LLM self-escalation for deeper reasoning".into(),
                    });
                }
                {
                    let s = self.session.read().await;
                    let mut msgs = s.messages.write().await;
                    msgs.pop();
                    if !flash_analysis.is_empty() { msgs.pop(); }
                    msgs.push(escalated_resp.message);
                }
                ctx.final_response = escalated_text;
            }
            Err(e) => {
                tracing::error!(error = %e, "model escalation failed, falling back to flash analysis");
                // 通知用户 escalation 失败
                if let Some(ref stx) = self.stream_tx {
                    let _ = stx.send(crate::llm::stream::StreamChunk::Error(
                        format!("Model escalation failed: {}. Using preliminary analysis.", e)
                    ));
                }
                {
                    let s = self.session.read().await;
                    let mut msgs = s.messages.write().await;
                    msgs.pop();
                    if !flash_analysis.is_empty() { msgs.pop(); }
                    msgs.push(Message {
                        role: MessageRole::Assistant,
                        content: Some(MessageContent::Text(flash_analysis.clone())),
                        name: None, tool_calls: None, tool_call_id: None, reasoning_content: None, prefix: false,
                    });
                }
                ctx.final_response = if flash_analysis.is_empty() {
                    "(escalation failed, no response available)".to_string()
                } else {
                    flash_analysis
                };
            }
        }
        Ok(None)
    }

    // ─── Progressive Gate result builder ────────────────────────────────────

    async fn build_gated_result(&self, ctx: &TurnContext, text: String) -> TurnResult {
        let progressive_state = {
            let s = self.session.read().await;
            let ctrl = s.progressive.read().await;
            Some(ctrl.current_state().clone())
        };
        let session_id = { let s = self.session.read().await; s.session_id.clone() };
        let mut s = self.session.write().await;
        s.turn_count = ctx.turn_number;
        TurnResult {
            response: text,
            stats: {
                let ctx_window = self.core.context_manager.window.read().await;
                TurnStats {
                    turn_number: ctx.turn_number,
                    tool_calls: ctx.total_tool_calls,
                    provider_id: ctx.provider_id.clone(),
                    model_id: self.core.config.default_model.0.clone(),
                    prompt_tokens: ctx.prompt_tokens,
                    completion_tokens: ctx.completion_tokens,
                    cached_tokens: ctx.cached_tokens,
                    total_tokens: ctx.prompt_tokens + ctx.completion_tokens,
                    thinking_tokens: ctx.thinking_tokens,
                    latency_ms: ctx.start_time.elapsed().as_millis() as u64,
                    skills_matched: ctx.matched_skills.iter().map(|s| s.id.0.clone()).collect(),
                    context_tokens: Some(ctx_window.current_tokens as u64),
                    context_max: Some(ctx_window.max_tokens as u64),
                    model_limit: Some(ctx_window.model_limit as u64),
                }
            },
            tool_outputs: ctx.all_tool_outputs.clone(),
            matched_skills: ctx.matched_skills.clone(),
            session_id,
            progressive_state,
            inertia_warning: None,
            pending_confirmations: Vec::new(),
        }
    }


    // ─── Phase 7: Persist & Build Result ────────────────────────────────────

    async fn persist_and_build_result(self, ctx: TurnContext) -> Result<TurnResult, KernelError> {
        // V43.6: 清除 ephemeral system 消息——它们是一次性内部指令，不应留在 context 中
        // 问题：每轮可能注入 2-4 条 "[Abacus" system 消息（工具名检测/续写/预算提示），
        // 13 轮后累积 80+ 条占大量 token，且对后续 LLM 调用毫无意义。
        // 修复：turn 结束后删除所有 "[Abacus" 前缀的 system 消息。
        {
            let s = self.session.read().await;
            let mut msgs = s.messages.write().await;
            msgs.retain(|m| {
                if m.role != MessageRole::System { return true; }
                match &m.content {
                    Some(MessageContent::Text(t)) => !t.starts_with("[Abacus"),
                    _ => true,
                }
            });
        }

        let latency = ctx.start_time.elapsed().as_millis() as u64;
        let session_id = { let s = self.session.read().await; s.session_id.clone() };

        let snapshot = SessionSnapshot {
            session_id: session_id.clone(),
            turn_count: ctx.turn_number,
            summary: if ctx.final_response.len() > 200 {
                let char_end = ctx.final_response.char_indices()
                    .take(200).last()
                    .map(|(i, c)| i + c.len_utf8())
                    .unwrap_or(0);
                ctx.final_response[..char_end].to_string()
            } else {
                ctx.final_response.clone()
            },
            token_estimate: (ctx.prompt_tokens + ctx.completion_tokens) as usize,
            created_at: chrono::Utc::now().timestamp(),
            key_decisions: Vec::new(),
        };
        // Task #80：双写——hot tier 内存快速访问 + cold tier 持久化
        // 引用关系：hot_snapshots 由 ContextManager.run_tier_migration 周期性老化至 warm/cold
        // 重复成本：snapshot 是摘要级别（summary 截断 200 字符 + 元数据），单份 < 1KB
        self.core.context_manager.record_snapshot(snapshot.clone()).await;
        if let Err(e) = self.core.context_manager.tiers.cold.save(snapshot).await {
            tracing::warn!("failed to persist session snapshot: {}", e);
        }

        let result = TurnResult {
            response: ctx.final_response,
            stats: {
                let ctx_window = self.core.context_manager.window.read().await;
                TurnStats {
                    turn_number: ctx.turn_number,
                    tool_calls: ctx.total_tool_calls,
                    provider_id: ctx.provider_id,
                    model_id: self.core.config.default_model.0.clone(),
                    prompt_tokens: ctx.prompt_tokens,
                    completion_tokens: ctx.completion_tokens,
                    cached_tokens: ctx.cached_tokens,
                    total_tokens: ctx.prompt_tokens + ctx.completion_tokens,
                    thinking_tokens: ctx.thinking_tokens,
                    latency_ms: latency,
                    skills_matched: ctx.matched_skills.iter().map(|s| s.id.0.clone()).collect(),
                    context_tokens: Some(ctx_window.current_tokens as u64),
                    context_max: Some(ctx_window.max_tokens as u64),
                    model_limit: Some(ctx_window.model_limit as u64),
                }
            },
            tool_outputs: ctx.all_tool_outputs,
            matched_skills: ctx.matched_skills,
            session_id,
            progressive_state: {
                let s = self.session.read().await;
                let ctrl = s.progressive.read().await;
                if ctrl.is_passthrough() { None } else { Some(ctrl.current_state().clone()) }
            },
            inertia_warning: ctx.inertia_warning,
            pending_confirmations: ctx.pending_confirmations,
        };

        // Hook: TurnEnd（持久化后触发，result 已构建）
        let _ = self.core.emit_pipeline_event(crate::mag_chain::PipelineEvent::TurnEnd {
            response_len: result.response.len(),
            tool_calls: result.stats.tool_calls as usize,
            latency_ms: result.stats.latency_ms,
            completion_tokens: result.stats.completion_tokens,
        }).await;

        Ok(result)
    }

}

// ─── Complexity → LLM parameter 映射函数 ────────────────────────────────────
// 引用：setup() 调用 → TurnContext 存储 → execute_loop() 消费
// 生命周期：每次 turn 独立计算，不跨 turn 缓存

/// Map ComplexityProfile → ThinkingIntent（fallback 层）。
///
/// ## 优先级语义（L1 后）
/// 输出作为 fallback：`config.thinking_intent` 显式设置时不会运行到。
/// 调用方：`execute_loop` 通过 `.or_else(|| ctx.complexity_thinking.clone())` 使用。
///
/// ## 阈值设计
/// - score < 0.25 + 无决策 + 低精度要求 → None（不开 thinking，省 token）
/// - precision_requirement > 0.6 OR score > 0.85 → Effort(High)
/// - score > 0.50 OR has_decisions → Effort(Medium)
/// - 其余 → Effort(Low)（最小 CoT 开销）
fn map_complexity_to_thinking(
    p: &abacus_types::progressive::ComplexityProfile,
) -> Option<abacus_types::ThinkingIntent> {
    use abacus_types::{EffortLevel, ThinkingIntent};
    if p.score < 0.25 && !p.has_decisions && p.dimensions.precision_requirement < 0.3 {
        return None;
    }
    let level = if p.dimensions.precision_requirement > 0.6 || p.score > 0.85 {
        EffortLevel::High
    } else if p.score > 0.50 || p.has_decisions {
        EffortLevel::Medium
    } else {
        EffortLevel::Low
    };
    Some(ThinkingIntent::Effort(level))
}

/// Map TaskKind + ComplexityProfile → temperature。
///
/// ## 优先级语义
/// 输出作为中间层：`req_ctx.temperature` 显式覆盖时优先，`config.default_temperature` 最低。
/// 调用方：`execute_loop` 通过 `.or(ctx.complexity_temperature)` 使用。
///
/// ## 精确域 vs 发散域
/// - 精确域（数学/数据/调试）：低温，确定性优先
/// - 发散域（通用聊天）：高温，允许多样性
/// - 高复杂度（score > 0.80）在精确域额外降温 0.10
fn task_temperature(
    kind: &crate::core::task_analyzer::TaskKind,
    complexity: &abacus_types::progressive::ComplexityProfile,
) -> f64 {
    use crate::core::task_analyzer::TaskKind;
    let base: f64 = match kind {
        TaskKind::Mathematics | TaskKind::DataAnalysis    => 0.20,
        TaskKind::Debugging                               => 0.35,
        TaskKind::CodeWriting | TaskKind::FileEdit        => 0.45,
        TaskKind::CodeReading | TaskKind::Review          => 0.50,
        TaskKind::Architecture | TaskKind::KnowledgeQuery => 0.65,
        TaskKind::Linguistics                             => 0.70,
        TaskKind::GeneralChat | TaskKind::WebSearch       => 0.85,
    };
    // 高复杂度精确域：额外降温，确保推理一致性
    if complexity.score > 0.80 && base < 0.55 {
        (base - 0.10).max(0.15)
    } else {
        base
    }
}

// ─── Tests ────────────────────────────────────────────────────────
//
// ## 测试范围（P0-2）
// 仅覆盖**无外部依赖的纯逻辑**：
// - `map_complexity_to_thinking`：4 阈值分支 + None 路径
// - `task_temperature`：8 种 TaskKind × 高复杂度降温阈值
// - `TurnPipeline::sanitize_dangling_tool_calls`：4 种 message 历史场景
//
// ## 不在本期范围
// `setup` / `execute_loop` / `handle_model_escalation` / `persist_and_build_result`
// 依赖 CoreLoop + LlmProvider + 数据库，需要专项 mock 框架，留给后续。

// ─── 错误信息净化 ────────────────────────────────────────────────────────────
//
// 目的：provider 错误注入 LLM history 前移除 URL 和潜在凭证
// 引用关系：execute_loop 各错误注入点调用
// 生命周期：无状态，每次调用独立执行（LazyLock regex 编译一次）

/// provider/dispatch 错误信息中可能含 URL 端点
static SANITIZE_URL_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"https?://\S+").expect("valid regex")
});

/// 常见 auth 凭证模式（Bearer token, api-key, Authorization header）
static SANITIZE_AUTH_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r"(?i)(bearer\s+\S+|api[-_]?key\s*[:=]\s*\S+|token\s*[:=]\s*\S+|authorization\s*:\s*\S+)"
    ).expect("valid regex")
});

/// provider/dispatch 错误信息净化：移除 URL、凭证，截断到 200 字符。
///
/// 防止 provider API 错误体（含端点 URL、request token 等）泄漏至 LLM context history。
/// 纯函数，无副作用。
fn sanitize_provider_error(raw: impl std::fmt::Display) -> String {
    let s = raw.to_string();
    let no_url = SANITIZE_URL_RE.replace_all(&s, "[url]");
    let no_auth = SANITIZE_AUTH_RE.replace_all(&no_url, "[redacted]");
    no_auth.chars().take(200).collect()
}

/// V41: 结构化摘要工具输出——保持 JSON 合法性，按工具类型提取关键信息
///
/// ## 设计意图
/// 替代盲目字符截断（会产生非法 JSON）。按工具类型语义提取：
/// - fs_read: 行数 + 首尾几行
/// - grep: 匹配数 + 首 3 条结果
/// - db_query: 行数 + 列名 + 首行
/// - 其他: 按 JSON 结构安全截断（保留顶层 key）
///
/// ## 引用关系
/// - 调用方: ToolAgent batch summarize=true 路径
/// - 设计: 输出永远是合法 JSON 字符串（LLM 可解析）
fn summarize_tool_output(tool_name: &str, output: &serde_json::Value, success: bool) -> String {
    if !success {
        // 失败输出通常很短，直接返回
        return serde_json::to_string(output).unwrap_or_else(|_| "{}".into());
    }

    let full = serde_json::to_string(output).unwrap_or_default();

    // 短输出直接返回（<600 bytes 不需要摘要）
    if full.len() <= 600 {
        return full;
    }

    // 按工具类型提取结构化摘要
    match tool_name {
        // 文件读取：提取行数 + 首尾行
        name if name.contains("read") || name == "fs_read" => {
            if let Some(content) = output.as_str()
                .or_else(|| output.get("content").and_then(|v| v.as_str()))
            {
                let lines: Vec<&str> = content.lines().collect();
                let total = lines.len();
                let head: Vec<&str> = lines.iter().take(5).copied().collect();
                let tail: Vec<&str> = if total > 8 { lines.iter().rev().take(3).copied().collect() } else { vec![] };
                return serde_json::json!({
                    "_summarized": true,
                    "total_lines": total,
                    "head": head.join("\n"),
                    "tail": tail.into_iter().rev().collect::<Vec<_>>().join("\n"),
                    "total_chars": content.len(),
                }).to_string();
            }
        }
        // 搜索/grep：提取匹配数 + 首几条
        name if name.contains("grep") || name.contains("search") => {
            if let Some(arr) = output.as_array() {
                let total = arr.len();
                let preview: Vec<&serde_json::Value> = arr.iter().take(3).collect();
                return serde_json::json!({
                    "_summarized": true,
                    "total_matches": total,
                    "preview": preview,
                }).to_string();
            }
            if let Some(matches) = output.get("matches").and_then(|v| v.as_array()) {
                let total = matches.len();
                let preview: Vec<&serde_json::Value> = matches.iter().take(3).collect();
                return serde_json::json!({
                    "_summarized": true,
                    "total_matches": total,
                    "preview": preview,
                }).to_string();
            }
        }
        // DB 查询：行数 + 列名 + 首行
        name if name.contains("db_query") || name.contains("db_read") => {
            if let Some(rows) = output.as_array() {
                let total = rows.len();
                let columns: Vec<String> = rows.first()
                    .and_then(|r| r.as_object())
                    .map(|obj| obj.keys().cloned().collect())
                    .unwrap_or_default();
                return serde_json::json!({
                    "_summarized": true,
                    "total_rows": total,
                    "columns": columns,
                    "first_row": rows.first(),
                }).to_string();
            }
        }
        _ => {}
    }

    // 通用回退：保留顶层 JSON 结构，截断长 value
    if let Some(obj) = output.as_object() {
        let mut summary = serde_json::Map::new();
        summary.insert("_summarized".into(), serde_json::json!(true));
        summary.insert("_keys".into(), serde_json::json!(obj.keys().collect::<Vec<_>>()));
        for (k, v) in obj.iter().take(5) {
            let v_str = v.to_string();
            if v_str.len() > 100 {
                summary.insert(k.clone(), serde_json::json!(format!("{}...[{}chars]", &v_str[..80], v_str.len())));
            } else {
                summary.insert(k.clone(), v.clone());
            }
        }
        return serde_json::Value::Object(summary).to_string();
    }

    // 最终回退：安全截断（按换行符切割，保证不破坏 JSON 结构）
    let lines: Vec<&str> = full.lines().take(10).collect();
    format!("{}\n...[truncated from {} bytes]", lines.join("\n"), full.len())
}

/// 判断是否为网络层错误（连接/DNS/TLS/超时），区别于 API 业务错误（401/429/400）
///
/// 网络错误：需要用户检查连接；API 错误：请求格式或认证问题
fn is_network_error(e: &str) -> bool {
    let lower = e.to_lowercase();
    lower.contains("error sending request")
        || lower.contains("connection refused")
        || lower.contains("connection reset")
        || lower.contains("connection timed out")
        || lower.contains("failed to connect")
        || lower.contains("dns error")
        || lower.contains("dns resolution")
        || lower.contains("no such host")
        || lower.contains("network unreachable")
        || lower.contains("tls error")
        || lower.contains("ssl error")
        || lower.contains("timed out")
        || lower.contains("timeout")
}

#[cfg(test)]
mod tests {
    use super::*;
    use abacus_types::progressive::{ComplexityDimensions, ComplexityProfile};
    use abacus_types::{EffortLevel, ThinkingIntent};
    use crate::core::task_analyzer::TaskKind;
    use crate::llm::{Message, MessageContent, MessageRole, ToolCall, ToolFunction};

    /// 构造 ComplexityProfile（仅设关键字段，其余清零）
    fn cp(score: f64, precision: f64, has_decisions: bool) -> ComplexityProfile {
        ComplexityProfile {
            score,
            dimensions: ComplexityDimensions {
                input_length: 0.0,
                structural: 0.0,
                domain_crossing: 0.0,
                decision_density: 0.0,
                output_scale: 0.0,
                external_dependency: 0.0,
                precision_requirement: precision,
            },
            estimated_output_chars: 0,
            has_decisions,
            needs_external_info: false,
            domain_count: 0,
            assessment_confidence: 0.0,
        }
    }

    // ─── map_complexity_to_thinking 阈值矩阵 ─────────────

    #[test]
    fn complexity_thinking_returns_none_for_trivial() {
        // score < 0.25 + no_decisions + precision < 0.3 → None（省 token）
        let p = cp(0.10, 0.10, false);
        assert!(map_complexity_to_thinking(&p).is_none());
    }

    #[test]
    fn complexity_thinking_high_when_precision_dominant() {
        // precision > 0.6 → High（即使 score 低）
        let p = cp(0.10, 0.7, false);
        assert_eq!(
            map_complexity_to_thinking(&p),
            Some(ThinkingIntent::Effort(EffortLevel::High))
        );
    }

    #[test]
    fn complexity_thinking_high_when_score_dominant() {
        // score > 0.85 → High（即使 precision 低）
        let p = cp(0.90, 0.10, false);
        assert_eq!(
            map_complexity_to_thinking(&p),
            Some(ThinkingIntent::Effort(EffortLevel::High))
        );
    }

    #[test]
    fn complexity_thinking_medium_for_decisions_or_mid_score() {
        // has_decisions → Medium（score 低也不是 None）
        let p = cp(0.10, 0.10, true);
        assert_eq!(
            map_complexity_to_thinking(&p),
            Some(ThinkingIntent::Effort(EffortLevel::Medium))
        );
        // score > 0.50 → Medium
        let p = cp(0.60, 0.10, false);
        assert_eq!(
            map_complexity_to_thinking(&p),
            Some(ThinkingIntent::Effort(EffortLevel::Medium))
        );
    }

    #[test]
    fn complexity_thinking_low_fallback() {
        // 不命中 None / High / Medium → Low（最小 CoT 开销）
        // score=0.30, precision=0.40, no_decisions
        // score < 0.25? no（0.30）→ 不 None
        // precision > 0.6? no | score > 0.85? no → 不 High
        // score > 0.50? no | has_decisions? no → 不 Medium
        // → Low
        let p = cp(0.30, 0.40, false);
        assert_eq!(
            map_complexity_to_thinking(&p),
            Some(ThinkingIntent::Effort(EffortLevel::Low))
        );
    }

    #[test]
    fn complexity_thinking_boundary_score_0_25() {
        // score == 0.25：不 < 0.25，不进 None 分支（即使其他条件都满足 None）
        // 落到 fallback Low
        let p = cp(0.25, 0.10, false);
        assert_eq!(
            map_complexity_to_thinking(&p),
            Some(ThinkingIntent::Effort(EffortLevel::Low))
        );
    }

    // ─── task_temperature 矩阵 ─────────────────────────

    #[test]
    fn temperature_base_per_kind_low_complexity() {
        let low = cp(0.20, 0.0, false); // score < 0.80，不触发降温
        // 精确域
        assert!((task_temperature(&TaskKind::Mathematics, &low) - 0.20).abs() < 1e-9);
        assert!((task_temperature(&TaskKind::DataAnalysis, &low) - 0.20).abs() < 1e-9);
        assert!((task_temperature(&TaskKind::Debugging, &low) - 0.35).abs() < 1e-9);
        assert!((task_temperature(&TaskKind::CodeWriting, &low) - 0.45).abs() < 1e-9);
        assert!((task_temperature(&TaskKind::FileEdit, &low) - 0.45).abs() < 1e-9);
        assert!((task_temperature(&TaskKind::CodeReading, &low) - 0.50).abs() < 1e-9);
        assert!((task_temperature(&TaskKind::Review, &low) - 0.50).abs() < 1e-9);
        // 发散域
        assert!((task_temperature(&TaskKind::Architecture, &low) - 0.65).abs() < 1e-9);
        assert!((task_temperature(&TaskKind::KnowledgeQuery, &low) - 0.65).abs() < 1e-9);
        assert!((task_temperature(&TaskKind::Linguistics, &low) - 0.70).abs() < 1e-9);
        assert!((task_temperature(&TaskKind::GeneralChat, &low) - 0.85).abs() < 1e-9);
        assert!((task_temperature(&TaskKind::WebSearch, &low) - 0.85).abs() < 1e-9);
    }

    #[test]
    fn temperature_high_complexity_cools_precision_domains() {
        let high = cp(0.90, 0.0, false); // score > 0.80
        // base < 0.55 的精确域 → base - 0.10（下限 0.15）
        assert!((task_temperature(&TaskKind::Mathematics, &high) - 0.15).abs() < 1e-9); // 0.20-0.10=0.10 → max 0.15
        assert!((task_temperature(&TaskKind::Debugging, &high) - 0.25).abs() < 1e-9); // 0.35-0.10=0.25
        assert!((task_temperature(&TaskKind::CodeWriting, &high) - 0.35).abs() < 1e-9); // 0.45-0.10=0.35
        assert!((task_temperature(&TaskKind::CodeReading, &high) - 0.40).abs() < 1e-9); // 0.50-0.10=0.40

        // base >= 0.55 的发散域 → 不降温
        assert!((task_temperature(&TaskKind::Architecture, &high) - 0.65).abs() < 1e-9);
        assert!((task_temperature(&TaskKind::GeneralChat, &high) - 0.85).abs() < 1e-9);
    }

    #[test]
    fn temperature_boundary_score_0_80() {
        // score == 0.80：不 > 0.80，不触发降温
        let edge = cp(0.80, 0.0, false);
        assert!((task_temperature(&TaskKind::Mathematics, &edge) - 0.20).abs() < 1e-9);
    }

    // ─── sanitize_dangling_tool_calls 场景 ─────────────────

    fn user_msg(text: &str) -> Message {
        Message {
            role: MessageRole::User,
            content: Some(MessageContent::Text(text.into())),
            name: None,
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            prefix: false,
        }
    }

    fn assistant_with_tool_calls(call_ids: &[&str]) -> Message {
        let calls: Vec<ToolCall> = call_ids.iter().map(|id| ToolCall {
            id: (*id).into(),
            type_: "function".into(),
            function: ToolFunction { name: "x".into(), arguments: "{}".into() },
        }).collect();
        Message {
            role: MessageRole::Assistant,
            content: None,
            name: None,
            tool_calls: Some(calls),
            tool_call_id: None,
            reasoning_content: None,
            prefix: false,
        }
    }

    fn tool_response(id: &str) -> Message {
        Message {
            role: MessageRole::Tool,
            content: Some(MessageContent::Text("ok".into())),
            name: None,
            tool_calls: None,
            tool_call_id: Some(id.into()),
            reasoning_content: None,
            prefix: false,
        }
    }

    #[test]
    fn sanitize_handles_empty_history() {
        let mut msgs: Vec<Message> = Vec::new();
        TurnPipeline::<'static>::sanitize_dangling_tool_calls(&mut msgs);
        assert!(msgs.is_empty());
    }

    #[test]
    fn sanitize_merges_consecutive_user_messages() {
        let mut msgs = vec![user_msg("hello"), user_msg("world")];
        TurnPipeline::<'static>::sanitize_dangling_tool_calls(&mut msgs);
        assert_eq!(msgs.len(), 1, "consecutive user messages merged");
        match &msgs[0].content {
            Some(MessageContent::Text(t)) => assert!(t.contains("hello") && t.contains("world")),
            _ => panic!("merged content should be Text"),
        }
    }

    #[test]
    fn sanitize_keeps_complete_tool_call_response_pair() {
        // assistant 发了 t1+t2，后续 tool 消息也都到位 → 不删
        let mut msgs = vec![
            user_msg("query"),
            assistant_with_tool_calls(&["t1", "t2"]),
            tool_response("t1"),
            tool_response("t2"),
        ];
        let before = msgs.len();
        TurnPipeline::<'static>::sanitize_dangling_tool_calls(&mut msgs);
        assert_eq!(msgs.len(), before, "complete pair → preserved");
        assert!(matches!(msgs[1].role, MessageRole::Assistant));
    }

    #[test]
    fn sanitize_removes_dangling_assistant_and_orphan_response() {
        // assistant 发了 t1+t2，但只有 t1 的 response → assistant 被移除
        // → t1 response 现在也是孤儿（其 tool_call_id 不再匹配任何 assistant）→ 也被移除
        let mut msgs = vec![
            user_msg("query"),
            assistant_with_tool_calls(&["t1", "t2"]),
            tool_response("t1"),
            // t2 response 缺失
        ];
        TurnPipeline::<'static>::sanitize_dangling_tool_calls(&mut msgs);
        assert_eq!(msgs.len(), 1, "dangling assistant + orphan tool response both removed");
        assert!(matches!(msgs[0].role, MessageRole::User));
    }

    #[test]
    fn sanitize_only_removes_last_dangling_and_its_orphans() {
        // 历史中有早期完整对、后期 dangling：
        // 移除 dangling assistant (b1+b2) → b1 response 变孤儿也被移除
        let mut msgs = vec![
            user_msg("first query"),
            assistant_with_tool_calls(&["a1"]),
            tool_response("a1"),
            user_msg("second query"),
            assistant_with_tool_calls(&["b1", "b2"]),
            tool_response("b1"),
            // b2 缺失 → 此 assistant 应被移除
        ];
        TurnPipeline::<'static>::sanitize_dangling_tool_calls(&mut msgs);
        // 早期 a1 对完整保留；b1+b2 的 assistant + b1 orphan 被移除
        assert_eq!(msgs.len(), 4);
        assert!(matches!(msgs[0].role, MessageRole::User));
        assert!(matches!(msgs[1].role, MessageRole::Assistant));
        assert!(matches!(msgs[2].role, MessageRole::Tool));
        assert_eq!(msgs[2].tool_call_id.as_deref(), Some("a1"));
        assert!(matches!(msgs[3].role, MessageRole::User));
    }

    #[test]
    fn sanitize_removes_pure_orphan_tool_response() {
        // tool response 前面完全没有 assistant tool_calls（proactive injection 场景）
        let mut msgs = vec![
            user_msg("hello"),
            tool_response("call_injected_123"),
        ];
        TurnPipeline::<'static>::sanitize_dangling_tool_calls(&mut msgs);
        assert_eq!(msgs.len(), 1, "orphan tool response removed");
        assert!(matches!(msgs[0].role, MessageRole::User));
    }
}
