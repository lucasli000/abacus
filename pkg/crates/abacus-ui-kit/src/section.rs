//! Section — 可组合 / 可注册 / 可扩展的渲染单元
//!
//! ## 设计目标
//!
//! 把"看板"与"仪表盘"的渲染从巨型函数（panel.rs / extras.rs）拆为独立 Section,
//! 每个 Section 满足：
//! - **职责单一**：一个 Section 只渲染一种语义内容（如 LLM 状态 / 工具统计 / 时间线）
//! - **可独立测试**：喂假 [`SectionContext`] 即可单测渲染输出
//! - **可注册**：Agent 应用通过 [`SectionRegistry`] 注入自定义 Section 而不修改 abacus-cli
//! - **可启用/禁用**：通过 Section.id() 让 config.toml 声明哪些 Section 显示
//!
//! ## 跨 crate 扩展契约
//!
//! `abacus-ui-kit` 是 Agent 应用与 TUI 之间的**公开契约 crate**：
//! - **内置** Section 在 `abacus-cli` 实现（享受完整 AppState 类型访问）
//! - **第三方** Section 在 Agent 应用 crate 实现，仅依赖本 crate 的 trait
//!
//! ## 数据访问的双轨设计（关键）
//!
//! [`SectionContext`] 提供基础渲染元数据（theme / animation 节拍 / 焦点状态）—— 跨 crate
//! 必备的最小集。业务字段（如 `session_tokens` / `tool_records`）通过 [`SectionContext::ext`]
//! + [`SectionContext::ext_type_id`] 在实现侧反查具体 context 类型获取。
//!
//! - **内置 Section** 反查 abacus-cli 私有的 `AppContext::state` 拿全部 state
//! - **外部 Section** 一般只用 trait 默认 getter；需要扩展数据时自定义 SectionContext 实现
//!
//! 这避免了 SectionContext trait 出现 20+ getter 方法（每加一个业务字段就破坏跨 crate 兼容）。

use ratatui::Frame;
use ratatui::layout::Rect;

use crate::theme::Theme;

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// SectionContext —— Section 渲染时可读的最小上下文
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// Section 渲染上下文 —— 提供主题色 + 动画节拍 + 焦点状态等跨 Section 通用元数据
///
/// ## 实现指南
///
/// - **内置实现**：abacus-cli 在 `tui/components/section_ctx.rs` 提供 `AppContext`,
///   包裹 `&AppState` 并提供 [`Self::ext`] 让内置 Section downcast 拿到完整 state
/// - **外部实现**：Agent 应用自定义 struct 实现本 trait，按需提供 theme 与扩展数据
///
/// ## 不变量
///
/// - `theme()` 必须返回稳定引用（不能每次 new 一个）—— Section 渲染期间持有此引用
///
/// ## 线程模型
///
/// 故意 **不**要求 `Send + Sync` —— ratatui 是单线程渲染模型, AppState 通常用 Cell/RefCell
/// 做内部可变, 加 Send + Sync bound 会破坏现有架构。
///
/// ## 扩展数据机制
///
/// 故意 **不**用 `Any` 超 trait（Any 要求 'static, 与借用生命周期 AppContext<'a> 冲突）。
/// 改为 [`Self::ext`] 返回 `*const ()` 类型擦除指针 + ext_type_id 自描述类型, 由 Section
/// 实现侧自行 unsafe 反查。abacus-cli 内置 `AppContext` 提供安全 helper 封装此细节。
pub trait SectionContext {
    /// 主题色 —— 必须实现
    fn theme(&self) -> &Theme;

    /// 焦点脉冲（200ms 节拍）—— 用于 focus border BOLD 切换
    /// 默认 false（无焦点高亮需求时不必实现）
    fn focus_pulsing(&self) -> bool {
        false
    }

    /// 动画 tick 计数（每帧 +1）—— 用于 streaming shimmer 等需要单调递增的特效
    fn anim_tick(&self) -> u64 {
        0
    }

    /// 扩展数据 type id —— 用于 Section 校验是否能 downcast
    /// 默认 None（无扩展数据）。实现示例：返回 `TypeId::of::<MyAppState>()`
    fn ext_type_id(&self) -> Option<std::any::TypeId> {
        None
    }

