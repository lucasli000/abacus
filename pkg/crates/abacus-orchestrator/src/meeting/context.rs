//! # ContextPool — 三层上下文分配器
//!
//! ## 场景
//! AgentMeeting 中管理三层上下文隔离:
//! - 共享层：全局主题（不随时间线变化）
//! - 时间线: 所有 Specialist 共享的历史记录
//! - 私有层: 每个 Specialist 独享的中间推理状态（FollowUp 时注入）
//!
//! ## 依赖链
//! ```text
//! crate::specialist (SpecialistId)
//!   └── crate::meeting::context ← 本文件
//! ```
//!
//! ## 引用关系
//! - `ContextPool` 被 `MeetingSession` 持有
//! - 路由后 `snapshot_private()` 被调用保存推理前快照
//!
//! ## 边界
//! - timeline token 总量超过 context_budget 的 60% 时触发压缩
//! - 私有上下文在 FollowUp 完成后被 `drop_private()` 清理

use crate::specialist::SpecialistId;
use std::collections::BTreeMap;

/// 默认 context budget (tokens)。
/// 可通过 `ContextPool::with_budget()` 覆盖。
const DEFAULT_CONTEXT_BUDGET: usize = 200_000;

/// 压缩触发比例：timeline token 占 budget 的此比例时触发压缩
const COMPRESS_RATIO: f64 = 0.60;

/// 共享时间线条目
#[derive(Debug, Clone)]
pub struct TimelineEntry {
    pub turn: u32,
    pub speaker: SpecialistId,
    pub conclusion: String,
    pub confidence: f64,
}

/// Specialist 私有上下文
///
/// ## 场景
/// FollowUp 路由时给目标 Specialist 独享的历史消息
#[derive(Debug, Clone)]
pub struct PrivateContext {
    pub turn_snapshot: u32,
    pub messages: Vec<String>,
}

// ─── ContextPool ──────────────────────────────────────────────────────
// 生命周期: 随 MeetingSession 创建 → 持续写入/读取 → 会议结束自动丢弃

pub struct ContextPool {
    timeline: Vec<TimelineEntry>,
    private: BTreeMap<SpecialistId, PrivateContext>,
    turn_counter: u32,
    /// timeline 累计 token 估算
    timeline_tokens: usize,
    /// context window 预算 (tokens)
    context_budget: usize,
}

impl Default for ContextPool {
    fn default() -> Self {
        Self::new()
    }
}

impl ContextPool {
    pub fn new() -> Self {
        Self {
            timeline: vec![],
            private: BTreeMap::new(),
            turn_counter: 0,
            timeline_tokens: 0,
            context_budget: DEFAULT_CONTEXT_BUDGET,
        }
    }

    /// 创建指定 context budget 的 ContextPool
    pub fn with_budget(budget: usize) -> Self {
        Self {
            context_budget: budget,
            ..Self::new()
        }
    }

    pub fn add_turn(&mut self, entry: TimelineEntry) -> u32 {
        self.timeline_tokens += estimate_entry_tokens(&entry);
        self.timeline.push(entry);
        self.turn_counter += 1;
        let threshold = (self.context_budget as f64 * COMPRESS_RATIO) as usize;
        if self.timeline_tokens > threshold {
            self.compress();
        }
        self.turn_counter
    }

    pub fn all(&self) -> &[TimelineEntry] {
        &self.timeline
    }

    pub fn recent(&self, n: usize) -> &[TimelineEntry] {
        let start = self.timeline.len().saturating_sub(n);
        &self.timeline[start..]
    }

    pub fn turn_count(&self) -> u32 {
        self.turn_counter
    }

    /// 压缩时间线: 合并最早一半为一条摘要，重算 token 计数
    ///
    /// ## 边界
    /// - 压缩后保留约一半条目（含一条摘要）
    /// - token 计数从保留部分重算（精确）
    fn compress(&mut self) {
        let mid = self.timeline.len() / 2;
        let summary = TimelineEntry {
            turn: self.timeline[0].turn,
            speaker: SpecialistId("system".into()),
            conclusion: format!("*** 时间线压缩: {} 轮合并为摘要 ***", mid),
            confidence: 0.0,
        };
        self.timeline.drain(..mid);
        self.timeline.insert(0, summary);
        // 重算 token（压缩后精确值）
        self.timeline_tokens = self.timeline.iter().map(estimate_entry_tokens).sum();
    }

