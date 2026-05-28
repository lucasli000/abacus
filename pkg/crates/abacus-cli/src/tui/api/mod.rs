//! Abacus TUI API — 后端引擎桥接层
//!
//! 桥接 TUI 与 abacus-core CoreLoop / SessionStore。
//! TUI main.rs 初始化时创建 EngineHandle，各组件通过引用调用。

use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use abacus_core::core::{CoreLoop, SessionState, TurnResult};
use abacus_types::TurnStats;
use abacus_orchestrator::team::{TeamSession, TeamBuilder, TeamStatus, AgentRole, TeamEvent};
use abacus_orchestrator::meeting::builder::{MeetingSessionBuilder, MeetingSessionHandle};

/// API 调用结果
pub enum ApiResult<T> {
    Ok(T),
    Err(String),
    Pending,
}

/// TUI 持有的引擎句柄（初始化后不可变）
///
/// 引用关系：
/// - core: 底层 LLM 循环引擎（所有模式共用）
/// - session: Chat 模式会话状态
/// - team_session: Team 模式会话（按需创建）
/// - meeting_adapter: Meeting 模式会话（按需创建）
///
/// 生命周期：TUI run() 创建 → 持续到进程退出
///
/// ## ⚠ 代码审查 @2025-01-23 (中等)
/// Clone 由 #[derive(Clone)] 自动派生，对所有 Arc 字段做浅克隆（正确）。
/// 但若未来新增非 Arc 字段（如 `tokio::sync::mpsc::Sender`），派生 Clone
/// 可能产生意外行为。建议实现手动 Clone 并加注释说明克隆语义。
#[derive(Clone)]
pub struct EngineHandle {
    pub core: Arc<CoreLoop>,
    pub session: Arc<RwLock<SessionState>>,
    /// 保证 create_session 写与 send_chat_message 读互斥
    pub session_swap_lock: Arc<RwLock<()>>,
    /// 限制并发请求数，防止快速输入导致请求堆积。
    /// Semaphore(2): 最多 2 个并发请求（1 个主 + 1 个 AI 补全）
    pub inflight_guard: Arc<tokio::sync::Semaphore>,
    /// Team 模式会话 — 模式切换到 Team 时按需创建
    pub team_session: Arc<RwLock<Option<Arc<TeamSession>>>>,
    /// Meeting 模式句柄 — 模式切换到 Meeting 时按需创建
    pub meeting_handle: Arc<RwLock<Option<MeetingSessionHandle>>>,
    /// Phase 4 undo：Engine 引用，slash 命令调度
    /// 注入：engine_init 创建（基于 paths::current_project_dir）
    /// 生命周期：与 EngineHandle 同（进程级）；Phase 7 清理 hook 单独清旧 session 目录
    pub undo_engine: Arc<abacus_core::undo::UndoEngine>,
}

impl EngineHandle {
    /// 初始化引擎连接。超时由调用方控制（run.rs 使用 15s）。
    pub async fn new(model: &str, thinking: &str) -> Result<Self, String> {
        match crate::engine_init::create_engine(model, None, thinking).await {
            Ok((core, session)) => {
                // Phase 4 undo：UndoEngine 绑定当前项目目录
                let undo_engine = Arc::new(
                    abacus_core::undo::UndoEngine::new(abacus_core::paths::current_project_dir())
                );

                // Phase 7 清理 hook：background spawn，不阻塞启动
                // 7 天阈值：log.jsonl mtime 早于 now-7d 的整个 session 目录递归删
                // 失败仅 tracing::warn，不 panic（启动序列稳定优先）
                {
                    let cleanup_engine = undo_engine.clone();
                    tokio::spawn(async move {
                        match cleanup_engine.cleanup_stale(chrono::Duration::days(7)).await {
                            Ok(0) => {}
                            Ok(n) => tracing::info!(cleaned = n, "undo: cleaned stale session dirs"),
                            Err(e) => tracing::warn!(error = %e, "undo cleanup failed"),
                        }
                    });
                }

                Ok(Self {
                    core,
                    session,
                    session_swap_lock: Arc::new(RwLock::new(())),
                    inflight_guard: Arc::new(tokio::sync::Semaphore::new(2)),
                    team_session: Arc::new(RwLock::new(None)),
                    meeting_handle: Arc::new(RwLock::new(None)),
                    undo_engine,
                })
            }
            Err(e) => Err(e.to_string()),
        }
    }

    /// 确保 Team 模式会话存在（惰性创建）。
    /// 首次进入 Team 模式时由用户首条消息作为 goal 创建。
    pub async fn ensure_team_session(&self, goal: &str) -> Arc<TeamSession> {
        let mut guard = self.team_session.write().await;
        if let Some(ref ts) = *guard {
            return ts.clone();
        }
        let session = TeamBuilder::new(
            format!("team_{}", chrono::Utc::now().timestamp_millis()),
            goal,
        )
            .with_role(AgentRole::Leader)
            .with_role(AgentRole::Member)
            .with_role(AgentRole::Advisor)
            .build();
        let arc = Arc::new(session);
        *guard = Some(arc.clone());
        arc
    }

    /// 确保 Meeting 模式会话存在（惰性创建）。
    /// 首次进入 Meeting 模式时由用户首条消息作为 topic 创建。
    pub async fn ensure_meeting_handle(&self, topic: &str) -> Result<(), String> {
        let mut guard = self.meeting_handle.write().await;
        if guard.is_some() {
            return Ok(());
        }
        // V35: 从 ~/.abacus/experts.yaml 加载专家配置（无配置文件时用内置默认 3 位专家）
        // 引用关系: expert_config::load_experts → to_orchestrator_config → with_config
        // 生命周期: 每次进入 Meeting 模式时重新加载（支持运行时 /expert 修改后立即生效）
        let expert_defs = crate::tui::expert_config::load_experts();
        let orch_cfg = crate::tui::expert_config::to_orchestrator_config(&expert_defs);
        let handle = MeetingSessionBuilder::new(topic)
            .with_config(orch_cfg)
            .build()
            .await
            .map_err(|e| e.to_string())?;
        *guard = Some(handle);
        Ok(())
    }

    /// 重置 Team 会话（切换模式或新建会话时调用）
    pub async fn reset_team(&self) {
        *self.team_session.write().await = None;
    }

    /// 重置 Meeting 会话
    pub async fn reset_meeting(&self) {
        *self.meeting_handle.write().await = None;
    }

    /// MCIP 授权后重运同一 turn
    ///
    /// ## 流程
    /// 1. 将用户的 grant decisions 写入 session（Always → 永久，Once → 单次）
    /// 2. 重新执行 pipeline（相同 input），之前被拦截的工具现在有授权可以执行
    /// 3. 返回完整 TurnResult（含工具执行结果）
    ///
    /// ## 引用关系
    /// - 消费方：run.rs 主循环（confirm dialog 用户响应后调用）
    /// - 依赖：CoreLoop::grant_and_rerun()
    pub async fn grant_and_rerun(
        &self,
        decisions: &[(String, abacus_core::mcip::McipGrantDecision)],
        original_input: &str,
    ) -> Result<EngineResponse, String> {
        let result = self.core.grant_and_rerun(decisions, original_input, &self.session)
            .await
            .map_err(|e| format!("MCIP rerun error: {e:?}"))?;
        let thinking = None; // grant_and_rerun 不单独产出 thinking
        Ok(EngineResponse::from_turn_result(result, thinking))
    }
}

/// 发送聊天消息到后端引擎 — 返回完整响应（文本+工具+统计+门控+惰性）
///
/// Concurrency limited via inflight_guard semaphore (max 2 concurrent requests).
///
/// A1 修复：用 process_turn_cancellable + 自管 CancellationToken，超时时主动 cancel
/// 让 provider 层 select! 中断 in-flight reqwest（之前 timeout drop future 不会真正
/// 中断 LLM 请求，请求会跑满 provider 自身超时才结束）。
pub async fn send_chat_message(
    handle: &EngineHandle,
    message: &str,
    req_ctx: abacus_core::core::RequestContext,
) -> ApiResult<EngineResponse> {
    let _permit = match handle.inflight_guard.try_acquire() {
        Ok(p) => p,
        Err(_) => return ApiResult::Err("请求过于频繁，请等待上一个请求完成".to_string()),
    };
    let _guard = handle.session_swap_lock.read().await;

    let cancel = tokio_util::sync::CancellationToken::new();
    // Gap A 修复：per-turn RequestContext 透传让 state.thinking_depth 运行时切换生效
    // 引用关系：process_turn_cancellable_with_context 定义于 core/mod.rs:2250
    let work = handle.core.process_turn_cancellable_with_context(message, &handle.session, req_ctx, cancel.clone());
    let timeout = tokio::time::sleep(Duration::from_secs(300));
    tokio::pin!(timeout);

    let result = tokio::select! {
        r = work => r,
        _ = &mut timeout => {
            cancel.cancel(); // 真正中断 in-flight reqwest
            return ApiResult::Err("请求超时 (300s)，已取消并释放连接".to_string());
        }
    };

    match result {
        Ok(r) => {
            let thinking = extract_thinking(&handle.session).await;
            ApiResult::Ok(EngineResponse::from_turn_result(r, thinking))
        }
        Err(e) => ApiResult::Err(e.user_message()),
    }
}

/// V0.2: 发送消息（流式模式）—— 通过 stream_tx 实时推送 chunk 到 TUI。
///
/// 生命周期: 调用后立即返回 task handle，chunks 通过 stream_tx 持续发送，
/// 最终发送 StreamChunk::Complete 信号表示结束。
pub async fn send_chat_message_streaming(
    handle: &EngineHandle,
    message: &str,
    stream_tx: tokio::sync::mpsc::UnboundedSender<abacus_core::llm::stream::StreamChunk>,
    req_ctx: abacus_core::core::RequestContext,
) -> ApiResult<EngineResponse> {
    let _permit = match handle.inflight_guard.try_acquire() {
        Ok(p) => p,
        Err(_) => return ApiResult::Err("请求过于频繁，请等待上一个请求完成".to_string()),
    };
    let _guard = handle.session_swap_lock.read().await;

    use abacus_core::llm::stream::StreamChunk;

    // A2 修复：用 process_turn_streaming_cancellable + 自管 CancellationToken
    // Gap A 修复：streaming 路径同步接通 RequestContext。引用关系：
    // process_turn_streaming_cancellable_with_context 定义于 core/mod.rs:2293
    let cancel = tokio_util::sync::CancellationToken::new();
    let work = handle.core.process_turn_streaming_cancellable_with_context(
        message,
        &handle.session,
        stream_tx.clone(),
        req_ctx,
        cancel.clone(),
    );
    let timeout = tokio::time::sleep(Duration::from_secs(300));
    tokio::pin!(timeout);

    let result = tokio::select! {
        r = work => r,
        _ = &mut timeout => {
            cancel.cancel();
            let _ = stream_tx.send(StreamChunk::Error("timeout after 300s".into()));
            return ApiResult::Err("request timed out after 300s".to_string());
        }
    };

    match result {
        Ok(r) => {
            let thinking = extract_thinking(&handle.session).await;
            // ToolHealth snapshot: 从 EffectivenessTracker 获取真实历史评分
            // 引用关系：CoreLoop.effectiveness → tool_health_snapshot() → TUI state
            if !r.tool_outputs.is_empty() {
                let tool_ids: Vec<abacus_types::ToolId> = r.tool_outputs.iter()
                    .map(|to| to.tool_id.clone())
                    .collect();
                let health = handle.core.tool_health_snapshot(&tool_ids).await;
                if !health.is_empty() {
                    let _ = stream_tx.send(StreamChunk::ToolHealth(health));
                }
            }
            let _ = stream_tx.send(StreamChunk::Complete(r.stats.clone()));
            ApiResult::Ok(EngineResponse::from_turn_result(r, thinking))
        }
        Err(e) => {
            let _ = stream_tx.send(StreamChunk::Error(e.to_string()));
            ApiResult::Err(e.to_string())
        }
    }
}

// ─── Plan Mode API (V33) ──────────────────────────────────────────────
//
// PlannerAgent 实现策略（cli 层 minimum viable）：
// 1. system prompt 注入：在 message 前加 Planner 角色框架（"你是规划师..."）
// 2. tool whitelist：限制 RequestContext.tool_filter 仅含只读工具（read/glob/grep/kb_query）
// 3. JSON 输出强约束：依赖 V31 prefix completion (待 V31 业务用例落地后启用，本批先用 prompt 强引导)
//
// 后续深度集成（V34+）：abacus-core/src/agents/planner.rs 独立 Agent trait 实现
// + 专属 system_segments + 自动 prefix 注入；本批是过渡方案
//
// 引用关系：run.rs::AbacusMode::Plan 分支调用本函数

