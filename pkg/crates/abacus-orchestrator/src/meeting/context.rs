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
//! - timeline 超过 25 条时触发压缩，合并最早一半
//! - 私有上下文在 FollowUp 完成后被 `drop_private()` 清理

use crate::specialist::SpecialistId;
use std::collections::BTreeMap;

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
}

impl Default for ContextPool {
    fn default() -> Self {
        Self::new()
    }
}

impl ContextPool {
    pub fn new() -> Self {
        Self { timeline: vec![], private: BTreeMap::new(), turn_counter: 0 }
    }

    pub fn add_turn(&mut self, entry: TimelineEntry) -> u32 {
        self.timeline.push(entry);
        self.turn_counter += 1;
        if self.timeline.len() > 25 {
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

    /// 压缩时间线: 合并最早一半为一条摘要
    ///
    /// ## 边界
    /// - 压缩后保留约一半条目（含一条摘要）
    /// - v0.1 压缩丢弃被合并轮次的实际内容
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
    fn test_compress_at_26_entries() {
        let mut pool = ContextPool::new();
        for i in 1..=26 {
            pool.add_turn(make_entry(i, "sp-coder"));
        }
        assert!(pool.all().len() < 26);
        assert!(pool.all().len() >= 14);
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
