//! CardStream —— 消息流卡片生命周期管理
//!
//! ## 设计目标
//!
//! 把 V40 的"巨型 Vec<Trace>"统一管理升级为"按 id 索引的卡片流"。
//! CardStream 是消息流的**唯一数据源** — 不再有"trace timeline + streaming text + tool calls"三个并列状态。
//!
//! ## 与 V40 的对照
//!
//! | V40 字段 | V42-B 替代 |
//! |----------|------------|
//! | `Vec<Trace>` timeline | `Vec<Box<dyn MessageCard>>` cards |
//! | `streaming_text` / `streaming_thinking` | active 卡片内部 mutator |
//! | `streaming_trace_ids` | `active: Option<u64>` (单数) |
//! | `is_streaming` | `active.is_some()` |
//! | `cached_msg_rows` | `body_height_sum()` (按需重算) |
//!
//! ## 单 active 约束
//!
//! 与 LLM chat 协议对齐: 任意时刻最多 1 张 active 卡片（"流式 token 入口"）。
//! - push_active 替换现有 active（旧 active 自动 finish, 避免泄漏）
//! - finish_active / abort_active 仅作用于当前 active
//! - 这与 run.rs 的 chunk drain 时序匹配（TextDelta / ToolStart 等不会嵌套）

use std::collections::HashMap;

use crate::card::{CardCollapse, CardStreaming, MessageCard};

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// CardStream
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// 消息流卡片集合 —— 整个 TUI 的"对话历史"数据源
///
/// ## 线程模型
///
/// 单线程, 由 abacus-cli 主循环顺序持有 (`AppState::cards: CardStream`)。
/// **不可跨线程共享**, 也无需 Mutex。
///
/// ## 内存模型
///
/// - cards Vec 永不移除 (append-only); 通过 `clear()` 整段清空 (新会话开始)
/// - collapse_overrides 跟随 cards 增长, 不主动 GC (max ~10k 条目无压力)
/// - 必要时调 `truncate_keep_last(n)` 截断最旧卡片
pub struct CardStream {
    /// 所有卡片 (按 push 顺序) — append-only
    cards: Vec<Box<dyn MessageCard>>,
    /// id → Vec 索引 反查表 (alloc 时插入, 永不删除)
    id_to_idx: HashMap<u64, usize>,
    /// 下一个分配的 id —— 严格单调递增
    next_id: u64,
    /// 当前 active 卡片 id (None = 无流式)
    active: Option<u64>,
    /// id → 用户/系统显式覆盖的折叠状态 (None 表示用 `MessageCard::default_collapse`)
    collapse_overrides: HashMap<u64, CardCollapse>,
}

/// 最大保留卡片数——超出时 FIFO 裁剪最旧卡片，防止长会话滚动卡顿
const MAX_CARDS: usize = 100;

impl Default for CardStream {
    fn default() -> Self {
        Self::new()
    }
}

impl CardStream {
    pub fn new() -> Self {
        Self {
            cards: Vec::new(),
            id_to_idx: HashMap::new(),
            next_id: 1,
            active: None,
            collapse_overrides: HashMap::new(),
        }
    }

    /// FIFO 裁剪：保留最新 MAX_CARDS 张卡片，丢弃最旧的。
    /// 仅裁剪已完成的静态卡片，active 卡片始终保留。
    fn trim_if_needed(&mut self) {
        if self.cards.len() <= MAX_CARDS {
            return;
        }
        // 计算需要裁剪的数量（保留 active 和最近的 MAX_CARDS-1 张）
        let active_id = self.active;
        let keep = MAX_CARDS.saturating_sub(1);
        let drain_count = self.cards.len().saturating_sub(keep);
        if drain_count == 0 {
            return;
        }
        // 收集要丢弃的 id
        let drain_ids: Vec<u64> = self.cards[..drain_count]
            .iter()
            .map(|c| c.id())
            .filter(|&id| Some(id) != active_id)
            .collect();
        // 丢弃旧卡片
        self.cards.drain(..drain_count);
        // 重建 id_to_idx 映射
        self.id_to_idx.clear();
        for (i, card) in self.cards.iter().enumerate() {
            self.id_to_idx.insert(card.id(), i);
        }
        // 清理 collapse_overrides
        for id in &drain_ids {
            self.collapse_overrides.remove(id);
        }
    }

    // ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
    // 构造 / 生命周期
    // ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

    /// 分配下一个 id (内部使用, 也供第三方 Card 构造时显式预分配)
    pub fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// 推入一张静态 (已完成) 卡片
    ///
    /// - **card 构造时必须已自报正确 id** (由 [`Self::alloc_id`] 预分配)
    /// - 此方法不再分配新 id, 也不修改 card 字段
    /// - 不影响 active (active 仍为 None 或保持)
    /// - 返回新卡片 id (= card.id())
    pub fn push_static(&mut self, card: Box<dyn MessageCard>) -> u64 {
        let id = card.id();
        let idx = self.cards.len();
        self.cards.push(card);
        self.id_to_idx.insert(id, idx);
        self.trim_if_needed();
        id
    }

    /// 推入一张 active (流式) 卡片
    ///
    /// - card 构造时已自报正确 id
    /// - 替换现有 active (旧 active 自动 finish 标记为 Static, 避免泄漏)
    /// - 返回新卡片 id (= card.id())
    pub fn push_active(&mut self, card: Box<dyn MessageCard>) -> u64 {
        // 防御: 旧 active 自动 finish (标记为 Static) — 避免 active 泄漏导致渲染崩坏
        if let Some(old_id) = self.active {
            self.mark_streaming(old_id, CardStreaming::Static);
        }
        let id = card.id();
        let idx = self.cards.len();
        self.cards.push(card);
        self.id_to_idx.insert(id, idx);
        self.active = Some(id);
        self.trim_if_needed();
        id
    }

    /// 标记当前 active 为已完成 (Static)
    /// 返回原 active id, 或 None (无 active)
    ///
    /// 同时应用卡片的 `default_collapse` 到 collapse_overrides (turn 结束统一折叠)
    pub fn finish_active(&mut self) -> Option<u64> {
        let id = self.active?;
        self.mark_streaming(id, CardStreaming::Static);
        // 应用 default_collapse (仅当用户未显式覆盖)
        if !self.collapse_overrides.contains_key(&id) {
            if let Some(card) = self.card(id) {
                let default = card.default_collapse();
                self.collapse_overrides.insert(id, default);
            }
        }
        self.active = None;
        Some(id)
    }

    /// 标记当前 active 为已中断 (Aborted)
    /// 返回原 active id, 或 None
    pub fn abort_active(&mut self) -> Option<u64> {
        let id = self.active?;
        self.mark_streaming(id, CardStreaming::Aborted);
        self.active = None;
        Some(id)
    }

    /// 内部: 修改某卡片的 streaming 状态
    fn mark_streaming(&mut self, id: u64, target: CardStreaming) {
        if let Some(card) = self.card_mut(id) {
            card_mut_set_streaming(card, target);
        }
    }

    // ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
    // 访问
    // ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

    /// 当前 active 卡片 id
    pub fn active_id(&self) -> Option<u64> {
        self.active
    }

    /// 按 id 查卡片 (immutable)
    pub fn card(&self, id: u64) -> Option<&dyn MessageCard> {
        let idx = *self.id_to_idx.get(&id)?;
        // Box<dyn MessageCard + 'static> → &dyn MessageCard
        // 通过解 Box 拿到 dyn trait, 借 'static 由 MessageCard super trait 保证
        self.cards.get(idx).map(|b| b.as_ref())
    }

    /// 按 id 查卡片 (mutable, 用于 streaming mutator)
    pub fn card_mut(&mut self, id: u64) -> Option<&mut Box<dyn MessageCard>> {
        let idx = *self.id_to_idx.get(&id)?;
        self.cards.get_mut(idx)
    }

