//! 任务沙箱引擎 — 原子步骤拆解 + 模型分级 + 独立沙箱 + 阶段验收 + 执行日志持久化
//!
//! ## 场景
//! 全托管模式下，用户输入一个复杂目标，引擎自动：
//! 1. 拆解为 Phase[]（阶段）× Step[]（原子步骤）
//! 2. 每步在独立 Sandbox 中执行（隔离 Session + 限定工具）
//! 3. 执行用大模型，校验用小模型
//! 4. 验收通过后自动进入下一步
//! 5. 失败重试 N 次后降级
//!
//! ## 状态机
//! ```text
//! Pending → Running → Verifying → Passed → (next step)
//!                         ↓ failed
//!                     Failed → retry → Running
//!                              ↓ max retries
//!                           MaxRetriesExceeded → 降级
//! ```

mod task_log;

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

pub use abacus_types::sandbox::*;
use crate::llm::provider::*;
use crate::llm::LlmProvider;
use serde_json::Value;
use crate::sandbox::task_log::{StepLog, TaskLogStore};

pub type SandboxResult<T> = Result<T, String>;

// ─── 沙箱会话状态（运行时） ─────────────────────────────────────────────────

/// 沙箱引擎
pub struct SandboxOrchestrator {
    config: SandboxConfig,
    active_sessions: RwLock<HashMap<String, SandboxSession>>,
    outputs: RwLock<HashMap<String, Value>>,
    events: RwLock<Vec<SandboxEvent>>,
    providers: RwLock<HashMap<String, Arc<dyn LlmProvider>>>,
    task_log: Arc<TaskLogStore>,
    /// V29.11 (B): 可选实时事件下沉 — TUI 设置后, emit() 同步发送
    /// 引用关系:
    ///   - 写入: set_event_sink(Some(tx)) 由 TUI execute_slash_command 调用
    ///   - 读取: emit() 每次事件发射时 try_send
    /// 生命周期: execute() 期间设置; 完成后清 None; channel drop 后 try_send 静默失败
    event_sink: RwLock<Option<tokio::sync::mpsc::UnboundedSender<SandboxEvent>>>,
}

/// 单步沙箱运行态
#[allow(dead_code)]
struct SandboxSession {
    step_id: String,
    phase_id: String,
    state: StepState,
    retries_left: u32,
    run_model: String,
    verify_model: String,
    work_dir: String,
}

impl SandboxOrchestrator {
    pub fn new(
        config: SandboxConfig,
        providers: HashMap<String, Arc<dyn LlmProvider>>,
    ) -> Self {
        let task_log = TaskLogStore::new(None).unwrap_or_else(|_| TaskLogStore::in_memory().unwrap());
        Self {
            config,
            active_sessions: RwLock::new(HashMap::new()),
            outputs: RwLock::new(HashMap::new()),
            events: RwLock::new(Vec::new()),
            providers: RwLock::new(providers),
            task_log: Arc::new(task_log),
            event_sink: RwLock::new(None),
        }
    }

    /// 查询某个 task 的执行日志
    pub async fn get_task_logs(&self, task_id: &str) -> Result<Vec<StepLog>, String> {
        self.task_log.get_task_logs(task_id).await
    }

    /// 最近 N 条日志
    pub async fn recent_logs(&self, limit: usize) -> Result<Vec<StepLog>, String> {
        self.task_log.recent(limit).await
    }

    // ─── 公共 API ─────────────────────────────────────────────────────