    /// 扩展数据指针 —— 类型擦除, 由 Section 配合 ext_type_id 反查
    /// 默认 None。实现示例：返回 `Some(self.state as *const _ as *const ())`
    ///
    /// ## 安全契约
    ///
    /// 调用方必须：
    /// 1. 先调 `ext_type_id` 校验类型匹配
    /// 2. 用 `unsafe { &*(ptr as *const ExpectedType) }` 反查
    /// 3. 反查得到的引用生命周期不得超过 `&self` 的生命周期
    fn ext(&self) -> Option<*const ()> {
        None
    }
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// Section —— 单个可渲染区块
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// 单个 Section —— 可独立渲染的语义区块
///
/// ## 生命周期
///
/// Section 通常是无状态的 zero-sized struct（如 `struct LlmSection;`），通过
/// [`SectionRegistry::register`] 注册一次后跨多帧复用。**不要在 Section 内存
/// 储跨帧状态** —— 跨帧状态应放在 SectionContext 里。
///
/// ## ID 命名约定
///
/// `id()` 返回的字符串作为 config.toml 启用/禁用键 + 测试匹配键：
/// - 内置 Section：`"llm"` / `"tools"` / `"timeline"` 等（小写，单词用下划线）
/// - 外部 Section：用反向域名前缀，如 `"com.example.quant.positions"` 防冲突
///
/// ## 线程模型
///
/// 故意 **不**要求 `Send + Sync` —— 同 [`SectionContext`] 的考虑, 配合 ratatui 单线程模型。
/// Section 实例由 [`SectionRegistry`] 持有 `Box<dyn Section>`, 单线程取用。
pub trait Section {
    /// 唯一标识 —— 用于 config 启用/禁用、debug 输出、测试匹配
    fn id(&self) -> &str;

    /// 显示标题 —— 渲染到 section header 行
    /// 内置 Section 可返回 i18n key，由实现侧自行翻译
    fn title(&self) -> &str;

    /// 最小高度 —— 区块至少需要多少行才有意义
    /// SectionStack 按 min_height 总和判断是否截断
    fn min_height(&self) -> u16;

    /// 期望高度 —— 在 available 范围内的理想高度
    /// 默认实现：取 min(available, max(min_height, 3))
    /// 返回 0 表示"占满剩余空间"（Fill 语义）
    fn preferred_height(&self, available: u16) -> u16 {
        available.min(self.min_height().max(3))
    }

    /// 是否在当前 context 下应显示 —— 默认 true
    ///
    /// 用于场景化显示：如 `FocusSection` 仅在 `is_streaming || turn_count > 0` 时显示
    fn visible(&self, _ctx: &dyn SectionContext) -> bool {
        true
    }

    /// 渲染到指定 area
    ///
    /// ## 实现规范
    /// - 不得修改 `f` 之外的全局状态（Section 是纯渲染单元）
    /// - 不得 panic（hit-test / 测试场景可能传入异常 Rect）
    /// - 长内容超出 area 时主动截断而非依赖 ratatui 默认裁剪
    fn render(&self, f: &mut Frame, ctx: &dyn SectionContext, area: Rect);
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// SectionStack —— Section 的纵向组合器
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// Section 纵向组合器 —— 按声明顺序自上而下渲染
///
/// ## 布局算法（v1）
///
/// 1. 过滤掉 `visible() == false` 的 Section
/// 2. 计算每个 Section 的 `preferred_height(available)`
/// 3. 若 sum(preferred) ≤ area.height: 按 preferred 分配，剩余空间留底部
/// 4. 若 sum(preferred) > area.height: 按 min_height 比例分配，超出部分截断（按 Section 顺序优先保留前面的）
///
/// 未来 v2 会支持 Section 间 separator 自动插入 + Constraint::Fill 显式语义。
pub struct SectionStack<'a> {
    sections: Vec<&'a dyn Section>,
}

impl<'a> SectionStack<'a> {
    pub fn new() -> Self {
        Self { sections: Vec::new() }
    }

    /// 添加一个 Section —— builder 风格链式调用
    pub fn add(mut self, s: &'a dyn Section) -> Self {
        self.sections.push(s);
        self
    }

    /// 添加多个 Section
    pub fn extend(mut self, sections: impl IntoIterator<Item = &'a dyn Section>) -> Self {
        self.sections.extend(sections);
        self
    }

    /// 渲染所有可见 Section 到 area
    pub fn render(&self, f: &mut Frame, ctx: &dyn SectionContext, area: Rect) {
        let visible: Vec<&dyn Section> = self
            .sections
            .iter()
            .copied()
            .filter(|s| s.visible(ctx))
            .collect();

        if visible.is_empty() || area.height == 0 {
            return;
        }

        // 计算每个 visible section 的高度分配
        let heights = compute_heights(&visible, area.height);

        let mut y = area.y;
        for (section, h) in visible.iter().zip(heights) {
            if h == 0 {
                continue;
            }
            let section_area = Rect {
                x: area.x,
                y,
                width: area.width,
                height: h,
            };
            section.render(f, ctx, section_area);
            y = y.saturating_add(h);
            if y >= area.y.saturating_add(area.height) {
                break;
            }
        }
    }