// V34-3: PLANNER prompt 硬约束强化 — 输出格式锁死，便于 V34-2 extract_plan_tasks_from_messages 解析
// 引用关系：send_planner_message_streaming/send_planner_message 拼接到用户消息前
// 设计意图：以 prompt 强引导承担"输出格式约束"职责；
//   未来 V35 接入 V31 prefix completion 后可降低 prompt 体积（改在 messages 末尾追加 prefix=true 的 ```json\n）
// 更新点（V34-3 vs V33）：
//   1. 输出顺序约束 — 任何分析/思考都在 JSON 之前；JSON 是消息**最后一段**
//   2. JSON 唯一性约束 — 仅一个代码块；不要前后嵌套多个 ```json
//   3. 数组顶层 — 与 V34-2 extract 优先解析 Vec<TaskSpec> 对齐（虽然单 TaskSpec 也兼容）
const PLANNER_SYSTEM_PROMPT: &str = "## 你的角色：Planner（规划师）\n\n\
你的唯一目标：把用户的需求拆解为结构化的任务列表。\n\n\
## 工作准则\n\
- 仅使用只读工具（fs_read / glob / grep / kb_query）了解上下文\n\
- 不要执行任何写操作（用户进入 Team 模式时才执行）\n\
- 拆解到 phase 粒度（每个 phase 5-15 分钟工作量）\n\
- 标注 phase 间的依赖关系\n\n\
## 输出格式（严格）\n\
1. 自由分析 / 调研结果在前\n\
2. **消息末尾**输出**唯一一个** ```json 代码块\n\
3. JSON 顶层为数组（即便只有一个任务也要包一层 `[ ... ]`）\n\
4. 不要在 JSON 后追加任何解释\n\n\
模板：\n\
```json\n\
[\n  {\n    \"goal\": \"<用户需求的简短摘要>\",\n    \
\"phases\": [\n      {\"id\": \"p1\", \"description\": \"<phase 描述>\", \"steps\": []}\n    ]\n  }\n]\n\
```\n\n\
## 用户需求";

/// 只读工具白名单（Plan 模式专属）
fn planner_tool_whitelist() -> Vec<abacus_types::ToolId> {
    use abacus_types::ToolId;
    vec![
        ToolId("filengine_fs_read".into()),
        ToolId("filengine_fs_read_multiple".into()),
        ToolId("filengine_fs_search".into()),
        ToolId("filengine_dir_list".into()),
        ToolId("filengine_dir_tree".into()),
        ToolId("filengine_file_kb_query".into()),
        ToolId("filengine_file_retrieval_search".into()),
    ]
}

/// V35-1: Planner 默认 prefix（仅对 supports_prefix_completion 模型生效）
///
/// 引用关系：abacus-core::pipeline::maybe_inject_prefix_message 消费此字段
/// 协议层硬约束：模型从 "```json\n[" 之后继续生成，使 V34-2 的 JSON 解析必中
/// 模型不支持时（如 Anthropic）静默降级，不报错——仍依赖 PLANNER_SYSTEM_PROMPT 软约束
const PLANNER_PREFIX_CONTENT: &str = "```json\n[";

/// V33: Plan 模式发消息（流式）— 注入 Planner 角色 + 只读工具白名单
/// V35-1: 增设 prefix completion（Plan→Team JSON 解析硬约束）
/// V35-2: PLANNER_SYSTEM_PROMPT 改走 system_prompt_override 通道（不再拼 user message）
///   理由：① KV cache 友好（system 段稳定可缓存）② 用户消息历史干净 ③ 与 Specialist 框架对齐
pub async fn send_planner_message_streaming(
    handle: &EngineHandle,
    message: &str,
    stream_tx: tokio::sync::mpsc::UnboundedSender<abacus_core::llm::stream::StreamChunk>,
    mut req_ctx: abacus_core::core::RequestContext,
) -> ApiResult<EngineResponse> {
    // 注入只读 tool 白名单（覆盖默认全工具）
    req_ctx.tool_filter = Some(planner_tool_whitelist());
    // V35-1: 注入 prefix completion — 模型从 "```json\n[" 继续生成
    req_ctx.prefix_assistant_content = Some(PLANNER_PREFIX_CONTENT.to_string());
    // V35-2: Planner 角色 system prompt 走独立通道；user message 保持原始用户需求
    req_ctx.system_prompt_override = Some(PLANNER_SYSTEM_PROMPT.to_string());
    send_chat_message_streaming(handle, message, stream_tx, req_ctx).await
}

/// V33: Plan 模式发消息（非流式）
/// V35-1: 增设 prefix completion（与流式版对齐）
/// V35-2: 同步走 system_prompt_override 通道
pub async fn send_planner_message(
    handle: &EngineHandle,
    message: &str,
    mut req_ctx: abacus_core::core::RequestContext,
) -> ApiResult<EngineResponse> {
    req_ctx.tool_filter = Some(planner_tool_whitelist());
    req_ctx.prefix_assistant_content = Some(PLANNER_PREFIX_CONTENT.to_string());
    req_ctx.system_prompt_override = Some(PLANNER_SYSTEM_PROMPT.to_string());
    send_chat_message(handle, message, req_ctx).await
}

// ─── Reviewer Mode API (V37-3) ────────────────────────────────────────
//
// V37-3: Reviewer 角色封装 — 复用 V35-2 system_prompt_override 通道
//
// ## 通道选择验证（V36-1 判定准则）
// - 角色 prompt 稳定：是（不同会话调用 review_plan，prompt 内容一致）
// - 用户输入独立：是（user message = 待审产物）
// - ⇒ 走 system_prompt_override 是正确选择
//
// ## 三种 review 类型
// 1. review_plan：审 Planner 输出 JSON（结构合理性 / 任务覆盖度）
// 2. review_diff：审代码 diff（正确性 / 风格 / 引用完整性）
// 3. review_security：审安全（OWASP / 秘密泄漏 / 权限）
//
// ## 输出契约
// 所有 review 返回统一格式：
// ```
// [verdict: pass|fail|needs_revision]
// 1. <issue 1 描述>
// 2. <issue 2 描述>
// ...
// ```
//
// ## 工具白名单（与 Planner 完全一致 — 只读）
// 复用 planner_tool_whitelist()

const REVIEW_PLAN_SYSTEM_PROMPT: &str = "## 你的角色：Plan Reviewer（计划审查员）\n\n\
你的唯一目标：审查给定的任务规划是否合理、可执行、覆盖完整。\n\n\
## 审查维度\n\
- 结构合理性：phases 是否独立可执行，依赖关系是否清晰\n\
- 任务覆盖度：是否遗漏关键步骤（测试 / 回滚 / 边界情况）\n\
- 粒度适中：每个 phase 5-15 分钟工作量；过粗或过细均要标注\n\
- 风险标注：是否识别破坏性 / 不可逆 / 跨服务边界的步骤\n\n\
## 输出格式（严格）\n\
首行：`[verdict: pass|fail|needs_revision]`\n\
后续：编号列出具体 issues（pass 时仅写 `1. 无明显问题`）\n\n\
## 待审计划";

const REVIEW_DIFF_SYSTEM_PROMPT: &str = "## 你的角色：Diff Reviewer（代码差异审查员）\n\n\
你的唯一目标：审查给定的代码 diff 是否正确、规范、安全。\n\n\
## 审查维度\n\
- 正确性：实现与意图是否一致；边界条件是否处理\n\
- 风格规范：命名 / 格式 / 注释 / 错误处理\n\
- 引用完整性：删除/重命名是否漏改其他引用点\n\
- 性能 / 副作用：是否引入无界循环 / 内存泄漏 / 阻塞调用\n\n\
## 输出格式（严格）\n\
首行：`[verdict: pass|fail|needs_revision]`\n\
后续：编号列出具体 issues（标注 file:line 便于定位）\n\n\
## 待审 diff";

const REVIEW_SECURITY_SYSTEM_PROMPT: &str = "## 你的角色：Security Reviewer（安全审查员）\n\n\
你的唯一目标：审查给定内容是否含安全风险。\n\n\
## 审查维度（OWASP Top 10 + 工程实践）\n\
- 注入：SQL / Command / XSS / Path traversal\n\
- 敏感信息：API key / token / 密码 / PII 泄漏\n\
- 权限边界：未授权访问 / 越权 / 提权\n\
- 加密 / 认证：弱算法 / 硬编码密钥 / token 错用\n\
- 依赖风险：已知漏洞包 / 不可信源\n\n\
## 输出格式（严格）\n\
首行：`[verdict: pass|fail|needs_revision]`\n\
后续：编号列出风险（[严重度: high/medium/low] + 描述 + 位置）\n\n\
## 待审内容";

/// V37-3: Review 类型枚举 — 决定使用哪个 system prompt
/// V41-4: derive Serialize/Deserialize 支持 ReviewReport.kind 持久化
/// 引用关系：send_reviewer_message_streaming 第一参数；与三个 SYSTEM_PROMPT 常量 1:1 对应
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub enum ReviewKind {
    /// 审查 Planner 输出的 JSON 任务规划（结构合理性 / 覆盖度）
    Plan,
    /// 审查代码 diff（正确性 / 风格 / 引用完整性）
    Diff,
    /// 审查安全风险（OWASP / 秘密泄漏 / 权限）
    Security,
}

impl ReviewKind {
    /// 获取对应的 system prompt
    fn system_prompt(self) -> &'static str {
        match self {
            ReviewKind::Plan => REVIEW_PLAN_SYSTEM_PROMPT,
            ReviewKind::Diff => REVIEW_DIFF_SYSTEM_PROMPT,
            ReviewKind::Security => REVIEW_SECURITY_SYSTEM_PROMPT,
        }
    }

    /// 用于 toast / log 的人类可读 label
    pub fn label(self) -> &'static str {
        match self {
            ReviewKind::Plan => "计划审查",
            ReviewKind::Diff => "代码审查",
            ReviewKind::Security => "安全审查",
        }
    }
}

/// V38-1: Review verdict — Reviewer 输出的判定结果
///
/// ## 引用关系
/// - 解析：parse_review_report() 从 LLM 文本输出解析
/// - 消费：上层（cli / hook / agent）按 verdict 做决策（pass→前进 / needs_revision→nudge / fail→阻断）
///
/// ## 设计意图
/// 把"自然语言决策"转译为"程序决策"——agent 系统的核心枢纽
/// V40-1: derive Serialize/Deserialize 让 ReviewReport 可持久化到 SessionExport
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ReviewVerdict {
    /// 无问题，可前进
    Pass,
    /// 有问题但可修，建议修正后重试
    NeedsRevision,
    /// 严重问题，停下
    Fail,
    /// 解析失败（LLM 输出偏离格式）—— 调用方应回退到 raw_output
    Unknown,
}

impl ReviewVerdict {
    pub fn label(self) -> &'static str {
        match self {
            ReviewVerdict::Pass => "通过",
            ReviewVerdict::NeedsRevision => "需修正",
            ReviewVerdict::Fail => "未通过",
            ReviewVerdict::Unknown => "未知",
        }
    }

    /// 是否允许"前进"——pass 是唯一允许后续动作的 verdict
    pub fn is_pass(self) -> bool {
        matches!(self, ReviewVerdict::Pass)
    }
}

/// V38-1: 单条 review issue
/// V40-1: derive Serialize/Deserialize 支持持久化
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ReviewIssue {
    /// 编号（按 LLM 输出原序）
    pub index: u32,
    /// 描述（含可选的 file:line / [严重度: ...] 标记）
    pub description: String,
}