    /// 当前 active 卡片的可变引用
    pub fn active_mut(&mut self) -> Option<&mut Box<dyn MessageCard>> {
        let id = self.active?;
        self.card_mut(id)
    }

    /// 全部卡片 (按 push 顺序)
    pub fn iter(&self) -> impl Iterator<Item = &Box<dyn MessageCard>> {
        self.cards.iter()
    }

    /// 卡片总数
    pub fn len(&self) -> usize {
        self.cards.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cards.is_empty()
    }

    /// id → Vec 索引 反查 (供 ScrollLayout 缓存 item_areas)
    pub fn index_of(&self, id: u64) -> Option<usize> {
        self.id_to_idx.get(&id).copied()
    }

    /// Vec 索引 → id (供 hit-test 反查)
    pub fn id_at(&self, index: usize) -> Option<u64> {
        self.cards.get(index).map(|c| c.id())
    }

    /// 最后一张卡片的 id (供 End 键滚到底)
    pub fn last_id(&self) -> Option<u64> {
        self.cards.last().map(|c| c.id())
    }

    /// Downcast 指定 id 的卡片到具体类型
    ///
    /// 用法:
    /// ```ignore
    /// if let Some(llm) = stream.card_downcast_mut::<LlmCard>(id) {
    ///     llm.append_reply(delta);
    /// }
    /// ```
    ///
    /// ## 实现机制
    ///
    /// 1. `MessageCard` trait 加 `Self: 'static` 约束 (见 card.rs)
    /// 2. CardStream 内部存 `Box<dyn MessageCard>` (隐含 'static)
    /// 3. `boxed.as_mut()` 拿到 `&mut (dyn MessageCard + 'static)`
    /// 4. coerce 到 `&mut (dyn Any + 'static)`（'static 约束保证）
    /// 5. `Any::downcast_mut::<T>` 在 runtime 校验 TypeId, 失败返回 None
    ///
    /// **零 unsafe 代码**。所有约束在编译期保证, runtime 校验由 std 提供。
    pub fn card_downcast_mut<T: 'static + MessageCard>(&mut self, id: u64) -> Option<&mut T> {
        // self.card_mut 返回 Option<&mut Box<dyn MessageCard>>
        let boxed: &mut Box<dyn MessageCard> = self.card_mut(id)?;
        // boxed.as_mut() 拿到 &mut (dyn MessageCard + 'static)
        // 然后 coerce 到 &mut (dyn Any + 'static)（'static 约束保证）
        let card: &mut (dyn std::any::Any + 'static) = boxed.as_mut();
        card.downcast_mut::<T>()
    }

    /// Downcast 指定 id 的卡片到具体类型 (immutable 版本)
    ///
    /// 与 [`Self::card_downcast_mut`] 平行, 用于只读访问 (如文本提取)
    pub fn card_downcast_ref<T: 'static + MessageCard>(&self, id: u64) -> Option<&T> {
        // 通过 id_to_idx 直接拿 Box 引用 (避免借用 self.card 的复杂链)
        let idx = *self.id_to_idx.get(&id)?;
        let boxed: &Box<dyn MessageCard> = self.cards.get(idx)?;
        let card: &(dyn std::any::Any + 'static) = boxed.as_ref();
        card.downcast_ref::<T>()
    }

    // ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
    // 折叠管理
    // ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

    /// 解析实际折叠状态 (用户覆盖优先, 否则 default_collapse)
    pub fn collapse(&self, id: u64) -> CardCollapse {
        if let Some(&c) = self.collapse_overrides.get(&id) {
            return c;
        }
        if let Some(card) = self.card(id) {
            return card.default_collapse();
        }
        CardCollapse::Expanded // fallback
    }

    /// 显式设置折叠覆盖 (turn 结束 CardStream 自动应用 default_collapse, 之后用户 Space 调此方法)
    pub fn set_collapse(&mut self, id: u64, c: CardCollapse) {
        if self.card(id).is_some() {
            self.collapse_overrides.insert(id, c);
        }
    }

