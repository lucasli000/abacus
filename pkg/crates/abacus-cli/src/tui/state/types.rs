//! 类型定义 — 从 state/mod.rs 提取的纯数据类型
//!
//! 包含 AppState 使用的所有 struct/enum 定义及其 impl 块。
//! 按依赖层级排列：Cluster A（叶类型）→ Cluster B → Cluster C。

use std::collections::HashSet;
use std::time::Instant;

use serde::{Deserialize, Serialize};

// Re-export 供本模块内类型使用
pub use abacus_types::AbacusMode;
pub use crate::tui::api::{ToolRecord, ToolStatus};


// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// TaskRegistry — 后台任务生命周期管理
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// 后台任务注册表 —— 管理所有 tokio::spawn 的 LLM/工具任务
///
/// ## 根因设计
/// 之前用 `Option<JoinHandle<()>>` 只能保存一个任务，Meeting 模式多个 specialist
/// 并发时旧 handle 被覆盖导致无法取消。TaskRegistry 支持多任务并发管理。
///
/// ## 使用方式
/// - `register(handle)` 注册新任务，返回 TaskId
/// - `cancel(id)` 取消单个任务
/// - `cancel_all()` 取消所有任务（Esc 取消时调用）
/// - `reap_finished()` 清理已完成任务（主循环定期调用）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TaskId(u64);

pub struct TaskRegistry {
    next_id: u64,
    active: Vec<(TaskId, tokio::task::JoinHandle<()>)>,
}

impl Default for TaskRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl TaskRegistry {
    pub fn new() -> Self {
        Self { next_id: 0, active: Vec::new() }
    }

    /// 注册一个新任务，返回 ID
    pub fn register(&mut self, handle: tokio::task::JoinHandle<()>) -> TaskId {
        let id = TaskId(self.next_id);
        self.next_id += 1;
        self.active.push((id, handle));
        id
    }

    /// 取消并移除单个任务
    pub fn cancel(&mut self, id: TaskId) {
        if let Some(pos) = self.active.iter().position(|(i, _)| *i == id) {
            let (_, handle) = self.active.remove(pos);
            handle.abort();
        }
    }

    /// 取消所有任务（Esc 取消或重置会话时用）
    pub fn cancel_all(&mut self) {
        for (_, handle) in self.active.drain(..) {
            handle.abort();
        }
    }

    /// 清理已完成的 task（避免 JoinHandle 泄漏）
    pub fn reap_finished(&mut self) {
        self.active.retain(|(_, h)| !h.is_finished());
    }

    pub fn active_count(&self) -> usize {
        self.active.len()
    }
}