/// V38-1: 结构化 review 报告
/// V40-1: derive Serialize/Deserialize 支持持久化（SessionExport 写入/读回）
/// V41-4: 加 timestamp + kind 字段以便历史回放
///
/// ## 引用关系
/// - 生产：parse_review_report() 从 EngineResponse.text 解析
/// - 消费：cli 命令 / 后续 agent step / hook 决策
/// - 持久化：SessionExport.last_review + review_history 跨重启保留
///
/// ## 字段语义
/// - verdict: 决策核心
/// - issues: 证据/原因列表
/// - raw_output: 原始 LLM 文本（调用方需要时可回退展示）
/// - kind: V41-4 review 类型（plan/diff/security）— 历史区分用
/// - time_rfc3339: V41-4 抵达时间 — 历史"5 分钟前 fail / 30 秒前 pass"展示
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ReviewReport {
    pub verdict: ReviewVerdict,
    #[serde(default)]
    pub issues: Vec<ReviewIssue>,
    #[serde(default)]
    pub raw_output: String,
    /// V41-4: review 类型（plan/diff/security）— 默认 Plan 兼容旧 last_review
    #[serde(default = "default_review_kind")]
    pub kind: ReviewKind,
    /// V41-4: review 抵达时间（RFC3339）— 默认空串兼容旧 last_review
    #[serde(default)]
    pub time_rfc3339: String,
}

/// V41-4: ReviewKind 默认 — 兼容 V40-1 持久化的 ReviewReport（无 kind 字段）
fn default_review_kind() -> ReviewKind { ReviewKind::Plan }

/// V38-1: 解析 Reviewer 文本输出为结构化报告
///
/// ## 解析协议（与 REVIEW_*_SYSTEM_PROMPT 输出格式契约对齐）
/// 1. 首行严格匹配 `[verdict: pass|needs_revision|fail]`
/// 2. 后续编号行（`1. ...`）作为 issues
/// 3. 解析失败 → verdict=Unknown，issues 空，raw_output 保留
///
/// ## 容错策略
/// - 大小写不敏感（`PASS` / `Pass` / `pass` 同一 verdict）
/// - 中文 verdict（"通过" / "需修正" / "未通过"）作为关键词回退识别
/// - 编号行允许 `1.` / `1)` / `①` 等多种风格
pub fn parse_review_report(text: &str) -> ReviewReport {
    let raw = text.trim();
    let lines: Vec<&str> = raw.lines().collect();

    // 1. verdict 抽取 — 三层匹配优先级：
    //   ① 严格 [verdict: X]（无 prefix 的自由输出）
    //   ② V38-4 prefix 残留（首行裸 "pass]" / "pass" — prefix completion 起头被服务端消耗）
    //   ③ 关键词回退
    let mut verdict = ReviewVerdict::Unknown;
    let to_verdict = |v: &str| -> ReviewVerdict {
        let v = v.to_ascii_lowercase();
        match v.as_str() {
            "pass" | "通过" => ReviewVerdict::Pass,
            "needs_revision" | "needs-revision" | "需修正" | "需要修正" => ReviewVerdict::NeedsRevision,
            "fail" | "未通过" | "失败" => ReviewVerdict::Fail,
            _ => ReviewVerdict::Unknown,
        }
    };
    // ① 严格匹配
    for line in &lines {
        let l = line.trim();
        if let Some(rest) = l.strip_prefix("[verdict:").or_else(|| l.strip_prefix("[Verdict:")) {
            let v = rest.trim_end_matches(']').trim();
            verdict = to_verdict(v);
            break;
        }
    }
    // ② V38-4 prefix 残留风格（首行 "pass]" / "fail" / "needs_revision]"）
    if matches!(verdict, ReviewVerdict::Unknown) {
        if let Some(first_line) = lines.iter().map(|l| l.trim()).find(|l| !l.is_empty()) {
            // 取 first_line 第一个 token（去除尾部 ] 和空白）
            let tok = first_line.split_whitespace().next().unwrap_or("");
            let tok_clean = tok.trim_end_matches(']').trim();
            let v = to_verdict(tok_clean);
            if !matches!(v, ReviewVerdict::Unknown) {
                verdict = v;
            }
        }
    }
    // ③ 关键词回退（仅在前两层失败时）
    if matches!(verdict, ReviewVerdict::Unknown) {
        let lower = raw.to_lowercase();
        if lower.contains("verdict: pass") || lower.contains("无明显问题") || lower.contains("通过审查") {
            verdict = ReviewVerdict::Pass;
        } else if lower.contains("严重") || lower.contains("阻断") || lower.contains("verdict: fail") {
            verdict = ReviewVerdict::Fail;
        } else if lower.contains("建议") || lower.contains("verdict: needs") {
            verdict = ReviewVerdict::NeedsRevision;
        }
    }

    // 2. issues 抽取 — 编号行（容忍 1./1)/①）
    let mut issues = Vec::new();
    let mut idx_counter: u32 = 0;
    for line in &lines {
        let l = line.trim();
        if l.is_empty() { continue; }
        // 匹配 "数字 + 分隔符 + 内容" 或 "编号符号 + 内容"
        let stripped = l.strip_prefix(|c: char| c.is_ascii_digit())
            .map(|s| s.trim_start_matches(|c: char| c.is_ascii_digit()))
            .and_then(|s| s.strip_prefix('.').or_else(|| s.strip_prefix(')')).or_else(|| s.strip_prefix('、')))
            .map(|s| s.trim());
        let content = match stripped {
            Some(c) if !c.is_empty() => c.to_string(),
            _ => continue,
        };
        idx_counter += 1;
        issues.push(ReviewIssue {
            index: idx_counter,
            description: content,
        });
    }

    ReviewReport {
        verdict,
        issues,
        raw_output: raw.to_string(),
        // V41-4: 默认 Plan + 空 time；调用方在抵达 review 响应时通过 with_kind/with_time 注入
        kind: ReviewKind::Plan,
        time_rfc3339: String::new(),
    }
}

impl ReviewReport {
    /// V41-4: 链式注入 review 类型
    pub fn with_kind(mut self, kind: ReviewKind) -> Self {
        self.kind = kind;
        self
    }
    /// V41-4: 链式注入抵达时间（RFC3339 字符串）
    pub fn with_time(mut self, time: impl Into<String>) -> Self {
        self.time_rfc3339 = time.into();
        self
    }
}

/// V38-4: Reviewer prefix completion — 让模型从 "[verdict: " 继续生成
///
/// ## 引用关系
/// - 设置：send_reviewer_message_streaming / send_reviewer_message 注入 req_ctx.prefix_assistant_content
/// - 消费：abacus-core::pipeline::maybe_inject_prefix_message（V35-1）
/// - 解析：V38-1 parse_review_report() — prefix 强制让首行从 [verdict: 开始，必中严格匹配分支
///
/// ## 协议层硬约束效果
/// - 模型必须紧接其后输出 pass/needs_revision/fail 之一（语义闭环）
/// - 关键词回退路径降级为兜底（仅在模型不支持 prefix 时启用）
///
/// ## 适用判定（V36-1 通道使用指导对齐）
/// - Reviewer：✅ 输出格式稳定（[verdict: + 编号 issues），prefix 锁起头让 V38-1 解析必中
/// - Meeting Specialist：❌ 自由讨论，prefix 会损害多样观点
/// - Team Leader：❌ 动态拆解任务，prefix 会限制规划自由度
const REVIEWER_PREFIX_CONTENT: &str = "[verdict: ";

/// V37-3: Reviewer 模式发消息（流式）— 复用 system_prompt_override 通道
/// V38-4: 加入 prefix completion 协议层硬约束
/// 引用关系：cli/tui 任意调用方传入 ReviewKind + 待审内容
/// 设计意图：证明 V35-2 + V35-1 通道的协同泛化性 — 同一通道服务多角色，且每角色按需启用 prefix
pub async fn send_reviewer_message_streaming(
    handle: &EngineHandle,
    kind: ReviewKind,
    content: &str,
    stream_tx: tokio::sync::mpsc::UnboundedSender<abacus_core::llm::stream::StreamChunk>,
    mut req_ctx: abacus_core::core::RequestContext,
) -> ApiResult<EngineResponse> {
    // 工具白名单：与 Planner 一致（只读，无 Write/Edit）
    req_ctx.tool_filter = Some(planner_tool_whitelist());
    // V35-2: 角色 system prompt 走独立通道
    req_ctx.system_prompt_override = Some(kind.system_prompt().to_string());
    // V38-4: prefix 锁起头到 "[verdict: "（输出格式硬约束，让 V38-1 解析必中）
    req_ctx.prefix_assistant_content = Some(REVIEWER_PREFIX_CONTENT.to_string());
    send_chat_message_streaming(handle, content, stream_tx, req_ctx).await
}

/// V37-3: Reviewer 模式发消息（非流式）
/// V38-4: 同步 prefix completion
pub async fn send_reviewer_message(
    handle: &EngineHandle,
    kind: ReviewKind,
    content: &str,
    mut req_ctx: abacus_core::core::RequestContext,
) -> ApiResult<EngineResponse> {
    req_ctx.tool_filter = Some(planner_tool_whitelist());
    req_ctx.system_prompt_override = Some(kind.system_prompt().to_string());
    req_ctx.prefix_assistant_content = Some(REVIEWER_PREFIX_CONTENT.to_string());
    send_chat_message(handle, content, req_ctx).await
}

// ─── Generic Agent Role API (L-3/L-4/L-5) ─────────────────────────────
//
// 通道复用：system_prompt_override（V35-2） + prefix_assistant_content（V35-1） + tool_filter
// 设计意图：把"角色化调用"从 Reviewer 三件套泛化到任意角色——新角色仅需加枚举变体 + SYSTEM_PROMPT 常量
//
// 与 Reviewer 的差异：
//   - Reviewer 是"判断 + 列证据"（[verdict: ...] 格式），RoleKind 是"产出制品"（代码/文档/测试）
//   - Reviewer 输出有结构（V38-1 parse_review_report），RoleKind 输出是自由文本（用户/上层自行处理）
//
// 工具白名单：与 Planner/Reviewer 一致（只读，无 Write/Edit）—— 角色内部不改文件，结果由用户/上层落地

const ROLE_CODE_FIXER_SYSTEM_PROMPT: &str = "## 你的角色：Code Fixer（代码修复员）\n\n\
你的唯一目标：根据用户描述的问题，对给定代码做最小修改使其正确。\n\n\
## 工作准则\n\
- 只读工具（fs_read / grep）了解上下文；不要写文件\n\
- 修改保持最小（只改需要改的）；不顺手重构、不加 docstring、不改格式\n\
- 保留原代码风格（命名 / 缩进 / 注释惯例）\n\
- 每个改动后跟一句简短解释（## 解释 标记）\n\n\
## 输出格式（严格）\n\
1. 修复后的完整代码块（```<lang>\\n...\\n```）\n\
2. ## 解释 标记后跟随 1-3 句话说明改了什么、为什么\n\n\
## 输入";

const ROLE_DOC_SUMMARIZER_SYSTEM_PROMPT: &str = "## 你的角色：Doc Summarizer（文档摘要员）\n\n\
你的唯一目标：把给定文本压缩为可快速扫读的 markdown 摘要。\n\n\
## 工作准则\n\
- 只读工具（fs_read / kb_query）了解相关上下文（如有引用其他文件）\n\
- 保留原文核心论点和数据；删除铺垫和重复\n\
- 摘要长度 ≤ 原文 30%\n\n\
## 输出格式（严格）\n\
- TL;DR：1-2 句话\n\
- 要点：3-7 条 bullet points（每条 ≤ 30 字）\n\
- 关键数据/术语：若原文有具体数字 / 名词，列出 3-5 项\n\n\
## 输入";

const ROLE_TEST_GENERATOR_SYSTEM_PROMPT: &str = "## 你的角色：Test Generator（测试生成员）\n\n\
你的唯一目标：为给定的函数 / 模块生成可直接编译运行的单元测试。\n\n\
## 工作准则\n\
- 只读工具（fs_read / grep）了解函数实现 / 现有测试风格\n\
- 与项目既有测试保持同款（如 #[test] / #[cfg(test)] mod tests / 命名风格）\n\
- 覆盖：① happy path ② 边界条件 ③ 错误路径\n\
- 每个测试函数命名清晰说明意图（test_<scenario>_<expected>）\n\n\
## 输出格式（严格）\n\
完整 ```rust 代码块，含 #[cfg(test)] mod tests 包装；不要解释\n\n\
## 输入";

/// L-3/L-4/L-5: 通用角色枚举 — 决定使用哪个 SYSTEM_PROMPT + prefix
///
/// ## 引用关系
/// send_role_message_streaming 第一参数；与三个 SYSTEM_PROMPT 常量 1:1 对应
///
/// ## 与 ReviewKind 的关系（命名对称）
/// 设计同构（都是"角色 enum + system_prompt() 方法"），但用途不同：
///   - ReviewKind: 审查/判定（输出 verdict）
///   - RoleKind: 产出制品（输出代码/文档/测试）
///
/// ## 命名为何叫 RoleKind 而非 AgentRole
/// abacus-orchestrator::team 已有 AgentRole（Leader/Member/Advisor 用于 Team specialist 分配）
/// 命名冲突 → 用 RoleKind 与 ReviewKind 对称
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum RoleKind {
    /// L-3: 修复代码
    CodeFixer,
    /// L-4: 总结文档
    DocSummarizer,
    /// L-5: 生成测试
    TestGenerator,
}