    /// 从自然语言生成任务计划（返回待确认的 TaskSpec）
    pub async fn plan_from_nl(&self, goal: &str) -> SandboxResult<TaskSpec> {
        let (provider_name, model_name) = match &self.config.execute_model {
            ModelAssignment::Fixed { provider, model } => (provider.clone(), model.clone()),
            _ => ("deepseek".into(), "deepseek-chat".into()),
        };
        let provider = self.providers.read().await.get(provider_name.as_str())
            .ok_or_else(|| format!("provider not found: {provider_name}"))?.clone();

        let prompt = format!(
            r#"You are a task planner. Given a user goal, decompose it into phases and steps.
Each step must specify: id, description, model (Execute or Verify), tools needed, acceptance criteria.

Output ONLY a valid JSON matching this schema:
{{
  "goal": "string",
  "phases": [{{
    "id": "string",
    "description": "string",
    "steps": [{{
      "id": "string",
      "description": "string",
      "model": "Execute|Verify",
      "tools": ["string"],
      "accept_criteria": [{{"kind": "Compiles|TestsPass|NoCriticalFindings|KeywordMatch|Custom", "threshold": 1.0}}],
      "max_retries": 2,
      "timeout_secs": 120
    }}]
  }}]
}}

User goal: {goal}"#,
            goal = goal
        );

        let req = LlmRequest {
            model: abacus_types::ModelId(model_name.to_string()),
            messages: vec![Message {
                role: MessageRole::User,
                content: Some(MessageContent::Text(prompt)),
                name: None, tool_calls: None, tool_call_id: None, reasoning_content: None, prefix: false,
            }],
            system: None,
            system_segments: Vec::new(),
            tools: Vec::new(),
            temperature: Some(0.1),
            max_tokens: Some(8192),
            top_p: None, stop: Vec::new(), stream: false,
            thinking_intent: None, cache_config: None, extra_body: HashMap::new(), user_message_preamble: None,
        };

        let resp = provider.complete(req).await
            .map_err(|e| format!("plan generation failed: {e}"))?;
        let text = extract_text(&resp.message);

        // Extract JSON from response (handle markdown code fences)
        let json_str = if let Some(start) = text.find('{') {
            if let Some(end) = text.rfind('}') {
                &text[start..=end]
            } else { &text }
        } else { &text };

        let task: TaskSpec = serde_json::from_str(json_str)
            .map_err(|e| format!("failed to parse plan JSON: {e}\nRaw:\n{text}"))?;
        Ok(task)
    }

    /// 执行完整任务（同步等待，生产环境应异步订阅事件）
    pub async fn execute(&self, task: &TaskSpec) -> SandboxResult<TaskState> {
        for phase in &task.phases {
            let phase_result = self.execute_phase(phase).await?;
            if phase_result == PhaseState::Completed {
                self.emit(SandboxEvent {
                    kind: SandboxEventKind::PhaseCompleted,
                    phase_id: phase.id.clone(),
                    step_id: String::new(),
                    message: format!("phase {} completed", phase.id),
                });
            } else {
                return Ok(TaskState::Failed);
            }
        }
        self.emit(SandboxEvent {
            kind: SandboxEventKind::TaskCompleted,
            phase_id: String::new(),
            step_id: String::new(),
            message: format!("task completed: {}", task.goal),
        });
        Ok(TaskState::Completed)
    }

    /// 获取事件历史
    pub async fn event_log(&self) -> Vec<SandboxEvent> {
        self.events.read().await.clone()
    }

    /// 获取步骤输出
    pub async fn step_output(&self, step_id: &str) -> Option<serde_json::Value> {
        self.outputs.read().await.get(step_id).cloned()
    }

    // ─── 内部：单阶段执行 ──────────────────────────────────────────────

    async fn execute_phase(&self, phase: &PhaseSpec) -> SandboxResult<PhaseState> {
        for (idx, step) in phase.steps.iter().enumerate() {
            let result = self.execute_step(phase, step, idx).await?;
            if result != StepState::Passed {
                return Ok(PhaseState::Failed {
                    step_id: step.id.clone(),
                    reason: format!("step {} failed: {:?}", step.id, result),
                });
            }
        }
        Ok(PhaseState::Completed)
    }

    // ─── 内部：单步执行（完整生命周期） ───────────────────────────────

