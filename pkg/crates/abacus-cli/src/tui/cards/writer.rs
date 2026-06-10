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
use crate::tui::cards::{AbacusCard, LlmCard, ThinkingCard, UserCard};
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
fn push_tool_output(state: &mut AppState, _name: &str, output: &str) {
    let active_id = state.cards.active_id();
    if let Some(id) = active_id {
        if let Some(abacus) = state.cards.card_downcast_mut::<AbacusCard>(id) {
            abacus.set_last_call_output(output.to_string());
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