impl RoleKind {
    /// 对应的 system prompt
    fn system_prompt(self) -> &'static str {
        match self {
            RoleKind::CodeFixer => ROLE_CODE_FIXER_SYSTEM_PROMPT,
            RoleKind::DocSummarizer => ROLE_DOC_SUMMARIZER_SYSTEM_PROMPT,
            RoleKind::TestGenerator => ROLE_TEST_GENERATOR_SYSTEM_PROMPT,
        }
    }

    /// V35-1: prefix completion 起头（None 表示不启用 prefix）
    /// 设计：Fixer/TestGen 用 prefix 锁代码块起头；Summarizer 自由生成 markdown
    fn prefix_content(self) -> Option<&'static str> {
        match self {
            RoleKind::CodeFixer => None,        // 不锁 — 让模型先决定语言（rust/python/...）
            RoleKind::DocSummarizer => None,    // 自由 markdown
            RoleKind::TestGenerator => Some("```rust\n#[cfg(test)]\n"), // 强制 Rust test 起头
        }
    }

    /// 人类可读 label（toast / log / UI）
    pub fn label(self) -> &'static str {
        match self {
            RoleKind::CodeFixer => "代码修复",
            RoleKind::DocSummarizer => "文档摘要",
            RoleKind::TestGenerator => "测试生成",
        }
    }

    /// CLI 子命令字符串解析（与 cmd_role 配对）
    pub fn from_cli_arg(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "fix" | "code-fix" | "fixer" => Some(RoleKind::CodeFixer),
            "summarize" | "summary" | "doc" => Some(RoleKind::DocSummarizer),
            "test" | "tests" | "testgen" => Some(RoleKind::TestGenerator),
            _ => None,
        }
    }
}

/// L-3/L-4/L-5: 通用角色调用（流式）— 复用 system_prompt_override + 可选 prefix
pub async fn send_role_message_streaming(
    handle: &EngineHandle,
    role: RoleKind,
    content: &str,
    stream_tx: tokio::sync::mpsc::UnboundedSender<abacus_core::llm::stream::StreamChunk>,
    mut req_ctx: abacus_core::core::RequestContext,
) -> ApiResult<EngineResponse> {
    // 工具白名单：复用 Planner/Reviewer 的只读集合
    req_ctx.tool_filter = Some(planner_tool_whitelist());
    // V35-2: 角色 system prompt 走独立通道
    req_ctx.system_prompt_override = Some(role.system_prompt().to_string());
    // V35-1: prefix completion（按角色按需启用）
    if let Some(prefix) = role.prefix_content() {
        req_ctx.prefix_assistant_content = Some(prefix.to_string());
    }
    send_chat_message_streaming(handle, content, stream_tx, req_ctx).await
}

/// L-3/L-4/L-5: 通用角色调用（非流式）
pub async fn send_role_message(
    handle: &EngineHandle,
    role: RoleKind,
    content: &str,
    mut req_ctx: abacus_core::core::RequestContext,
) -> ApiResult<EngineResponse> {
    req_ctx.tool_filter = Some(planner_tool_whitelist());
    req_ctx.system_prompt_override = Some(role.system_prompt().to_string());
    if let Some(prefix) = role.prefix_content() {
        req_ctx.prefix_assistant_content = Some(prefix.to_string());
    }
    send_chat_message(handle, content, req_ctx).await
}

// ─── Team Mode API ─────────────────────────────────────────────────────

/// Team 模式消息处理 — Leader 分解 + SubAgent 执行
///
/// ## 流程
/// 1. 首次调用：用 message 作为 goal 创建 TeamSession
/// 2. Leader 通过 LLM 分解目标为 TaskSpec 列表
/// 3. 按角色分派就绪任务，执行后合并结果
///
/// ## 返回
/// 合并所有 subtask 结果为单个 EngineResponse
///
/// ## A3 修复：整体 600s overall timeout
/// 之前没有任何超时，LLM 卡死时 UI 永远等待。Team 涉及多次 LLM 调用，
/// budget 比 chat 的 300s 更宽（600s）；超时触发后 UI 立即收到错误并解锁，
/// plan 阶段用 cancellable 版本可真正中断 reqwest，execute_ready_tasks 由
/// orchestrator 内部决定（外层 timeout 至少保证 UI 不卡）。
/// 发送 Team 模式消息（Leader 分解 + SubAgent 并行执行）
///
/// ## 引用关系
/// - 调用方: run.rs Team spawn
/// - stream_tx: 进度通知推送到 TUI 更新 state.tasks 面板
///
/// ## 生命周期
/// - stream_tx clone 自 run.rs 主 channel，send_team_message 结束后 clone drop
pub async fn send_team_message(
    handle: &EngineHandle,
    message: &str,
    stream_tx: tokio::sync::mpsc::UnboundedSender<abacus_core::llm::stream::StreamChunk>,
) -> ApiResult<EngineResponse> {
    let _permit = match handle.inflight_guard.try_acquire() {
        Ok(p) => p,
        Err(_) => return ApiResult::Err("请求过于频繁，请等待上一个请求完成".to_string()),
    };

    let cancel = tokio_util::sync::CancellationToken::new();
    let cancel_for_work = cancel.clone();

    let work = async move {
        // V28.7: 二次消息状态修复——Completed/Failed 时重置 team 让新消息重新走 Planning
        //
        // 引用关系：
        //   - 旧实现仅 status==Created 走 Planning → Completed/Failed 后用户再发消息无回应
        //   - 新实现：终态（Completed/Failed）时 reset_team() 后重新 ensure，新消息=新 goal
        // 状态机规则（abacus-orchestrator/src/team/mod.rs:113）:
        //   Created → Planning → Executing → Reviewing → Completed
        //   any → Failed (各阶段)
        let team = handle.ensure_team_session(message).await;
        let initial_status = team.status().await;
        let team = if matches!(initial_status, TeamStatus::Completed { .. } | TeamStatus::Failed { .. }) {
            handle.reset_team().await;
            handle.ensure_team_session(message).await
        } else {
            team
        };

        // Phase 1: Leader 用 LLM 分解任务（首次或需要补充时）
        let status = team.status().await;
        if matches!(status, TeamStatus::Created) {
            // Transition to Planning
            if let Err(e) = team.transition_to(TeamStatus::Planning).await {
                return ApiResult::Err(format!("状态转换失败: {}", e));
            }
            team.emit(TeamEvent::PlanningStarted { task_count: 0 });

            // ── 进度通知: planning 阶段开始 ──
            let _ = stream_tx.send(abacus_core::llm::stream::StreamChunk::TeamProgress {
                phase: "planning".into(),
                tasks: vec![],
            });

            // 使用 CoreLoop 让 Leader 分解目标
            let plan_prompt = format!(
                "You are a team Leader. Decompose the following goal into 2-5 concrete subtasks.\n\
                 For each subtask, output a numbered list:\n\
                 1. [task description]\n\
                 2. [task description]\n\
                 ...\n\n\
                 Goal: {}",
                message
            );
            let plan_session = abacus_core::core::SessionState::new("_planning");
            let plan_session = tokio::sync::RwLock::new(plan_session);
            // A3：plan 阶段用 cancellable，与外层 timeout 联动真正中断
            let plan_result = handle.core
                .process_turn_cancellable(&plan_prompt, &plan_session, cancel_for_work.clone())
                .await;
            match plan_result {
                Ok(result) => {
                    // 解析 LLM 输出为 TaskSpec（简单按行拆分）
                    let tasks = parse_task_list(&result.response, message);
                    for task in &tasks {
                        team.add_task(task.clone()).await;
                    }
                    team.emit(TeamEvent::PlanningStarted { task_count: tasks.len() });

                    // ── TextDelta: 展示 Leader 分工结果 ──
                    {
                        let mut assign_text = String::from("已分解任务并指派：\n");
                        for (i, task) in tasks.iter().enumerate() {
                            assign_text.push_str(&format!(" → agent_{}: {}\n", i, task.description));
                        }
                        assign_text.push('\n');
                        let _ = stream_tx.send(abacus_core::llm::stream::StreamChunk::TextDelta(assign_text));
                    }

                    // ── 进度通知: planning 完成，tasks 已入看板（全部 pending）──
                    let task_infos: Vec<abacus_core::llm::stream::TeamTaskInfo> = tasks.iter().map(|t| {
                        abacus_core::llm::stream::TeamTaskInfo {
                            id: t.id.clone(),
                            title: t.description.clone(),
                            status: "pending".into(),
                            output_preview: None,
                        }
                    }).collect();
                    let _ = stream_tx.send(abacus_core::llm::stream::StreamChunk::TeamProgress {
                        phase: "executing".into(),
                        tasks: task_infos,
                    });
                }
                Err(e) => {
                    let _ = team.transition_to(TeamStatus::Failed {
                        reason: e.to_string(),
                    }).await;
                    return ApiResult::Err(format!("Leader 分解任务失败: {}", e));
                }
            }
        }

        // Phase 2: Execute ready tasks — 逐个执行并实时推送进度
        let _ = team.transition_to(TeamStatus::Executing {
            active_tasks: 0, completed_tasks: 0,
        }).await;

        let ready_by_role = team.ready_tasks_by_role().await;
        let mut results: Vec<(String, String)> = Vec::new();

        for (role, tasks) in &ready_by_role {
            for (idx, task) in tasks.iter().enumerate() {
                // ── 进度通知: agent 开始执行 ──
                let agent_label = format!("agent_{}", idx);
                let _ = stream_tx.send(abacus_core::llm::stream::StreamChunk::TextDelta(
                    format!("⚙ {} 开始执行...\n", agent_label),
                ));

                // 推送 TeamProgress: 当前 task → running
                {
                    use abacus_orchestrator::team::TaskStatus as TTS;
                    let all_tasks = team.list_tasks().await;
                    let task_infos: Vec<abacus_core::llm::stream::TeamTaskInfo> = all_tasks.iter().map(|t| {
                        let is_current = t.spec.id == task.id;
                        let status_str = if is_current {
                            "running"
                        } else {
                            match &t.status {
                                TTS::Pending | TTS::Blocked { .. } => "pending",
                                TTS::Assigned { .. } | TTS::Running { .. } => "running",
                                TTS::Completed { .. } => "done",
                                TTS::Failed { .. } => "failed",
                            }
                        };
                        abacus_core::llm::stream::TeamTaskInfo {
                            id: t.spec.id.clone(),
                            title: t.spec.description.clone(),
                            status: status_str.into(),
                            output_preview: None,
                        }
                    }).collect();
                    let _ = stream_tx.send(abacus_core::llm::stream::StreamChunk::TeamProgress {
                        phase: "executing".into(),
                        tasks: task_infos,
                    });
                }

                // ── SubAgent 分隔标识: 开始 ──
                let _ = stream_tx.send(abacus_core::llm::stream::StreamChunk::TextDelta(
                    format!("\n── {} ({:?}) ─────────────────────────\n", agent_label, role),
                ));

                // 执行任务（流式: agent 的 thinking/tool/text 实时流入主消息区）
                match team.execute_task_with_core_streaming(
                    &handle.core, task, role, stream_tx.clone()
                ).await {
                    Ok(r) => {
                        // ── SubAgent 分隔标识: 完成 ──
                        let _ = stream_tx.send(abacus_core::llm::stream::StreamChunk::TextDelta(
                            format!("\n── {} 完成 ──────────────────────────\n", agent_label),
                        ));
                        results.push((task.id.clone(), r));
                    }
                    Err(e) => {
                        // ── SubAgent 分隔标识: 失败 ──
                        let _ = stream_tx.send(abacus_core::llm::stream::StreamChunk::TextDelta(
                            format!("\n── {} 失败: {} ──────────────────\n", agent_label, e),
                        ));
                        tracing::warn!("Task {} failed: {}", task.id, e);
                    }
                }

                // ── 进度通知: task 完成后推送最新状态 ──
                {
                    use abacus_orchestrator::team::TaskStatus as TTS;
                    let all_tasks = team.list_tasks().await;
                    let task_infos: Vec<abacus_core::llm::stream::TeamTaskInfo> = all_tasks.iter().map(|t| {
                        let status_str = match &t.status {
                            TTS::Pending | TTS::Blocked { .. } => "pending",
                            TTS::Assigned { .. } | TTS::Running { .. } => "running",
                            TTS::Completed { .. } => "done",
                            TTS::Failed { .. } => "failed",
                        };
                        let preview = if let TTS::Completed { result } = &t.status {
                            let s = result.to_string();
                            if s.chars().count() > 100 {
                                Some(format!("{}...", s.chars().take(97).collect::<String>()))
                            } else { Some(s) }
                        } else {
                            None
                        };
                        abacus_core::llm::stream::TeamTaskInfo {
                            id: t.spec.id.clone(),
                            title: t.spec.description.clone(),
                            status: status_str.into(),
                            output_preview: preview,
                        }
                    }).collect();
                    let phase = if all_tasks.iter().all(|t| matches!(t.status, TTS::Completed { .. } | TTS::Failed { .. })) {
                        "completed"
                    } else {
                        "executing"
                    };
                    let _ = stream_tx.send(abacus_core::llm::stream::StreamChunk::TeamProgress {
                        phase: phase.into(),
                        tasks: task_infos,
                    });
                }
            }
        }

        // Phase 3: Check if all done
        // V28.7: 状态机合法转换路径——Executing → Reviewing → Completed
        //   旧实现单跳 Executing → Completed 被 can_transition_to 拒绝（line 113-124）
        //   silent ignore 后状态卡 Executing，下一条消息进死结
        //   修复：显式经 Reviewing 中转，每步 transition 失败时记 warn 但继续
        //         （兜底确保最终态标记，不让流程因状态机分歧而无声 stuck）
        if team.all_tasks_done().await {
            let (total, completed, failed) = team.stats().await;
            let summary = format!("{}/{} 完成, {} 失败", completed, total, failed);
            if let Err(e) = team.transition_to(TeamStatus::Reviewing).await {
                tracing::warn!(error = %e, "Team Executing → Reviewing 转换失败");
            }
            if let Err(e) = team.transition_to(TeamStatus::Completed { summary: summary.clone() }).await {
                tracing::warn!(error = %e, "Team Reviewing → Completed 转换失败");
            }
            team.emit(TeamEvent::TeamCompleted { summary });
        }

        // Phase 4: Leader 汇总 — 综合所有 agent 结果给用户最终回答
        // 连续流程：需求→分解→执行→汇总，无需用户介入
        let _ = stream_tx.send(abacus_core::llm::stream::StreamChunk::TextDelta(
            "\n═══ 汇总 ══════════════════════════════\n".into(),
        ));

        let combined_text = if results.is_empty() {
            // 没有新 agent 结果：可能是对话性跟进问题，或 team 任务已全部提前完成。
            // 用 LLM 直接响应而非固定文本，避免循环输出同一句话。
            let fallback_prompt = format!(
                "You are the Abacus team Leader. The user sent: '{}'\
                 \nAll team tasks have already been completed. \
                 If this is a follow-up question about the completed work, answer it directly. \
                 If this is a new task request, acknowledge it and suggest using /team to start a new session. \
                 Be concise and natural.",
                message
            );
            let fb_session = abacus_core::core::SessionState::new("_team_followup");
            let fb_session = tokio::sync::RwLock::new(fb_session);
            match handle.core.process_turn_streaming(
                &fallback_prompt, &fb_session, stream_tx.clone()
            ).await {
                Ok(r) => r.response,
                Err(_) => "所有任务已完成。可发送新指令继续。".to_string(),
            }
        } else {
            // 构建汇总 prompt：让 Leader 综合各 agent 结果
            let mut agent_outputs = String::new();
            for (task_id, output) in &results {
                agent_outputs.push_str(&format!("## Agent result for task '{}':\n{}\n\n", task_id, output));
            }
            let summary_prompt = format!(
                "You are the team Leader. Your agents have completed their tasks.\n\
                 Original user request: {}\n\n\
                 Agent results:\n{}\n\
                 Now synthesize all agent outputs into a cohesive final answer for the user. \
                 Be concise, highlight key findings, and present actionable conclusions.",
                message, agent_outputs
            );

            // Leader 汇总也走流式——用户实时看到最终回答流入
            let summary_session = abacus_core::core::SessionState::new("_summary");
            let summary_session = tokio::sync::RwLock::new(summary_session);
            match handle.core.process_turn_streaming(
                &summary_prompt, &summary_session, stream_tx.clone()
            ).await {
                Ok(result) => result.response,
                Err(e) => {
                    // 汇总失败时 fallback 到原始拼接
                    tracing::warn!("Leader summary failed: {}, falling back to raw concat", e);
                    results.iter()
                        .map(|(id, out)| format!("### {}\n{}", id, out))
                        .collect::<Vec<_>>()
                        .join("\n\n")
                }
            }
        };

        ApiResult::Ok(EngineResponse {
            text: combined_text,
            thinking: None,
            tool_records: vec![],
            stats: None,
            progressive_state: None,
            inertia_warning: None,
            pending_confirmations: vec![],
            meeting_experts: None,
            auto_fallback_chat: None,
            turnkey_plan: None, needs_clarify: None,
        })
    };

    let timeout = tokio::time::sleep(Duration::from_secs(600));
    tokio::pin!(timeout);

    let result = tokio::select! {
        r = work => r,
        _ = &mut timeout => {
            cancel.cancel();
            ApiResult::Err("Team 任务超时 (600s)，已取消".to_string())
        }
    };

    // V28.7: 异常兜底 — Team 失败时显式 fallback 到 chat（与 Meeting 同策略）
    // 引用关系：
    //   - 触发：plan/execute/timeout 失败
    //   - 兜底：调 send_chat_message 并设 auto_fallback_chat → run.rs 自动切 Chat + toast
    //   - 设计：用户消息不丢；mode 状态对齐
    match result {
        ApiResult::Ok(resp) => ApiResult::Ok(resp),
        ApiResult::Err(team_err) => {
            tracing::warn!(error = %team_err, "Team 路径失败，兜底到 Chat");
            // Gap A 修复：兑底路径不能访问 TUI state，default 即可；thinking 还原为
            // session sticky / config，与原 Team 路径能力表一致。
            match send_chat_message(handle, message, abacus_core::core::RequestContext::default()).await {
                ApiResult::Ok(mut resp) => {
                    resp.auto_fallback_chat = Some(format!("Team 失败：{}", team_err));
                    resp.text = format!(
                        "ℹ️ Team 模式执行失败（{}），已切到 Chat 继续：\n\n{}",
                        team_err, resp.text
                    );
                    ApiResult::Ok(resp)
                }
                ApiResult::Err(chat_err) => ApiResult::Err(format!(
                    "Team 失败({}); Chat 兜底也失败({})",
                    team_err, chat_err
                )),
                other => other,
            }
        }
        other => other,
    }
}

