//! MessageCard —— 消息流卡片公开契约
//!
//! ## 设计目标
//!
//! V42-B 重构：把消息流从"巨型 Vec<Line>"模型升级为"按角色分立的卡片流"。
//! 每张卡片是一个独立的渲染 + 折叠 + hit-test 单元, 通过 [`MessageCard`] trait 抽象。
//!
//! ## 与 Section 的关系
//!
//! `Section` 是**看板**配置化展示（每帧重渲染，纯函数）；
//! `MessageCard` 是**消息流**对话历史（可累积，含 streaming 状态，可滚动）。
//! 两者同源（trait 形态相似），但语义独立 — 互不混用。
//!
//! ## 跨 crate 扩展契约
//!
//! - **内置 Card** 在 `abacus-cli` 实现（UserCard / AbacusCard / LlmCard / ExpertCard）
//! - **第三方 Card** 在 Agent 应用 crate 实现, 仅依赖本 crate 的 trait
//!   通过 `CardRegistry::register` 注入（Phase 9 实现）
//!
//! ## 三档折叠语义
//!
//! - **Expanded**: 完整调试视图（含全部 args + raw output + 完整 diff）
//! - **Collapsed (默认)**: 用户友好总结视图（含 EditDiff 必要详情 + 路径/命令摘要）
//! - **Headless**: 极简, 只保留 header 行（用于长会话归档浏览, 通过 Ctrl+Shift+Space 切换）

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Color;

use crate::theme::Theme;
use crate::SectionContext;

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// CardKind —— 卡片类型标识
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// 卡片类型标识 —— 用 `&'static str` 而非 enum, 支持第三方扩展
/// 内置 4 种 + 任意第三方 (建议反向域名前缀, 如 "com.example.quant.position")
pub type CardKind = &'static str;

/// 内置 CardKind 常量
pub mod kinds {
    /// 用户消息卡（含 Enter 提交 + mid-turn signal 注入）
    pub const USER: &str = "user";
    /// Abacus 本地工作卡（工具调用 / skill 执行 / 工作流）
    pub const ABACUS: &str = "abacus";
    /// LLM 思考+回复卡
    pub const LLM: &str = "llm";
    /// Meeting 模式专家卡（含 LLM 内核 + 专家身份）
    pub const EXPERT: &str = "expert";
    /// LLM Thinking 卡（思考过程独立呈现）
    pub const THINKING: &str = "thinking";
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// CardCollapse —— 三档折叠状态
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// 卡片折叠状态 —— 三档语义
///
/// ## 切换路径
/// - 默认: 流式期间 Expanded（看实时进度）, turn 结束按 [`MessageCard::default_collapse`] 自动切档
/// - 用户 Space: Expanded ↔ Collapsed 切换（最常用）
/// - 用户 Ctrl+Shift+Space: 在三档间循环 Expanded → Collapsed → Headless → Expanded
///
/// ## 视觉规则（每张 Card 实现 [`MessageCard::render_body`] 时遵守）
/// - Expanded: 完整调试视图（raw args + 完整 output + 完整 diff）
/// - Collapsed: 用户友好视图（EditDiff 必显, 其他工具仅文件路径/命令）
/// - Headless: 不画 body, 仅边框 + header 行（卡片高度 = 2 边框 + 1 header = 3 行）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardCollapse {
    /// 完全展开（调试视图）
    Expanded,
    /// 折叠到用户友好总结（含 EditDiff 详情）
    Collapsed,
    /// 仅显示 header 行（最紧凑）
    Headless,
}

impl CardCollapse {
    /// 二档切换（Space 键, 跳过 Headless）
    pub fn toggle_binary(self) -> Self {
        match self {
            CardCollapse::Expanded => CardCollapse::Collapsed,
            CardCollapse::Collapsed => CardCollapse::Expanded,
            CardCollapse::Headless => CardCollapse::Expanded,
        }
    }

