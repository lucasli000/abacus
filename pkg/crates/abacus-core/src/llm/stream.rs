//! V0.2 Streaming types for progressive LLM output delivery.
//!
//! ## 层级
//! - `StreamEvent`: Provider 层产出（SSE 解析后）
//! - `StreamChunk`: TUI/Server 消费的高层事件
//!
//! ## 生命周期
//! - 创建: pipeline 构建 stream channel 时
//! - 激活: execute_loop 调用 stream_complete 后
//! - 销毁: Done 事件发送后 tx drop

use abacus_types::TurnStats;

/// LLM provider 层产出的流式事件（对应 SSE data frames）。
///
/// ## 引用关系
/// - 生产者: LlmProvider::stream_complete() 实现
/// - 消费者: TurnPipeline::execute_loop (聚合为 StreamChunk)
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// 正文文本增量
    TextDelta(String),
    /// 思考过程增量（仅支持思考的模型）
    ThinkingDelta(String),
    /// 工具调用开始
    ToolCallStart { id: String, name: String },
    /// 工具调用参数增量
    ToolCallArgDelta { id: String, delta: String },
    /// 工具调用声明结束
    ToolCallEnd { id: String },
    /// 用量统计（流结束时）
    Usage { prompt_tokens: u64, completion_tokens: u64 },
    /// Provider 层错误（stream 中断或 HTTP 失败时发出，pipeline 转发为 StreamChunk::Error）
    /// - 生产者: LlmProvider::stream_complete() 遇到不可恢复错误时
    /// - 消费者: TurnPipeline::execute_loop → 转发为 StreamChunk::Error
    Error(String),
    /// 流结束信号
    Done,
}

/// TUI/Server 消费的高层流式块。
///
/// ## 引用关系
/// - 生产者: TurnPipeline (从 StreamEvent 聚合/转发)
/// - 消费者: TUI run loop (更新 streaming state)、HTTP SSE handler
///
/// ## 设计
/// 比 StreamEvent 更粗粒度——合并了工具执行结果、统计等信息，
/// TUI 不需要关心 tool arg delta。
#[derive(Debug, Clone)]
pub enum StreamChunk {
    /// 新一轮 LLM 调用开始（execute_loop 迭代边界）
    /// TUI 收到后清空 streaming_thinking，准备接收新一轮内容
    IterationStart { iteration: u32 },
    /// 思考过程增量文本
    Thinking(String),
    /// 正文增量文本
    TextDelta(String),
    /// 工具开始执行
    ToolStart { name: String },
    /// V29.11: 工具输入参数（紧跟 ToolStart 之后发送）
    /// TUI 用于 fs_edit diff 渲染 — args_json 是 LLM 产出的 JSON 字符串(tc.function.arguments)
    ToolArgs { name: String, args_json: String },
    /// V29.11: 工具输出内容（紧跟 ToolEnd 之前或同时发送）
    /// TUI 用于 trace 展开时显示工具返回
    ToolOutput { name: String, output_json: String },
    /// 工具执行完成
    ///
    /// failure_kind: 失败时的分类标签（Timeout/Panic/Cooldown/NoExecutor/BusinessError 等）
    /// TUI 用于差异化渲染失败原因（可重试 vs 永久失败 vs 环境问题）
    ToolEnd { name: String, success: bool, duration_ms: u64, failure_kind: Option<String> },
    /// V28：实时授权请求——pipeline dispatch 暂停等待用户授权时发出
    /// UI 收到后弹授权对话框；用户决策通过 SessionState.mcip_confirm_channels[nonce] 直发回去
    /// nonce 是从 channel 找回 sender 的 key
    ConfirmRequired(crate::mcip::McipConfirmRequest),
    /// 上下文压缩开始（TUI 切换到 Compacting 工作态）
    CompressStart,
    /// 上下文压缩完成（携带压缩统计）
    CompressEnd { messages_compressed: usize, tokens_saved: usize },
    /// Turn 完成（携带最终统计）
    Complete(TurnStats),
    /// Provider retry 通知（用户在等待重试时可见）
    /// - 生产者: TurnPipeline（从 complete() 的 retry 路径发出，未来可扩展）
    /// - 消费者: TUI run loop（显示 retry 进度）
    RetryProgress { attempt: u32, max_attempts: u32, reason: String },
    /// Team 模式进度通知（SubAgent 执行过程中实时推送）
    ///
    /// ## 引用关系
    /// - 生产者: send_team_message (abacus-cli/src/tui/api/mod.rs)
    /// - 消费者: TUI run loop (更新 state.tasks 面板)
    ///
    /// ## 生命周期
    /// - 创建: send_team_message 各阶段转换时
    /// - 销毁: TUI 消费后立即释放
    TeamProgress {
        phase: String,              // "planning" / "executing" / "reviewing" / "completed"
        tasks: Vec<TeamTaskInfo>,
    },
    /// 工具健康快照 — 每 turn 结束前发送一次（仅含本轮调用过的工具）
    ///
    /// ## 引用关系
    /// - 生产者: pipeline execute_loop 完成后、Complete 之前发送
    /// - 消费者: TUI run loop → 写入 state.tool_health_map
    ///
    /// ## 设计
    /// 轻量：只传本轮工具的 tier + blocked_by_env，不传全量 200+ 工具
    /// TUI 用于：工具名称旁标注 tier badge、blocked 工具灰色警示
    ToolHealth(Vec<ToolHealthEntry>),
    /// 错误（非致命，pipeline 继续；致命错误通过 channel drop 信号）
    Error(String),

