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
    /// 压缩后自动续行（Execution 阶段，phase=execution 时发出）
    ///
    /// TUI 收到后：① 显示"压缩完成，继续执行" toast
    ///             ② 检查 pending_compress_input，有则自动发送；无则发送续行提示
    /// 引用关系：pipeline post_process → checkpoint.overall_phase == Execution 时发出
    /// 生命周期：单次消费，TUI 处理后清空 pending_compress_input
    CompressAutoResume,
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
    /// 授权结果通知（工具授权通过或拒绝后发出）
    ///
    /// - 生产者: pipeline execute_loop MCIP 确认路径
    /// - 消费者: TUI 显示 toast 通知，不产生假工具 trace
    AuthResult {
        tool: String,
        approved: bool,
    },
    /// 错误（非致命，pipeline 继续；致命错误通过 channel drop 信号）
    Error(String),
    /// 长时间操作提示（pipeline 在工具执行耗时超过阈值时发送）
    ///
    /// ## 引用关系
    /// - 生产者: TurnPipeline 工具 dispatch 超过耗时阈值时
    /// - 消费者: TUI toast + HTTP SSE `long_operation` 事件
    LongOperation { tool_name: String, estimated_secs: u64 },

    /// V41: ToolAgent 批量执行结果 — 替代多个 ToolStart/ToolEnd 刷屏
    ///
    /// ## 引用关系
    /// - 生产者: pipeline tool dispatch 检测到 ToolAgent 匹配后批量执行完毕
    /// - 消费者: TUI 消息流渲染（折叠展示：图标+名称+数量+摘要）
    ///
    /// ## 设计意图
    /// 主消息流只看到一条汇总，详情可折叠展开查看每个工具的独立输出
    ToolAgentResult {
        /// ToolAgent ID（如 "explorer", "coder"）
        agent_id: String,
        /// 显示图标
        icon: String,
        /// 显示名
        name: String,
        /// 执行的工具调用数
        call_count: usize,
        /// 一句话摘要（首个输出的前 100 字符）
        summary: String,
        /// 各工具输出详情（折叠可展开）
        details: Vec<String>,
    },

    /// 流中断后重试——TUI 收到后清除当前 iteration 的已渲染流式内容
    ///
    /// ## 引用关系
    /// - 生产者: TurnPipeline::execute_loop 流式错误重试前
    /// - 消费者: TUI run loop → 清空 streaming_timeline 中当前 iteration 的 TextDelta/Thinking
    ///
    /// ## 设计
    /// 重试意味着 LLM 将重新生成完整响应，之前部分接收的内容不可用。
    /// partial_text: 中断前已收到的文本（TUI 可选择性保留为"已中断"标记）
    StreamRetryReset { partial_text: String },

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

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// StreamingToolCallCollector — 统一流式工具调用组装器
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// 统一流式工具调用组装器
///
/// ## 设计意图
/// 各家 LLM 的流式 tool_calls 格式不同，但最终都要组装成统一的 `Vec<ToolCall>`。
/// 本结构体提供通用的 index→id 映射、参数累积、StreamEvent 发射，
/// 让各 provider 只需做格式解析，组装逻辑复用。
///
/// ## 使用方式
/// ```rust
/// let mut collector = StreamingToolCallCollector::new();
///
/// // 收到第一个 chunk（含 id + name）
/// collector.on_tool_call_start(index, id, name, &tx);
///
/// // 收到参数 delta
/// collector.on_tool_call_args(index, args_delta, &tx);
///
/// // 流结束时获取组装结果
/// let tool_calls = collector.finish();
/// ```
pub struct StreamingToolCallCollector {
    /// index → id 映射（OpenAI/DeepSeek 流式只在第一个 chunk 发 id）
    index_to_id: std::collections::HashMap<u32, String>,
    /// 已发射 ToolCallStart 的 index 集合（防重复）
    started_indices: std::collections::HashSet<u32>,
    /// index → 工具名
    index_to_name: std::collections::HashMap<u32, String>,
    /// index → 累积的参数 JSON 字符串
    index_to_args: std::collections::HashMap<u32, String>,
}