    /// 三档循环（Ctrl+Shift+Space 键）
    pub fn cycle_tri(self) -> Self {
        match self {
            CardCollapse::Expanded => CardCollapse::Collapsed,
            CardCollapse::Collapsed => CardCollapse::Headless,
            CardCollapse::Headless => CardCollapse::Expanded,
        }
    }
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// CardStreaming —— 卡片流式生命周期
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// 卡片流式状态 —— 影响边框形态 + 视觉特效
///
/// ## 视觉契约（由 [`crate::render_card`] 统一处理）
/// - `Static`: BorderType::Rounded `╭─╮` （已完成）
/// - `Active`: BorderType::Double `╔═╗` + 顶部 shimmer 光带动效（流式中）
/// - `Aborted`: BorderType::Plain `┌─┐` + 边框 error 色 + 标题加 "· interrupted"（中断）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardStreaming {
    /// 已完成静态卡片
    Static,
    /// 流式中（顶部 shimmer + 边框双线）
    Active,
    /// 已中断（用户 Esc / 网络错误 / 超时）
    Aborted,
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// CardHeader —— 卡片标题元数据
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// 卡片标题信息 —— 由 [`MessageCard::header`] 返回, 渲染层用于画 header 行
///
/// ## 渲染格式
/// ```text
/// ╭─ {title} ··· {trailing} ──╮     ← 标题左 + 右贴边 + 折叠箭头
/// │ {preview}                  │     ← 仅 Collapsed 状态显示（如 thinking 首句）
/// ╰────────────────────────────╯
/// ```
#[derive(Debug, Clone)]
pub struct CardHeader {
    /// 左侧标题（含 icon + 角色 + 元数据）, 例: "● LLM · deepseek-v4 · think:high"
    pub title: String,
    /// 右侧 trailing（时间戳 / 耗时 / 状态）, 例: "2.3s · ✓"
    pub trailing: String,
    /// 卡片整体色调（边框 + 标题）, None = 按 kind 取默认
    /// 默认映射: USER → theme.user, ABACUS → theme.abacus, LLM → theme.session, EXPERT → theme.expert
    pub color: Option<Color>,
    /// Collapsed 状态下 header 后追加的预览行（可选）
    /// 例: LLM 卡折叠时 preview = "思考: 用户对生命周期混淆..."
    pub preview: Option<String>,
}

impl CardHeader {
    /// 便捷构造 —— 仅 title + trailing
    pub fn new(title: impl Into<String>, trailing: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            trailing: trailing.into(),
            color: None,
            preview: None,
        }
    }

    pub fn with_color(mut self, color: Color) -> Self {
        self.color = Some(color);
        self
    }

