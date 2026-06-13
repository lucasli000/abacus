//! CardStream writer —— chunk → CardStream 写入适配器
//!
//! V42-B Phase 10: 把 run.rs 的 StreamChunk 转换为 CardStream 操作。
//!
//! ## 物理时序（严格遵循 LLM 输出顺序）
//!
//! 任意时刻最多 1 张 active 卡片。新 chunk 触发新卡片时，旧 active 自动 finish。
//!
//! ```text
//! IterationStart   → 不强制 finish（保留跨迭代累积，如需新卡由后续 chunk 触发）
//! Thinking         → 若 active 不是 ThinkingCard, finish + push ThinkingCard; append
//! ToolStart        → finish active; push AbacusCard
//! ToolArgs         → active 必为 AbacusCard, push event
//! ToolOutput       → 同上
//! ToolEnd          → finish active AbacusCard
//! TextDelta        → 若 active 不是 LlmCard/ExpertCard, finish + push LlmCard; append_reply
//! ToolAgentResult  → 确保 active 是 LlmCard, append_reply
//! Complete         → finish active
//! StreamRetryReset → abort_active
//! Error            → abort_active
//! ```

use abacus_core::llm::stream::StreamChunk;
use crate::tui::cards::{AbacusCard, ExpertCard, LlmCard, ThinkingCard, UserCard};
use crate::tui::state::{AppState, EventLevel, TraceEvent, TraceKind, ToolStatus};

/// 处理单个 chunk, 写入 CardStream
pub fn handle_chunk(state: &mut AppState, chunk: &StreamChunk) {
    match chunk {
        StreamChunk::Thinking(_) => {
            ensure_thinking_card_active(state);
        }
        StreamChunk::TextDelta(_) => {
            ensure_reply_card_active(state);
        }
        StreamChunk::ToolStart { .. } => {
            ensure_no_active(state);
        }
        StreamChunk::ToolArgs { name, args_json } => {
            push_tool_event(state, name, args_json, ToolStatus::Running);
        }
        StreamChunk::ToolOutput { name, output_json } => {
            push_tool_output(state, name, output_json);
        }
        StreamChunk::ToolEnd { name, success, .. } => {
            let status = if *success { ToolStatus::Success } else { ToolStatus::Failed };
            push_tool_event(state, name, "{}", status);
            state.cards.finish_active();
        }
        StreamChunk::ToolAgentResult { icon, name, call_count, summary, .. } => {
            ensure_reply_card_active(state);
            let display = format!("\n{} {} · {} calls", icon, name, call_count);
            if let Some(id) = state.cards.active_id() {
                if let Some(llm) = state.cards.card_downcast_mut::<LlmCard>(id) {
                    llm.append_reply(&display);
                    if !summary.is_empty() {
                        llm.append_reply(&format!("  → {}\n", summary));
                    }
                }
            }
        }
        StreamChunk::Complete(_) => {
            state.cards.finish_active();
        }
        StreamChunk::IterationStart { .. } => {
            // V42-B: 迭代边界不强制 finish，让同一轮对话内容跨迭代累积
        }
        StreamChunk::StreamRetryReset { .. } => {
            state.cards.abort_active();
        }
        StreamChunk::Error(_) => {
            state.cards.abort_active();
        }
        _ => {
            // 其他 chunk (Compress, Confirm, Auth, Team, etc.) 不影响 CardStream
        }
    }

    // 追加内容到 active 卡片
    if let StreamChunk::TextDelta(t) = chunk {
        if let Some(id) = state.cards.active_id() {
            if let Some(llm) = state.cards.card_downcast_mut::<LlmCard>(id) {
                llm.append_reply(t);
            }
        }
    } else if let StreamChunk::Thinking(t) = chunk {
        if let Some(id) = state.cards.active_id() {
            if let Some(th) = state.cards.card_downcast_mut::<ThinkingCard>(id) {
                th.append(t);
            }
        }
    }
}

/// 确保 active 是 ThinkingCard，否则 finish + push 新的
fn ensure_thinking_card_active(state: &mut AppState) {
    let need_new = match state.cards.active_id() {
        None => true,
        Some(id) => {
            state.cards.card(id).map(|c| c.kind() != abacus_ui_kit::kinds::THINKING).unwrap_or(true)
        }
    };
    if need_new {
        state.cards.finish_active();
        let id = state.cards.alloc_id();
        let model = if state.model_name.is_empty() { "llm".into() } else { state.model_name.clone() };
        let card = ThinkingCard::new(id, model);
        state.cards.push_active(Box::new(card));
    }
}

/// 确保 active 是 LlmCard（Reply），否则 finish + push 新的
fn ensure_reply_card_active(state: &mut AppState) {
    let need_new = match state.cards.active_id() {
        None => true,
        Some(id) => {
            state.cards.card(id).map(|c| c.kind() != abacus_ui_kit::kinds::LLM).unwrap_or(true)
        }
    };
    if need_new {
        state.cards.finish_active();
        let id = state.cards.alloc_id();
        let model = if state.model_name.is_empty() { "llm".into() } else { state.model_name.clone() };
        let card = LlmCard::new(id, model);
        state.cards.push_active(Box::new(card));
    }
}

/// 确保无 active（ToolStart 强制切换）
fn ensure_no_active(state: &mut AppState) {
    if state.cards.active_id().is_some() {
        state.cards.finish_active();
    }
    let id = state.cards.alloc_id();
    let card = AbacusCard::new(id, "tool");
    state.cards.push_active(Box::new(card));
}

