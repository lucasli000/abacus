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
        StreamChunk::Thinking(t) => {
            // V42-B FIX: 仅在内容非空时创建 ThinkingCard，避免空头卡
            if !t.is_empty() {
                ensure_thinking_card_active(state);
            }
        }
        StreamChunk::TextDelta(t) => {
            // V42-B FIX: 仅在内容非空时创建 LlmCard，避免空头卡
            if !t.is_empty() {
                ensure_reply_card_active(state);
            }
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
            // 压力测试修复3：截断过长的 tool name，防止溢出屏幕宽度
            let name_short: String = name.chars().take(20).collect();
            let display = format!("\n{} {} · {} calls", icon, name_short, call_count);
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
    // V42-B FIX: 流式路径已通过 TextDelta 累积内容到 LlmCard，
    // 此时 EngineResponse 抵达再调 add_message 会重复创建 LlmCard。
    //
    // 关键时序：
    //   1. 流式累积阶段：handle_chunk → ensure_reply_card_active → push_active LlmCard #N
    //   2. StreamChunk::Complete 抵达 → writer::handle_chunk → cards.finish_active()
    //   3. EngineResponse 抵达 → process_engine_response → add_message → push_session_message
    //
    // 关键修复点：
    //   - 必须在 finish_active 之后再做 dedup 检查（否则 active LlmCard 被 filter 掉，dedup miss）
    //   - 不依赖 active 状态，遍历所有 LlmCard/ExpertCard 检查
    //   - 匹配条件：text 已在 existing 里，或 existing 包含 text，或反向包含
    //   - 文本规范化：trim 空白/换行 + collapse 多空白，再做相等/包含比较
    //     （应对：流式末尾换行 vs response.text trim 后的差异、ToolAgentResult 注入前缀等）
    if !text.is_empty() {
        // 先把 active finish 掉（让 LlmCard #N 不再被 filter 排除）
        let _ = state.cards.finish_active();

        // B3: 逆序遍历（最新卡片优先），限制 5 张，命中即 return。
        // 通常第一张就命中（流式累积的 LlmCard），O(1) 替代原 O(n)。
        for card in state.cards.iter_rev().take(5) {
            let cid = card.id();
            if expert_name.is_some() {
                if let Some(expert) = state.cards.card_downcast_ref::<ExpertCard>(cid) {
                    let existing = expert.reply_text_for_copy();
                    if dedup_match(&existing, text) {
                        return;
                    }
                }
            } else if let Some(llm) = state.cards.card_downcast_ref::<LlmCard>(cid) {
                let existing = llm.reply_text_for_copy();
                if dedup_match(&existing, text) {
                    return;
                }
            }
        }
    }
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

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// Dedup 辅助
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// 规范化文本用于 dedup 比较：trim 前后空白 + collapse 连续空白为单空格
///
/// 目的：消除流式累积末尾换行、API response.text trim、ToolAgentResult 注入前缀
/// 等场景导致的 dedup miss。
///
/// 复杂度：O(n) 单次扫描，无堆分配（除返回 String 外）。
fn normalize_for_dedup(s: &str) -> String {
    let trimmed = s.trim();
    let mut out = String::with_capacity(trimmed.len());
    let mut prev_was_space = false;
    for ch in trimmed.chars() {
        if ch.is_whitespace() {
            if !prev_was_space && !out.is_empty() {
                out.push(' ');
                prev_was_space = true;
            }
        } else {
            out.push(ch);
            prev_was_space = false;
        }
    }
    out
}

/// Dedup 匹配：existing 与 text 在规范化后是否相等或 existing 是 text 的前缀扩展
///
/// 返回 true 表示可视为同一消息（流式累积内容已覆盖 response.text），跳过新建。
///
/// 设计原则：
///   - 流式累积总是包含 response.text 的全部内容（LLM 不会先 emit 一部分然后
///     改变主意输出不同内容），所以匹配只需考虑 existing 是 text 的超集的情况
///   - 命中条件：
///     1. normalized 后完全相等（处理 trailing whitespace、换行差异）
///     2. existing 规范化后以 text 规范化后结尾（处理 ToolAgent prefix 注入场景）
///   - **不**做 substring contains：避免 markdown stripping 场景的误合并
///     （流式带 **，response.text 不带 → 误判为子集 → 错误 dedup）
///
/// 反例（不应 dedup）：
///   - existing="**你好。**", text="你好。"（markdown 标记不同，语义不同）
///   - existing="你好。世界", text="你好。"（前缀相同但尾部不同）
///
/// 输入参数：均接受未规范化文本（函数内部统一 normalize）
fn dedup_match(existing: &str, text: &str) -> bool {
    if existing.is_empty() || text.is_empty() {
        return false;
    }
    let normalized_existing = normalize_for_dedup(existing);
    let normalized_text = normalize_for_dedup(text);
    if normalized_existing.is_empty() || normalized_text.is_empty() {
        return false;
    }
    // 条件 1: 规范化后完全相等
    if normalized_existing == normalized_text {
        return true;
    }
    // 条件 2: existing 以 text 结尾（处理 ToolAgent prefix 注入）
    // 使用 ends_with 而非 contains，避免中间子串误判
    if normalized_existing.len() > normalized_text.len()
        && normalized_existing.ends_with(&normalized_text)
    {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_strips_whitespace() {
        assert_eq!(normalize_for_dedup("  你好。  \n"), "你好。");
        assert_eq!(normalize_for_dedup("\n\nhello\n\nworld\n"), "hello world");
        assert_eq!(normalize_for_dedup("a\t\tb  c"), "a b c");
        assert_eq!(normalize_for_dedup(""), "");
        assert_eq!(normalize_for_dedup("   "), "");
    }

    #[test]
    fn dedup_match_exact() {
        assert!(dedup_match("你好。准备开始什么任务？", "你好。准备开始什么任务？"));
    }

    #[test]
    fn dedup_match_trailing_newline_diff() {
        // 流式末尾带换行，response.text trim 后
        assert!(dedup_match("你好。准备开始什么任务？\n", "你好。准备开始什么任务？"));
        assert!(dedup_match("你好。准备开始什么任务？", "你好。准备开始什么任务？\n"));
    }

    #[test]
    fn dedup_match_internal_whitespace_diff() {
        // normalize 语义：trim + collapse 连续空白为单空格
        // 所以 "你好。\n\n准备" 和 "你好。 准备" 是等价的（normalize 后都成 "你好。 准备"）
        assert!(dedup_match("你好。\n\n准备开始", "你好。 准备开始"));
        assert!(dedup_match("你好。\t准备开始", "你好。 准备开始"));
        assert!(dedup_match("你好。  准备开始", "你好。 准备开始"));
    }

    #[test]
    fn dedup_match_suffix() {
        // existing 以 text 结尾 → dedup（ToolAgent prefix 场景）
        assert!(dedup_match("🔍 code · 1 calls\n\n你好。准备开始？", "你好。准备开始？"));
        assert!(dedup_match("\n[thinking]\n\n你好。", "你好。"));
    }

    #[test]
    fn dedup_no_match_for_middle_substring() {
        // existing 在中间包含 text（但前后不同）→ 不应 dedup
        assert!(!dedup_match("前缀。中间内容。你好。后缀。", "中间内容。你好。"));
        assert!(!dedup_match("前后\n中间\n你好\n剩余", "中间\n你好"));
    }

    #[test]
    fn dedup_no_match_for_different_content() {
        assert!(!dedup_match("你好", "再见"));
        assert!(!dedup_match("", "任何文本"));
        assert!(!dedup_match("任何文本", ""));
    }
}