impl Drop for TaskRegistry {
    fn drop(&mut self) {
        // tokio JoinHandle::drop 只 detach 不 abort，
        // 必须显式 abort 防止任务泄漏到新 session
        for (_, handle) in self.active.drain(..) {
            handle.abort();
        }
    }
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// Enums
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// V29.16: 滚动动作 — set_scroll 单一入口的语义参数
///
/// ## 设计意图 (SSOT)
/// V29.5/V29.15 暴露问题: state.scroll 写入散落在 14 处 (event/state/components),
/// clamp/dirty 逻辑各处重复, 漏写一处即 bug (V29.15 add_message 漏 dirty 即先例).
/// V29.16 把所有写入收敛到 AppState::set_scroll(ScrollAction) 单一入口:
/// - clamp / max_scroll 计算 一次写, 调用方零重复
/// - rendered_lines_dirty 内部统一标记, 不再依赖调用方手动 set
/// - 调用方只表达 "意图" (向上 N 行 / 到底 / 锚定 delta), 不暴露实现
///
/// ## 引用关系
/// - 写: AppState::set_scroll (state/mod.rs)
/// - 读消费: render_messages_in_card / panel render (components/mod.rs)
/// - 触发源: event/mod.rs (键盘/鼠标), state/mod.rs (set_mode), components (锚定)
///
/// ## 生命周期
/// 值类型, 调用即用即抛; 不持有状态, 仅作为 set_scroll 参数传递
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ScrollAction {
    /// 滚到底部 (auto-follow); Home/End/clear_state/scroll_to_message-bottom 用
    ToBottom,
    /// 向上滚 N 行 (远离底部, scroll 增加); 自动 clamp 到 max_scroll
    Up(usize),
    /// 向下滚 N 行 (接近底部, scroll 减少); saturating 到 0
    Down(usize),
    /// 直接设到绝对位置; clamp 到 max_scroll
    Absolute(usize),
    /// V29.11 折叠锚定: 锚点行号变化的 delta 调整
    /// after_rows >= before_rows: scroll += diff (锚点下移, 视图也下移)
    /// after_rows  < before_rows: scroll -= diff (锚点上移, 视图也上移)
    AnchorAdjust { after_rows: usize, before_rows: usize },
    /// 模式切换恢复; 不 clamp (新模式 max_scroll 此刻可能未刷新)
    Restore(usize),
}

/// V33: AbacusMode 已迁到 abacus-types::AbacusMode（4 模式 DAG 流转 SSoT）
/// 本文件 re-export，保持 cli 内既有 `use crate::tui::state::AbacusMode` 不破坏
/// 消息角色
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub enum MsgRole {
    User,
    Session,
    Expert(String),
}

/// 命令参数 Picker 类型
///
/// 引用关系：PickerState.kind 驱动 render_picker_popup 渲染分支 + apply_picker_selection 分发
/// 生命周期：state.picker = Some 时弹出；Enter/Esc 关闭
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PickerKind {
    /// 模型选择（带 thinking slider）
    Model,
    /// 主题选择（带色板预览）
    Theme,
    /// 思考深度（off/low/medium/high/max）
    Thinking,
    /// 模式切换（Clarify / Meeting）
    Mode,
    /// 审查类型（plan / diff / security）+ strict toggle
    Review,
    /// 历史 session 恢复
    Resume,
    /// 输入历史重发
    History,
    /// Meeting 操作菜单（进入会诊 / 专家配置 / 历史记录）
    Meeting,
    /// 2026-05-28: 场景预设选择
    Preset,
}

/// Picker 状态
///
/// 引用关系：state.picker = Some(...) 时 overlay 渲染 picker_popup；
///           Esc 关闭、Enter 应用、↑↓ 移选中
/// 生命周期：handle_slash_command 拦截无参命令时打开 / Enter|Esc 关闭
/// 设计意图：把输入框 `/cmd <param>` 体验升级为 IDE 风格"先选后执行"
#[derive(Clone, Debug)]
pub struct PickerState {
    pub kind: PickerKind,
    /// 候选项 ID（传给 cmd handler 的实际参数值）
    pub items: Vec<String>,
    /// 候选项显示标签（可含描述/图标，与 items 一一对应）
    pub labels: Vec<String>,
    /// 当前选中索引
    pub selected: usize,
    /// 当前活跃值的索引（▶ 标记当前已生效项；Some=已知，None=无）
    pub current: Option<usize>,
    /// V29.8: 分组数据 — Some(vec![(provider_name, items[range])])
    ///   None = 不分组(Theme/Thinking picker), Some = 按组渲染(Model picker)
    ///   渲染时遍历 groups, 每组先插组标题行再渲染该 range 内的 items
    pub groups: Option<Vec<(String, std::ops::Range<usize>)>>,
    /// V29.8: 是否在底部显示 thinking 深度调节器
    ///   true = 渲染 "思考: ◀ {depth} ▶ · ←→ 调整" 行, ←→ 拦截路由到 thinking 调整
    ///   false = 默认 picker 行为
    pub show_thinking_slider: bool,
    /// 防键重复保护：picker 打开时记录时刻，150ms 内 Enter 无效
    pub opened_at: std::time::Instant,
    /// Review picker 专用：Space 切换 strict 模式（verdict≠pass 阻断后续执行）
    /// 引用：render_picker_popup 渲染 toggle 行；apply_picker_selection 读取并传给 /review
    /// 生命周期：picker 打开时初始化为 false；picker 关闭时随 PickerState take 消耗
    pub review_strict: bool,
}

/// 流式 tool 执行状态
/// V11: 区分进行中 / 成功 / 失败三态（之前 bool 仅区分"完成与否"丢失了 success 信息）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamingToolStatus {
    /// 工具调用进行中，耗时未知
    Running,
    /// 工具调用成功完成
    Success,
    /// 工具调用失败（StreamChunk::ToolEnd.success=false）
    Failed,
}

/// V40: 统一时序流条目 — 替代分区渲染模型
///
/// 所有 streaming 事件按到达顺序 push 到 `streaming_timeline: Vec<TimelineEntry>`。
/// 渲染时顺序遍历，每种类型用对应样式直接构建 `Line<'static>`。
///
/// 引用关系：
/// - 写入：run.rs chunk drain loop（ToolStart/ToolArgs/ToolEnd/Thinking/TextDelta/Iteration）
/// - 读取：components/mod.rs streaming appendix 遍历渲染
/// 生命周期：streaming 开始时首次 push → streaming 结束时 reset_streaming 清空
#[derive(Clone, Debug)]
pub enum TimelineEntry {
    /// LLM 思考摘要（仅保存首行 preview，全文在 streaming_thinking）
    Thinking { summary: String },
    /// 工具生命周期（ToolStart 创建，ToolArgs/ToolEnd 原地更新）
    Tool {
        name: String,
        /// 从 args 提取的关键上下文（路径/命令/URL/查询）
        context: String,
        status: StreamingToolStatus,
        duration_ms: Option<u64>,
        /// 失败分类（Timeout/Panic/Cooldown 等）
        failure_kind: Option<String>,
        /// 对应 trace_events 中的 id（用于获取 diff/output）
        trace_id: u64,
    },
    /// 工具输出摘要（bash stdout 首行/read 行数/search 匹配数）
    ToolOutput { summary: String },
    /// 正文文本区段（指向 streaming_text 的 byte range）
    /// mdstream 渲染 streaming_text[start..end]
    Text { start: usize, end: usize },
    /// 迭代边界（多轮工具调用之间的分隔）
    Iteration { number: u32 },
    /// V41: ToolAgent 批量执行汇总（替代多个 Tool entry 刷屏）
    ToolAgent {
        icon: String,
        name: String,
        call_count: usize,
        summary: String,
        details: Vec<String>,
    },
}