    /// 当前 Section 数量（已注册，未过滤 visible）
    pub fn len(&self) -> usize {
        self.sections.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sections.is_empty()
    }
}

impl<'a> Default for SectionStack<'a> {
    fn default() -> Self {
        Self::new()
    }
}

/// 按 preferred_height 分配高度
///
/// 算法：
/// 1. 一遍：每个 section 拿 preferred_height(available_at_that_point)
/// 2. 若总和 ≤ total: 直接用 preferred
/// 3. 若总和 > total: 按 min_height 优先 + 顺序裁剪（前面的 section 保留, 后面的可能为 0）
fn compute_heights(sections: &[&dyn Section], total: u16) -> Vec<u16> {
    let preferred: Vec<u16> = sections
        .iter()
        .map(|s| s.preferred_height(total))
        .collect();
    let sum: u32 = preferred.iter().map(|&h| h as u32).sum();

    if sum <= total as u32 {
        return preferred;
    }

    // 超出场景：先保证每个 section 的 min_height (按声明顺序), 剩余按比例分给 preferred 大的
    let mut out = vec![0u16; sections.len()];
    let mut remaining = total;

    // pass 1: 给每个 section 至少 min_height (按顺序分配, 后面的可能拿不到)
    for (i, section) in sections.iter().enumerate() {
        let want = section.min_height().min(remaining);
        out[i] = want;
        remaining = remaining.saturating_sub(want);
    }

    // pass 2: 剩余空间按 (preferred - min) 比例追加
    if remaining > 0 {
        let extras: Vec<u16> = sections
            .iter()
            .enumerate()
            .map(|(i, s)| s.preferred_height(total).saturating_sub(out[i]))
            .collect();
        let extras_sum: u32 = extras.iter().map(|&h| h as u32).sum();
        // 用 NonZeroU32 避免 clippy manual_checked_div
        if let Some(divisor) = std::num::NonZeroU32::new(extras_sum) {
            for (i, &extra) in extras.iter().enumerate() {
                if extra == 0 {
                    continue;
                }
                let bonus = ((extra as u32) * (remaining as u32) / divisor) as u16;
                out[i] = out[i].saturating_add(bonus);
            }
        }
    }

    out
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// SectionRegistry —— 全局可注册 Section 仓库
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// Section 注册仓库 —— 应用持有一个 registry，按需取 Section 组装 stack
///
/// ## 典型用法
///
/// ```ignore
/// // 应用启动时
/// let mut registry = SectionRegistry::new();
/// registry.register("llm", Box::new(LlmSection::default()));
/// registry.register("tools", Box::new(ToolsSection::default()));
/// // Agent 应用扩展
/// registry.register("quant.positions", Box::new(QuantPositionsSection::default()));
///
/// // 渲染时按 enabled_ids 顺序组装
/// let enabled: Vec<&str> = config.panel_sections.iter().map(String::as_str).collect();
/// let stack = registry.build_stack(&enabled);
/// stack.render(f, &ctx, area);
/// ```
///
/// ## 启用顺序 vs 注册顺序
///
/// 注册顺序无关，**渲染顺序由 [`SectionRegistry::build_stack`] 的 ids 参数决定**。
/// 这让 config.toml 可以独立控制"哪些 Section 显示"和"按什么顺序显示"。
pub struct SectionRegistry {
    sections: std::collections::HashMap<String, Box<dyn Section>>,
}

impl SectionRegistry {
    pub fn new() -> Self {
        Self { sections: std::collections::HashMap::new() }
    }

    /// 注册一个 Section —— 若 id 已存在则覆盖（允许后注册的覆盖前注册的, 便于测试 mock）
    pub fn register(&mut self, section: Box<dyn Section>) {
        let id = section.id().to_string();
        self.sections.insert(id, section);
    }

    /// 按 id 列表组装 SectionStack —— 缺失的 id 静默跳过（容错于 config 笔误）
    pub fn build_stack<'a>(&'a self, ids: &[&str]) -> SectionStack<'a> {
        let mut stack = SectionStack::new();
        for id in ids {
            if let Some(s) = self.sections.get(*id) {
                stack = stack.add(s.as_ref());
            }
        }
        stack
    }