/// 推送 ToolCall event 到 active AbacusCard
fn push_tool_event(state: &mut AppState, _name: &str, _args: &str, status: ToolStatus) {
    let active_id = state.cards.active_id();
    if let Some(id) = active_id {
        if let Some(abacus) = state.cards.card_downcast_mut::<AbacusCard>(id) {
            let trace = TraceEvent {
                id: 0,
                time: chrono::Local::now().format("%H:%M").to_string(),
                category: "tool".into(),
                level: EventLevel::Info,
                kind: TraceKind::ToolCall {
                    name: _name.to_string(),
                    args: _args.to_string(),
                    output: None,
                    status,
                },
                duration_ms: None,
            };
            abacus.push_event(trace);
        }
    }
}

/// 推送 ToolOutput 到 active AbacusCard —— 直接绑定到最后一个 ToolCall
/// V42-B: 解析 JSON 输出，存结构化数据
fn push_tool_output(state: &mut AppState, name: &str, output: &str) {
    let active_id = state.cards.active_id();
    if let Some(id) = active_id {
        if let Some(abacus) = state.cards.card_downcast_mut::<AbacusCard>(id) {
            let parsed = parse_tool_output_from_str(name, output);
            abacus.set_last_call_output(parsed.stdout_summary.clone());
            // 同时存储完整解析结果到扩展字段
            abacus.set_last_call_parsed(parsed);
        }
    }
}

/// 工具输出解析结果
#[derive(Debug, Clone)]
pub struct ToolOutputParsed {
    pub command: String,
    pub stdout_summary: String,
    pub stdout_full: String,
    pub exit_code: Option<i64>,
    pub duration_ms: Option<u64>,
}

/// 解析工具输出 JSON
pub fn parse_tool_output_from_str(name: &str, output: &str) -> ToolOutputParsed {
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(output) {
        let command = json.get("command")
            .and_then(|v| v.as_str())
            .unwrap_or(name)
            .to_string();
        let stdout = json.get("stdout")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let exit_code = json.get("exit_code").and_then(|v| v.as_i64());
        let duration_ms = json.get("duration_ms").and_then(|v| v.as_u64());
        ToolOutputParsed {
            command,
            stdout_summary: stdout.lines().next().unwrap_or("").to_string(),
            stdout_full: stdout.to_string(),
            exit_code,
            duration_ms,
        }
    } else {
        ToolOutputParsed {
            command: name.to_string(),
            stdout_summary: output.lines().next().unwrap_or("").to_string(),
            stdout_full: output.to_string(),
            exit_code: None,
            duration_ms: None,
        }
    }
}

/// 推送 UserCard (turn 开始时调用, 由 add_message 触发)
pub fn push_user_message(state: &mut AppState, text: &str, time: &str) {
    state.cards.finish_active();
    let id = state.cards.alloc_id();
    let card = UserCard::new(id, text, time);
    state.cards.push_static(Box::new(card));
}

/// 推送 SessionCard (Session 消息落档时调用, 由 add_message 触发)
///
/// ## 修复背景 (BUG-2)
/// V42-B 拆卡重构后, 渲染层只读 state.cards 不读 state.messages。
/// 原 add_message 只对 MsgRole::User 调 push_user_message 桥接到 CardStream,
/// Session/Expert 非流式消息(直接调 add_message 落档, 不经 handle_chunk) 缺失
/// CardStream 表示 → render_cards 找不到对应 Card → 聊天区不显示。
///
/// ## 语义
/// - 提取引用语义: 拼接所有 MsgContent::Stream 文本作为 reply 主体
/// - 提取引用: 头(thinking 摘要) + 工具事件 + Stream 文本
/// - Session 与 Expert 共用 LlmCard (仅专家用 ExpertCard)
///
/// ## 设计权衡
/// - 不重复 finish_active 多重: 上游 add_message 调用方通常保证该消息在落档前
///   不存在 active (流式路径已 flush), 此处 finish_active 仍是安全兜底
/// - 不携带 Trace/Block kind: 该层仅展示 plain markdown reply, 详细 trace 由
///   state.trace_events 单独渲染
pub fn push_session_message(
    state: &mut AppState,
    text: &str,
    time: &str,
    expert_name: Option<&str>,
) {
    // 兜底: 上游残留 active 先 finish 掉
    state.cards.finish_active();
    let id = state.cards.alloc_id();
    let model = if state.model_name.is_empty() {
        "llm".into()
    } else {
        state.model_name.clone()
    };
    if let Some(name) = expert_name {
        let card = ExpertCard::new(id, name, model);
        // 非流式落档: 立即 finish_active 让其进入 static 渲染路径
        state.cards.push_active(Box::new(card));
        // append_reply 走 mutable borrow, 上面 push_active 借用结束后再操作
        if let Some(expert) = state.cards.card_downcast_mut::<ExpertCard>(id) {
            expert.append_reply(text);
        }
        state.cards.finish_active();
    } else {
        let card = LlmCard::new(id, model);
        state.cards.push_active(Box::new(card));
        if let Some(llm) = state.cards.card_downcast_mut::<LlmCard>(id) {
            llm.append_reply(text);
        }
        state.cards.finish_active();
    }
    // time 当前未挂到 Card header(渲染层从 self.messages 读 time);
    // 保留参数供未来扩展, 避免破坏现有 API 签名
    let _ = time;
}