/// Phase 3: 流式内容块 — 按逻辑分组（thinking/tool-group/text/iteration）
/// 替代 TimelineEntry 的线性渲染，支持折叠/展开和噪音过滤
#[derive(Clone, Debug)]
pub enum StreamingBlock {
    Thinking {
        id: u64,
        summary: String,       // 最近 2 行
        full_text: String,     // 完整内容
        collapsed: bool,
        duration_ms: Option<u64>,
    },
    ToolGroup {
        id: u64,
        tool_name: String,
        calls: Vec<ToolCallSummary>,
        collapsed: bool,
    },
    Text {
        id: u64,
        byte_range: (usize, usize),
    },
    Iteration {
        number: u32,
    },
}

impl StreamingBlock {
    pub fn id(&self) -> u64 {
        match self {
            StreamingBlock::Thinking { id, .. }
            | StreamingBlock::ToolGroup { id, .. }
            | StreamingBlock::Text { id, .. } => *id,
            StreamingBlock::Iteration { number } => *number as u64,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ToolCallSummary {
    pub trace_id: u64,
    pub context: String,
    pub status: StreamingToolStatus,
    pub duration_ms: Option<u64>,
    pub failure_kind: Option<String>,
}

/// 块类型
#[derive(Clone, Serialize, Deserialize)]
pub enum BlockKind {
    Think,
    ToolCall,
    Checklist,
}

/// 消息内容段
///
/// V28 Trace 重构: 新增 `Trace { event_ids }` 变体, 让消息只持有对 trace_events 的 u64 引用,
/// 而不再内嵌 Block(Think) / Block(ToolCall) 的全文。Block 变体保留供 Checklist 使用 + 旧
/// session 文件向上兼容(旧 messages 中的 Block(Think/ToolCall) 仍能正常渲染)。
#[derive(Clone, Serialize, Deserialize)]
pub enum MsgContent {
    Stream(String),
    Block {
        kind: BlockKind,
        summary: String,
        detail: String,
        collapsed: bool,
    },
    /// V28: 引用一组 trace events(SSOT 在 state.trace_events)
    /// - collapsed=true: 单行 `▸ trace · N行思考 · M工具 · X.Ys` 摘要
    /// - collapsed=false: 按 event_ids 顺序就地展开,每个 event 一子块
    /// - expanded_event_ids: 单 event 详情"超 N 行折叠"中点击全展开的 id 集合
    Trace {
        event_ids: Vec<u64>,
        collapsed: bool,
        #[serde(default)]
        expanded_event_ids: HashSet<u64>,
    },
}

/// 一条消息 (多内容段混排)
#[derive(Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: MsgRole,
    pub parts: Vec<MsgContent>,
    pub time: String,
}

impl Message {
    pub fn new_user(text: impl Into<String>, time: impl Into<String>) -> Self {
        Self {
            role: MsgRole::User,
            parts: vec![MsgContent::Stream(text.into())],
            time: time.into(),
        }
    }

    pub fn new_session(parts: Vec<MsgContent>, time: impl Into<String>) -> Self {
        Self {
            role: MsgRole::Session,
            parts,
            time: time.into(),
        }
    }

    pub fn new_expert(
        name: impl Into<String>,
        parts: Vec<MsgContent>,
        time: impl Into<String>,
    ) -> Self {
        Self {
            role: MsgRole::Expert(name.into()),
            parts,
            time: time.into(),
        }
    }
}

/// 事件流条目
///
/// V28 起被 TraceEvent 取代;此 struct 仅保留供 v1 session 反序列化兜底
/// (`SessionExport.events: Vec<EventEntry>` 在加载时转换为 TraceEvent::Generic)。
/// 新代码请用 push_trace 而非 add_event 直写 EventEntry。
#[derive(Clone, Serialize, Deserialize)]
pub struct EventEntry {
    pub time: String,
    pub category: String, // "llm" | "tool" | "skill" | "session" | "mcip" | "inertia"
    pub content: String,
    pub level: EventLevel,
}

#[derive(Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum EventLevel {
    Info,
    Notice,
    Warning,
}


// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// V28 Trace 数据模型 — Single Source of Truth
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// 一条 trace 事件:覆盖原 EventEntry 全部能力 + LLM 思考/工具调用富数据
///
/// 引用关系:
/// - 由 `state.trace_events: Vec<TraceEvent>` 持有(SSOT)
/// - 被 `MsgContent::Trace.event_ids` 引用(消息内折叠摘要 + 就地展开)
/// - 被 `streaming_trace_ids` 引用(流式期间临时聚集,落档时转移)
/// - 被 `render_tab_timeline` 读取(右侧 panel 时间线)
///
/// 生命周期:
/// - 创建: state.push_trace(...) 分配 next_trace_id 后 push
/// - 销毁: MAX_EVENTS 上限触发 FIFO 裁剪;外部引用悬挂时渲染层 fallback `[event 已过期]`
#[derive(Clone, Serialize, Deserialize)]
pub struct TraceEvent {
    pub id: u64,
    pub time: String,
    pub category: String, // 保留 EventEntry 旧字段,timeline 图标映射(◐⚙●○)
    pub level: EventLevel,
    pub kind: TraceKind,
    pub duration_ms: Option<u64>,
}

/// trace 事件分类,决定渲染形态
///
/// `#[serde(tag = "type")]` 让 v2 session 文件自描述,便于版本演化
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum TraceKind {
    /// 兼容旧 EventEntry — 单行文本事件(llm 状态 / mcip 通知 / inertia 警告等)
    Generic { content: String },
    /// 模型思考块 — markdown 富文本,消息区可折叠展开
    Thinking { text: String, lines: usize },
    /// 工具调用 — 含 args / output / 执行状态,详情可展开
    ToolCall {
        name: String,
        args: String,
        output: Option<String>,
        status: ToolStatus,
    },
    /// 回复完成标记 — token 统计,主要给 timeline 用(消息区不重复展示)
    Reply { tokens: u32 },
}

/// 看板 Tab
///
/// V33 场景化拆分（引用关系: render_panel 4 mode 分支 + cycle_focus 过滤）：
/// - Timeline: 「现场」场景——timeline 主导 + 当前激活的轻量记忆/工具摘要
/// - Tasks/Agenda: 「任务/议程」场景——Team/Plan 看板 / Meeting 议程
/// - Quant: 「量化」场景——📊 统计（token/费用/轮次）+ 知识宫殿全量层级树
///   设计意图：把"跟现场"与"复盘量化"两种用户意图分到独立 Tab，避免摘要拥挤
/// - Custom: 用户通过 custom_tabs 注册的扩展 Tab
///
/// V33 移除：Memory/Components 变体（render_panel 4 mode 都未路由；render_tab_memory
///   被 Timeline 复用作"现场"内子区块；render_tab_components 已删除）
///
/// 生命周期：每帧渲染时通过 panel_tab 决定走哪个分支；set_mode 会做边界保护
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum PanelTab {
    Timeline,
    Tasks,
    /// V33: 「量化」场景——会话统计 + 知识宫殿全量层级
    Quant,
    /// 用户自定义 Tab（通过 custom_tabs 注册）
    Custom(usize), // index into AppState.custom_tabs
}

/// V35: Timeline 分组缓存条目 — 按语义阶段分组的工具事件
///
/// 引用关系:
///   生产者: panel::rebuild_timeline_groups (render_tab_scene 内按需调用)
///   消费者: panel::render_tab_scene Timeline 区渲染
///   生命周期: AppState.timeline_groups_cache 持有，trace_events 变化时整体重建
#[derive(Debug, Clone)]
pub struct TimelineGroup {
    /// 阶段标签（信息收集 / 代码修改 / 执行验证 / ...）
    pub label: String,
    /// 时间戳字符串（"09:23"）
    pub timestamp: String,
    /// 已格式化的子事件行（直接渲染）
    pub lines: Vec<String>,
    /// 是否为最后一组（最后一组可能仍在进行）
    pub is_active: bool,
}

/// V40: 仪表盘 Tab（右下区域）
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DashboardTab {
    /// 健康仪表盘（默认）：Context% + KV + 费用 + 轮次
    Health,
    /// 自动化状态：Cron/Watch 任务 + 待审阅
    Auto,
}

/// 面板滚动焦点区块（↑↓ 操作哪个区块）
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PanelSection {
    Timeline,
    Knowledge,
}

impl PanelSection {
    pub fn toggle(&self) -> Self {
        match self {
            PanelSection::Timeline => PanelSection::Knowledge,
            PanelSection::Knowledge => PanelSection::Timeline,
        }
    }
}

impl PanelTab {
    pub fn label(&self) -> &'static str {
        match self {
            PanelTab::Timeline => "现场",
            PanelTab::Tasks => "任务",   // V42-B: 保留 variant 用于向后兼容，panel dispatch 不再使用
            PanelTab::Quant => "仓库",   // V35: 量化 → 仓库
            PanelTab::Custom(_) => "自定义",
        }
    }

