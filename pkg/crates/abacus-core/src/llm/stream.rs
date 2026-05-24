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
    ToolEnd { name: String, success: bool, duration_ms: u64 },
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
    /// 错误（stream 中断）
    Error(String),
}