    pub fn with_preview(mut self, preview: impl Into<String>) -> Self {
        self.preview = Some(preview.into());
        self
    }
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// CardHit —— hit-test 结果
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// 鼠标点击在卡片内部的命中分类 —— 主循环按此路由动作
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardHit {
    /// 点中 header 行（默认行为: toggle 折叠 Collapsed ↔ Expanded）
    Header,
    /// 点中 body 内某子元素（inner_y = 相对 body 顶部的 y 偏移）
    /// 卡片实现自己决定该 inner_y 对应什么动作（如 LLM 卡 inner_y=0 是 thinking 区, > 0 是 reply）
    Body { inner_y: u16 },
    /// 点中 header 右侧 ▸/▾ 折叠按钮（精细位置, 等同 Header 但意图明确）
    CollapseToggle,
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// MessageCard —— 核心 trait
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// 单张消息卡片 —— 独立渲染 / 折叠 / 滚动定位 / hit-test 的最小单元
///
/// ## 不变量（实现必须遵守）
///
/// 1. **`body_height(ctx, max_width)` 必须严格等于 `render_body` 实际输出的行数**
///    （滚动 + hit-test 的正确性依赖此契约）
///
/// 2. **`id()` 在卡片整个生命周期内不变**
///    （由 [`crate::CardStream`] 在 push 时分配, 不可修改）
///
/// 3. **render_body 不得修改 `f` 之外的全局状态**
///    （卡片是纯渲染单元, 状态变更走 [`MessageCard::append_*`] 等 mutator 方法）
///
/// ## `Self: 'static` 约束
///
/// `MessageCard` 强制实现类型是 `'static` —— 编译期保证所有 Card 都可安全
/// downcast 到具体类型（[`crate::CardStream::card_downcast_mut`] 走标准
/// `&mut dyn Trait → &mut dyn Any` coerce, 不需要 unsafe）。
///
/// 实现方应保证:
/// - Card 不持有非 'static 的借用（如 `&'a str`）
/// - 所有字段都是 owned data (String, Vec, Box 等)
///
/// 4. **不得 panic**
///    （hit-test 可能传入异常 Rect, 内容超长应主动截断）
///
/// ## 生命周期模型
///
/// - 创建: CardStream::push_static / push_active 时构造, 由 stream 分配 id
/// - streaming 期间: 通过卡片自身的 mutator 方法（如 LlmCard::append_reply）追加内容
/// - 完成: CardStream::finish_active 时 streaming → Static
/// - 折叠状态变更: CardStream::set_collapse / toggle_collapse, 卡片自身不存储 collapse
///   （collapse 状态由 CardStream::collapse_overrides 集中管理, 与卡片解耦）
pub trait MessageCard: 'static + std::any::Any {
    /// 卡片类型标识（kinds::USER 等 / 第三方自定义字符串）
    fn kind(&self) -> CardKind;

    /// 卡片唯一 id（由 CardStream 分配, 卡片实现需在构造时接受并存储）
    fn id(&self) -> u64;

    /// 卡片标题元数据（每帧重新计算）
    /// ctx 提供 theme 等元数据, 卡片自己决定 title 格式
    fn header(&self, ctx: &dyn SectionContext) -> CardHeader;

    /// 当前流式生命周期状态
    fn streaming(&self) -> CardStreaming;

    /// 默认折叠策略 —— turn 结束后 CardStream 自动应用
    /// User → Expanded（默认 expand, 短文本）
    /// Abacus → Collapsed（折叠总结视图）
    /// LLM → Expanded（reply 是核心内容, 不折叠）
    /// Expert → Expanded（同 LLM）
    fn default_collapse(&self) -> CardCollapse {
        CardCollapse::Expanded
    }

    /// 计算 body 渲染的行数（不含边框 + header）
    /// 必须严格等于 render_body 实际输出的行数
    ///
    /// 给定 collapse 状态计算
    fn body_height(
        &self,
        ctx: &dyn SectionContext,
        max_width: u16,
        collapse: CardCollapse,
    ) -> u16;

    /// 渲染 body 到指定区域（不含边框 + header）
    /// inner 已扣除边框 + header 行的区域
    ///
    /// collapse 由 CardStream 传入, 卡片按此切档渲染
    fn render_body(
        &self,
        f: &mut Frame,
        ctx: &dyn SectionContext,
        inner: Rect,
        collapse: CardCollapse,
    );

    /// hit-test —— 鼠标点击 (x, y) 落在卡片 body 内的命中分类
    /// inner 是 body 区域的 Rect, (x, y) 是绝对屏幕坐标
    /// 返回 None 表示不响应（如点击在空白行）
    fn hit_test(&self, inner: Rect, x: u16, y: u16) -> Option<CardHit> {
        // 默认实现: body 内任意点视为 Body { inner_y }
        if !rect_contains(inner, x, y) {
            return None;
        }
        let inner_y = y.saturating_sub(inner.y);
        Some(CardHit::Body { inner_y })
    }

    /// 返回实现类型的 TypeId —— downcast 的核心机制
    ///
    /// ## 为什么需要这个方法
    ///
    /// 标准 Rust downcast 模式需要 `dyn Any::downcast_mut::<T>`,
    /// 但 `dyn MessageCard` 不是 `dyn Any` (生命周期 + vtable 都不匹配)。
    /// 在 trait 内部暴露 TypeId, 让外部的 downcast 逻辑可以安全比较:
    ///
    /// ```ignore
    /// pub fn card_downcast_mut<T: 'static + MessageCard>(...) -> Option<&mut T> {
    ///     if std::any::TypeId::of::<T>() == card.card_type_id() {
    ///         // SAFETY: TypeId 匹配, 把 data 指针转成 T
    ///         unsafe { Some(&mut *(data_ptr as *mut T)) }
    ///     } else {
    ///         None
    ///     }
    /// }
    /// ```
    ///
    /// ## 实现要求
    ///
    /// builtin Card (UserCard / LlmCard / ExpertCard / AbacusCard) 必须 override,
    /// 返回 `std::any::TypeId::of::<Self>()`。第三方 Card 同样。
    /// 默认实现 panic, 防止"忘记 override 导致 downcast 永远失败"被静默接受。
    fn card_type_id(&self) -> std::any::TypeId
    where
        Self: 'static + Sized,
    {
        std::any::TypeId::of::<Self>()
    }
}

/// 辅助: Rect 是否包含坐标
fn rect_contains(rect: Rect, x: u16, y: u16) -> bool {
    x >= rect.x
        && x < rect.x.saturating_add(rect.width)
        && y >= rect.y
        && y < rect.y.saturating_add(rect.height)
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// 默认颜色映射 —— 内置 4 种 kind 的默认 theme 色
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// 按 kind 取默认色 —— 卡片 header.color 为 None 时调用
///
/// 内置映射:
/// - USER → theme.user (蓝)
/// - ABACUS → theme.abacus (紫, V42-B 新增字段)
/// - LLM → theme.session (绿/品牌色, 按 model 切换)
/// - EXPERT → theme.expert (紫, Meeting 模式)
/// - 第三方 → theme.muted (灰, 容错)
pub fn default_color_for_kind(kind: CardKind, theme: &Theme) -> Color {
    match kind {
        kinds::USER => theme.user,
        kinds::ABACUS => theme.abacus,
        kinds::LLM => theme.session,
        kinds::EXPERT => theme.expert,
        kinds::THINKING => theme.accent,
        _ => theme.muted,
    }
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// 测试
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SectionContext;

    struct TestCtx {
        theme: Theme,
    }

    impl SectionContext for TestCtx {
        fn theme(&self) -> &Theme {
            &self.theme
        }
    }

    fn ctx() -> TestCtx {
        TestCtx { theme: Theme::brand() }
    }

    struct FakeCard {
        id: u64,
        kind: CardKind,
        body_h: u16,
        streaming: CardStreaming,
        collapse_default: CardCollapse,
    }

    impl MessageCard for FakeCard {
        fn kind(&self) -> CardKind { self.kind }
        fn id(&self) -> u64 { self.id }
        fn header(&self, _: &dyn SectionContext) -> CardHeader {
            CardHeader::new(format!("test {}", self.id), "0s")
        }
        fn streaming(&self) -> CardStreaming { self.streaming }
        fn default_collapse(&self) -> CardCollapse { self.collapse_default }
        fn body_height(&self, _: &dyn SectionContext, _: u16, _: CardCollapse) -> u16 { self.body_h }
        fn render_body(&self, _: &mut Frame, _: &dyn SectionContext, _: Rect, _: CardCollapse) {}
    }

    #[test]
    fn collapse_toggle_binary_skips_headless() {
        assert_eq!(CardCollapse::Expanded.toggle_binary(), CardCollapse::Collapsed);
        assert_eq!(CardCollapse::Collapsed.toggle_binary(), CardCollapse::Expanded);
        // Headless 跳回 Expanded（不进入 Collapsed → Headless 循环）
        assert_eq!(CardCollapse::Headless.toggle_binary(), CardCollapse::Expanded);
    }

    #[test]
    fn collapse_cycle_tri_iterates_three_states() {
        let mut c = CardCollapse::Expanded;
        c = c.cycle_tri(); assert_eq!(c, CardCollapse::Collapsed);
        c = c.cycle_tri(); assert_eq!(c, CardCollapse::Headless);
        c = c.cycle_tri(); assert_eq!(c, CardCollapse::Expanded);
    }

    #[test]
    fn card_header_builder() {
        let h = CardHeader::new("> You", "09:23")
            .with_color(Color::Blue)
            .with_preview("hi there");
        assert_eq!(h.title, "> You");
        assert_eq!(h.trailing, "09:23");
        assert_eq!(h.color, Some(Color::Blue));
        assert_eq!(h.preview.as_deref(), Some("hi there"));
    }

    #[test]
    fn fake_card_basic_invariants() {
        let card = FakeCard {
            id: 42,
            kind: kinds::USER,
            body_h: 3,
            streaming: CardStreaming::Static,
            collapse_default: CardCollapse::Expanded,
        };
        let c = ctx();
        assert_eq!(card.kind(), "user");
        assert_eq!(card.id(), 42);
        assert_eq!(card.streaming(), CardStreaming::Static);
        assert_eq!(card.default_collapse(), CardCollapse::Expanded);
        assert_eq!(card.body_height(&c, 80, CardCollapse::Expanded), 3);
    }

    #[test]
    fn default_hit_test_returns_body_with_relative_y() {
        let card = FakeCard {
            id: 1, kind: kinds::USER, body_h: 5,
            streaming: CardStreaming::Static, collapse_default: CardCollapse::Expanded,
        };
        let inner = Rect::new(10, 20, 60, 5);
        // 点击 (15, 22) 在 inner 内, inner_y = 22 - 20 = 2
        assert_eq!(card.hit_test(inner, 15, 22), Some(CardHit::Body { inner_y: 2 }));
        // 点击 (5, 22) 在 inner 外（x 范围之外）
        assert_eq!(card.hit_test(inner, 5, 22), None);
        // 点击 (15, 26) 在 inner 外（y 范围之外, 26 ≥ 20+5）
        assert_eq!(card.hit_test(inner, 15, 26), None);
    }

    #[test]
    fn default_color_for_kind_maps_correctly() {
        let theme = Theme::brand();
        // 4 个内置 kind 各取对应色
        assert_eq!(default_color_for_kind(kinds::USER, &theme), theme.user);
        assert_eq!(default_color_for_kind(kinds::ABACUS, &theme), theme.abacus);
        assert_eq!(default_color_for_kind(kinds::LLM, &theme), theme.session);
        assert_eq!(default_color_for_kind(kinds::EXPERT, &theme), theme.expert);
        // 未知 kind fallback 到 muted
        assert_eq!(default_color_for_kind("com.example.custom", &theme), theme.muted);
    }

    #[test]
    fn kinds_constants_are_stable() {
        // 不变量: 内置 kind 字符串必须保持稳定 (session 文件按字符串持久化)
        assert_eq!(kinds::USER, "user");
        assert_eq!(kinds::ABACUS, "abacus");
        assert_eq!(kinds::LLM, "llm");
        assert_eq!(kinds::EXPERT, "expert");
        assert_eq!(kinds::THINKING, "thinking");
    }
}