    /// V35: 两模式统一两 Tab — 现场(Timeline) + 仓库(Quant)
    /// Meeting 专家议程并入现场 Tab 的 Focus 区，不再单独 Tasks Tab
    pub fn all_with_custom(_mode: AbacusMode, custom_count: usize) -> Vec<PanelTab> {
        let mut tabs = vec![PanelTab::Timeline, PanelTab::Quant];
        for i in 0..custom_count {
            tabs.push(PanelTab::Custom(i));
        }
        tabs
    }

    /// 静态 Tab 列表（两模式统一）
    pub fn all(_mode: AbacusMode) -> &'static [PanelTab] {
        &[PanelTab::Timeline, PanelTab::Quant]
    }

    pub fn next(&self, mode: AbacusMode) -> PanelTab {
        let tabs = PanelTab::all(mode);
        let idx = tabs.iter().position(|t| t == self).unwrap_or(0);
        tabs[(idx + 1) % tabs.len()]
    }

    pub fn prev(&self, mode: AbacusMode) -> PanelTab {
        let tabs = PanelTab::all(mode);
        let idx = tabs.iter().position(|t| t == self).unwrap_or(0);
        tabs[(idx + tabs.len() - 1) % tabs.len()]
    }

    /// 包含自定义 Tab 的 next/prev
    pub fn next_with_custom(&self, mode: AbacusMode, custom_count: usize) -> PanelTab {
        let tabs = PanelTab::all_with_custom(mode, custom_count);
        let idx = tabs.iter().position(|t| t == self).unwrap_or(0);
        tabs[(idx + 1) % tabs.len()]
    }

    pub fn prev_with_custom(&self, mode: AbacusMode, custom_count: usize) -> PanelTab {
        let tabs = PanelTab::all_with_custom(mode, custom_count);
        let idx = tabs.iter().position(|t| t == self).unwrap_or(0);
        tabs[(idx + tabs.len() - 1) % tabs.len()]
    }
}