    /// 当前 timeline token 使用量
    pub fn timeline_token_usage(&self) -> usize {
        self.timeline_tokens
    }

    /// context budget
    pub fn context_budget(&self) -> usize {
        self.context_budget
    }

    pub fn snapshot_private(&mut self, sp_id: SpecialistId, messages: Vec<String>) {
        self.private.insert(sp_id, PrivateContext { turn_snapshot: self.turn_counter, messages });
    }

    pub fn get_private(&self, sp_id: &SpecialistId) -> Option<&PrivateContext> {
        self.private.get(sp_id)
    }

    pub fn drop_private(&mut self, sp_id: &SpecialistId) {
        self.private.remove(sp_id);
    }
}

/// 估算单条 TimelineEntry 的 token 开销（CJK-aware）
fn estimate_entry_tokens(entry: &TimelineEntry) -> usize {
    // speaker id + turn 元数据 ~ 10 tokens
    let meta = 10;
    let text = &entry.conclusion;
    if text.is_empty() {
        return meta + 1;
    }
    let mut cjk_chars = 0usize;
    let mut ascii_bytes = 0usize;
    for ch in text.chars() {
        if matches!(ch,
            '\u{4E00}'..='\u{9FFF}' | '\u{3400}'..='\u{4DBF}' |
            '\u{F900}'..='\u{FAFF}' | '\u{3000}'..='\u{303F}' |
            '\u{FF00}'..='\u{FFEF}' | '\u{AC00}'..='\u{D7AF}' |
            '\u{3040}'..='\u{309F}' | '\u{30A0}'..='\u{30FF}'
        ) {
            cjk_chars += 1;
        } else {
            ascii_bytes += ch.len_utf8();
        }
    }
    let cjk_tokens = (cjk_chars as f64 * 1.2) as usize;
    let ascii_tokens = (ascii_bytes as f64 * 0.25) as usize;
    meta + cjk_tokens + ascii_tokens + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entry(turn: u32, speaker: &str) -> TimelineEntry {
        TimelineEntry {
            turn,
            speaker: SpecialistId(speaker.into()),
            conclusion: format!("结论{}", turn),
            confidence: 0.8,
        }
    }

    #[test]
    fn test_add_and_recent() {
        let mut pool = ContextPool::new();
        assert_eq!(pool.turn_count(), 0);
        pool.add_turn(make_entry(1, "sp-coder"));
        assert_eq!(pool.turn_count(), 1);
        assert_eq!(pool.recent(10).len(), 1);
    }

    #[test]
    fn test_recent_respects_n() {
        let mut pool = ContextPool::new();
        for i in 1..=10 {
            pool.add_turn(make_entry(i, "sp-coder"));
        }
        assert_eq!(pool.recent(3).len(), 3);
        assert_eq!(pool.all().len(), 10);
    }

    #[test]
    fn test_compress_triggered_by_token_budget() {
        // 使用小 budget 快速触发压缩
        let mut pool = ContextPool::with_budget(500);
        // 每条 entry 约 14 tokens ("结论N" ~ 4 CJK + meta)
        // 500 * 0.6 = 300 token threshold → ~21 条触发
        for i in 1..=30 {
            pool.add_turn(make_entry(i, "sp-coder"));
        }
        // 压缩应该已触发，条目数 < 30
        assert!(pool.all().len() < 30);
        // token 使用量应低于 budget
        assert!(pool.timeline_token_usage() < 500);
    }

    #[test]
    fn test_no_compress_under_budget() {
        // 大 budget，10 条不会触发
        let mut pool = ContextPool::with_budget(100_000);
        for i in 1..=10 {
            pool.add_turn(make_entry(i, "sp-coder"));
        }
        assert_eq!(pool.all().len(), 10);
    }

    #[test]
    fn test_private_context_flow() {
        let sp_id = SpecialistId("sp-coder".into());
        let mut pool = ContextPool::new();
        pool.snapshot_private(sp_id.clone(), vec!["第一步".into()]);
        let ctx = pool.get_private(&sp_id);
        assert!(ctx.is_some());
        assert_eq!(ctx.unwrap().messages[0], "第一步");
        pool.drop_private(&sp_id);
        assert!(pool.get_private(&sp_id).is_none());
    }
}