/// Meeting 模式消息处理 — 路由到 Specialist，专家会诊
///
/// ## 流程
/// 1. 首次调用：用 message 作为 topic 创建 MeetingSession
/// 2. 路由输入到匹配的 Specialist
/// 3. Specialist 通过 CoreLoop 推理
/// 4. 记录意见并返回
///
/// ## A4 修复：整体 600s overall timeout
/// 之前没有任何超时；mtg.process 内部调用 LLM，卡死时 UI 永远等待。
/// 加 600s timeout 保证 UI 至少能解锁。Meeting 内部 LLM 调用是否
/// 支持 cancel 由 orchestrator 决定，外层 timeout 是最低保障。
pub async fn send_meeting_message(
    handle: &EngineHandle,
    message: &str,
) -> ApiResult<EngineResponse> {
    let _permit = match handle.inflight_guard.try_acquire() {
        Ok(p) => p,
        Err(_) => return ApiResult::Err("请求过于频繁".to_string()),
    };

    let work = async {
        // Ensure meeting handle exists
        if let Err(e) = handle.ensure_meeting_handle(message).await {
            return ApiResult::Err(format!("Meeting 初始化失败: {}", e));
        }

        let mut mtg_guard = handle.meeting_handle.write().await;
        // 防御性：ensure_meeting_handle 成功理应设置 Some，但显式处理避免 panic
        let mtg = match mtg_guard.as_mut() {
            Some(m) => m,
            None => return ApiResult::Err("Meeting handle 未初始化（ensure 后仍为 None）".into()),
        };

        // V28.7: Start meeting if not already running
        //   - build() 邀请后已推到 Inviting，start() 走合法 Inviting → Running
        //   - 多次调用：第二次 mtg.start() 会失败（Running 不能 → Running），是预期，
        //     用 status 检查替代 silent-ignore Err（避免掩盖真实状态机问题）
        // 引用关系：abacus-orchestrator::meeting::core::MeetingStatus 状态机
        {
            let status = mtg.session().status.clone();
            if !matches!(status, abacus_orchestrator::meeting::core::MeetingStatus::Running) {
                if let Err(e) = mtg.start() {
                    return ApiResult::Err(format!("Meeting 启动失败 (status={:?}): {}", status, e));
                }
            }
        }

        // Execute turn through meeting handle
        match mtg.process(message, &handle.core, &handle.session).await {
            Ok(result) => {
                let specialist_name = result.target_specialist.0.clone();
                let text = format!(
                    "**[{}]** {}\n",
                    specialist_name,
                    result.engine_output
                );

                // V28.7 ── Meeting 专家快照：从 session.participants 提取，映射到 TUI Expert ──
                // 引用关系：
                //   生产者：MeetingSession.participants(BTreeMap<String, SpecialistInstance>)
                //   消费者：run.rs response 处理 → state.experts → render_panel_meeting_agenda
                // 状态映射（SpecialistStatus → ExpertStatus）：
                //   - Speaking | Thinking | AwaitingInput → Active（正在参与）
                //   - Completed | Inactive → Done（已结束发言）
                //   - 其余（Listening/Registered/Invited/Error）→ Idle
                // confidence：specialist 暂无置信度字段，给 0.85 默认值（保留 UI 视觉密度）
                use crate::tui::state::{Expert, ExpertStatus};
                use abacus_orchestrator::specialist::SpecialistStatus;
                let target_id = result.target_specialist.0.clone();
                let experts: Vec<Expert> = mtg.session().participants.values().map(|inst| {
                    let status = match &inst.status {
                        SpecialistStatus::Speaking
                        | SpecialistStatus::Thinking
                        | SpecialistStatus::AwaitingInput => ExpertStatus::Active,
                        SpecialistStatus::Completed
                        | SpecialistStatus::Inactive => ExpertStatus::Done,
                        _ => ExpertStatus::Idle,
                    };
                    // 当前 turn 命中的 specialist 强制标记为 Active（确保 UI 立即可见"谁在说"）
                    let final_status = if inst.id.0 == target_id { ExpertStatus::Active } else { status };
                    Expert {
                        name: inst.name.clone(),
                        domain: inst.specialty.domain.clone(),
                        status: final_status,
                        // V28.7: confidence 0.0 = "未评估"语义——orchestrator 暂无
                        // SpecialistInstance 置信度评估机制（不在 cli 层造启发式伪信号）。
                        // UI 渲染层若 == 0.0 显示 "—" 而不显示百分比；
                        // orchestrator 后续加真实评估时直接填 inst.confidence
                        confidence: 0.0,
                    }
                }).collect();

                ApiResult::Ok(EngineResponse {
                    text,
                    thinking: None,
                    tool_records: vec![],
                    stats: None,
                    progressive_state: None,
                    inertia_warning: None,
                    pending_confirmations: vec![],
                    meeting_experts: Some(experts),
                    auto_fallback_chat: None,
                    turnkey_plan: None, needs_clarify: None,
                })
            }
            Err(e) => ApiResult::Err(format!("Meeting 轮次失败: {}", e)),
        }
    };

    let timeout = tokio::time::sleep(Duration::from_secs(600));
    tokio::pin!(timeout);

    let result = tokio::select! {
        r = work => r,
        _ = &mut timeout => {
            ApiResult::Err("Meeting 轮次超时 (600s)，已放弃".to_string())
        }
    };

    // V29.3: 条件兜底 — 区分"首次启动失败" vs "已建立 session 后失败"
    //
    // 用户场景驱动设计:
    //   ① 用户首次启动 abacus 进入 Meeting → 发第一条消息 → ensure/start 阶段失败
    //      → 此时 Meeting 上下文为空, Chat 是合理降级 → 保住"用户消息不丢"
    //   ② Meeting 已建立, 多轮会诊中某轮失败(process 异常 / timeout)
    //      → 此时 Meeting 上下文已积累价值, fallback 到 Chat 等于把会诊历史扔了
    //      → 应保持 Meeting 模式, 让用户看到错误自主决策(重试/手动切/等)
    //
    // 错误信息前缀约定(send_meeting_message 内部错误格式定义):
    //   "Meeting 初始化失败"  → ensure_meeting_handle 失败 = 首次启动失败 ✓ 兜底
    //   "Meeting handle 未初始化" → 极少见的防御性错误 = 首次启动失败 ✓ 兜底
    //   "Meeting 启动失败" → mtg.start() 失败 = 首次启动失败 ✓ 兜底
    //   "Meeting 轮次失败" → mtg.process() 失败 = 已建立后失败 ✗ 不兜底
    //   "Meeting 轮次超时" → 600s timeout, 大概率 process 阶段 = 已建立后失败 ✗ 不兜底
    //
    // 引用关系: send_meeting_message → ApiResult::Err → 此处分流 →
    //   ① 兜底分支: send_chat_message + auto_fallback_chat → run.rs 切 Chat
    //   ② 直返分支: ApiResult::Err → run.rs 错误 toast, 保持 Meeting
    match result {
        ApiResult::Ok(resp) => ApiResult::Ok(resp),
        ApiResult::Err(meeting_err) => {
            let is_first_start_failure = meeting_err.starts_with("Meeting 初始化失败")
                || meeting_err.starts_with("Meeting handle 未初始化")
                || meeting_err.starts_with("Meeting 启动失败");
            if is_first_start_failure {
                tracing::warn!(error = %meeting_err, "Meeting 首次启动失败, 兜底到 Chat");
                // V29.4: 清掉失败的 meeting_handle, 避免下次切回 Meeting 时
                //   ensure_meeting_handle 看到 is_some 直接 return Ok 复用 zombie handle
                //   引用关系: ensure_meeting_handle line 96 短路 → handle.reset_meeting() 让其失效
                //   生命周期: 兜底瞬间清, 下次 send_meeting_message 调 ensure 时重新 build
                handle.reset_meeting().await;
                // Gap A 修复：Meeting 兑底路径同步传 default RequestContext。
                match send_chat_message(handle, message, abacus_core::core::RequestContext::default()).await {
                    ApiResult::Ok(mut resp) => {
                        resp.auto_fallback_chat = Some(format!("Meeting 启动失败: {}", meeting_err));
                        resp.text = format!(
                            "ℹ️ Meeting 启动失败({}), 已切到 Chat 继续:\n\n{}",
                            meeting_err, resp.text
                        );
                        ApiResult::Ok(resp)
                    }
                    // V29.4 续: 双失败也仍切到 Chat — 用户已经看到"首次进入 Meeting 不可用",
                    //   不应再卡在 Meeting 模式; 用 EngineResponse + auto_fallback_chat 触发 mode 切换,
                    //   text 把两段错误都展示出来便于用户诊断
                    //   引用关系: run.rs response 处理 → auto_fallback_chat=Some → switch_mode(Chat)
                    ApiResult::Err(chat_err) => {
                        tracing::error!(meeting_err = %meeting_err, chat_err = %chat_err, "双失败");
                        ApiResult::Ok(EngineResponse {
                            text: format!(
                                "⚠️ Meeting 启动失败({}); Chat 兜底也失败({})\n请检查模型/网络配置后重试",
                                meeting_err, chat_err
                            ),
                            thinking: None,
                            tool_records: vec![],
                            stats: None,
                            progressive_state: None,
                            inertia_warning: None,
                            pending_confirmations: vec![],
                            meeting_experts: None,
                            auto_fallback_chat: Some("Meeting + Chat 双失败".to_string()),
                            turnkey_plan: None, needs_clarify: None,
                        })
                    }
                    other => other,
                }
            } else {
                // 已建立 session 后失败 — 保持 Meeting 模式, 错误透传给 UI
                ApiResult::Err(meeting_err)
            }
        }
        other => other,
    }
}