/// 输入框状态
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InputState {
    Ready,
    Typing,
    /// 补全候选列表可见
    Completing,
    /// LLM 推理/思考
    Thinking,
    /// 工具调用执行中
    Executing,
    /// LLM 流式输出中
    Outputting,
    Paused,
    /// 2026-05-28: 全屏编辑器模式 — 接管所有键盘输入
    /// 引用关系：handle_editor_key 消费 + render_fullscreen_editor 渲染
    /// 生命周期：open_editor() 创建 → close_editor() / submit 销毁
    Editor,
}

/// 2026-05-28: 全屏编辑器状态
/// 引用关系：render_fullscreen_editor 读取渲染 + handle_editor_key 更新
/// 生命周期：open_editor() 创建 → close_editor() 销毁
#[derive(Debug, Clone)]
pub struct EditorState {
    /// 编辑器内滚动偏移（首行行号，0-based）
    pub scroll_top: usize,
    /// 打开时刻（防 150ms 内重复触发）
    pub opened_at: std::time::Instant,
    /// 渲染侧写入的实际可见行数（键盘侧用于精确 PgUp/PgDn 计算）
    /// 引用关系：render_fullscreen_editor 每帧写入 → handle_editor_key PgUp/PgDn 读取
    /// 使用 Cell 允许在 &self（渲染期间 &AppState）下修改
    pub last_visible_h: std::cell::Cell<usize>,
    /// Shift+Arrow 选区起始 byte offset（None = 无选区）
    pub selection_anchor: Option<usize>,
}

/// 全局焦点区域
///
/// ## 切换契机（V32 起多触发器并存）
/// - **显式快捷键**：`Ctrl+B` cycle Panel ↔ CommandHint（兜底）
/// - **意图前置**（auto_route_focus, V32）：方向键/Tab → Panel；首位 `/` → CommandHint
/// - **事件磁吸**（try_magnet_focus, V32）：agent 消息/trace 事件抵达 + 用户离手 ≥ 2s → Panel
/// - **鼠标点击**：panel 列上半 → Panel；下半 → CommandHint
/// - **命令选完 Enter**：自动切回 Panel
///
/// ## 设计
/// 焦点区域只在「看板（Panel）」和「命令提示框（CommandHint）」之间切换。
/// 输入栏始终可接收字符**不参与焦点循环** —— Esc 在非输入态会回到默认浏览（Panel）。
/// 焦点仅影响方向键路由 + 视觉高亮（边框/标题色）+ 200ms 切换脉冲。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Focus {
    /// 输入框（V32 新增显式档；用户输入字符默认归此处，便于 Esc 链统一回点）
    Input,
    /// 看板面板（上下滚动内容 + Tab/Shift+Tab 切换 Tab 页）
    Panel,
    /// 命令提示框（底部命令栏，方向键选择候选命令）
    CommandHint,
}

impl Focus {
    /// 正向焦点切换（Ctrl+B cycle 兜底）
    /// V32 三档循环：Input → Panel → CommandHint → Input
    pub fn next(&self) -> Self {
        match self {
            Focus::Input => Focus::Panel,
            Focus::Panel => Focus::CommandHint,
            Focus::CommandHint => Focus::Input,
        }
    }

    /// 从快捷键数字选择焦点（保留 API 用于未来 Ctrl+1/2/3 扩展，当前未绑定）
    pub fn from_number(n: u8) -> Option<Self> {
        match n {
            1 => Some(Focus::Input),
            2 => Some(Focus::Panel),
            3 => Some(Focus::CommandHint),
            _ => None,
        }
    }
}

