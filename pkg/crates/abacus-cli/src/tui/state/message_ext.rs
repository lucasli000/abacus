//! AppState 消息/事件/trace 相关方法提取

use super::{AppState, EventLevel, Focus, MAX_EVENTS, MAX_MESSAGES, MsgContent, MsgRole, PanelSection, TraceEvent, TraceKind, Message};

impl AppState {
    pub fn add_message(&mut self, msg: Message) {
        if msg.role == MsgRole::User {
            self.turn_count += 1;
        }

        // V42-B C-1 修复：写入 state.cards 桥接
        if msg.role == MsgRole::User {
            let text = msg.parts.iter()
                .find_map(|p| match p {
                    MsgContent::Stream(s) => Some(s.as_str()),
                    _ => None,
                })
                .unwrap_or("")
                .to_string();
            crate::tui::cards::writer::push_user_message(self, &text, &msg.time);
        }
        // BUG-2 fix: Session / Expert 非流式落档同样需要桥接到 CardStream
        match &msg.role {
            MsgRole::Session | MsgRole::Expert(_) => {
                let mut text = String::new();
                for part in &msg.parts {
                    if let MsgContent::Stream(s) = part {
                        text.push_str(s);
                    }
                }
                let expert_name = if let MsgRole::Expert(name) = &msg.role {
                    Some(name.as_str())
                } else {
                    None
                };
                crate::tui::cards::writer::push_session_message(
                    self,
                    &text,
                    &msg.time,
                    expert_name,
                );
            }
            _ => {}
        }
        if let MsgRole::Expert(ref name) = msg.role {
            self.expert_names_cache.insert(name.clone());
        }

        // 超出上限时从最旧消息开始裁剪
        const COMPRESS_BATCH: usize = 20;
        if self.messages.len() >= MAX_MESSAGES {
            let mut stripped = 0usize;
            for msg in self.messages.iter_mut().take(COMPRESS_BATCH) {
                let had_trace = msg.parts.iter().any(|p| matches!(p, MsgContent::Trace { .. }));
                if had_trace {
                    msg.parts.retain(|p| !matches!(p, MsgContent::Trace { .. }));
                    stripped += 1;
                }
            }
            let before = self.messages.len();
            self.messages.retain(|m| !m.parts.is_empty());
            let removed = before - self.messages.len();
            let mut hard_removed = 0usize;
            while self.messages.len() >= MAX_MESSAGES {
                self.messages.pop_front();
                hard_removed += 1;
            }
            let cards_to_drop = removed + hard_removed;
            if cards_to_drop > 0 {
                self.cards.truncate_keep_last(self.cards.len().saturating_sub(cards_to_drop));
            }
            if removed > 0 || stripped > 0 {
                let compressed_count = removed.max(stripped);
                let placeholder = Message {
                    role: MsgRole::Session,
                    parts: vec![MsgContent::Stream(
                        format!("[ 已压缩 {} 条历史消息 ]", compressed_count)
                    )],
                    time: String::new(),
                };
                self.messages.push_front(placeholder);
            }
        }

        let from_agent = !matches!(msg.role, MsgRole::User);
        self.messages.push_back(msg);
        if from_agent {
            self.try_magnet_focus(Focus::Panel, PanelSection::Timeline);
        }
        self.mark_render_dirty();
        self.stream_cursor = 0;
    }

    pub fn push_trace_with_time(
        &mut self,
        time: impl Into<String>,
        category: impl Into<String>,
        level: EventLevel,
        kind: TraceKind,
    ) -> u64 {
        self.push_trace_full(time.into(), category.into(), level, kind, None)
    }

    pub fn push_trace(&mut self, category: impl Into<String>, level: EventLevel, kind: TraceKind) -> u64 {
        let time = chrono::Local::now().format("%H:%M").to_string();
        self.push_trace_full(time, category.into(), level, kind, None)
    }

    pub(crate) fn push_trace_full(
        &mut self,
        time: String,
        category: String,
        level: EventLevel,
        kind: TraceKind,
        duration_ms: Option<u64>,
    ) -> u64 {
        let id = self.next_trace_id;
        self.next_trace_id = self.next_trace_id.saturating_add(1);
        if matches!(&kind, TraceKind::ToolCall { .. }) {
            self.tool_freq_dirty.set(true);
        }
        self.trace_events.push(TraceEvent { id, time, category, level, kind, duration_ms });
        self.trace_event_index.insert(id, self.trace_events.len() - 1);
        if self.trace_events.len() > MAX_EVENTS {
            let drain_end = self.trace_events.len() - MAX_EVENTS / 2;
            self.trace_events.drain(0..drain_end);
            self.trace_event_index.clear();
            for (i, ev) in self.trace_events.iter().enumerate() {
                self.trace_event_index.insert(ev.id, i);
            }
        }
        self.try_magnet_focus(Focus::Panel, PanelSection::Timeline);
        id
    }

    pub fn push_system_note(&mut self, text: &str) {
        let now = chrono::Local::now().format("%H:%M:%S").to_string();
        let msg = Message::new_session(
            vec![MsgContent::Stream(text.to_string())],
            now,
        );
        self.add_message(msg);
    }

    pub fn add_event(
        &mut self,
        time: impl Into<String>,
        category: impl Into<String>,
        content: impl Into<String>,
        level: EventLevel,
    ) {
        self.push_trace_full(
            time.into(),
            category.into(),
            level,
            TraceKind::Generic { content: content.into() },
            None,
        );
    }

    pub fn track_knowledge_call(&mut self, file_path: &str) {
        let palace_owned: String;
        let palace: &str = if let Some(after_proj) = file_path.split("/.abacus/projects/").nth(1) {
            let slug = after_proj.split('/').next().unwrap_or("");
            if file_path.contains("/memory/") {
                palace_owned = format!("记忆/{}", slug.rsplit('-').next().unwrap_or(slug));
                &palace_owned
            } else {
                "配置"
            }
        } else if file_path.contains("记忆宫殿") {
            "记忆/主体"
        } else if file_path.contains("/.abacus/") || file_path.contains("/.claude/") {
            "配置"
        } else if file_path.contains("/src/") || file_path.contains("/pkg/")
            || file_path.contains("/crates/") || file_path.contains("/lib/") {
            "代码"
        } else if file_path.contains("/docs/") || file_path.contains("README")
            || file_path.ends_with(".md") {
            "文档"
        } else {
            "文件"
        };

        let domain = if let Some(pos) = file_path.find("memory/") {
            let after = &file_path[pos + 7..];
            let parts: Vec<&str> = after.split('/').collect();
            if parts.len() > 1 { parts[0] } else { "root" }
        } else if let Some(pos) = file_path.find("记忆宫殿/") {
            let after = &file_path[pos + "记忆宫殿/".len()..];
            let parts: Vec<&str> = after.split('/').collect();
            if parts.len() > 1 { parts[0] } else { "root" }
        } else {
            let parts: Vec<&str> = file_path.rsplitn(3, '/').collect();
            if parts.len() >= 2 { parts[1] } else { "root" }
        };

        let entity = file_path.rsplit('/').next().unwrap_or("unknown");

        if let Some(entry) = self.knowledge_calls.iter_mut()
            .find(|e| e.palace == palace && e.domain == domain && e.entity == entity)
        {
            entry.count += 1;
        } else {
            self.knowledge_calls.push(super::KnowledgeCallEntry {
                palace: palace.to_string(),
                domain: domain.to_string(),
                entity: entity.to_string(),
                count: 1,
            });
        }
    }
}