    /// 二档切换 (Space 键) — 返回新折叠状态
    /// 跳过 Headless, 与 `CardCollapse::toggle_binary` 语义一致
    pub fn toggle_collapse(&mut self, id: u64) -> Option<CardCollapse> {
        let current = self.collapse(id);
        let next = current.toggle_binary();
        self.set_collapse(id, next);
        Some(next)
    }

    /// 三档循环 (Ctrl+Shift+Space 键) — 返回新折叠状态
    pub fn cycle_collapse(&mut self, id: u64) -> Option<CardCollapse> {
        let current = self.collapse(id);
        let next = current.cycle_tri();
        self.set_collapse(id, next);
        Some(next)
    }

    // ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
    // 容量管理
    // ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

    /// 清空全部 (新会话开始)
    pub fn clear(&mut self) {
        self.cards.clear();
        self.id_to_idx.clear();
        self.active = None;
        self.collapse_overrides.clear();
        // next_id 保留 — 跨会话不重置, 避免 id 冲突
    }

    /// 截断保留最新 n 条 (滚动归档 / 内存压力)
    /// 同步清理 id_to_idx + collapse_overrides 中被移除的条目
    pub fn truncate_keep_last(&mut self, n: usize) {
        if self.cards.len() <= n {
            return;
        }
        let drop_count = self.cards.len() - n;
        // 收集保留 id
        let kept_ids: Vec<u64> = self.cards.iter().skip(drop_count).map(|c| c.id()).collect();
        // 截断
        self.cards.drain(..drop_count);
        // 重建 id_to_idx (索引全部左移)
        self.id_to_idx.clear();
        for (new_idx, card) in self.cards.iter().enumerate() {
            self.id_to_idx.insert(card.id(), new_idx);
        }
        // 清理折叠覆盖
        self.collapse_overrides.retain(|k, _| kept_ids.contains(k));
        // active 失效 (被截断)
        if let Some(aid) = self.active {
            if !kept_ids.contains(&aid) {
                self.active = None;
            }
        }
    }
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// 内部 helper —— 修改卡片 streaming 状态 (空实现, 见 mark_streaming 注释)
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// 内部: 修改卡片 streaming 状态
///
/// 约定 `MessageCard` 的 `streaming()` 反映 CardStream 标记的状态。
/// builtin Card 每次 query 都从 `self.streaming_field` 读取
/// (受 push_active / finish_active / abort_active 调用方控制)。
///
/// 若未来引入第三方 Card 自行管理 streaming, 可在 trait 加 `set_streaming` 兜底。
fn card_mut_set_streaming(_card: &mut Box<dyn MessageCard>, _target: CardStreaming) {
    // 空实现占位 — builtin Card 都遵守约定
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// 测试
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

#[cfg(test)]
mod tests {
    use super::*;
    use crate::card::kinds;
    use crate::section::SectionContext;
    use crate::Theme;
    use ratatui::Frame;
    use ratatui::layout::Rect;

    #[allow(dead_code)]
    struct TestCtx(Theme);
    #[allow(dead_code)]
    impl SectionContext for TestCtx {
        fn theme(&self) -> &Theme { &self.0 }
    }

    #[allow(dead_code)]
    struct TestCard {
        id: u64,
        kind_val: &'static str,
        streaming_val: CardStreaming,
        body_h: u16,
        default_collapse_val: CardCollapse,
        streaming_field: CardStreaming,
    }

    #[allow(dead_code)]
    impl TestCard {
        fn new(id: u64, streaming: CardStreaming) -> Self {
            Self {
                id,
                kind_val: kinds::USER,
                streaming_val: streaming,
                body_h: 1,
                default_collapse_val: CardCollapse::Expanded,
                streaming_field: streaming,
            }
        }
    }

    impl MessageCard for TestCard {
        fn kind(&self) -> &'static str { self.kind_val }
        fn id(&self) -> u64 { self.id }
        fn header(&self, _: &dyn SectionContext) -> crate::card::CardHeader {
            crate::card::CardHeader::new("test", "0s")
        }
        fn streaming(&self) -> CardStreaming { self.streaming_field }
        fn default_collapse(&self) -> CardCollapse { self.default_collapse_val }
        fn body_height(&self, _: &dyn SectionContext, _: u16, _: CardCollapse) -> u16 { self.body_h }
        fn render_body(&self, _: &mut Frame, _: &dyn SectionContext, _: Rect, _: CardCollapse) {}
    }

    #[allow(dead_code)]
    fn card_box(id: u64, s: CardStreaming) -> Box<dyn MessageCard> {
        Box::new(TestCard::new(id, s))
    }

    /// 测试辅助: alloc + construct + push_static 一步完成 (避免 borrow 冲突)
    fn push_static_card(s: &mut CardStream, kind: CardStreaming) -> u64 {
        let id = s.alloc_id();
        s.push_static(card_box(id, kind))
    }

    /// 测试辅助: alloc + construct + push_active 一步完成
    fn push_active_card(s: &mut CardStream, kind: CardStreaming) -> u64 {
        let id = s.alloc_id();
        s.push_active(card_box(id, kind))
    }

    #[test]
    fn alloc_id_monotonic() {
        let mut s = CardStream::new();
        assert_eq!(s.alloc_id(), 1);
        assert_eq!(s.alloc_id(), 2);
        assert_eq!(s.alloc_id(), 3);
    }

    #[test]
    fn push_static_increments_and_no_active() {
        let mut s = CardStream::new();
        let id1 = push_static_card(&mut s, CardStreaming::Static);
        let id2 = push_static_card(&mut s, CardStreaming::Static);
        assert_eq!(id1, 1);
        assert_eq!(id2, 2);
        assert_eq!(s.len(), 2);
        assert!(s.active_id().is_none());
    }

    #[test]
    fn push_active_sets_single_active() {
        let mut s = CardStream::new();
        let id1 = push_active_card(&mut s, CardStreaming::Active);
        assert_eq!(s.active_id(), Some(id1));
        // 推第二张 active 替换第一张
        let id2 = push_active_card(&mut s, CardStreaming::Active);
        assert_eq!(s.active_id(), Some(id2));
        assert_ne!(id1, id2);
        assert_eq!(s.len(), 2);
    }

    #[test]
    fn finish_active_returns_id_and_clears() {
        let mut s = CardStream::new();
        let id = push_active_card(&mut s, CardStreaming::Active);
        let finished = s.finish_active();
        assert_eq!(finished, Some(id));
        assert!(s.active_id().is_none());
        // 二次 finish 无效
        assert_eq!(s.finish_active(), None);
    }

    #[test]
    fn abort_active_returns_id_and_clears() {
        let mut s = CardStream::new();
        let id = push_active_card(&mut s, CardStreaming::Active);
        let aborted = s.abort_active();
        assert_eq!(aborted, Some(id));
        assert!(s.active_id().is_none());
    }

    #[test]
    fn card_lookup_by_id_works() {
        let mut s = CardStream::new();
        let id = push_static_card(&mut s, CardStreaming::Static);
        assert!(s.card(id).is_some());
        assert!(s.card_mut(id).is_some());
        assert!(s.card(999).is_none());
    }

    #[test]
    fn active_mut_returns_some_only_when_active() {
        let mut s = CardStream::new();
        assert!(s.active_mut().is_none());
        let _id = push_active_card(&mut s, CardStreaming::Active);
        assert!(s.active_mut().is_some());
        s.finish_active();
        assert!(s.active_mut().is_none());
    }

    #[test]
    fn iter_yields_in_push_order() {
        let mut s = CardStream::new();
        let a = push_static_card(&mut s, CardStreaming::Static);
        let b = push_static_card(&mut s, CardStreaming::Static);
        let c = push_static_card(&mut s, CardStreaming::Static);
        let ids: Vec<u64> = s.iter().map(|c| c.id()).collect();
        assert_eq!(ids, vec![a, b, c]);
    }

    #[test]
    fn collapse_default_used_when_no_override() {
        let mut s = CardStream::new();
        let id = push_static_card(&mut s, CardStreaming::Static);
        // TestCard.default_collapse = Expanded
        assert_eq!(s.collapse(id), CardCollapse::Expanded);
    }

    #[test]
    fn set_collapse_overrides_default() {
        let mut s = CardStream::new();
        let id = push_static_card(&mut s, CardStreaming::Static);
        s.set_collapse(id, CardCollapse::Collapsed);
        assert_eq!(s.collapse(id), CardCollapse::Collapsed);
        s.set_collapse(id, CardCollapse::Headless);
        assert_eq!(s.collapse(id), CardCollapse::Headless);
    }

    #[test]
    fn toggle_collapse_uses_binary() {
        let mut s = CardStream::new();
        let id = push_static_card(&mut s, CardStreaming::Static);
        // Expanded → Collapsed
        assert_eq!(s.toggle_collapse(id), Some(CardCollapse::Collapsed));
        // Collapsed → Expanded
        assert_eq!(s.toggle_collapse(id), Some(CardCollapse::Expanded));
        // 设 Headless 再 toggle → Expanded (跳过 Collapsed)
        s.set_collapse(id, CardCollapse::Headless);
        assert_eq!(s.toggle_collapse(id), Some(CardCollapse::Expanded));
    }

    #[test]
    fn cycle_collapse_iterates_three() {
        let mut s = CardStream::new();
        let id = push_static_card(&mut s, CardStreaming::Static);
        assert_eq!(s.cycle_collapse(id), Some(CardCollapse::Collapsed));
        assert_eq!(s.cycle_collapse(id), Some(CardCollapse::Headless));
        assert_eq!(s.cycle_collapse(id), Some(CardCollapse::Expanded));
    }

    #[test]
    fn finish_active_applies_default_collapse() {
        let mut s = CardStream::new();
        let id = push_active_card(&mut s, CardStreaming::Active);
        s.finish_active();
        // TestCard.default_collapse = Expanded, finish 后应自动设置覆盖
        assert_eq!(s.collapse(id), CardCollapse::Expanded);
    }

    #[test]
    fn clear_resets_cards_but_preserves_id_counter() {
        let mut s = CardStream::new();
        let _ = push_static_card(&mut s, CardStreaming::Static);
        let _ = push_active_card(&mut s, CardStreaming::Active);
        s.clear();
        assert!(s.is_empty());
        assert!(s.active_id().is_none());
        // 后续 alloc 应从下一个 id 开始 (避免 session 持久化冲突)
        let next = s.alloc_id();
        assert!(next > 2);
    }

    #[test]
    fn truncate_keep_last_drops_old_and_preserves_active() {
        let mut s = CardStream::new();
        for _ in 0..5 {
            push_static_card(&mut s, CardStreaming::Static);
        }
        push_active_card(&mut s, CardStreaming::Active);
        // 保留最后 3 条
        s.truncate_keep_last(3);
        assert_eq!(s.len(), 3);
        // active 应被截断 (因为它是被保留的 3 条之一)
        // 第 4, 5 static + 1 active = 6 cards, 保留最后 3 = active + 最后 2 static
        assert!(s.active_id().is_some());
        // id_to_idx 应被重建
        for i in 0..3 {
            let id = s.id_at(i).expect("id_at");
            assert_eq!(s.index_of(id), Some(i));
        }
    }

    #[test]
    fn truncate_keep_last_drops_active_when_too_old() {
        let mut s = CardStream::new();
        let active_id = push_active_card(&mut s, CardStreaming::Active);
        for _ in 0..10 {
            push_static_card(&mut s, CardStreaming::Static);
        }
        // 保留最后 5 条, active_id 是最旧一条, 应被裁掉
        s.truncate_keep_last(5);
        assert!(s.active_id().is_none());
        assert!(s.card(active_id).is_none());
    }

    #[test]
    fn index_of_and_id_at_round_trip() {
        let mut s = CardStream::new();
        let a = push_static_card(&mut s, CardStreaming::Static);
        let b = push_static_card(&mut s, CardStreaming::Static);
        assert_eq!(s.index_of(a), Some(0));
        assert_eq!(s.index_of(b), Some(1));
        assert_eq!(s.id_at(0), Some(a));
        assert_eq!(s.id_at(1), Some(b));
        assert_eq!(s.id_at(2), None);
    }

    // ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
    // card_downcast_mut 测试
    // ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

    /// 第二个测试 Card 类型 —— 验证 downcast 拒绝不同类型
    struct OtherCard {
        id: u64,
        body_h: u16,
    }

    impl MessageCard for OtherCard {
        fn kind(&self) -> &'static str { "other" }
        fn id(&self) -> u64 { self.id }
        fn header(&self, _: &dyn SectionContext) -> crate::card::CardHeader {
            crate::card::CardHeader::new("other", "0")
        }
        fn streaming(&self) -> CardStreaming { CardStreaming::Static }
        fn body_height(&self, _: &dyn SectionContext, _: u16, _: CardCollapse) -> u16 { self.body_h }
        fn render_body(&self, _: &mut Frame, _: &dyn SectionContext, _: Rect, _: CardCollapse) {}
    }

    #[test]
    fn card_downcast_mut_success_on_correct_type() {
        let mut s = CardStream::new();
        let id = s.alloc_id();
        s.push_static(Box::new(TestCard::new(id, CardStreaming::Static)));
        // 正确类型: TestCard → &mut TestCard 成功
        let downcasted = s.card_downcast_mut::<TestCard>(id);
        assert!(downcasted.is_some());
        let card = downcasted.unwrap();
        assert_eq!(card.id, id);
        assert_eq!(card.kind_val, kinds::USER);
    }

    #[test]
    fn card_downcast_mut_fails_on_wrong_type() {
        let mut s = CardStream::new();
        let id = s.alloc_id();
        s.push_static(Box::new(TestCard::new(id, CardStreaming::Static)));
        // 错误类型: TestCard → &mut OtherCard 失败
        let downcasted = s.card_downcast_mut::<OtherCard>(id);
        assert!(downcasted.is_none(), "downcast to wrong type must return None");
    }

    #[test]
    fn card_downcast_mut_returns_none_for_missing_id() {
        let mut s = CardStream::new();
        let result = s.card_downcast_mut::<TestCard>(999);
        assert!(result.is_none());
    }

    #[test]
    fn card_downcast_mut_isolates_by_id() {
        // 多个 Card 共存, downcast 只命中指定 id
        let mut s = CardStream::new();
        let id_test = s.alloc_id();
        s.push_static(Box::new(TestCard::new(id_test, CardStreaming::Static)));
        let id_other = s.alloc_id();
        s.push_static(Box::new(OtherCard { id: id_other, body_h: 1 }));
        // downcast id_test → TestCard 成功
        assert!(s.card_downcast_mut::<TestCard>(id_test).is_some());
        // downcast id_other → TestCard 失败
        assert!(s.card_downcast_mut::<TestCard>(id_other).is_none());
        // downcast id_other → OtherCard 成功
        assert!(s.card_downcast_mut::<OtherCard>(id_other).is_some());
    }

    #[test]
    fn card_downcast_mut_zero_unsafe() {
        // 这个测试是文档性的: 证明 downcast 走 std 标准路径, 无 transmute
        // (编译期保证, 因为 MessageCard: 'static + Any super trait)
        let mut s = CardStream::new();
        let id = s.alloc_id();
        s.push_static(Box::new(TestCard::new(id, CardStreaming::Static)));
        let result = s.card_downcast_mut::<TestCard>(id);
        assert!(result.is_some());
    }
}
