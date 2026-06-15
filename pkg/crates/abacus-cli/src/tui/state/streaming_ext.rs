//! AppState 流式会话方法提取
//!
//! V42-B streaming 升级：active LlmCard/ThinkingCard 读写、reset、flush。

use super::{
    AppState, BlockKind, MsgContent, TimelineEntry, TraceKind,
};

impl AppState {
    pub fn is_streaming_active(&self) -> bool {
        self.cards.active_id().is_some()
    }

    pub fn begin_streaming_session(&mut self) {
        if self.cards.active_id().is_some() {
            self.cards.finish_active();
        }
        let id = self.cards.alloc_id();
        let model = if self.model_name.is_empty() { "llm".into() } else { self.model_name.clone() };
        let card = crate::tui::cards::LlmCard::new(id, model);
        self.cards.push_active(Box::new(card));
    }

    pub fn end_streaming_session(&mut self) {
        self.cards.finish_active();
    }

    pub fn is_streaming_active_set_false(&mut self) {
        self.cards.finish_active();
    }

    pub fn active_llm_text(&self) -> String {
        if let Some(id) = self.cards.active_id() {
            if let Some(llm) = self.cards.card_downcast_ref::<crate::tui::cards::LlmCard>(id) {
                return llm.reply_text_for_copy();
            }
        }
        String::new()
    }

    pub fn active_llm_text_len(&self) -> usize {
        if let Some(id) = self.cards.active_id() {
            if let Some(llm) = self.cards.card_downcast_ref::<crate::tui::cards::LlmCard>(id) {
                return llm.reply_text_len();
            }
        }
        0
    }

    pub fn active_llm_thinking(&self) -> String {
        if let Some(id) = self.cards.active_id() {
            if let Some(th) = self.cards.card_downcast_ref::<crate::tui::cards::ThinkingCard>(id) {
                return th.text_for_copy();
            }
        }
        String::new()
    }

    pub fn active_llm_thinking_len(&self) -> usize {
        if let Some(id) = self.cards.active_id() {
            if let Some(th) = self.cards.card_downcast_ref::<crate::tui::cards::ThinkingCard>(id) {
                return th.text_len();
            }
        }
        0
    }

    pub fn last_llm_text(&self) -> String {
        let mut last_id: Option<u64> = None;
        for card in self.cards.iter() {
            if card.kind() == abacus_ui_kit::kinds::LLM {
                last_id = Some(card.id());
            }
        }
        if let Some(id) = last_id {
            if let Some(llm) = self.cards.card_downcast_ref::<crate::tui::cards::LlmCard>(id) {
                return llm.reply_text_for_copy();
            }
        }
        String::new()
    }

    pub fn take_last_llm_text(&mut self) -> String {
        let mut last_id: Option<u64> = None;
        for card in self.cards.iter() {
            if card.kind() == abacus_ui_kit::kinds::LLM {
                last_id = Some(card.id());
            }
        }
        if let Some(id) = last_id {
            if let Some(llm) = self.cards.card_downcast_mut::<crate::tui::cards::LlmCard>(id) {
                return std::mem::take(llm.reply_text_field());
            }
        }
        String::new()
    }

    pub fn take_last_llm_thinking(&mut self) -> String {
        let mut last_id: Option<u64> = None;
        for card in self.cards.iter() {
            if card.kind() == abacus_ui_kit::kinds::THINKING {
                last_id = Some(card.id());
            }
        }
        if let Some(id) = last_id {
            if let Some(th) = self.cards.card_downcast_mut::<crate::tui::cards::ThinkingCard>(id) {
                let text = th.text_for_copy();
                th.append("");
                return text;
            }
        }
        String::new()
    }

    pub fn last_llm_thinking(&self) -> String {
        let mut last_id: Option<u64> = None;
        for card in self.cards.iter() {
            if card.kind() == abacus_ui_kit::kinds::THINKING {
                last_id = Some(card.id());
            }
        }
        if let Some(id) = last_id {
            if let Some(th) = self.cards.card_downcast_ref::<crate::tui::cards::ThinkingCard>(id) {
                return th.text_for_copy();
            }
        }
        String::new()
    }

    pub fn push_timeline_entry(&mut self, entry: TimelineEntry) {
        const MAX_ENTRIES: usize = 1000;
        if self.streaming_timeline.len() >= MAX_ENTRIES {
            self.streaming_timeline.remove(0);
        }
        self.streaming_timeline.push(entry);
    }

    pub fn reset_streaming(&mut self) {
        self.is_streaming_active_set_false();
        self.streaming_complete = false;
        self.streaming_tools.clear();
        self.streaming_timeline.clear();
        self.expanded_block_ids.borrow_mut().clear();
        self.streaming_trace_ids.clear();
        *self.streaming_md.borrow_mut() = None;
        self.streaming_content_dirty.set(false);
        self.streaming_content_dirty.set(false);
        self.user_scrolled_away.set(false);
    }

    pub fn flush_streaming_to_message(&mut self) -> bool {
        let text = self.take_last_llm_text();
        let thinking = self.take_last_llm_thinking();
        if text.is_empty() && thinking.is_empty() && self.streaming_trace_ids.is_empty() {
            return false;
        }
        let ts = chrono::Local::now().format("%H:%M").to_string();
        let trace_ids = std::mem::take(&mut self.streaming_trace_ids);

        let mut parts: Vec<MsgContent> = Vec::new();
        if !thinking.is_empty() {
            let line_count = thinking.lines().count();
            let preview: String = thinking.lines()
                .find(|l| !l.trim().is_empty()).unwrap_or("")
                .chars().take(40).collect();
            let summary = if preview.is_empty() {
                format!("💭 {} lines", line_count)
            } else {
                format!("💭 {} lines · {}", line_count, preview)
            };
            parts.push(MsgContent::Block {
                kind: BlockKind::Think,
                summary,
                collapsed: true,
                detail: thinking,
            });
        }
        let tool_ids: Vec<u64> = trace_ids.iter().copied()
            .filter(|id| self.trace_events.iter().any(|e|
                e.id == *id && matches!(e.kind, TraceKind::ToolCall { .. })
            ))
            .collect();
        if !tool_ids.is_empty() {
            parts.push(MsgContent::Trace {
                event_ids: tool_ids,
                collapsed: true,
                expanded_event_ids: std::collections::HashSet::new(),
            });
        }
        if !text.is_empty() {
            parts.push(MsgContent::Stream(text));
        }
        if parts.is_empty() {
            return false;
        }
        self.add_message(super::Message::new_session(parts, &ts));
        true
    }
}
