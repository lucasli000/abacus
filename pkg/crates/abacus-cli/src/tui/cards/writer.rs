//! CardStream writer —— chunk → CardStream 写入适配器
//!
//! V42-B Phase 10: 把 run.rs 的 StreamChunk 转换为 CardStream 操作
//! (push_active / finish_active / append_reply / append_event)。
//!
//! ## 设计目标
//!
//! 在 run.rs 的 chunk drain 循环末尾调用 [`handle_chunk`],
//! 把每个 chunk 翻译成 Card 操作。新老字段并存——
//! 旧 `state.streaming_*` 字段仍被渲染层使用, 保证现有功能不受影响;
//! `state.cards` 作为"未来唯一数据源"逐步累积, Phase 14 切换。

use abacus_core::llm::stream::StreamChunk;
use crate::tui::cards::{AbacusCard, LlmCard, UserCard};
use crate::tui::state::{AppState, EventLevel, TraceEvent, TraceKind, ToolStatus};

/// 处理单个 chunk, 写入 CardStream
///
/// 调用方: run.rs `while let Ok(chunk) = stream_rx.try_recv()`
/// 的 match 分支末尾。
///
/// ## 状态机
///
/// ```text
/// IterationStart   → 若有 active, finish_active
/// TextDelta        → 若 active 不是 LlmCard, finish + push LlmCard; append_reply
/// Thinking         → 若 active 不是 LlmCard, finish + push LlmCard; append_thinking
/// ToolStart        → finish active; push AbacusCard
/// ToolArgs         → active 必为 AbacusCard, push event
/// ToolOutput       → 同上
/// ToolEnd          → finish active AbacusCard
/// ToolAgentResult  → 确保 active 是 LlmCard, append_reply
/// Complete         → finish active
/// ```
pub fn handle_chunk(state: &mut AppState, chunk: &StreamChunk) {
    match chunk {
        StreamChunk::TextDelta(_) | StreamChunk::Thinking(_) => {
            ensure_llm_card_active(state);
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
            // V42-B: 追加到 active LlmCard 的 reply
            ensure_llm_card_active(state);
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
            // V42-B: 迭代边界: 保留当前 LLM Card 内容, 不强制 finish
            // (active LlmCard 跨迭代累积 reply_text)
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

    // LlmCard 的 append_reply / append_thinking 需要在 push_active 之后
    if let StreamChunk::TextDelta(t) = chunk {
        if let Some(id) = state.cards.active_id() {
            if let Some(llm) = state.cards.card_downcast_mut::<LlmCard>(id) {
                llm.append_reply(t);
            }
        }
    } else if let StreamChunk::Thinking(t) = chunk {
        if let Some(id) = state.cards.active_id() {
            if let Some(llm) = state.cards.card_downcast_mut::<LlmCard>(id) {
                llm.append_thinking(t);
            }
        }
    }
}

/// 确保 active 是 LlmCard, 否则 finish + push 新的
fn ensure_llm_card_active(state: &mut AppState) {
    let need_new = match state.cards.active_id() {
        None => true,
        Some(id) => {
            // 查 active 卡的 kind
            state.cards.card(id).map(|c| c.kind() != abacus_ui_kit::kinds::LLM).unwrap_or(true)
        }
    };
    if need_new {
        state.cards.finish_active();
        let id = state.cards.alloc_id();
        let model = if state.model_name.is_empty() { "llm".into() } else { state.model_name.clone() };
        let card = LlmCard::new(id, model, "default");
        state.cards.push_active(Box::new(card));
    }
}

/// 确保无 active (ToolStart 强制切换)
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
                id: 0, // 占位, AbacusCard 内部不依赖
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

/// 推送 ToolOutput 到 active AbacusCard
fn push_tool_output(state: &mut AppState, _name: &str, output: &str) {
    let active_id = state.cards.active_id();
    if let Some(id) = active_id {
        if let Some(abacus) = state.cards.card_downcast_mut::<AbacusCard>(id) {
            // 简化: 追加一个 Generic event 表示 output
            let trace = TraceEvent {
                id: 0,
                time: chrono::Local::now().format("%H:%M").to_string(),
                category: "tool-output".into(),
                level: EventLevel::Info,
                kind: TraceKind::Generic { content: output.to_string() },
                duration_ms: None,
            };
            abacus.push_event(trace);
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