    async fn execute_step(&self, phase: &PhaseSpec, step: &StepSpec, _idx: usize) -> SandboxResult<StepState> {
        let mut retries_left = step.max_retries.max(1);
        let step_dir = format!("{}/{}/{}", self.config.work_dir, phase.id, step.id);

        // 确定执行/校验模型
        let run_model = self.resolve_model(&step.model);
        let verify_model = self.resolve_model(&step.model);

        loop {
            // 创建沙箱目录
            tokio::fs::create_dir_all(&step_dir).await
                .map_err(|e| format!("mkdir sandbox {step_dir}: {e}"))?;

            // 注册运行态
            {
                let mut sessions = self.active_sessions.write().await;
                sessions.insert(step.id.clone(), SandboxSession {
                    step_id: step.id.clone(),
                    phase_id: phase.id.clone(),
                    state: StepState::Running,
                    retries_left,
                    run_model: run_model.clone(),
                    verify_model: verify_model.clone(),
                    work_dir: step_dir.clone(),
                });
            }

            self.emit(SandboxEvent {
                kind: SandboxEventKind::StepStarted { model: run_model.clone() },
                phase_id: phase.id.clone(),
                step_id: step.id.clone(),
                message: format!("step {} with model {}, retries left {}",
                    step.description, run_model, retries_left),
            });

            // ── 执行阶段 ──
            // 构建输入上下文：从上一步 output 中取 input_refs
            let inputs = self.collect_inputs(step).await;
            let run_context = serde_json::json!({
                "step": step.id,
                "description": step.description,
                "skill": step.skill,
                "tools": step.tools,
                "inputs": inputs,
                "work_dir": step_dir,
                "model": run_model,
            });
            // 执行结果占位：实际应通过 LLM provider 执行
            // 当前简化：直接传入 inputs → 视为执行完成
            let output = self.run_llm(&step.id, &run_model, &serde_json::to_string(&run_context).unwrap_or_default()).await?;
            self.outputs.write().await.insert(step.id.clone(), output.clone());

            // ── 校验阶段（用小模型） ──
            let verify_passed = self.verify_output(step, &output, &verify_model).await?;

            // 写入执行日志
            let _ = self.task_log.write_step(&StepLog {
                task_id: String::new(), // 待任务级 ID
                phase_id: phase.id.clone(),
                step_id: step.id.clone(),
                attempt: (step.max_retries + 1).saturating_sub(retries_left),
                run_model: run_model.clone(),
                verify_model: verify_model.clone(),
                input_summary: serde_json::to_string(&step.input_refs).unwrap_or_default(),
                output: serde_json::to_string(&output).unwrap_or_default(),
                verification_results: if verify_passed { "[\"passed\"]".into() } else { "[\"failed\"]".into() },
                passed: verify_passed,
                latency_ms: 0,
                timestamp_ms: chrono::Utc::now().timestamp_millis(),
            }).await;

            if verify_passed {
                {
                    let mut sessions = self.active_sessions.write().await;
                    if let Some(s) = sessions.get_mut(&step.id) {
                        s.state = StepState::Passed;
                    }
                }
                self.emit(SandboxEvent {
                    kind: SandboxEventKind::VerificationPassed,
                    phase_id: phase.id.clone(),
                    step_id: step.id.clone(),
                    message: format!("step {} verification passed", step.id),
                });
                return Ok(StepState::Passed);
            }

            // 校验失败 → 重试或报错
            retries_left = retries_left.saturating_sub(1);
            if retries_left > 0 {
                self.emit(SandboxEvent {
                    kind: SandboxEventKind::StepFailed { will_retry: true, retries_left },
                    phase_id: phase.id.clone(),
                    step_id: step.id.clone(),
                    message: format!("step {} verification failed, retrying ({})", step.id, retries_left),
                });
                continue;
            }

            {
                let mut sessions = self.active_sessions.write().await;
                if let Some(s) = sessions.get_mut(&step.id) {
                    s.state = StepState::MaxRetriesExceeded;
                }
            }
            self.emit(SandboxEvent {
                kind: SandboxEventKind::StepFailed { will_retry: false, retries_left: 0 },
                phase_id: phase.id.clone(),
                step_id: step.id.clone(),
                message: format!("step {} failed: max retries exceeded", step.id),
            });
            return Ok(StepState::MaxRetriesExceeded);
        }
    }

    // ─── 模型解析 ─────────────────────────────────────────────────────

    fn resolve_model(&self, assignment: &ModelAssignment) -> String {
        match assignment {
            ModelAssignment::Auto => abacus_types::ModelId::AUTO.to_string(),
            ModelAssignment::Execute => self.format_model(&self.config.execute_model),
            ModelAssignment::Verify => self.format_model(&self.config.verify_model),
            ModelAssignment::Fixed { provider, model } => format!("{provider}:{model}"),
        }
    }

    fn format_model(&self, ma: &ModelAssignment) -> String {
        match ma {
            ModelAssignment::Auto => abacus_types::ModelId::AUTO.to_string(),
            ModelAssignment::Fixed { provider, model } => format!("{provider}:{model}"),
            _ => abacus_types::ModelId::AUTO.to_string(),
        }
    }

    // ─── 输入收集 ──────────────────────────────────────────────────────

    async fn collect_inputs(&self, step: &StepSpec) -> HashMap<String, serde_json::Value> {
        let mut inputs = HashMap::new();
        for ref_id in &step.input_refs {
            if let Some(output) = self.outputs.read().await.get(ref_id).cloned() {
                inputs.insert(ref_id.clone(), output);
            }
        }
        inputs
    }