/// Meeting 模式流式连续流程 — 多专家顺序发言 + Host 综合结论
///
/// ## 连续流程（用户一条消息触发完整会诊）
/// Phase 1: 初始化会议 + 确定参与专家
/// Phase 2: 各专家顺序流式推理（streaming TextDelta 实时可见）
/// Phase 3: Host 综合所有意见，流式输出最终结论
///
/// ## 引用关系
/// - 消费者: run.rs Meeting 分支 spawn
/// - stream_tx: 与 run.rs 主 channel 同源 clone
/// - MeetingPromptAssembler: abacus-orchestrator::meeting::assembler（组装 per-specialist prompt）
/// - core.process_turn_streaming: 流式推理入口
///
/// ## 生命周期
/// - meeting_handle: ensure 时创建，reset_meeting 时销毁
/// - stream_tx clone: 本函数结束时 drop
pub async fn send_meeting_message_streaming(
    handle: &EngineHandle,
    message: &str,
    stream_tx: tokio::sync::mpsc::UnboundedSender<abacus_core::llm::stream::StreamChunk>,
) -> ApiResult<EngineResponse> {
    // 不获取 inflight_guard：内部子调用 (process_turn_streaming) 直接走 core 层，
    // 由 run.rs dispatch 层保证同一时刻只有一条用户消息在处理

    let work = async {
        // Phase 1: 确保 Meeting 存在并 Running
        if let Err(e) = handle.ensure_meeting_handle(message).await {
            return ApiResult::Err(format!("Meeting 初始化失败: {}", e));
        }

        let mut mtg_guard = handle.meeting_handle.write().await;
        let mtg = match mtg_guard.as_mut() {
            Some(m) => m,
            None => return ApiResult::Err("Meeting handle 未初始化".into()),
        };

        {
            let status = mtg.session().status.clone();
            if !matches!(status, abacus_orchestrator::meeting::core::MeetingStatus::Running) {
                if let Err(e) = mtg.start() {
                    return ApiResult::Err(format!("Meeting 启动失败 (status={:?}): {}", status, e));
                }
            }
        }

        // 2026-05-27: Phase 1.5 — 路由预检：如果输入无法匹配任何专家 → 返回 needs_clarify
        {
            let decision = mtg.session().route_input(message);
            if let abacus_orchestrator::meeting::router::RoutingDecision::NoMatch { suggestion, .. } = decision {
                let clarify_msg = format!(
                    "我无法确定应由哪位专家来处理你的问题。{}\n请更具体地描述你的需求，或使用 @专家名 直接指定。",
                    suggestion
                );
                let _ = stream_tx.send(abacus_core::llm::stream::StreamChunk::TextDelta(clarify_msg.clone()));
                let experts_snapshot: Vec<crate::tui::state::Expert> = mtg.session().participants.values()
                    .map(|sp| crate::tui::state::Expert {
                        name: sp.name.clone(),
                        domain: sp.specialty.domain.clone(),
                        status: crate::tui::state::ExpertStatus::Idle,
                        confidence: 0.0,
                    })
                    .collect();
                return ApiResult::Ok(EngineResponse {
                    text: clarify_msg,
                    meeting_experts: Some(experts_snapshot),
                    needs_clarify: Some(suggestion),
                    ..Default::default()
                });
            }
        }

        // Phase 2: 各专家顺序流式推理
        let participants: Vec<(String, String, String)> = mtg.session().participants.values()
            .map(|sp| (sp.id.0.clone(), sp.name.clone(), sp.specialty.domain.clone()))
            .collect();

        if participants.is_empty() {
            return ApiResult::Err("Meeting 无参与专家".into());
        }

        let _ = stream_tx.send(abacus_core::llm::stream::StreamChunk::TextDelta(
            format!("会诊开始 — {} 位专家参与\n\n", participants.len()),
        ));

        let mut expert_outputs: Vec<(String, String, String)> = Vec::new();

        for (sp_id, sp_name, sp_domain) in &participants {
            use abacus_orchestrator::meeting::assembler::MeetingPromptAssembler;
            use abacus_orchestrator::meeting::router::RoutingMode;

            let sp_instance = match mtg.session().participants.get(sp_id) {
                Some(sp) => sp.clone(),
                None => continue,
            };

            let prompt = MeetingPromptAssembler::assemble_specialist_prompt(
                &mtg.session().topic,
                &mtg.session().participants,
                &mtg.session().context_pool,
                &sp_instance,
                &RoutingMode::Fresh,
            );
            let full_prompt = format!("{}\n\n用户提问: {}", prompt, message);

            let _ = stream_tx.send(abacus_core::llm::stream::StreamChunk::TextDelta(
                format!("\n── {} ({}) ─────────────────────────\n", sp_name, sp_domain),
            ));

            // 流式推理（per-specialist 独立 session）
            let sp_session = abacus_core::core::SessionState::new(&format!("_meeting_{}", sp_id));
            let sp_session = tokio::sync::RwLock::new(sp_session);

            let req_ctx = if let Some(ref model) = sp_instance.preferred_model {
                abacus_core::core::RequestContext::with_model(model)
            } else {
                abacus_core::core::RequestContext::default()
            };

            match handle.core.process_turn_streaming_cancellable_with_context(
                &full_prompt,
                &sp_session,
                stream_tx.clone(),
                req_ctx,
                tokio_util::sync::CancellationToken::new(),
            ).await {
                Ok(result) => {
                    let opinion = abacus_orchestrator::specialist::SpecialistOpinion {
                        specialist_id: abacus_orchestrator::specialist::SpecialistId(sp_id.clone()),
                        turn: mtg.session().context_pool.turn_count() + 1,
                        conclusion: result.response.clone(),
                        confidence: 0.8,
                        reasoning_summary: String::new(),
                        tool_evidence: vec![],
                        suggestions: vec![],
                        requires_attention: vec![],
                        auto_approve: true,
                        host_review_required: false,
                    };
                    let _ = mtg.session_mut().process_opinion(opinion);
                    expert_outputs.push((sp_id.clone(), sp_name.clone(), result.response));

                    let _ = stream_tx.send(abacus_core::llm::stream::StreamChunk::TextDelta(
                        format!("\n── {} 完成 ──────────────────────────\n", sp_name),
                    ));
                }
                Err(e) => {
                    let _ = stream_tx.send(abacus_core::llm::stream::StreamChunk::TextDelta(
                        format!("\n── {} 失败: {} ──────────────────\n", sp_name, e),
                    ));
                    tracing::warn!(specialist = %sp_id, error = %e, "specialist streaming failed");
                }
            }
        }

        // Phase 3: Host 综合结论（流式）
        if expert_outputs.is_empty() {
            return ApiResult::Err("所有专家推理失败".into());
        }

        let _ = stream_tx.send(abacus_core::llm::stream::StreamChunk::TextDelta(
            "\n═══ 综合结论 ══════════════════════════════\n".into(),
        ));

        let mut opinions_summary = String::new();
        for (_, name, output) in &expert_outputs {
            opinions_summary.push_str(&format!("## {} 的观点:\n{}\n\n", name, output));
        }

        let synthesis_prompt = format!(
            "你是会议主持人。以下是各领域专家对用户问题的分析。\n\
             请综合所有专家意见，给出结构化的最终结论。\n\
             突出共识、标注分歧、给出可执行建议。\n\n\
             用户原始问题: {}\n\n\
             各专家观点:\n{}",
            message, opinions_summary
        );

        let host_session = abacus_core::core::SessionState::new("_meeting_host");
        let host_session = tokio::sync::RwLock::new(host_session);

        let final_text = match handle.core.process_turn_streaming(
            &synthesis_prompt, &host_session, stream_tx.clone()
        ).await {
            Ok(result) => result.response,
            Err(e) => {
                tracing::warn!("Host synthesis failed: {}, using raw concat", e);
                opinions_summary
            }
        };

        use crate::tui::state::{Expert, ExpertStatus};
        let experts: Vec<Expert> = mtg.session().participants.values().map(|inst| {
            let has_output = expert_outputs.iter().any(|(id, _, _)| id == &inst.id.0);
            Expert {
                name: inst.name.clone(),
                domain: inst.specialty.domain.clone(),
                status: if has_output { ExpertStatus::Done } else { ExpertStatus::Idle },
                confidence: 0.0,
            }
        }).collect();

        ApiResult::Ok(EngineResponse {
            text: final_text,
            thinking: None,
            tool_records: vec![],
            stats: None,
            progressive_state: None,
            inertia_warning: None,
            pending_confirmations: vec![],
            meeting_experts: Some(experts),
            auto_fallback_chat: None,
            turnkey_plan: None, needs_clarify: None,
        })
    };

    let timeout = tokio::time::sleep(Duration::from_secs(600));
    tokio::pin!(timeout);

    let result = tokio::select! {
        r = work => r,
        _ = &mut timeout => {
            ApiResult::Err("Meeting 会诊超时 (600s)".to_string())
        }
    };

    match result {
        ApiResult::Ok(resp) => ApiResult::Ok(resp),
        ApiResult::Err(meeting_err) => {
            let is_first_start_failure = meeting_err.starts_with("Meeting 初始化失败")
                || meeting_err.starts_with("Meeting handle 未初始化")
                || meeting_err.starts_with("Meeting 启动失败");
            if is_first_start_failure {
                tracing::warn!(error = %meeting_err, "Meeting 首次启动失败, 兜底到 Chat (streaming)");
                handle.reset_meeting().await;
                match send_chat_message_streaming(
                    handle, message, stream_tx,
                    abacus_core::core::RequestContext::default()
                ).await {
                    ApiResult::Ok(mut resp) => {
                        resp.auto_fallback_chat = Some(format!("Meeting 启动失败: {}", meeting_err));
                        ApiResult::Ok(resp)
                    }
                    ApiResult::Err(chat_err) => ApiResult::Err(format!(
                        "Meeting 失败({}); Chat 兜底也失败({})", meeting_err, chat_err
                    )),
                    other => other,
                }
            } else {
                ApiResult::Err(meeting_err)
            }
        }
        other => other,
    }
}