/// Toast 通知
#[derive(Clone)]
pub struct Toast {
    pub message: String,
    pub expire_at: Instant,
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// 专家 (Meeting / Team 模式)
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// 2026-05-27: Meeting 结论后的执行提案 — 等待用户确认
///
/// ## 生命周期
/// 创建(try_switch_mode) → 30s 内等待确认 → 确认(Y)/拒绝(n)/超时/其他输入 → 清空
#[derive(Debug, Clone)]
pub struct MeetingExecutionPrompt {
    /// 从结论中提取的行动项文本列表
    pub action_items: Vec<String>,
    /// 完整结论文本（用于组装 /plan 的 goal）
    pub full_conclusion: String,
    /// 是否建议使用 /team 而非 /plan（action_items > 3 且多领域）
    pub suggest_team: bool,
    /// 创建时刻（用于 30s 超时判断）
    pub created_at: std::time::Instant,
}

#[derive(Clone, Debug)]
pub struct Expert {
    pub name: String,
    pub domain: String,
    pub status: ExpertStatus,
    pub confidence: f64,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum ExpertStatus {
    Active,
    Idle,
    Done,
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// 任务卡片 (Team 模式)
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

#[derive(Clone)]
pub struct TaskCard {
    pub id: String,
    pub title: String,
    pub assignee: String,
    pub status: TaskStatus,
    pub progress: u8, // 0-100
    pub deps: Vec<String>,
    pub description: String,
}

#[derive(Clone, Copy, PartialEq)]
pub enum TaskStatus {
    Pending,
    InProgress,
    Done,
    Blocked,
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// SlashCommand — TUI 内可执行的后端命令
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// TUI slash commands 映射到后端 CoreLoop API。
/// 由 event handler 设置到 pending_slash_command，主循环异步执行。
#[derive(Debug, Clone)]
pub enum SlashCommand {
    ContextStatus,
    ContextCompress,
    ContextInject(String),
    /// V42: 资源感知状态（cost / token / latency 三维 budget）
    BudgetStatus,
    ToolList,
    ToolStats,
    SafetyStatus,
    ModelList,
    SessionInfo,
    /// V0.2: 查询当前 provider 信息
    Provider,
    // ─── Phase 4 file-undo ─────────────────────────────────
    /// 撤销最后一次写操作（None = 全 session 找最新）
    UndoLast { session_id: Option<String> },
    /// 撤销特定 seq
    UndoSeq { session_id: String, seq: u64 },
    /// 撤销整个 turn
    UndoTurn { session_id: String, turn: u32 },
    /// 重做（恢复最后一次撤销）
    Redo { session_id: String },
    /// 查看 session undo 历史（last N 条）
    UndoHistory { session_id: Option<String>, limit: usize },
    /// 跨 session 时间线（since hours 之前）
    UndoTimeline { since_hours: u64 },
    // ─── V29.9 (C4): Turnkey 全托管 ───────────────────────────
    /// 调 sandbox_engine.plan_from_nl(goal), 把生成的 TaskSpec 渲染为文本回显;
    /// 第一阶段不接 execute(), 仅展示 plan 让用户审阅
    /// 引用关系: cmd_turnkey 在显式 plan 标志时构造此变体
    /// 生命周期: 一次性 dispatch, 完成 → EngineResponse.turnkey_plan 携带 TaskSpec
    /// 回写到 state.pending_turnkey_plan; 用户后续 /turnkey execute 触发执行
    TurnkeyPlan(String),
    /// V29.10 (C4-Phase2): 用户审阅过 plan 后触发实际执行
    /// 引用关系: cmd_turnkey 'execute' 子命令读 state.pending_turnkey_plan 构造
    /// 生命周期: dispatch → sandbox_engine.execute(&task) → 文本结果
    /// 副作用: sandbox.execute 是非交互式自动循环, 通过 sandbox 事件 log 暴露进度
    TurnkeyExecute(abacus_types::sandbox::TaskSpec),

    // ─── V37-3: Reviewer 角色 API ─────────────────────────────
    /// 触发 Reviewer 角色调用（计划/代码/安全 三种）
    ///
    /// ## 引用关系
    /// - 设置：cmd_review（slash_commands.rs）解析 `/review <kind> [content]` 后构造
    /// - 消费：run.rs 主循环检测 pending_slash_command，调 send_reviewer_message_streaming
    ///
    /// ## 参数语义
    /// - kind: 决定使用哪个 system_prompt
    /// - content:
    ///   - 非空 → 直接审查该字符串
    ///   - 空 → 自动用 state.messages 末尾 Session 消息内容（review_plan 默认行为）
    ///
    /// ## 生命周期
    /// 一次性 dispatch；review 输出走标准 LLM 流式渲染（与 Planner 同样进入 messages）
    ReviewRole {
        kind: crate::tui::api::ReviewKind,
        content: String,
    },

    // ─── L-3/L-4/L-5: 通用 Agent 角色调用 ─────────────────────
    /// 触发任意 RoleKind 角色调用（CodeFixer / DocSummarizer / TestGenerator）
    ///
    /// ## 引用关系
    /// - 设置：cmd_role（slash_commands.rs）解析 `/role <kind> <content>` 后构造
    /// - 消费：run.rs 主循环 pending_slash_command 处理分支调 send_role_message_streaming
    ///
    /// ## 与 ReviewRole 的对偶
    /// - ReviewRole: 审查/判定（输出 verdict）
    /// - RoleInvoke: 产出制品（输出代码/文档/测试）
    /// 同构调用模式 + 同一 system_prompt_override 通道
    RoleInvoke {
        role: crate::tui::api::RoleKind,
        content: String,
    },

    // ─── V34: 执行策略 slash commands ─────────────────────────
    /// 触发规划+执行策略 — /plan <task> 直接发起，不切换 mode
    ///
    /// ## 引用关系
    /// - 设置：slash_commands.rs::cmd_plan 解析 `/plan <task>` 后构造
    /// - 消费：run.rs 主循环检测 pending_slash_command，调 send_plan_and_execute_streaming
    ///
    /// ## 生命周期
    /// 一次性 dispatch；在当前 Clarify mode 内部执行，不切换 mode
    ExecuteWithPlan {
        task: String,
    },

    /// 触发多 agent 执行策略 — /team <task> 直接发起，不切换 mode
    ///
    /// ## 引用关系
    /// - 设置：slash_commands.rs::cmd_team 解析 `/team <task>` 后构造
    /// - 消费：run.rs 主循环检测 pending_slash_command，调 send_team_message
    ///
    /// ## 生命周期
    /// 一次性 dispatch；在当前 Clarify mode 内部执行，不切换 mode
    ExecuteWithTeam {
        task: String,
    },
}

/// V41: Plan 策略两阶段状态机
///
/// ## 状态流转
/// ```text
/// /plan → Researching → AwaitingApproval → (用户选择) → Executing → None
/// ```
///
/// ## 引用关系
/// - 写入: api/mod.rs send_plan_and_execute_streaming
/// - 读取: run.rs 主循环（检测 Approval UI）+ bars.rs 状态指示
#[derive(Debug, Clone, PartialEq)]
pub enum PlanPhase {
    /// Phase 1: Planner 研究 + 生成任务计划（期间可限制只读工具）
    Researching,
    /// Decision Point: 计划已生成，等待用户选择执行策略
    AwaitingApproval {
        /// 计划文本摘要
        plan_summary: String,
        /// 解析出的任务列表
        tasks: Vec<String>,
    },
    /// Phase 2: 按用户选定的策略执行中
    Executing {
        strategy: PlanExecutionStrategy,
    },
}

/// Plan 执行策略（用户在 Approval 时选择）
///
/// 对齐 abacus_core::ExecutionStrategy
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanExecutionStrategy {
    /// 自动执行——工具调用自动放行
    Auto,
    /// 逐步确认——每个敏感操作需确认
    StepByStep,
    /// 转为 Team 模式多专家并行执行
    Team,
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// AppState — 集中式状态
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// 记忆宫殿快照（异步从 core 拉取，用于仓库 Tab 展示 palace 本体数据）
#[derive(Debug, Clone, Default)]
pub struct PalaceSnapshot {
    /// 行为宫殿条目总数
    pub behavior_count: usize,
    /// 行为宫殿活跃条目数（confidence >= 0.1）
    pub behavior_active: usize,
    /// 行为宫殿高频 tag（按 frequency 降序，取 top 3）
    pub behavior_top_tags: Vec<(String, u32)>,
    /// 知识宫殿按 domain 聚合：(domain_name, entry_count)
    pub knowledge_domains: Vec<(String, u32)>,
    /// 知识宫殿总条目数
    pub knowledge_total: u32,
    /// 知识宫殿待复习条目数（SM-2 next_review <= now）
    pub knowledge_due: usize,
}



/// 知识宫殿调用记录（三层：宫殿 → 领域 → 实体）
#[derive(Debug, Clone)]
pub struct KnowledgeCallEntry {
    /// 宫殿名称（从 .abacus/projects/{slug}/memory/ 路径中解析的 slug 末段；记忆宫殿/ 路径默认为 "主体"）
    pub palace: String,
    /// 领域/子目录（如 "知识库/推演", "工作流", "图谱"）
    pub domain: String,
    /// 具体文件名（如 "execution-protocol.md"）
    pub entity: String,
    /// 调用次数
    pub count: u32,
}

/// V36-3: 单模型 token 统计（per_model 拆分）
/// V37-2: 加 derive Serialize/Deserialize 支持持久化（SessionExport 一并写出）
///
/// 引用关系：
/// - 生产者：run.rs 在累加 SessionTokenStats 时按 canonical model_id 同步写入
/// - 消费者：components::render_tab_quant 模型分布区块渲染
/// - canonical 化：lookup_model.aliased_to 解析后聚合（避免 deepseek-chat / deepseek-v4-flash 分两条）
/// - 持久化：SessionExport 通过 SessionTokenStats 整体序列化跨重启保留
///
/// 生命周期：会话级累计；切换模型时新增 entry，已有模型 entry 持续累加
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelTokenStats {
    #[serde(default)]
    pub prompt: u64,
    #[serde(default)]
    pub completion: u64,
    #[serde(default)]
    pub cached: u64,
    #[serde(default)]
    pub thinking: u64,
    #[serde(default)]
    pub cost_cny: f64,
    #[serde(default)]
    pub turns: u32,
}

/// 会话 Token 统计
///
/// 引用关系：
/// - 生产者：run.rs 接收 EngineResponse.stats(TurnStats) 时累加
/// - 消费者：components::render_tab_memory / render_tab_quant 统计区渲染
/// - 持久化：SessionExport 写入/读回（V37-2）
/// 生命周期：会话级累计；切换模型不重置（用户跨模型也想看总开销）
///
/// V37-2: 加 derive Serialize/Deserialize 支持持久化；所有字段加 #[serde(default)] 兼容旧文件
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionTokenStats {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
    #[serde(default)]
    pub cached_tokens: u64,
    /// V30：会话级思考 tokens 累加（completion_tokens 子集；信息透明用，不重复计费）
    /// 来源：TurnStats.thinking_tokens；引用 components::render_tab_memory 单独显示一行
    #[serde(default)]
    pub thinking_tokens: u64,
    /// 最新一轮的 prompt_tokens（set 语义，非累加）
    ///
    /// ## 设计意图
    /// total_tokens 是所有轮次的累计账单 token，不反映当前 context 窗口占用。
    /// prompt_tokens 是当轮发送给 LLM 的完整 context（含历史），才是真正的
    /// "context window 使用量"。InputBar 百分比用此字段，避免虚高。
    ///
    /// ## 引用关系
    /// - 写：run.rs 每轮 stats 到达时 set（不 +=）
    /// - 读：bars.rs InputBar context % 计算
    #[serde(default)]
    pub latest_prompt_tokens: u64,
    /// V31: 累计费用（CNY canonical）—— 按每轮 stats.model_id 查 model_registry 即时计算
    /// 引用关系：cost::estimate_turn_cost_cny → abacus_types::lookup_model_or_default
    /// 数据源真相：CNY（DeepSeek 官方计费货币），USD 经 fx_rate 现算
    #[serde(default)]
    pub cost_cny: f64,
    /// V31: 累计费用（USD）—— 由 cost_cny 经汇率折算保留，便于历史兼容查询
    /// 引用关系：cost::estimate_turn_cost_usd（fx_rate 来自 session FX 配置）
    #[serde(default)]
    pub cost_usd: f64,
    /// V36-3: 按 canonical model_id 拆分的 per-model 累计统计
    ///
    /// ## 引用关系
    /// - 生产者：run.rs 在累加 session_tokens 时同步写入（按 lookup_model.id 标准化 key）
    /// - 消费者：components::render_tab_quant 模型分布区块
    ///
    /// ## key 标准化
    /// - 通过 abacus_types::lookup_model 解析别名（aliased_to），失败回退原始 id
    /// - 例：deepseek-chat 与 deepseek-v4-flash 聚合到同一 key
    #[serde(default)]
    pub per_model: std::collections::HashMap<String, ModelTokenStats>,

    /// 累计压缩次数 — 每次 CompressEnd 事件 +1
    ///
    /// 引用关系：run.rs CompressEnd handler 累加；panel compact_stats 展示
    #[serde(default)]
    pub compress_count: u32,
    /// 累计压缩回收的 tokens — 每次 CompressEnd 累加 tokens_saved
    ///
    /// 引用关系：run.rs CompressEnd handler 累加；panel compact_stats 展示
    #[serde(default)]
    pub compress_tokens_saved: u64,

    /// V39-4: 按 AbacusMode 拆分的 per-mode 累计统计
    ///
    /// ## 引用关系
    /// - 生产者：run.rs 在累加 session_tokens 时同步写入（按当前 state.mode.label() 作 key）
    /// - 消费者：components::render_tab_quant 模式分布区块
    ///
    /// ## 与 per_model 的正交性
    /// - per_model: 按"用了哪个 LLM"切分 — 关注经济性
    /// - per_mode: 按"在哪个会话阶段"切分 — 关注生产力（澄清 vs 执行各占多少）
    ///
    /// ## key 设计
    /// 用 mode.label() 字符串（"Clarify" / "Meeting" / "Plan" / "Team"），
    /// 与 AbacusMode 枚举保持解耦（持久化文件可读 + 未来 mode 增减不破坏旧文件）
    ///
    /// ## V40-3: 推荐通过 mode_stats() helper 查询而非直接 .get(label)
    #[serde(default)]
    pub per_mode: std::collections::HashMap<String, ModelTokenStats>,
}

impl SessionTokenStats {
    /// V40-3: 按 AbacusMode 查询 per-mode 统计
    ///
    /// ## 设计意图
    /// 把"label 来自 mode.label()"封装为 contract，避免 cli 命令中硬编码 "Plan"/"Team" 字符串；
    /// 未来 mode 增减只需改 AbacusMode::label()，调用方零修改
    ///
    /// ## 返回值
    /// - Some(stats): 该 mode 已发生过 ≥1 次调用
    /// - None: 该 mode 未被使用过（与"花了 0 块"区分语义）
    pub fn mode_stats(&self, mode: AbacusMode) -> Option<&ModelTokenStats> {
        self.per_mode.get(mode.label())
    }

    /// V40-3: per-mode 累计费用总和（CNY）
    ///
    /// ## 与 cost_cny 的关系
    /// 理论上 == cost_cny；浮点累加有 ε 误差时以本值为准（mode 视图自洽）
    /// 调用方场景：跨 mode 决策（如"如果 Team 占比 > 50% 时警告"）
    pub fn total_per_mode_cny(&self) -> f64 {
        self.per_mode.values().map(|s| s.cost_cny).sum()
    }

    /// V40-3: 指定 mode 的费用占比（0.0..=1.0）— 跨 mode 比较的标准化接口
    ///
    /// ## 返回值
    /// - 0.0..=1.0：占比
    /// - 0.0：当 mode 无数据 或 总和为 0
    pub fn mode_cost_ratio(&self, mode: AbacusMode) -> f64 {
        let total = self.total_per_mode_cny();
        if total <= 0.0 { return 0.0; }
        self.mode_stats(mode).map(|s| s.cost_cny / total).unwrap_or(0.0)
    }
}

/// 文本选择区域（V40 鼠标拖拽选区）
///
/// V42-B: 字段保留, V40 drag selection 复制功能**保留**（升级而非删除）
/// 升级路径:
/// - 旧 V40: (msg_idx, char_idx) 精确定位 Vec<Message> + Vec<TraceEvent>
/// - 新 V42-B: 仍用 V40 路径 (本字段有效), Phase 14.1+ 可基于 CardStream 重写
#[derive(Debug, Clone)]
pub struct TextSelection {
    pub start_msg_idx: usize,
    pub start_char_idx: usize,
    pub end_msg_idx: usize,
    pub end_char_idx: usize,
}