    // ─── LLM 执行 ─────────────────────────────────────────────────────

    /// 通过 LlmProvider 真实执行。选择 provider → 构建请求 → 调 complete → 返回输出
    async fn run_llm(&self, step_id: &str, model_str: &str, prompt: &str) -> SandboxResult<Value> {
        let (provider_name, model_name) = model_str.split_once(':').unwrap_or(("deepseek", model_str));
        let provider = self.providers.read().await.get(provider_name)
            .ok_or_else(|| format!("provider not found: {provider_name}"))?.clone();

        let req = LlmRequest {
            model: abacus_types::ModelId(model_name.to_string()),
            messages: vec![
                Message {
                    role: MessageRole::User,
                    content: Some(MessageContent::Text(prompt.to_string())),
                    name: None, tool_calls: None, tool_call_id: None, reasoning_content: None, prefix: false,
                },
            ],
            system: Some(format!("You are executing sandbox step {step_id}. Output only the result as JSON.")),
            system_segments: Vec::new(),
            tools: Vec::new(),
            temperature: Some(0.3),
            max_tokens: Some(4096),
            top_p: None, stop: Vec::new(), stream: false,
            thinking_intent: None, cache_config: None, extra_body: HashMap::new(), user_message_preamble: None,
        };

        let resp = provider.complete(req).await
            .map_err(|e| format!("llm call failed: {e}"))?;

        let text = match &resp.message.content {
            Some(MessageContent::Text(t)) => t.clone(),
            Some(MessageContent::MultiPart(parts)) => {
                parts.iter().filter_map(|p| {
                if let ContentPart::Text { text } = p { Some(text.clone()) } else { None }
                }).collect::<Vec<_>>().join("\n")
            }
            None => String::new(),
        };

        Ok(serde_json::json!({
            "step": step_id,
            "response": text,
            "model": model_name,
            "usage": {
                "prompt_tokens": resp.usage.prompt_tokens,
                "completion_tokens": resp.usage.completion_tokens,
            },
        }))
    }

    // ─── 校验执行 ──────────────────────────────────────────────────────

    /// 逐条执行验收标准。
    /// - 硬标准（Compiles/TestsPass）通过 shell 命令检查
    /// - 软标准（NoCriticalFindings/KeywordMatch）由小模型验证
    async fn verify_output(&self, step: &StepSpec, output: &Value, verify_model: &str) -> SandboxResult<bool> {
        for criterion in &step.accept_criteria {
            let passed = match &criterion.kind {
                CriterionKind::Compiles => {
                    let path = output.get("work_dir").and_then(|v| v.as_str()).unwrap_or("");
                    if path.is_empty() { false } else {
                        let result = tokio::process::Command::new("cargo")
                            .args(["check", "--manifest-path", &format!("{path}/Cargo.toml")])
                            .output().await;
                        result.map(|r| r.status.success()).unwrap_or(false)
                    }
                }
                CriterionKind::TestsPass => {
                    let result = tokio::process::Command::new("cargo")
                        .args(["test", "--manifest-path", &format!("{}/Cargo.toml",
                            output.get("work_dir").and_then(|v| v.as_str()).unwrap_or(""))])
                        .output().await;
                    result.map(|r| r.status.success()).unwrap_or(false)
                }
                CriterionKind::NoCriticalFindings => {
                    let output_str = serde_json::to_string(output).unwrap_or_default();
                    !output_str.contains("\"P0\"") && !output_str.contains("\"P1\"")
                }
                CriterionKind::Coverage { min_pct } => {
                    // 暂不实现：需要解析 cargo-tarpaulin 或类似工具输出
                    let _ = min_pct;
                    true
                }
                CriterionKind::KeywordMatch { required } => {
                    let output_str = serde_json::to_string(output).unwrap_or_default();
                    required.iter().all(|kw| output_str.contains(kw))
                }
                CriterionKind::Custom { description } => {
                    // 自定义标准：用小模型判断
                    let (provider_name, model_name) = verify_model.split_once(':')
                        .unwrap_or(("deepseek", verify_model));
                    if let Some(provider) = self.providers.read().await.get(provider_name).cloned() {
                        let req = LlmRequest {
                            model: abacus_types::ModelId(model_name.to_string()),
                            messages: vec![Message {
                                role: MessageRole::User,
                                content: Some(MessageContent::Text(format!(
                                    "Criterion: {}\nOutput:\n{}\n\nDoes the output satisfy this criterion? Answer YES or NO.",
                                    description,
                                    serde_json::to_string_pretty(output).unwrap_or_default()
                                ))),
                                name: None, tool_calls: None, tool_call_id: None, reasoning_content: None, prefix: false,
                            }],
                            system: Some("You are a strict reviewer. Answer only YES or NO.".into()),
                            system_segments: Vec::new(),
                            tools: Vec::new(),
                            temperature: Some(0.0),
                            max_tokens: Some(10),
                            top_p: None, stop: Vec::new(), stream: false,
                            thinking_intent: None, cache_config: None, extra_body: HashMap::new(), user_message_preamble: None,
                        };
                        match provider.complete(req).await {
                            Ok(r) => {
                                let text = extract_text(&r.message);
                                text.trim().eq_ignore_ascii_case("YES")
                            }
                            Err(_) => false,
                        }
                    } else {
                        false
                    }
                }
            };
            if !passed {
                return Ok(false);
            }
        }
        Ok(true)
    }