/// Plan 模式流式连续流程 — 规划 + 自动执行 + 汇总
///
/// ## 连续流程（用户一条消息触发完整规划→执行→结果）
/// Phase 1: Planner 分析需求、生成任务列表（流式，用户实时可见分析过程）
/// Phase 2: 解析任务列表后自动进入执行（复用 Team 模式执行能力）
/// Phase 3: Leader 综合执行结果，流式输出最终回答
///
/// ## 引用关系
/// - 消费者: run.rs Plan 分支 spawn
/// - stream_tx: 与 run.rs 主 channel 同源 clone
/// - send_planner_message_streaming: Phase 1 规划（复用）
/// - TeamSession: Phase 2 执行（复用 Team 基础设施）
/// - parse_task_list: 解析 LLM 输出为 TaskSpec
///
/// ## 生命周期
/// - team_session: 本函数内创建，函数结束后由 Arc 引用计数管理
/// - stream_tx clone: 本函数结束时 drop
pub async fn send_plan_and_execute_streaming(
    handle: &EngineHandle,
    message: &str,
    stream_tx: tokio::sync::mpsc::UnboundedSender<abacus_core::llm::stream::StreamChunk>,
    req_ctx: abacus_core::core::RequestContext,
) -> ApiResult<EngineResponse> {
    // 不获取 inflight_guard：内部调用 send_chat_message_streaming 已有 permit 保护，
    // 且后续 team 执行用 process_turn_streaming 直接走 core 层

    let work = async {
        // Phase 1: Planner 分析 + 生成任务列表（流式）
        let _ = stream_tx.send(abacus_core::llm::stream::StreamChunk::TextDelta(
            "规划阶段 — 分析需求并拆解任务...\n\n".into(),
        ));

        // 复用 Planner 流式调用
        let mut plan_req = req_ctx.clone();
        plan_req.tool_filter = Some(planner_tool_whitelist());
        plan_req.prefix_assistant_content = Some(PLANNER_PREFIX_CONTENT.to_string());
        plan_req.system_prompt_override = Some(PLANNER_SYSTEM_PROMPT.to_string());

        let plan_result = send_chat_message_streaming(
            handle, message, stream_tx.clone(), plan_req
        ).await;

        let plan_text = match plan_result {
            ApiResult::Ok(resp) => resp.text,
            ApiResult::Err(e) => return ApiResult::Err(format!("规划失败: {}", e)),
            _ => return ApiResult::Err("规划中断".into()),
        };

        // 从 Planner 输出解析任务列表
        let tasks = parse_task_list(&plan_text, message);
        if tasks.is_empty() {
            // 规划无结构化任务输出，直接返回规划结果
            return ApiResult::Ok(EngineResponse {
                text: plan_text,
                thinking: None,
                tool_records: vec![],
                stats: None,
                progressive_state: None,
                inertia_warning: None,
                pending_confirmations: vec![],
                meeting_experts: None,
                auto_fallback_chat: None,
                turnkey_plan: None, needs_clarify: None,
            });
        }

        // Phase 2: 自动执行（复用 Team 执行逻辑）
        let _ = stream_tx.send(abacus_core::llm::stream::StreamChunk::TextDelta(
            format!("\n执行阶段 — {} 个任务开始执行\n\n", tasks.len()),
        ));

        // 确保 Team session 存在并添加任务
        let team = handle.ensure_team_session(message).await;
        // 重置已有任务（如果是新的 plan 执行）
        let initial_status = team.status().await;
        let team = if matches!(initial_status, TeamStatus::Completed { .. } | TeamStatus::Failed { .. }) {
            handle.reset_team().await;
            handle.ensure_team_session(message).await
        } else {
            team
        };

        if let Err(e) = team.transition_to(TeamStatus::Planning).await {
            tracing::warn!("Plan→Team status transition failed: {}", e);
        }

        for task in &tasks {
            team.add_task(task.clone()).await;
        }

        // 展示任务分配
        {
            let mut assign_text = String::from("已规划任务：\n");
            for (i, task) in tasks.iter().enumerate() {
                assign_text.push_str(&format!(" → task_{}: {}\n", i + 1, task.description));
            }
            assign_text.push('\n');
            let _ = stream_tx.send(abacus_core::llm::stream::StreamChunk::TextDelta(assign_text));
        }

        // 执行
        let _ = team.transition_to(TeamStatus::Executing {
            active_tasks: 0, completed_tasks: 0,
        }).await;

        let ready_by_role = team.ready_tasks_by_role().await;
        let mut results: Vec<(String, String)> = Vec::new();

        for (role, role_tasks) in &ready_by_role {
            for (idx, task) in role_tasks.iter().enumerate() {
                let agent_label = format!("task_{}", idx + 1);
                let _ = stream_tx.send(abacus_core::llm::stream::StreamChunk::TextDelta(
                    format!("⚙ {} 开始执行...\n", agent_label),
                ));

                let _ = stream_tx.send(abacus_core::llm::stream::StreamChunk::TextDelta(
                    format!("\n── {} ({:?}) ─────────────────────────\n", agent_label, role),
                ));

                match team.execute_task_with_core_streaming(
                    &handle.core, task, role, stream_tx.clone()
                ).await {
                    Ok(r) => {
                        let _ = stream_tx.send(abacus_core::llm::stream::StreamChunk::TextDelta(
                            format!("\n── {} 完成 ──────────────────────────\n", agent_label),
                        ));
                        results.push((task.id.clone(), r));
                    }
                    Err(e) => {
                        let _ = stream_tx.send(abacus_core::llm::stream::StreamChunk::TextDelta(
                            format!("\n── {} 失败: {} ──────────────────\n", agent_label, e),
                        ));
                        tracing::warn!("Plan task {} failed: {}", task.id, e);
                    }
                }
            }
        }

        // 状态转换
        if team.all_tasks_done().await {
            let (total, completed, failed) = team.stats().await;
            let summary = format!("{}/{} 完成, {} 失败", completed, total, failed);
            let _ = team.transition_to(TeamStatus::Reviewing).await;
            let _ = team.transition_to(TeamStatus::Completed { summary }).await;
        }

        // Phase 3: Leader 综合结果
        let _ = stream_tx.send(abacus_core::llm::stream::StreamChunk::TextDelta(
            "\n═══ 汇总 ══════════════════════════════\n".into(),
        ));

        let final_text = if results.is_empty() {
            "所有任务执行失败，请检查上方错误信息。".to_string()
        } else {
            let mut agent_outputs = String::new();
            for (task_id, output) in &results {
                agent_outputs.push_str(&format!("## Task '{}':\n{}\n\n", task_id, output));
            }
            let summary_prompt = format!(
                "You are the team Leader. Tasks have been planned and executed.\n\
                 Original user request: {}\n\n\
                 Task results:\n{}\n\
                 Synthesize all results into a cohesive final answer. Be concise and actionable.",
                message, agent_outputs
            );

            let summary_session = abacus_core::core::SessionState::new("_plan_summary");
            let summary_session = tokio::sync::RwLock::new(summary_session);
            match handle.core.process_turn_streaming(
                &summary_prompt, &summary_session, stream_tx.clone()
            ).await {
                Ok(result) => result.response,
                Err(e) => {
                    tracing::warn!("Plan summary failed: {}", e);
                    results.iter()
                        .map(|(id, out)| format!("### {}\n{}", id, out))
                        .collect::<Vec<_>>()
                        .join("\n\n")
                }
            }
        };

        ApiResult::Ok(EngineResponse {
            text: final_text,
            thinking: None,
            tool_records: vec![],
            stats: None,
            progressive_state: None,
            inertia_warning: None,
            pending_confirmations: vec![],
            meeting_experts: None,
            auto_fallback_chat: None,
            turnkey_plan: None, needs_clarify: None,
        })
    };

    let timeout = tokio::time::sleep(Duration::from_secs(900)); // Plan+Execute 更长超时
    tokio::pin!(timeout);

    let result = tokio::select! {
        r = work => r,
        _ = &mut timeout => {
            ApiResult::Err("Plan+Execute 超时 (900s)".to_string())
        }
    };

    result
}

/// 从 LLM 输出解析任务列表（简单行号格式）
fn parse_task_list(llm_output: &str, _goal: &str) -> Vec<abacus_orchestrator::team::TaskSpec> {
    let mut tasks = Vec::new();
    for (i, line) in llm_output.lines().enumerate() {
        let trimmed = line.trim();
        // 匹配 "1. xxx" 或 "- xxx" 格式
        let desc = if let Some(rest) = trimmed.strip_prefix(|c: char| c.is_ascii_digit()) {
            rest.trim_start_matches(|c: char| c == '.' || c == ')' || c.is_whitespace())
        } else if let Some(rest) = trimmed.strip_prefix('-') {
            rest.trim()
        } else {
            continue;
        };
        if desc.is_empty() || desc.len() < 5 {
            continue;
        }
        tasks.push(abacus_orchestrator::team::TaskSpec {
            id: format!("task_{}", i + 1),
            description: desc.to_string(),
            required_capabilities: vec![],
            allowed_tools: vec![],
            priority: i as u32,
            depends_on: vec![],
            required_role: None,
            needs_review: false,
        });
    }
    // Fallback: if no structured tasks found, create single task from full output
    if tasks.is_empty() && !llm_output.trim().is_empty() {
        tasks.push(abacus_orchestrator::team::TaskSpec {
            id: "task_1".into(),
            description: llm_output.trim().chars().take(200).collect(),
            required_capabilities: vec![],
            allowed_tools: vec![],
            priority: 0,
            depends_on: vec![],
            required_role: None,
            needs_review: false,
        });
    }
    tasks
}

/// 从 session 提取最后一条 assistant 消息的 reasoning_content
async fn extract_thinking(session: &Arc<RwLock<SessionState>>) -> Option<String> {
    let s = session.read().await;
    let msgs = s.messages.read().await;
    msgs.iter().rev()
        .find(|m| matches!(m.role, abacus_core::llm::MessageRole::Assistant))
        .and_then(|m| m.reasoning_content.clone())
}

/// 获取会话列表
pub async fn list_sessions(handle: &EngineHandle) -> ApiResult<Vec<String>> {
    let s = handle.session.read().await;
    ApiResult::Ok(vec![s.session_id.clone()])
}

/// 创建新会话 — 写锁保护防止并发 chat 读到半替换状态
pub async fn create_session(handle: &EngineHandle, name: &str) -> ApiResult<String> {
    let session_id = format!("tui_{}_{}", name, chrono::Utc::now().timestamp_millis());
    let new_session = SessionState::new(&session_id);
    handle.core.register_session_context_tools(&new_session).await;
    let _guard = handle.session_swap_lock.write().await;
    *handle.session.write().await = new_session;
    ApiResult::Ok(session_id)
}

/// 模型列表 — 查询引擎注册的 provider
pub async fn list_models(handle: &EngineHandle) -> ApiResult<Vec<String>> {
    ApiResult::Ok(handle.core.list_models().await)
}