    // ─── 预留事件（已定义接口，逐步接入生产者）─────────────────────────

    /// 模型升级通知（LLM 自动从低价模型切换到高能力模型）
    ///
    /// - 生产者: pipeline handle_model_escalation
    /// - 消费者: TUI toast + Web 前端成本提示
    ModelEscalation {
        from_model: String,
        to_model: String,
        reason: String,
    },

    /// Session 焦点更新（LLM 通过 session_set_focus 工具设置）
    ///
    /// - 生产者: session_set_focus 工具执行后
    /// - 消费者: 前端焦点面板、多端同步
    SessionFocusUpdate {
        goal: String,
        phase: String,
        next_step: String,
    },

    /// 工具被环境阻塞详情（比 ToolHealth 更具体的失败原因）
    ///
    /// - 生产者: tool dispatch 失败后的 classify_env_failure
    /// - 消费者: 前端显示"缺少 API key"/"网络超时"等可操作提示
    ToolBlocked {
        tool_id: String,
        kind: String,       // Network/Timeout/Unauthorized/RateLimited/DependencyMissing
        message: String,
        recoverable: bool,
    },

    /// 会议状态变更（Meeting 模式下实时推送）
    ///
    /// - 生产者: MeetingSession 状态机转换
    /// - 消费者: 前端会议面板状态指示器
    MeetingStatusChange {
        meeting_id: String,
        old_status: String,
        new_status: String,
    },

    /// Specialist 思考中间过程（Meeting 模式下）
    ///
    /// - 生产者: MeetingEngineAdapter 调用 Specialist 时
    /// - 消费者: 前端 specialist 卡片的 thinking indicator
    SpecialistThinking {
        specialist_id: String,
        content: String,
    },

    /// Sandbox 执行进度（代码执行/编译/测试）
    ///
    /// - 生产者: sandbox executor
    /// - 消费者: 前端执行进度条
    SandboxProgress {
        phase: String,      // "compiling" / "linking" / "running" / "testing"
        message: String,
    },

    /// 惰性检测通知（LLM 陷入循环时的诊断信息）
    ///
    /// - 生产者: pipeline inertia detector
    /// - 消费者: 前端显示循环警告 + 可选干预按钮
    InertiaDetected {
        signals: Vec<String>,
        recommendation: String,
    },
}

/// 单个工具的健康状态（StreamChunk::ToolHealth 的载荷）
///
/// 引用关系：
/// - 生产者：pipeline 从 EffectivenessTracker 提取
/// - 消费者：TUI state.tool_health_map
#[derive(Debug, Clone)]
pub struct ToolHealthEntry {
    pub tool_id: String,
    /// Visibility tier: "S"/"A"/"B"/"C"/"D"
    pub tier: String,
    /// 环境阻塞标志（如 API key 缺失、网络不可达）
    pub blocked_by_env: bool,
    /// 综合评分 0.0-1.0
    pub score: f64,
}

/// Team 模式任务进度信息（StreamChunk::TeamProgress 的载荷）
///
/// ## 引用关系
/// - 生产者: send_team_message 从 TaskInstance 构建
/// - 消费者: TUI run loop 映射为 state::TaskCard
#[derive(Clone, Debug)]
pub struct TeamTaskInfo {
    pub id: String,
    pub title: String,
    pub status: String,             // "pending" / "running" / "done" / "failed"
    pub output_preview: Option<String>,  // 前 100 字符结果预览
}