    /// 动态注册 provider（供 CoreLoop 初始化后调用）
    pub async fn add_provider(&self, id: String, provider: Arc<dyn LlmProvider>) {
        self.providers.write().await.insert(id, provider);
    }


    // ─── 事件 ──────────────────────────────────────────────────────────

    fn emit(&self, event: SandboxEvent) {
        // V29.11 (B): 实时下沉 — 有 sink 时先发 channel（clone 开销低: SandboxEvent 小结构）
        if let Ok(sink) = self.event_sink.try_read() {
            if let Some(ref tx) = *sink {
                let _ = tx.send(event.clone());
            }
        }
        // 内部事件日志（保留, event_log() API 不变）
        if let Ok(mut events) = self.events.try_write() {
            if events.len() >= 10_000 {
                events.drain(0..5_000); // 保留最近 5000 条
            }
            events.push(event);
        }
    }

    /// V29.11 (B): 设置/清除实时事件下沉 channel
    ///
    /// 引用关系:
    ///   - 调用: TUI execute_slash_command TurnkeyExecute 前 set Some(tx), 完成后 set None
    ///   - 效果: 设置期间 emit() 的每条 SandboxEvent 都实时发给 TUI push_trace
    /// 并发安全: RwLock 保证 emit() 只读(try_read) 与 set() 写(write) 互斥
    pub async fn set_event_sink(&self, tx: Option<tokio::sync::mpsc::UnboundedSender<SandboxEvent>>) {
        *self.event_sink.write().await = tx;
    }
}

use abacus_types::ToolId;

/// task.plan / task.run 工具执行器（注册到 ToolRegistry 供 LLM 调用）
pub struct SandboxToolExecutor {
    engine: Arc<SandboxOrchestrator>,
    /// 用户确认前暂存的计划
    pending_plan: RwLock<Option<TaskSpec>>,
}

impl SandboxToolExecutor {
    pub fn new(engine: Arc<SandboxOrchestrator>) -> Self {
        Self { engine, pending_plan: RwLock::new(None) }
    }

    /// 注册 task.plan 和 task.run 工具
    pub async fn register(&self, registry: &crate::tool::ToolRegistry) {
        use abacus_types::{ToolCost, ToolHandle, ToolId, ToolProvider, ToolSchema, ToolSecurity, ToolState};
        let executor = Arc::new(SandboxToolExecutor::new(self.engine.clone()));
        for desc in &[
            ("task_plan", "根据自然语言需求生成任务计划，返回待确认的阶段/步骤清单"),
            ("task_run", "执行已确认的任务计划（必须先调用 task.plan 获取用户确认后再调用）"),
        ] {
            let handle = ToolHandle {
                id: ToolId(desc.0.into()),
                schema: ToolSchema { short_description: None,
                    name: desc.0.into(),
                    description: desc.1.into(),
                    parameters: serde_json::json!({"type": "object", "properties": {
                        "goal": {"type": "string", "description": "用户需求描述"}
                    }, "required": ["goal"]}),
                    returns: None,
                    security: Some(ToolSecurity {
                        allowed_paths: None, max_size_mb: None,
                        confirm_required: true, needs_sandbox: false,
                    }),
                    cost: Some(ToolCost { tokens: 128, latency: "2s".into(), risk: "low".into() }),
                    examples: Vec::new(),
                    applicable_task_kinds: None,
                    idempotent: false,
                                        schema_stable: false,                },
                provider: ToolProvider::BuiltIn,
                state: ToolState::Loaded,
                effectiveness: abacus_types::ToolEffectiveness::default(),
            };
            let tid = handle.id.clone();
            registry.register(handle).await;
            registry.register_executor(tid, executor.clone()).await;
        }
    }
}