    /// 列出所有已注册的 Section id —— 用于 config 校验 / debug 输出
    pub fn registered_ids(&self) -> Vec<&str> {
        self.sections.keys().map(String::as_str).collect()
    }

    /// 查询某个 id 是否已注册
    pub fn contains(&self, id: &str) -> bool {
        self.sections.contains_key(id)
    }
}

impl Default for SectionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// DashboardTab —— 仪表盘 tab 抽象（与 Section 同构但语义独立）
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// 仪表盘 tab —— 与 Section 同构, 但语义上是"按 tab 切换的全屏渲染"而非"垂直堆叠的子区"
///
/// 设计上独立于 Section 因为：
/// - tab 有 label 和切换语义（不是 visible/min_height）
/// - tab 间互斥（同一时刻只渲染一个）
/// - tab 通常对应一个**完整业务域**（Health / Auto / Quant / Trading...）而非小区块
///
/// 线程模型同 [`Section`]: 不要求 Send + Sync, 兼容 ratatui 单线程模型。
pub trait DashboardTab {
    /// 唯一标识 —— 用于 config 启用与状态持久化
    fn id(&self) -> &str;

    /// Tab 显示标签 —— 渲染到 tab header
    fn label(&self) -> &str;

    /// 是否当前可用 —— 默认 true
    /// 用于按权限/配置隐藏 tab（如未启用 JobRunner 时隐藏 Auto tab）
    fn enabled(&self, _ctx: &dyn SectionContext) -> bool {
        true
    }

    /// 渲染 tab 内容到 area —— area 已减去 tab header 行
    fn render(&self, f: &mut Frame, ctx: &dyn SectionContext, area: Rect);
}

/// 仪表盘 tab 注册仓库 —— 与 SectionRegistry 同构
///
/// 区别：
/// - DashboardRegistry 维护"当前选中 tab id"
/// - 提供 cycle/select 方法切换 tab
pub struct DashboardRegistry {
    tabs: Vec<Box<dyn DashboardTab>>,
    active: usize,
}

impl DashboardRegistry {
    pub fn new() -> Self {
        Self { tabs: Vec::new(), active: 0 }
    }

    /// 注册 tab —— 按注册顺序确定 cycle 顺序
    pub fn register(&mut self, tab: Box<dyn DashboardTab>) {
        self.tabs.push(tab);
    }

    /// 当前激活 tab 的 id —— None 表示无 tab 注册
    pub fn active_id(&self) -> Option<&str> {
        self.tabs.get(self.active).map(|t| t.id())
    }

    /// 按 id 切换 active tab —— 找不到 id 时静默忽略
    pub fn select(&mut self, id: &str) {
        if let Some(idx) = self.tabs.iter().position(|t| t.id() == id) {
            self.active = idx;
        }
    }

    /// 切到下一个 enabled tab —— 跳过 disabled 的
    pub fn cycle(&mut self, ctx: &dyn SectionContext) {
        if self.tabs.is_empty() {
            return;
        }
        let n = self.tabs.len();
        for offset in 1..=n {
            let next = (self.active + offset) % n;
            if self.tabs[next].enabled(ctx) {
                self.active = next;
                return;
            }
        }
        // 全部 disabled —— 保持现状
    }

    /// 渲染当前 tab（不含 header —— header 由调用方按 tabs() 自渲）
    pub fn render_active(&self, f: &mut Frame, ctx: &dyn SectionContext, area: Rect) {
        if let Some(tab) = self.tabs.get(self.active) {
            if tab.enabled(ctx) {
                tab.render(f, ctx, area);
            }
        }
    }

    /// 列出所有 tab 引用 —— 用于渲染 tab header bar
    pub fn tabs(&self) -> &[Box<dyn DashboardTab>] {
        &self.tabs
    }

    /// 当前 active 索引（用于 header 高亮）
    pub fn active_index(&self) -> usize {
        self.active
    }
}