/// 列出当前工作目录下的文件和子目录（用于路径补全）。
/// 只返回当前 token 匹配前缀的条目，上限 30。
pub async fn list_cwd_files(prefix: &str) -> ApiResult<Vec<String>> {
    let dir = prefix.rfind('/').map(|i| &prefix[..i + 1]).unwrap_or("./");
    let file_prefix = prefix.rfind('/').map(|i| &prefix[i + 1..]).unwrap_or(prefix);

    let mut names = Vec::new();
    if let Ok(mut entries) = tokio::fs::read_dir(dir).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with(file_prefix) || file_prefix.is_empty() {
                let is_dir = entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false);
                names.push(if is_dir { format!("{}/", name) } else { name });
            }
            if names.len() >= 30 { break; }
        }
    }
    ApiResult::Ok(names)
}

/// AI 按需补全 — 使用独立 session，不污染主对话历史。
/// Ctrl+Tab 触发。超时 3 秒自动取消。
///
/// A7 修复：与 A1/A2 一致改用 process_turn_cancellable + 主动 cancel——
/// 之前 tokio::time::timeout drop future 不真正中断 reqwest，
/// 用户看到 3s 超时但底层 LLM 请求继续跑（最长到 provider 自身超时），
/// 烧 token + 占网络。现在 timeout 触发 cancel.cancel() 真正释放连接。
pub async fn ai_complete(
    handle: &EngineHandle,
    prefix: &str,
) -> ApiResult<String> {
    let prompt = format!(
        "Complete the following input naturally and concisely. \
         Only output the completion, nothing else. Do not repeat the prefix.\n\n\
         Input: {prefix}"
    );
    // 创建独立的临时 session 避免污染主对话上下文
    let temp_session_id = format!("_completion_{}", chrono::Utc::now().timestamp_millis());
    let temp_session = abacus_core::core::SessionState::new(&temp_session_id);
    let temp_session = tokio::sync::RwLock::new(temp_session);

    let cancel = tokio_util::sync::CancellationToken::new();
    let work = handle.core.process_turn_cancellable(&prompt, &temp_session, cancel.clone());
    let timeout = tokio::time::sleep(Duration::from_secs(3));
    tokio::pin!(timeout);

    let result = tokio::select! {
        r = work => r,
        _ = &mut timeout => {
            cancel.cancel(); // 真正中断 in-flight reqwest，停止扣费
            return ApiResult::Err("AI 补全超时 (3s)，已取消请求".to_string());
        }
    };

    match result {
        Ok(r) => ApiResult::Ok(r.response.trim().to_string()),
        Err(e) => ApiResult::Err(e.to_string()),
    }
}

// ─── EngineResponse — 完整承载 TurnResult 产出 ───────────────────────────

/// 引擎完整响应，承载 TurnResult 的全部结构化数据
#[derive(Debug, Clone)]
pub struct EngineResponse {
    pub text: String,
    pub thinking: Option<String>,
    pub tool_records: Vec<ToolRecord>,
    pub stats: Option<TurnStats>,
    pub progressive_state: Option<abacus_types::progressive::ProgressiveState>,
    pub inertia_warning: Option<String>,
    /// MCIP 待用户授权的工具列表
    ///
    /// 非空时：TUI 展示 ConfirmDialog，用户选择后触发 grant_and_rerun()
    /// 空时：turn 正常完成
    pub pending_confirmations: Vec<abacus_core::mcip::McipConfirmRequest>,
    /// V28.7: 异常兜底信号——非空时让 run loop 自动切到 Chat 模式 + toast 显示原因
    /// 引用关系：
    ///   - 生产者：send_meeting_message 在 meeting 失败 fallback 到 chat 时填充
    ///             （也可被 send_team_message 等扩展未来使用）
    ///   - 消费者：run.rs response 处理 → switch_mode(Chat) + add_toast
    /// 生命周期：单条 response 抵达即消费；不持有
    pub auto_fallback_chat: Option<String>,
    /// V28.7: Meeting 模式专属——参会专家快照（仅 send_meeting_message 路径填充）
    ///
    /// 引用关系：
    ///   - 生产者：send_meeting_message 内 mtg.session().participants 提取
    ///   - 消费者：run.rs response 处理：写入 state.experts → render_panel_meeting_agenda
    /// 生命周期：随每条 EngineResponse 创建并消费；不持有
    /// 设计：可选——非 Meeting 路径恒为 None，避免污染 chat/team 状态
    pub meeting_experts: Option<Vec<crate::tui::state::Expert>>,
    /// V29.10 (C4-Phase2): Turnkey plan_from_nl 产出的 TaskSpec
    ///
    /// 引用关系:
    ///   - 生产者: SlashCommand::TurnkeyPlan 异步 dispatch 完成后填充
    ///   - 消费者: run.rs response 处理 → 写入 state.pending_turnkey_plan,
    ///             用户随后用 /turnkey execute 触发实际执行
    /// 生命周期: 随 EngineResponse 创建; state 持有直到下一次 plan / execute / clear
    /// 设计: 可选 — 仅 Turnkey 路径填充, 不污染普通 chat 路径
    pub turnkey_plan: Option<abacus_types::sandbox::TaskSpec>,
    /// 2026-05-27: Meeting 路由失败信号 — 需要用户澄清
    ///
    /// ## 引用关系
    /// - 生产者: send_meeting_message_streaming 路由预检返回 NoMatch 时填充
    /// - 消费者: run.rs response 处理 → switch_mode(Clarify) + toast + 保留用户输入
    /// ## 生命周期
    /// 单条 response 消费；非 Meeting 路径恒为 None
    pub needs_clarify: Option<String>,
}

impl Default for EngineResponse {
    fn default() -> Self {
        Self {
            text: String::new(),
            thinking: None,
            tool_records: vec![],
            stats: None,
            progressive_state: None,
            inertia_warning: None,
            pending_confirmations: vec![],
            meeting_experts: None,
            auto_fallback_chat: None,
            turnkey_plan: None,
            needs_clarify: None,
        }
    }
}

impl EngineResponse {
    pub fn from_turn_result(result: TurnResult, thinking: Option<String>) -> Self {
        let tool_records = result.tool_outputs.into_iter().map(|to| ToolRecord {
            name: to.tool_id.0.clone(),
            args: serde_json::to_string(&to.output).unwrap_or_default(),
            status: if to.success { ToolStatus::Success } else { ToolStatus::Failed },
            duration_ms: to.latency_ms as u32,
            time: chrono::Local::now().format("%H:%M").to_string(),
        }).collect();

        let inertia_warning = result.inertia_warning.map(|w| {
            match &w {
                abacus_core::core::inertia::InertiaSignal::ToolAvoidance { .. } =>
                    "回答可能未经工具验证，建议追问要求引用来源".to_string(),
                abacus_core::core::inertia::InertiaSignal::PrematureGiveUp { .. } =>
                    "工具失败后未充分重试，可尝试换个方式描述需求".to_string(),
                abacus_core::core::inertia::InertiaSignal::IncompleteTask { .. } =>
                    "任务可能未完全完成，可发送「请继续」".to_string(),
                abacus_core::core::inertia::InertiaSignal::UncertaintyAvoidance { .. } =>
                    "回答含不确定表述但未查证，可要求验证".to_string(),
                abacus_core::core::inertia::InertiaSignal::ShallowResponse { .. } =>
                    "回答可能过于简短，可要求详细展开".to_string(),
            }
        });

        Self {
            text: result.response,
            thinking,
            tool_records,
            stats: Some(result.stats),
            progressive_state: result.progressive_state,
            inertia_warning,
            pending_confirmations: result.pending_confirmations,
            meeting_experts: None,  // 非 meeting 路径恒 None；Meeting 路径在 send_meeting_message 内单独填充
            auto_fallback_chat: None,  // V28.7: 默认无兜底，send_meeting_message 失败 fallback 时单独设置
            turnkey_plan: None, needs_clarify: None,  // V29.10: 非 turnkey 路径恒 None；TurnkeyPlan 分支在 execute_slash_command 内填充
        }
    }
}

// ─── 支持类型 ────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ToolRecord {
    pub name: String,
    pub args: String,
    pub status: ToolStatus,
    pub duration_ms: u32,
    pub time: String,
}

// V28 Trace 重构: 加 serde derive 让 ToolStatus 能被 TraceKind::ToolCall 嵌入序列化
// 引用关系: 被 state::TraceKind::ToolCall 持有, 通过 SessionExport 写入 latest.json
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum ToolStatus {
    Success,
    Failed,
    Running,
}

// V38-1: parse_review_report 单元测试
//   覆盖：① 标准格式（verdict: pass/fail/needs_revision + 编号 issues）
//        ② 关键词回退（中文 verdict / 缺 [verdict] 前缀）
//        ③ 容错（多种编号风格 / 大小写）
#[cfg(test)]
mod review_parse_tests {
    use super::{parse_review_report, ReviewVerdict};

    #[test]
    fn parse_standard_pass() {
        let text = "[verdict: pass]\n1. 无明显问题";
        let r = parse_review_report(text);
        assert_eq!(r.verdict, ReviewVerdict::Pass);
        assert_eq!(r.issues.len(), 1);
        assert_eq!(r.issues[0].description, "无明显问题");
    }

    #[test]
    fn parse_standard_needs_revision_with_issues() {
        let text = "[verdict: needs_revision]\n1. task[0] 缺 description\n2. phase 数过多";
        let r = parse_review_report(text);
        assert_eq!(r.verdict, ReviewVerdict::NeedsRevision);
        assert_eq!(r.issues.len(), 2);
        assert_eq!(r.issues[1].description, "phase 数过多");
    }

    #[test]
    fn parse_fail_uppercase() {
        let text = "[Verdict: FAIL]\n1. critical leak";
        let r = parse_review_report(text);
        assert_eq!(r.verdict, ReviewVerdict::Fail);
    }

    #[test]
    fn parse_chinese_keyword_fallback() {
        let text = "审查结果：通过审查\n1. 无明显问题";
        let r = parse_review_report(text);
        assert_eq!(r.verdict, ReviewVerdict::Pass);
    }

    #[test]
    fn parse_unknown_when_no_signals() {
        let text = "随便一段没有 verdict 标记的文本";
        let r = parse_review_report(text);
        assert_eq!(r.verdict, ReviewVerdict::Unknown);
        assert_eq!(r.issues.len(), 0);
    }

    #[test]
    fn parse_chinese_punct_numbering() {
        let text = "[verdict: needs_revision]\n1、缺 description\n2)粒度过细";
        let r = parse_review_report(text);
        assert_eq!(r.issues.len(), 2);
    }

    #[test]
    fn parse_preserves_raw_output() {
        let text = "[verdict: pass]\n1. ok";
        let r = parse_review_report(text);
        assert_eq!(r.raw_output, text);
    }

    #[test]
    fn verdict_is_pass_only_for_pass() {
        assert!(ReviewVerdict::Pass.is_pass());
        assert!(!ReviewVerdict::NeedsRevision.is_pass());
        assert!(!ReviewVerdict::Fail.is_pass());
        assert!(!ReviewVerdict::Unknown.is_pass());
    }

    // V38-4: prefix completion 残留风格 — DeepSeek prefix 后模型输出无 [verdict: 起头
    // parser 应识别 "pass]" / "fail]" / "needs_revision]" 等首 token 即 verdict 词的格式

    #[test]
    fn parse_prefix_residue_pass() {
        // prefix = "[verdict: " 被消耗，模型只输出后续 token
        let text = "pass]\n1. 无明显问题";
        let r = parse_review_report(text);
        assert_eq!(r.verdict, ReviewVerdict::Pass);
        assert_eq!(r.issues.len(), 1);
    }

    #[test]
    fn parse_prefix_residue_fail() {
        let text = "fail]\n1. 严重权限错";
        let r = parse_review_report(text);
        assert_eq!(r.verdict, ReviewVerdict::Fail);
    }

    #[test]
    fn parse_prefix_residue_needs_revision() {
        let text = "needs_revision]\n1. 缺 description";
        let r = parse_review_report(text);
        assert_eq!(r.verdict, ReviewVerdict::NeedsRevision);
    }

    #[test]
    fn parse_prefix_residue_no_bracket() {
        // 即便模型忘了 ]
        let text = "pass\n1. ok";
        let r = parse_review_report(text);
        assert_eq!(r.verdict, ReviewVerdict::Pass);
    }
}