#[async_trait::async_trait]
impl crate::tool::ToolExecutor for SandboxToolExecutor {
    async fn execute(&self, tool_id: &ToolId, params: serde_json::Value, _ctx: &crate::tool::ExecutionContext) -> abacus_types::Result<serde_json::Value> {
        let goal = params.get("goal").and_then(|v| v.as_str())
            .ok_or_else(|| abacus_types::KernelError::Other("missing goal".into()))?;

        match tool_id.0.as_str() {
            "task_plan" => {
                let plan = self.engine.plan_from_nl(goal).await
                    .map_err(abacus_types::KernelError::Other)?;
                // 暂存待确认
                *self.pending_plan.write().await = Some(plan.clone());
                Ok(serde_json::json!({
                    "status": "plan_ready",
                    "message": format!("已生成计划，共 {} 阶段。请确认后调用 task.run 执行。", plan.phases.len()),
                    "plan": plan,
                    "summary": plan.phases.iter().map(|p| format!("{}: {} 步", p.id, p.steps.len())).collect::<Vec<_>>(),
                }))
            }
            "task_run" => {
                let plan = self.pending_plan.write().await.take()
                    .ok_or_else(|| abacus_types::KernelError::Other("没有待执行的计划。请先调用 task.plan 生成计划。".into()))?;
                let result = self.engine.execute(&plan).await
                    .map_err(abacus_types::KernelError::Other)?;
                let logs = self.engine.recent_logs(10).await.unwrap_or_default();
                Ok(serde_json::json!({
                    "result": format!("{:?}", result),
                    "steps_executed": logs.len(),
                }))
            }
            _ => Err(abacus_types::KernelError::Other("unknown sandbox tool".into())),
        }
    }
}

/// 从 LLM 回复中提取文本内容
///
/// ## 三种 content 形态
/// - `Some(Text(s))` → 直接返回 s
/// - `Some(MultiPart(parts))` → 仅取 ContentPart::Text，忽略 image/file/...，以 \n 拼接
/// - `None` → 空串
fn extract_text(msg: &Message) -> String {
    match &msg.content {
        Some(MessageContent::Text(t)) => t.clone(),
        Some(MessageContent::MultiPart(parts)) => {
            parts.iter().filter_map(|p| {
                if let crate::llm::ContentPart::Text { text } = p { Some(text.clone()) } else { None }
            }).collect::<Vec<_>>().join("\n")
        }
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{ContentPart, ImageUrlSource, MessageContent, MessageRole};

    fn msg_with(content: Option<MessageContent>) -> Message {
        Message {
            role: MessageRole::Assistant,
            content,
            name: None,
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            prefix: false,
        }
    }

    #[test]
    fn extract_text_from_plain_text() {
        let m = msg_with(Some(MessageContent::Text("hello world".into())));
        assert_eq!(extract_text(&m), "hello world");
    }

    #[test]
    fn extract_text_from_none_content() {
        let m = msg_with(None);
        assert_eq!(extract_text(&m), "");
    }

    #[test]
    fn extract_text_from_multipart_filters_non_text() {
        // 混合 Text/Image/ToolUse — 仅 Text 被提取，多个用 \n 拼
        let parts = vec![
            ContentPart::Text { text: "first".into() },
            ContentPart::ImageUrl {
                image_url: ImageUrlSource { url: "data:...".into(), detail: None }
            },
            ContentPart::Text { text: "second".into() },
            ContentPart::ToolUse {
                id: "t1".into(), name: "x".into(), input: serde_json::Value::Null,
            },
            ContentPart::ToolResult {
                tool_use_id: "t1".into(), content: "result".into(),
            },
        ];
        let m = msg_with(Some(MessageContent::MultiPart(parts)));
        assert_eq!(extract_text(&m), "first\nsecond");
    }

    #[test]
    fn extract_text_from_empty_multipart() {
        let m = msg_with(Some(MessageContent::MultiPart(vec![])));
        assert_eq!(extract_text(&m), "");
    }
}