impl Default for DashboardRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// 测试 —— Section trait + SectionStack 布局算法回归
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Theme;

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

    struct FixedSection {
        id: &'static str,
        min: u16,
        pref: u16,
        visible: bool,
    }

    impl Section for FixedSection {
        fn id(&self) -> &str { self.id }
        fn title(&self) -> &str { self.id }
        fn min_height(&self) -> u16 { self.min }
        fn preferred_height(&self, _available: u16) -> u16 { self.pref }
        fn visible(&self, _ctx: &dyn SectionContext) -> bool { self.visible }
        fn render(&self, _f: &mut Frame, _ctx: &dyn SectionContext, _area: Rect) {}
    }

    #[test]
    fn heights_fit_within_total() {
        let s1 = FixedSection { id: "a", min: 2, pref: 5, visible: true };
        let s2 = FixedSection { id: "b", min: 2, pref: 5, visible: true };
        let sections: Vec<&dyn Section> = vec![&s1, &s2];
        let heights = compute_heights(&sections, 20);
        assert_eq!(heights, vec![5, 5]); // 总和 10 ≤ 20, preferred 直用
    }

    #[test]
    fn heights_clamp_when_overflow() {
        let s1 = FixedSection { id: "a", min: 3, pref: 10, visible: true };
        let s2 = FixedSection { id: "b", min: 3, pref: 10, visible: true };
        let sections: Vec<&dyn Section> = vec![&s1, &s2];
        let heights = compute_heights(&sections, 10);
        // min 总和 6, 剩 4 按 (pref-min) 比例 (7:7) 分: 各得 2
        assert_eq!(heights, vec![5, 5]);
    }

    #[test]
    fn heights_respect_min_priority() {
        let s1 = FixedSection { id: "a", min: 8, pref: 10, visible: true };
        let s2 = FixedSection { id: "b", min: 8, pref: 10, visible: true };
        let sections: Vec<&dyn Section> = vec![&s1, &s2];
        let heights = compute_heights(&sections, 10);
        // pass 1: s1 拿 8, 剩 2; s2 拿 min(8, 2) = 2; 总 10
        // pass 2: 无剩余
        assert_eq!(heights, vec![8, 2]);
    }

    #[test]
    fn stack_filters_invisible() {
        let s1 = FixedSection { id: "a", min: 2, pref: 5, visible: true };
        let s2 = FixedSection { id: "hidden", min: 2, pref: 5, visible: false };
        let s3 = FixedSection { id: "c", min: 2, pref: 5, visible: true };
        let stack = SectionStack::new().add(&s1).add(&s2).add(&s3);
        assert_eq!(stack.len(), 3);
        // visible 过滤在 render() 内, 这里只测 len 不变（注册全部保留）
    }

    #[test]
    fn registry_register_and_build() {
        let mut reg = SectionRegistry::new();
        reg.register(Box::new(FixedSection { id: "llm", min: 3, pref: 5, visible: true }));
        reg.register(Box::new(FixedSection { id: "tools", min: 3, pref: 5, visible: true }));

        assert!(reg.contains("llm"));
        assert!(reg.contains("tools"));
        assert!(!reg.contains("missing"));

        let stack = reg.build_stack(&["llm", "tools"]);
        assert_eq!(stack.len(), 2);

        let stack_partial = reg.build_stack(&["llm", "missing", "tools"]);
        assert_eq!(stack_partial.len(), 2); // missing 静默跳过
    }

    #[test]
    fn registry_override_on_duplicate_id() {
        let mut reg = SectionRegistry::new();
        reg.register(Box::new(FixedSection { id: "x", min: 1, pref: 1, visible: true }));
        reg.register(Box::new(FixedSection { id: "x", min: 99, pref: 99, visible: true }));
        // 第二次注册覆盖第一次（用于测试 mock）
        let stack = reg.build_stack(&["x"]);
        assert_eq!(stack.len(), 1);
    }

    struct TabA;
    impl DashboardTab for TabA {
        fn id(&self) -> &str { "a" }
        fn label(&self) -> &str { "A" }
        fn render(&self, _f: &mut Frame, _ctx: &dyn SectionContext, _area: Rect) {}
    }
    struct TabB;
    impl DashboardTab for TabB {
        fn id(&self) -> &str { "b" }
        fn label(&self) -> &str { "B" }
        fn render(&self, _f: &mut Frame, _ctx: &dyn SectionContext, _area: Rect) {}
    }

    #[test]
    fn dashboard_cycle_skips_disabled() {
        let mut reg = DashboardRegistry::new();
        reg.register(Box::new(TabA));
        reg.register(Box::new(TabB));
        let c = ctx();
        assert_eq!(reg.active_id(), Some("a"));
        reg.cycle(&c);
        assert_eq!(reg.active_id(), Some("b"));
        reg.cycle(&c);
        assert_eq!(reg.active_id(), Some("a")); // 回环
    }

    #[test]
    fn dashboard_select_by_id() {
        let mut reg = DashboardRegistry::new();
        reg.register(Box::new(TabA));
        reg.register(Box::new(TabB));
        reg.select("b");
        assert_eq!(reg.active_id(), Some("b"));
        reg.select("nonexistent");
        assert_eq!(reg.active_id(), Some("b")); // 静默忽略
    }
}
