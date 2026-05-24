use std::collections::HashMap;
use std::sync::Arc;

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
        let session = self.session.read().await;
        let mut decision_guard = session.thinking_decision.write().await;
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
        let session = self.session.read().await;
        let guard = session.thinking_decision.read().await;
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
        let cg_model = self.core.config.default_model.clone();
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
            max_tokens: Some(self.core.config.default_max_tokens),
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

        let response = provider.complete_cancellable(req, self.cancel_token()).await?;
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
            stats: TurnStats {
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
        if self.core.context_manager.take_shed_pending() {
            if let Some(ref stx) = self.stream_tx {
                let _ = stx.send(crate::llm::stream::StreamChunk::CompressStart);
            }
            let s = self.session.read().await;
            let mut msgs = s.messages.write().await;
            let compressed = self.core.context_manager.auto_compress_messages(&mut msgs).await;
            if !compressed.is_empty() {
                tracing::info!("pressure shed: compressed {} messages", compressed.len());
            }
            if let Some(ref stx) = self.stream_tx {
                let tokens_saved: usize = compressed.iter()
                    .map(|c| c.original_tokens.saturating_sub(c.compressed_tokens))
                    .sum();
                let _ = stx.send(crate::llm::stream::StreamChunk::CompressEnd {
                    messages_compressed: compressed.len(),
                    tokens_saved,
                });
            }
        }

        // Copy session messages to session-level context_messages
        {
            let s = self.session.read().await;
            let msgs = s.messages.read().await;
            *s.context_messages.write().await = msgs.clone();
        }

        let turn_number = { let s = self.session.read().await; s.turn_count + 1 };

        if turn_number == 1 {
            if let Some(ref spec) = self.core.config.model_spec {
                self.core.context_manager.set_window(spec.context_window, spec.context_window);
            }
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
            let rule_report = PreflightChecker::check(self.input, &classification.kind, Some(&complexity));
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

        // Progressive prompt injection（text + segments 同步）
        {
            let s = self.session.read().await;
            let ctrl = s.progressive.read().await;
            if let Some((_priority, prompt_text)) = build_progressive_prompt(
                ctrl.current_state(), ctrl.current_strategy(),
            ) {
                sys_out.push_dynamic(&prompt_text);
            }
        }

        // Model Self-Escalation prompt（flash models only，text + segments 同步）
        if self.core.config.default_model.0.contains("flash") {
            sys_out.push_dynamic("[Model Routing]\n\
                If this request requires deep multi-step reasoning, complex architecture analysis, \
                security auditing, or tasks where accuracy is critical over speed:\n\
                1. Output `[ESCALATE]` as your FIRST line\n\
                2. Then provide your preliminary analysis (key observations, identified structure, initial conclusions)\n\
                3. A stronger model will continue from your analysis, verify it, and produce the final response\n\
                For all other requests, proceed normally without [ESCALATE].");
        }

        // ─── EpistemicGuard 累积违规声明注入（text + segments 同步）─────────────
        // 与 post_process EpistemicPostCheck 形成双层防护：
        //   PostCheck  — 事后检测，修改输出（per-turn）
        //   Declaration — 预防注入，约束生成（cumulative session 级）
        if let Some(declaration) = self.core.epistemic_guard.declaration_if_needed().await {
            sys_out.push_dynamic(&declaration);
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

        let enriched_system = sys_out.text;
        let dynamic_blocks = sys_out.segments.len().saturating_sub(1); // 稳定段之外的动态块数
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
            complexity_thinking,
            complexity_temperature,
            user_message_preamble, // Phase 4：setup() 阶段已构建（ICL 检索结果）
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
        let last_assistant_with_tools = msgs.iter().rposition(|m| {
            m.role == MessageRole::Assistant && m.tool_calls.as_ref().is_some_and(|tc| !tc.is_empty())
        });
        if let Some(idx) = last_assistant_with_tools {
            let tool_calls = msgs[idx].tool_calls.as_ref().unwrap();
            let expected_ids: std::collections::HashSet<&str> = tool_calls.iter()
                .map(|tc| tc.id.as_str())
                .collect();
            let found_ids: std::collections::HashSet<&str> = msgs[idx + 1..].iter()
                .filter(|m| m.role == MessageRole::Tool)
                .filter_map(|m| m.tool_call_id.as_deref())
                .collect();
            if !expected_ids.is_subset(&found_ids) {
                tracing::warn!(
                    idx,
                    missing = ?(expected_ids.difference(&found_ids).collect::<Vec<_>>()),
                    "removing dangling tool_calls message from history"
                );
                msgs.remove(idx);
            }
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
        const MAX_TOTAL_TOOL_CALLS: u32 = 200;

        for loop_iter in 0..self.core.config.max_turns_per_request {
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
            let escalated = self.session.read().await.escalated_model.read().await.clone();
            let effective_model = self.req_ctx.model.clone()
                .or(escalated)
                .unwrap_or_else(|| self.core.config.default_model.clone());
            let effective_temperature = self.req_ctx.temperature
                .or(ctx.complexity_temperature)
                .unwrap_or(self.core.config.default_temperature);
            let effective_max_tokens = self.req_ctx.max_tokens
                .unwrap_or(self.core.config.default_max_tokens);

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
                tools: ctx.tool_defs.clone(),
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
                while let Some(event) = event_rx.recv().await {
                    match event {
                        crate::llm::stream::StreamEvent::TextDelta(t) => {
                            let _ = stx.send(crate::llm::stream::StreamChunk::TextDelta(t));
                        }
                        crate::llm::stream::StreamEvent::ThinkingDelta(t) => {
                            let _ = stx.send(crate::llm::stream::StreamChunk::Thinking(t));
                        }
                        crate::llm::stream::StreamEvent::ToolCallStart { name, .. } => {
                            let _ = stx.send(crate::llm::stream::StreamChunk::ToolStart { name });
                        }
                        crate::llm::stream::StreamEvent::Done => break,
                        _ => {} // ArgDelta/Usage handled via final response
                    }
                }

                handle.await
                    .map_err(|e| KernelError::Other(format!("stream task panicked: {e}")))?
            } else {
                // Blocking path: 有 tools 或未启用 streaming
                ctx.provider.complete_cancellable(req.clone(), self.cancel_token()).await
            };

            // ── 400 Auto-Repair: 检测到 400 时尝试修复消息序列并重试一次 ──
            let response = match provider_result {
                Ok(resp) => resp,
                Err(KernelError::ApiError { status: 400, ref body }) => {
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
                        ctx.provider.complete_cancellable(retry_req, self.cancel_token()).await?
                    } else {
                        return Err(KernelError::ApiError { status: 400, body: body.clone() });
                    }
                }
                Err(e) => return Err(e),
            };

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

            {
                let s = self.session.read().await;
                let mut msgs = s.messages.write().await;
                msgs.push(response.message.clone());
            }

            if tool_calls.is_empty() {
                let text = super::extract_text(&response.message);

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

            if ctx.total_tool_calls + tool_calls.len() as u32 > MAX_TOTAL_TOOL_CALLS {
                ctx.final_response = "(max total tool calls reached)".to_string();
                break;
            }
            if tool_calls.len() > self.core.config.max_tool_calls_per_turn as usize {
                return Err(KernelError::Other(format!(
                    "tool calls exceed limit: {} > {}",
                    tool_calls.len(), self.core.config.max_tool_calls_per_turn
                )));
            }

            ctx.total_tool_calls += tool_calls.len() as u32;
            self.core.safety_guard.check_tool_call_count(ctx.total_tool_calls)
                .map_err(|e| KernelError::Other(e.to_string()))?;

            let user_role = { let s = self.session.read().await; s.user_role };
            let mut tool_results: Vec<Message> = Vec::new();
            for tc in &tool_calls {
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

                let mut output = if let Some(cached) = dedup_hit.clone() {
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
                    // 授权检查：优先于session 永久授权和本 turn 单次授权，匹配则跳过 MCIP 策略
                    let is_user_granted = {
                        let s = self.session.read().await;
                        let grants = s.mcip_grants.read().unwrap();
                        grants.contains(tool_id.0.as_str())
                    } || self.req_ctx.mcip_once_grants.contains(tool_id.0.as_str());

                    let decision = if is_user_granted {
                        McipDecision::Allowed
                    } else {
                        self.core.mcip_gateway.check(&tool_id, &params, user_role)
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
                            let nonce = format!("{}_{}", tool_id.0,
                                std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH).unwrap_or_default()
                                    .as_nanos());
                            let (tx_one, rx_one) = tokio::sync::oneshot::channel::<bool>();
                            {
                                let s = self.session.read().await;
                                s.mcip_confirm_channels.lock().unwrap().insert(nonce.clone(), tx_one);
                            }
                            let confirm_req = crate::mcip::McipConfirmRequest {
                                tool_id: tool_id.0.clone(),
                                reason: reason.clone(),
                                kind: crate::mcip::McipConfirmKind::McipPolicy,
                                params_preview: None,
                                nonce: nonce.clone(),
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
                            // 等待用户决策（drop sender = false / 显式 send true|false）
                            let approved = rx_one.await.unwrap_or(false);
                            if approved {
                                // 走真 execute 路径（mag_chain.before → registry.execute → wrap → after）
                                self.core.mag_chain.read().await.before(&tool_id, &params).await?;
                                let exec_ctx = {
                                    let s = self.session.read().await;
                                    crate::tool::ExecutionContext {
                                        session_id: s.session_id.clone(),
                                        filengine: s.filengine_session.clone(),
                                        turn_number: ctx.turn_number,
                                    }
                                };
                                let mut output = self.core.registry.execute(&tool_id, params.clone(), &exec_ctx).await?;
                                output = self.core.mcip_gateway.wrap_output(output);
                                self.core.mag_chain.read().await.after(&tool_id, &mut output).await?;
                                Ok(output)
                            } else {
                                Ok(ToolOutput {
                                    tool_id: tool_id.clone(),
                                    success: false,
                                    output: serde_json::json!({
                                        "error": "User denied authorization",
                                        "tool": tool_id.0,
                                        "reason": reason,
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
                                self.core.mag_chain.read().await.before(&tool_id, &params).await?;
                                // 构建 per-request ExecutionContext（携带当前 session 的 filengine 状态）
                                // 持有 Arc 克隆，不阻塞 session write lock
                                let exec_ctx = {
                                    let s = self.session.read().await;
                                    crate::tool::ExecutionContext {
                                        session_id: s.session_id.clone(),
                                        filengine: s.filengine_session.clone(),
                                        turn_number: ctx.turn_number,
                                    }
                                };
                                // W2 (Task #100): clone params 因下游 dedup.record 还需要原值
                                let mut output = self.core.registry.execute(&tool_id, params.clone(), &exec_ctx).await?;
                                output = self.core.mcip_gateway.wrap_output(output);
                                self.core.mag_chain.read().await.after(&tool_id, &mut output).await?;
                                Ok(output)
                            }
                        }
                    }
                }?;
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
                if let Some(ref stx) = self.stream_tx {
                    let _ = stx.send(crate::llm::stream::StreamChunk::ToolEnd {
                        name: tc.function.name.clone(),
                        success: output.success,
                        duration_ms: output.latency_ms,
                    });
                }
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
            max_tokens: Some(self.core.config.default_max_tokens),
            top_p: None, stop: Vec::new(), stream: false,
            // L1: thinking_intent 单通道——per-request 覆盖优先，否则 escalation 沿用 sticky 决策
            thinking_intent: self.req_ctx.thinking_intent.clone()
                .or(self.resolve_thinking_config_sticky(ctx.complexity_thinking.clone()).await),
            cache_config: Some(crate::llm::prompt_cache::PromptCacheConfig::default()), // 修复: model escalation 路径开启缓存
            extra_body: Default::default(),
            // Escalation 沿用 ctx.user_message_preamble
            user_message_preamble: ctx.user_message_preamble.clone(),
        };

        match ctx.provider.complete_cancellable(escalated_req, self.cancel_token()).await {
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
                {
                    let s = self.session.read().await;
                    let mut msgs = s.messages.write().await;
                    msgs.pop();
                    if !flash_analysis.is_empty() { msgs.pop(); }
                    msgs.push(escalated_resp.message);
                }
                ctx.final_response = escalated_text;
            }
            Err(_) => {
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
            stats: TurnStats {
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
            },
            tool_outputs: ctx.all_tool_outputs.clone(),
            matched_skills: ctx.matched_skills.clone(),
            session_id,
            progressive_state,
            inertia_warning: None,
            pending_confirmations: Vec::new(), // gated 结果未进入工具分发阶段
        }
    }


    // ─── Phase 7: Persist & Build Result ────────────────────────────────────

    async fn persist_and_build_result(self, mut ctx: TurnContext) -> Result<TurnResult, KernelError> {
        if ctx.final_response.is_empty() {
            ctx.final_response = "(max turns reached)".to_string();
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
            stats: TurnStats {
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