impl StreamingToolCallCollector {
    pub fn new() -> Self {
        Self {
            index_to_id: std::collections::HashMap::new(),
            started_indices: std::collections::HashSet::new(),
            index_to_name: std::collections::HashMap::new(),
            index_to_args: std::collections::HashMap::new(),
        }
    }

    /// 处理工具调用开始事件
    ///
    /// - `index`: 工具调用在本批次中的序号（0-based）
    /// - `id`: 工具调用唯一 ID（可能只在第一个 chunk 出现）
    /// - `name`: 工具名（可能只在第一个 chunk 出现）
    /// - `tx`: StreamEvent 发送通道
    pub fn on_tool_call_start(
        &mut self,
        index: u32,
        id: Option<&str>,
        name: Option<&str>,
        tx: &tokio::sync::mpsc::UnboundedSender<StreamEvent>,
    ) {
        if let Some(id) = id {
            self.index_to_id.insert(index, id.to_string());
        }
        if let Some(name) = name {
            if !name.is_empty() {
                self.index_to_name.insert(index, name.to_string());
            }
        }

        // 只在第一次见到完整 id+name 时发射 ToolCallStart
        if !self.started_indices.contains(&index) {
            if let (Some(id), Some(name)) = (self.index_to_id.get(&index), self.index_to_name.get(&index)) {
                self.started_indices.insert(index);
                let _ = tx.send(StreamEvent::ToolCallStart {
                    id: id.clone(),
                    name: name.clone(),
                });
            }
        }
    }

    /// 处理工具调用参数增量
    ///
    /// - `index`: 工具调用序号
    /// - `delta`: 参数 JSON 增量片段
    /// - `tx`: StreamEvent 发送通道
    pub fn on_tool_call_args(
        &mut self,
        index: u32,
        delta: &str,
        tx: &tokio::sync::mpsc::UnboundedSender<StreamEvent>,
    ) {
        if delta.is_empty() {
            return;
        }
        self.index_to_args
            .entry(index)
            .or_insert_with(String::new)
            .push_str(delta);

        if let Some(id) = self.index_to_id.get(&index) {
            let _ = tx.send(StreamEvent::ToolCallArgDelta {
                id: id.clone(),
                delta: delta.to_string(),
            });
        }
    }

    /// 处理工具调用结束事件
    pub fn on_tool_call_end(
        &mut self,
        index: u32,
        tx: &tokio::sync::mpsc::UnboundedSender<StreamEvent>,
    ) {
        if let Some(id) = self.index_to_id.get(&index) {
            let _ = tx.send(StreamEvent::ToolCallEnd { id: id.clone() });
        }
    }

    /// 完成组装，返回统一的 ToolCall 列表
    ///
    /// 按 index 排序，返回 `Some(Vec<ToolCall>)`，无工具调用时返回 `None`
    pub fn finish(&self) -> Option<Vec<crate::llm::provider::ToolCall>> {
        if self.index_to_id.is_empty() {
            return None;
        }

        let mut calls: Vec<(u32, crate::llm::provider::ToolCall)> = self
            .index_to_id
            .iter()
            .filter_map(|(&idx, id)| {
                let name = self.index_to_name.get(&idx)?.clone();
                let args = self.index_to_args.get(&idx).cloned().unwrap_or_default();
                Some((idx, crate::llm::provider::ToolCall {
                    id: id.clone(),
                    type_: "function".to_string(),
                    function: crate::llm::provider::ToolFunction { name, arguments: args },
                }))
            })
            .collect();

        calls.sort_by_key(|(idx, _)| *idx);
        Some(calls.into_iter().map(|(_, c)| c).collect())
    }

    /// 是否有正在组装的工具调用
    pub fn has_pending(&self) -> bool {
        !self.index_to_id.is_empty()
    }
}
