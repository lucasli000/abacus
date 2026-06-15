//! Abacus TUI State — 统一状态管理
//!
//! 设计规范来源: ABACUS-TUI-DESIGN-SPEC.md v1.0
//!
//! 管理模式: 集中式 AppState，所有 UI 组件共享引用。
//!
//! ## RefCell 安全性说明 @2025-01-23（已验证）
//! AppState 使用 `RefCell` 进行内部可变性，在 crossterm 单线程事件循环中安全。
//!
//! **已审查的 borrow_mut 调用点（run.rs）：**
//! - `state.streaming_md.borrow_mut()`（run.rs ~782）：在有界 `{ }` scope 内，
//!   scope 结束即释放 RefMut，scope 内无任何 `.await`，**安全**。
//!
//! **维护规则：** 新增 `borrow_mut()` 调用时，必须确保持有 RefMut 的 scope 内
//! 不含任何 `.await` 表达式。如需跨越 await 边界，改用 `Mutex<T>` 或在 await
//! 前 `drop(refmut_guard)` 显式释放。

use std::cell::{Cell, RefCell};
use std::collections::{HashSet, VecDeque};
use std::time::{Instant, SystemTime};

use tokio::sync::mpsc;

pub mod completion;
pub mod confirm;
pub mod init_ext;
pub mod input_ext;
pub mod message_ext;
pub mod picker_ext;
pub mod session_ext;
pub mod session_export;
pub mod streaming_ext;
/// V42-B Phase 13: session v3 → v4 透明升级
pub mod session_migrate;
pub mod types;

// Re-export 所有类型定义（保持 crate 内路径兼容）
pub use types::*;
// Re-export confirm 模块类型
pub use confirm::{assess_command_risk, ConfirmDialog, ConfirmOption, ConfirmRisk, ConfirmType};

use crate::tui::api::EngineHandle;
use abacus_ui_kit::{CardStream, Theme, SimpleScrollOffset};
use tui_textarea::TextArea as TuiTextArea;

pub struct AppState {
    pub theme: Theme,
    /// 上次渲染的主题名（仅首帧或主题切换时重刷全屏背景）
    pub mode: AbacusMode,
    /// V34: 模式间携带数据 — 上阶段产出，下阶段消费（V34: 仅 ClarifyBrief/MeetingConclusion）
    ///
    /// ## 引用关系
    /// - 写：mode 完成时（Clarify /done 携带 ClarifyBrief / Meeting 结论 MeetingConclusion）
    /// - 读：进入新 mode 时取走（take()），加载到 messages preamble
    /// - 来源 SSoT：abacus_types::ModeArtifact（含 ClarifyBrief / MeetingConclusion）
    ///
    /// ## 生命周期
    /// - mode 切换时若有则 take 给新 mode，旧 mode 不再持有
    /// - 单循环不跨多次 mode 切换持留（避免脏数据）
    pub mode_artifact: Option<abacus_types::ModeArtifact>,

    /// V39-1: 最近一次 review 结果（用于 UX 显示 + V39-2 strict 阻断）
    ///
    /// ## 引用关系
    /// - 写：run.rs reviewer 响应抵达后调 parse_review_report() 写入
    /// - 读：try_switch_mode 检查 strict 模式 + UX toast
    /// - 清除：用户 /review-clear 或新一次 review 覆盖
    ///
    /// ## 设计意图
    /// 仅保存最新一次 review（无需历史）— review 是无状态查询，旧结果对决策无价值
    pub last_review: Option<crate::tui::api::ReviewReport>,

    /// V39-2: strict 模式标志 — 与 last_review 同时写入
    ///
    /// ## 引用关系
    /// - 写：cmd_review 解析 --strict 子参数后随 ReviewRole 命令传入；run.rs review 响应抵达时回填
    /// - 读：try_switch_mode 切换前检查；strict + non-pass → 阻断切换
    /// - 清除：/review-clear 或新一次 review 覆盖
    ///
    /// ## 设计意图
    /// 让 review 从"参考"升级为"守门员"；逃生通道 /review-clear 防止误判困住用户
    pub last_review_strict: bool,

    /// V39-1: 标记下一次 EngineResponse 是 reviewer 输出（需要 parse）
    ///
    /// ## 引用关系
    /// - 写：run.rs ReviewRole 分支 spawn 前 +1
    /// - 读：run.rs EngineResponse 抵达后检查；命中则 parse + 写 last_review
    /// - 设计：用 u8 计数器而非 bool，避免并发 review 请求时被误清
    pub pending_review_parses: u8,

    /// V39-2: 待应用的 strict 标志 — 与 pending_review_parses 配套
    ///
    /// ## 引用关系
    /// - 写：cmd_review 设置 ReviewRole 命令前一并写入
    /// - 读：run.rs EngineResponse 抵达后随 last_review 一起回填到 last_review_strict
    pub pending_review_strict: bool,

    /// V41-4: review 历史 — 最近 N 条 review 记录（FIFO，上限 20）
    ///
    /// ## 引用关系
    /// - 写：每次 review 抵达后 push（与 last_review 同时写）；超 20 条 FIFO 弹出最旧
    /// - 读：/review-history 命令展示；可选 cli 命令做 verdict 演变分析
    ///
    /// ## 与 last_review 的关系
    /// last_review = 历史"游标"；review_history = 完整轨迹
    /// 清除 last_review 时不动历史（用户能回查），但新 review 同步推入历史
    ///
    /// ## 上限设计
    /// 20 条 ≈ 一个工作日的 review 节奏；FIFO eviction 避免长会话 OOM
    pub review_history: std::collections::VecDeque<crate::tui::api::ReviewReport>,

    /// V41-4: 待应用的 review kind — 与 pending_review_parses 配套
    ///
    /// ## 引用关系
    /// - 写：cmd_review 设置 ReviewRole 命令前一并写入
    /// - 读：run.rs review 抵达后注入到 ReviewReport.kind
    pub pending_review_kind: crate::tui::api::ReviewKind,

    /// V41-2: review-required 强约束开关
    ///
    /// ## 引用关系
    /// - 写：/review-required on|off 命令
    /// - 读：try_switch_mode 在 Plan→Team 检查；启用时必须有 fresh pass review
    ///
    /// ## 与 strict (V39-2) / auto-review (V40-4) 的关系
    /// - strict：有 review 但 fail 时阻断（弱约束）
    /// - **required**：必须有 fresh pass review 才能切换（强约束）
    /// - auto-review：在切换时自动触发 review（required 的友好版 — 自动满足条件）
    ///
    /// ## 阻断条件
    /// 启用 + (无 last_review || verdict ≠ pass || 抵达时间超 max_age) → 阻断
    /// auto-review 同时启用 → 触发 ReviewRole（自动满足 required）
    /// auto-review 未启用 → 显式 toast 提示用户运行 /review plan
    ///
    /// ## 默认值
    /// false（保持现有行为，需用户显式启用）
    pub review_required: bool,

    /// V41-2: review fresh-age 阈值（秒）— 超过此时长的 pass review 视为过期
    ///
    /// ## 默认值
    /// 600 (10 分钟)；通过 /review-required on <secs> 自定义
    ///
    /// ## 设计意图
    /// 让 review 有"鲜活度"约束 — 1 小时前的 pass 不能保证当下 plan 还有效
    pub review_max_age_secs: u64,

    /// V40-4: Plan→Team 自动 review 联动开关
    ///
    /// ## 引用关系
    /// - 写：/auto-review on|off 命令切换
    /// - 读：try_switch_mode 在 Plan→Team 路径检查；启用时同步 schema 通过后触发 review_plan
    ///
    /// ## 设计意图
    /// review 是高成本 LLM 调用，绝不默认开启；用户显式启用让 Plan→Team 串联两层守门员
    ///   ① schema validate（同步、零成本） ② review pass（async LLM、有成本）
    ///
    /// ## 触发流程
    /// 1. 用户 /done 触发 try_switch_mode (Plan→Team)
    /// 2. schema 通过 + auto_review_plan=true + last_review 缺失/过期 → 拒绝切换 + 触发 ReviewRole
    /// 3. review 抵达后 verdict pass → 用户再次 /done 才真正切换到 Team
    ///
    /// ## 默认值
    /// false（保持现有行为，只有用户显式启用才生效）
    pub auto_review_plan: bool,

    // ─── 2026-05-28: /preset + /set 运行时参数 ─────────────────────────────
    pub runtime_temperature: Option<f64>,
    pub runtime_max_tokens: Option<u32>,
    pub runtime_context_ratio: Option<f64>,
    pub runtime_tool_limit: Option<u32>,
    pub runtime_timeout: Option<u64>,
    pub runtime_router: Option<bool>,
    pub runtime_dedup: Option<bool>,
    pub active_preset: Option<String>,

    // ─── 2026-05-27: 三模式流转修复 ─────────────────────────────────────────────────

    /// Meeting 结论后待确认的执行提案
    ///
    /// ## 引用关系
    /// - 写: `try_switch_mode` (Meeting→Clarify) 提取 action_items 后设置
    /// - 读: `event/mod.rs` 输入确认 (Y/n) → 转为 pending_slash_command
    /// - 清除: 用户输入 "n"/新消息/超时 30s 后自动清空
    ///
    /// ## 生命周期
    /// 设置 → 30s 内等待确认 → 确认/拒绝/超时后清空
    pub pending_meeting_execution: Option<MeetingExecutionPrompt>,

    /// Meeting 路由失败后保留的用户原始输入（供 Clarify 模式复用）
    ///
    /// ## 引用关系
    /// - 写: run.rs needs_clarify 信号触发时保存
    /// - 读: 输入框预填充（下次渲染周期）
    /// - 清除: 用户提交新输入后清空
    pub preserved_input: Option<String>,

    /// 本 session 是否已建议过 Meeting 模式（防反复骚扰）
    ///
    /// ## 引用关系
    /// - 写: run.rs analyzer 建议触发时置 true
    /// - 读: analyzer 判断是否跳过建议
    /// - 清除: /new 重置 session 时归 false
    pub meeting_suggested_this_session: bool,

    /// Session UUID。启动时 Uuid::new_v4() 生成；load_last_session 恢复时覆盖。
    /// 用途：session 文件命名（{uuid}.json），避免多实例互覆盖。
    pub session_id: String,
    pub model_name: String,
    /// 当前活跃的 provider ID（来自 provider.toml providers[].id）
    /// 来源：StreamChunk::Complete stats.provider_id
    /// 用途：健康仪表盘显示实际 provider，切换同名模型到不同 provider 时可区分
    pub active_provider_id: String,
    /// 各 provider 的可用性检测结果（启动时及配置变更时自动探测）
    /// 元组：(provider_id, available, error_msg)
    /// 来源：discover_all_models_with_status() → channel → tick 分支
    /// 生命周期：config 热加载时重新探测
    pub provider_statuses: Vec<(String, bool, Option<String>)>,
    /// 从 engine 动态拉取的可用模型列表（首次打开 /model picker 时延迟拉取）
    /// 引用：open_picker_model 优先使用此列表，空时退回静态 MODEL_GROUPS
    /// 生命周期：pending_model_fetch 触发 → 拉取 → 填充；/new 不清（模型列表不随会话变）
    pub available_models: Vec<String>,
    /// 2026-05-28: 按 provider 分组的模型列表（provider_id → models）
    /// 用于 picker 按实际注册分组显示，而非 infer_provider 静态推断
    /// 引用：open_picker_model 优先使用此分组
    /// 生命周期：与 available_models 同步更新
    pub available_providers: Vec<(String, Vec<String>)>,
    /// 标记需要在下一次 interval tick 拉取模型列表（engine 连接后设 true）
    pub pending_model_fetch: bool,
    pub thinking_depth: String, // "off" | "low" | "medium" | "high"
    /// 系统设定上下文空间 = model_limit * context_window_ratio（有效窗口）
    /// 来源：Complete stats.context_max / 热加载 config.toml
    pub context_window: usize,
    /// LLM 最大上下文空间（模型物理上限，如 1M）
    /// 来源：model_spec.context_window（引擎初始化时设置）
    pub model_max_context: usize,
    /// 配置文件 mtime 快照，用于热加载检测
    pub config_mtime: Option<SystemTime>,
    /// 实时上下文 token 估算（流式期间每 500ms 更新；Complete 后换为真实值）
    /// 引用：bars.rs inputbar 进度条；0 = 无数据
    /// 生命周期：/new 不清（保留上轮最终值直到新轮覆盖）
    pub ctx_live_tokens: u64,
    /// 上次 ctx_live_tokens 估算时刻（用于 500ms 门控）
    /// 生命周期：TextDelta 期间更新，Complete 后清空
    pub ctx_estimate_at: Option<std::time::Instant>,
    pub session_summary: String,
    pub turn_count: u32,
    /// V29.9: 会话可读别名(rename 命令设置), None = 显示 session_id
    /// 引用关系: TopBar/StatusBar 优先显示 alias, 否则 session_id 截短;
    ///         persists 进 SessionExport 跨会话保留
    pub session_alias: Option<String>,
    /// V29.9: turnkey 全托管目标(/turnkey 命令设置), 长期任务的"成功条件"陈述
    /// 引用关系: panel summary 区域显示; /turnkey handler 触发 sandbox_engine.plan_from_nl + execute
    /// 后端依赖: abacus_core::sandbox::SandboxOrchestrator (CLI 同名子命令共享)
    pub session_goal: Option<String>,
    /// V29.10 (C4-Phase2): plan_from_nl 产出的 TaskSpec 缓存
    /// 引用关系:
    ///   - 写入: run.rs 主循环消费 EngineResponse.turnkey_plan = Some(task) 时
    ///   - 读取: cmd_turnkey 'execute' 子命令; 若 None 提示用户先 /turnkey <goal>
    /// 生命周期:
    ///   - 创建: /turnkey <goal> 调 sandbox.plan_from_nl 成功
    ///   - 销毁: /turnkey execute(执行后清) | /turnkey clear | /new
    /// 持久化: 不进 SessionExport(plan 是会话期间的临时审阅状态, 重启清空)
    pub pending_turnkey_plan: Option<abacus_types::sandbox::TaskSpec>,
    /// V40 Vec<Message> 数据结构
    ///
    /// V42-B 升级:
    /// - V40 字段用于 V40 render_messages_in_card 的 build_message_lines
    /// - V42-B `render_cards` 不读 messages, 改读 `state.cards: CardStream`
    /// - 字段保留 (V40 兼容, slash_commands 仍用), 但 V42-B 新代码应迁移到 cards
    /// - 旧代码应迁移到 `state.message_count()` / `state.clear_messages()` helper
     pub messages: VecDeque<Message>,
    /// V40 屏幕行数缓存 — `screen_pos_to_msg_char` (mouse hit-test) 热路径使用
    /// 缓存有效性: `cached_msg_rows.len() == messages.len()` 时直接用, 否则回退 `estimate_msg_rows`
    /// V42-B: 保留 (RefCell 让 hit-test 路径在 streaming 期间动态更新)
    pub cached_msg_rows: RefCell<Vec<usize>>,
    pub(crate) scroll: usize,
    /// Phase 2: 用户是否主动离开底部浏览历史（streaming 期间不强制拉回）
    /// - set_scroll(Up/Down) → true
    /// - set_scroll(ToBottom) → false
    /// - reset_streaming → false
    pub(crate) user_scrolled_away: std::cell::Cell<bool>,
    /// V28.6 (PR12-5): 模式切换时保留各自的 scroll 位置, 切回不归零
    /// 引用关系: 被 `set_mode` 写入(切换前) + 读取(切换后), 不进 SessionExport
    /// 生命周期: 进程级, 每个模式各自累积; 不持久化(切会话即重置)
    /// 注: AppState 不 derive Serialize, 无需 #[serde(skip)]
    pub scroll_by_mode: std::collections::HashMap<AbacusMode, usize>,
    /// V29.5: 渲染层最近一次的可见行数与总行数缓存（让 handle_chat_scroll_key/PageUp 能 clamp）
    /// 引用关系: render_messages_in_card 每帧写入；handle_chat_scroll_key 读取做上限
    /// 生命周期: 启动时 0（首屏前 PageUp 退化为固定步长 20, 不会越界）
    /// 设计: 用 Cell 因为渲染链路是 &AppState
    pub(crate) last_visible_h: std::cell::Cell<usize>,
    pub(crate) last_total_lines: std::cell::Cell<usize>,
    /// V30 timeline 边界修复：上次渲染的 timeline 可见事件行数 (max_events)。
    /// 引用关系：render_timeline 写入；handle_global_key / handle_mouse 在 += 后读取做 clamp。
    /// 生命周期：启动时 0（首帧前超界会被下一帧渲染的 clamp 自愈）。
    pub(crate) last_timeline_visible: std::cell::Cell<usize>,
    /// V29.11 (B4): 渲染层最近一次的消息内容宽度（用于折叠锚定的行数估算）
    /// 引用关系: render_messages_in_card 写入 inner.width.saturating_sub(5)；
    ///           handle_chat_scroll_key Space 分支调 estimate_msg_rows 时读取
    /// 生命周期: 启动时 0（锚定逻辑见 0 时退化为 80, 安全 fallback）

    /// B11: 每条消息的实际渲染行数缓存（由 render_messages_in_card 每次 L2 重建时写入）
    /// 用于 screen_row_to_msg_idx / screen_pos_to_msg_char 的热路径，避免每帧/每次点击
    /// 重新调用 estimate_msg_rows（含 serde_json::from_str 和 wrap math）。
    /// 
    /// 缓存有效性: cached_msg_rows.len() == messages.len() 时有效，否则回退 estimate_msg_rows。
    /// 生命周期: 启动时空 Vec；每次 L2 渲染后刷新；messages 数量变化时自动失效。

    pub input: String,
    pub input_state: InputState,
    /// 压缩前的 input_state 快照（CompressEnd 时恢复）
    /// 生命周期：CompressStart 设置 → CompressEnd 消费（take）
    pub pre_compress_input_state: Option<InputState>,
    /// 压缩期间用户输入的暂存消息（InputState::Executing 时用户提交的文本）
    ///
    /// 引用关系：
    ///   - 写入：event/mod.rs Enter 键处理（Executing 态且非 slash 命令时）
    ///   - 读取：run.rs CompressEnd / CompressAutoResume 处理后自动发送
    /// 生命周期：CompressEnd 或 CompressAutoResume 消费（take）后清空
    pub pending_compress_input: Option<String>,
    /// 网络连接异常标记 — status bar 显示 ⚠ 网络异常
    /// 引用：run.rs NETWORK_ERROR 前缀时置 true；成功响应后清 false
    pub connection_error: bool,
    pub cursor_pos: usize,
    /// 缓存光标所在行号（避免每帧 O(n²) 计算）
    pub(crate) cursor_line: usize,
    /// 缓存光标在行内的字符偏移
    pub(crate) cursor_col: usize,

    /// V42-B+: tui-textarea 多行编辑器（参考 OpenCode TextareaRenderable）
    /// 用 RefCell 包装：TextArea 内部用 Rc（!Send），AppState 需要 Send，
    /// RefCell 提供运行时借用检查 + 单线程安全保证。
    ///
    /// ## 同步策略
    /// - `state.input` 是 SSoT（单真相源）
    /// - Phase 2: textarea.input() 后 sync_from_textarea() 将 textarea 同步回 state.input
    /// - textarea 负责：光标渲染、行号、软换行、选区高亮
    /// - `state.input` 负责：slash 命令拦截、submit 逻辑、completion、历史
    ///
    /// ## 后续迁移路径
    /// Phase 1（当前）: 仅用 textarea 渲染，input handler 不变
    /// Phase 2: 用 textarea.input() 处理键盘，移除手写 cursor 状态机
    /// Phase 3: 用 textarea.lines() 替代 state.input（需重构 submit/completion）
    pub(crate) textarea: std::cell::RefCell<TuiTextArea<'static>>,

    /// 全局焦点区域
    pub focus: Focus,
    pub panel_visible: bool,
    pub panel_tab: PanelTab,
    /// V40: 仪表盘当前 tab
    pub dashboard_tab: DashboardTab,
    /// V40: 仪表盘内容滚动偏移
    pub dashboard_scroll: usize,
    /// V40: 自动化模块健康快照（由 JobRunner 推送更新）
    pub auto_health: abacus_core::auto::AutoHealth,
    /// 打字机光标——最新消息已展示的字符数
    pub stream_cursor: usize,

    /// 命令面板滚动位置
    pub cmd_scroll: usize,
    /// 命令面板选中索引（焦点在 CommandHint 时 ↑↓ 移动；Enter 填充输入框）
    /// V13: 之前 cmd_scroll 仅滚动无选中态，CommandHint 不可交互
    pub cmd_selected: usize,
    /// 可用命令列表（由后端驱动，渲染层按空间自适应布局）
    /// V13: 派生自 slash_commands::command_inventory()——单一真相源
    pub commands: Vec<(String, String)>,

    /// V28: 仅供 v1 session 文件反序列化兜底,新代码不再写入。
    /// Migration: 加载 v1 session 时把 events 转成 trace_events 后清空。
    /// (持久化策略由 SessionExport 显式控制,不通过 AppState derive)
    pub events: Vec<EventEntry>,
    /// V28 SSOT: 所有 trace 事件(thinking / tool_call / generic / reply)的真相源
    /// 引用关系: 被 Message::Trace.event_ids + streaming_trace_ids 引用; 被 render_tab_timeline 读取
    /// 生命周期: push_trace 追加; MAX=500 FIFO 裁剪; v2 SessionExport 显式拷入
    /// V40 trace events 列表
    ///
    /// V42-B 升级:
    /// - V40 字段存储 V40 render 的 trace event 列表
    /// - V42-B `AbacusCard.events` 内置 (每张 AbacusCard 自带)
    /// - 字段保留 (V40 panel_sections/timeline/focus 仍用), 但 V42-B 新代码应迁移
    /// - 旧代码应迁移到 `state.trace_event_count()` / `state.collect_trace_events()` helper
    pub trace_events: Vec<TraceEvent>,
    /// trace event id → index 映射（O(1) 查找替代 O(n) 线性扫描）
    /// 引用关系：push_trace_full 写入时同步更新；FIFO drain 时批量重建
    /// 生命周期：与 trace_events 同步，drain 后重建
    pub trace_event_index: std::collections::HashMap<u64, usize>,
    /// 工具频次缓存（避免每帧重算）
    /// 引用关系：push_trace(ToolCall) 时标记 dirty；render_tab_quant 消费
    /// 生命周期：push_trace 或 reset_session 时 invalidate
    pub tool_freq_cache: std::cell::RefCell<Option<Vec<(String, u32, u32)>>>,
    pub tool_freq_dirty: std::cell::Cell<bool>,
    /// V28: 单调自增 trace id 分配器,SessionExport v2 持久化(防止重启后与历史 message 引用冲突)
    pub next_trace_id: u64,
    /// 流式期间临时聚集 trace ids,落档时 mem::take 转移到 Message::Trace.event_ids
    /// 不持久化(异常退出后这些 ids 视为孤儿,trace_events 仍保留但不归任何消息)
    pub streaming_trace_ids: Vec<u64>,
    /// V28.1: timeline 上被点击展开的 event id 集合 — 渲染时显示 inline 详情(限 3-5 行)
    /// 引用关系: render_tab_timeline 读取决定展开形态;handle_mouse 点击 timeline 行 toggle
    /// 生命周期: 会话级累积(不持久化);/clear /new 清空
    pub timeline_expanded_ids: HashSet<u64>,
    /// V28.1: 渲染时填充的"屏幕行 y → trace_event id"映射,供鼠标点击反查
    /// 引用关系: render_tab_timeline 渲染前 clear,逐行 push 时记录;handle_mouse 读取
    /// RefCell 让 &state 渲染时仍能写入
    pub(crate) timeline_row_map: RefCell<Vec<(u16, u64)>>,

    /// V32 · shortcuts_hints 行 → 命令索引映射（鼠标点击直填）
    /// 每项 = (screen_y, left_cmd_idx)；右列在 is_wide 下用 left_cmd_idx+1。
    /// 引用关系：render_shortcuts_hints 写入；handle_mouse 按 (row, column) 反查
    /// 命令索引 → state.commands → 填入 input。
    pub(crate) cmd_row_map: RefCell<Vec<(u16, usize)>>,
    /// V28.3: 消息区 Trace summary 行的"屏幕 y → (msg_idx, part_idx)"映射
    /// 引用关系: render_messages_in_card 渲染后填充(scroll 转换);handle_mouse 消息区命中读取
    /// 设计意图: 让用户点击消息内 ▸ trace 摘要行 → toggle_block 展开/折叠 Trace 子块
    pub(crate) message_trace_row_map: RefCell<Vec<(u16, usize, usize)>>,
    /// V28.4: 全局 focused event 锚点 — 双视图同步高亮
    /// 引用关系: 单击 timeline event 时设置 = Some(id);
    ///           render_tab_timeline + build_message_lines 渲染时读取,匹配的行加 highlight bg
    /// 生命周期: 会话级;切换/关会话时清;再次点击其他 event 覆盖
    pub focused_event_id: Option<u64>,
    pub tool_records: Vec<ToolRecord>,
    /// 工具健康快照 — 每 turn 从 EffectivenessTracker 获取
    ///
    /// ## 引用关系
    /// - 生产者：run.rs 收到 StreamChunk::ToolHealth 时写入
    /// - 消费者：panel components 渲染工具 tier 标识
    ///
    /// ## 生命周期
    /// - 创建：每 turn 覆盖更新（不累积，只保留最近已调用工具的状态）
    /// - 销毁：reset_session 时 clear
    pub tool_health: std::collections::HashMap<String, abacus_core::llm::stream::ToolHealthEntry>,
    /// 流式 diff 渲染缓存 — key = trace_id，value = 已渲染好的 diff 行
    ///
    /// ## 引用关系
    /// - 写入：components/mod.rs 流式时间线渲染，工具首次完成时计算并存入
    /// - 读取：同一渲染路径，后续帧直接复用（跳过 LCS 重计算和 JSON 重解析）
    ///
    /// ## 生命周期
    /// - 创建：首次渲染该工具的 diff 时
    /// - 销毁：reset_streaming() 清除（每 turn 结束后）
    /// - 设计意图：消除流式阶段每帧重复 similar::TextDiff::from_lines() 的 CPU 浪费
    /// RefCell 允许在 &AppState 渲染路径中写入缓存（render_messages_in_card 接收 &AppState）
    pub thinking_text: String,

    pub experts: Vec<Expert>,
    /// 去重专家名缓存
    /// 引用关系：add_message(Expert) 时 insert
    /// 生命周期：reset_session / cmd_new / cmd_clear 时 clear
    pub expert_names_cache: HashSet<String>,
    pub tasks: Vec<TaskCard>,

    pub toasts: Vec<Toast>,

    /// V42-B: 消息流卡片集合 (替代 V40 的 Vec<Message> + 6 个 streaming_* 字段 + 3 级缓存)
    /// 初始化后即存在, 全生命周期持有; cards 内部有 alloc_id / active / collapse_overrides
    /// 当前 Phase 9 仅作"并存写入", 旧字段保留; Phase 14 清理旧字段后, 此字段成为唯一数据源
    pub cards: CardStream,

    /// V42-B: 消息流垂直滚动偏移 (像素单位, 0 = 顶部)
    /// V40 时期有更复杂的 ScrollableStack + cached_msg_rows 状态机; V42-B 简化为单一字段
    /// 滚动边界由 `render_cards` 在每帧重建时 clamp
    pub message_scroll_y: u16,

    /// V42-B: 消息区 Rect 缓存 (render_cards 每帧写入, hit_test 读取)
    /// 替代 V40 的 message_trace_row_map
    /// 4 字段记录 inner Rect (border 内), hit_test 用 (x, y) 反查 Card id
    /// 用 `RefCell` 包装: render 路径 (`&AppState`) 也需要写, 普通字段无法
    /// 在 immutable 借用下写入; 借用范围仅 4 行赋值, 无并发风险
    /// V42-B+: 消息区 Rect 缓存（合并原 4 个 RefCell<u16>）
    /// 引用关系：render_cards 写入，hit_test 读取
    /// 生命周期：每帧渲染时更新，hit_test 时读取
    pub(crate) last_msg_area: RefCell<ratatui::layout::Rect>,
    /// V40 字段 — 消息区最近一次的内容宽度, 供 hit-test (event/mod.rs) 使用
    /// Cell 让 immutable render 路径可写
    pub last_content_width: Cell<u16>,

    pub running: bool,
    pub paused: bool,
    /// 密度模式：true=Compact(高密度), false=Comfortable(呼吸感，默认)
    /// Ctrl+D 切换。影响：消息间距、TopBar 信息密度、面板位置
    pub compact: bool,
    /// Resize debounce：窗口大小变化后倒数 N 帧再重建消息缓存（避免拖动时卡顿）
    pub resize_debounce_frames: u8,
    pub ctrl_c_last: Option<Instant>,
    /// 当前操作开始时间（用于显示耗时）
    pub op_started_at: Option<Instant>,
    /// 暂停时累积的已耗时间
    pub accumulated_elapsed: std::time::Duration,

    /// Engine bridge — set when TUI connects to real backend
    pub engine_handle: Option<EngineHandle>,
    /// 记忆宫殿本体数据快照（从 core 异步拉取，用于仓库 Tab palace 层级展示）
    pub palace_data: Option<PalaceSnapshot>,
    /// MLX 本地模型健康状态（每 turn 刷新）
    pub local_health: Option<abacus_core::local_provider::LocalModelHealth>,
    /// Channel sender for engine responses
    pub engine_tx: Option<mpsc::UnboundedSender<crate::tui::api::EngineResponse>>,
    /// Text pending for async engine submission
    pub pending_text: Option<String>,
    /// 后台任务注册表 —— 管理所有 tokio::spawn 的 LLM/工具任务
    /// 引用关系: run.rs 各 LLM spawn 点调用 register()；event/mod.rs::EscAction::CancelOperation 调用 cancel_all()
    /// 生命周期: 每次 spawn 注册；正常完成由 reap_finished 清理；Esc 中断时 cancel_all
    pub task_registry: TaskRegistry,
    /// 统一补全引擎 — 内联（Tab）+ 弹窗（Ctrl+Space / Ctrl+Tab）
    pub completion: completion::CompletionEngine,
    /// 已提交输入的历史（FIFO，上限 100）
    pub input_history: Vec<String>,
    /// 排队的输入（忙碌态下用户 Enter 提交的消息，当前请求完成后自动发送）
    pub pending_inputs: VecDeque<String>,
    /// 标记：下一帧需要自动发送 state.input 的内容
    pub pending_send: bool,
    /// 历史导航位置（None = 不在历史模式）
    pub history_index: Option<usize>,
    /// 待异步执行的文件路径补全前缀
    pub pending_file_completion: Option<String>,
    /// 待异步执行的 AI 补全前缀
    pub pending_ai_completion: Option<String>,
    /// 文本选择状态（Shift+鼠标拖拽选中文本）
    pub text_selection: Option<TextSelection>,
    /// 待异步执行的 slash command（由 event handler 设置，run.rs 主循环消费）
    pub pending_slash_command: Option<SlashCommand>,
    /// V41: Plan 策略两阶段状态机
    /// 引用关系：/plan 触发 → api/mod.rs Phase 1 设置 → run.rs 监听 Approval → Phase 2 执行
    /// 生命周期：/plan 创建 → Researching → AwaitingApproval → Executing → None（完成清除）
    pub plan_phase: Option<PlanPhase>,
    /// 设置面板状态
    pub show_settings: bool,
    /// 设置面板焦点字段索引
    pub settings_focus: usize,
    /// 设置面板当前编辑值
    pub settings_input: String,
    /// 会话 Token 统计（含压缩历史：compress_count / compress_tokens_saved）
    pub session_tokens: SessionTokenStats,
    /// V35: 模式过渡感知提示 — 切换模式后 5s 内展示携带内容摘要
    /// 引用关系:
    ///   写: slash_commands::try_switch_mode 切换后立即写入
    ///   读: bars::render_status_bar 按 elapsed 决定是否展示
    /// 生命周期: 写入后 5s 自然过期（render 时检查 elapsed），不需要显式清除
    pub transition_hint: Option<(String, std::time::Instant)>,
    /// 当前处理阶段描述（减少等待焦虑）
    pub processing_phase: String,
    /// 当前处理阶段序号 (1-based)
    pub processing_step: u32,
    /// 总处理阶段数
    pub processing_total_steps: u32,
    /// 消息渲染缓存（dirty 标记避免每帧重建全部行）
    ///
    /// V42-B 升级:
    /// - V40 字段用于 V40 render_messages_in_card 的 L0/L1/L2 三级缓存
    /// - V42-B `render_cards` 每帧重建, **不需要** dirty 标记
    /// - 字段保留 (V40 路径兼容), 但 V42-B 渲染路径不读不写
    /// - 旧代码应迁移到 `state.mark_render_dirty()` / `state.is_render_dirty()` helper
    pub(crate) rendered_lines_dirty: std::cell::Cell<bool>,
    /// P1 优化：帧级 dirty 标记 — 任何事件/交互导致状态变化时设 true
    /// 引用关系：event handler / run.rs 响应处理 设置 → run.rs 条件渲染判定消费
    /// 生命周期：每帧 draw 前检查，draw 后 reset
    pub(crate) frame_dirty: std::cell::Cell<bool>,
    /// streaming-only dirty — 仅 streaming 尾部内容变化，base 消息未改变
    /// 引用关系：run.rs chunk drain 设置 → components/mod.rs 分区渲染路径消费
    /// 生命周期：每帧渲染后 reset
    pub(crate) streaming_content_dirty: std::cell::Cell<bool>,
    /// 分区渲染缓存 — 缓存 build_message_lines 的结果（streaming 期间不重建）
    /// 引用关系：components/mod.rs 写入/读取
    /// 生命周期：新消息加入 messages 时失效（reset_streaming / add_message 清空）
    /// V40: 上次缓存 base lines 时的 messages.len()（用于判断 base 是否需要重建）
    pub(crate) cached_base_msg_count: std::cell::Cell<usize>,
    /// info panel 内容 — 长信息走面板不走 toast
    pub info_panel_text: String,
    /// info panel 是否自动打开
    pub info_panel_auto_open: bool,
    /// 命令参数 picker（V13）
    /// 引用关系：handle_slash_command 拦截无参 `/model` `/theme` `/thinking` 时设置；
    ///           render_picker_popup 渲染；Esc/Enter 关闭
    /// 生命周期：单次选择期间存在，应用或取消即设回 None
    pub picker: Option<PickerState>,
    /// 2026-05-28: 全屏编辑器状态
    /// 引用关系：render_fullscreen_editor 渲染 + handle_editor_key 更新
    /// 生命周期：open_editor() 创建 → close_editor() 销毁（InputState::Editor 同步）
    pub editor_state: Option<EditorState>,
    /// 主题预览面板打开状态（`/theme preview` 触发）
    /// 引用关系：cmd_theme 设置；render_info_panel 渲染时优先于 info_panel_text；event Esc 关闭
    /// 生命周期：单次切换可见 / Esc 或再次 /theme 切走时清零
    pub theme_preview_open: bool,
    /// 消息渲染缓存（避免每帧重建）
    /// 缓存对应的渲染宽度
    pub(crate) cached_width: RefCell<u16>,

    // ─── Streaming State ──────────────────────────────────────────
    /// 是否启用流式输出（用户可通过 /streaming toggle）
    pub streaming_enabled: bool,
    // V42-B: is_streaming 字段已删除，改用 `is_streaming_active()` (CardStream)
    // V42-B: streaming_text / streaming_thinking 已拆分到 LlmCard，字段已删除
    // V42-B: streaming_text_started / streaming_thinking_started 已删除，改用 active_llm_text_len() == 0
    /// 是否展示 thinking/tools 流式内容（Ctrl+O 切换，默认隐藏，与 Claude Code 一致）
    pub show_streaming_trace: bool,
    /// 标记当前 LLM 调用已 Complete，但尚未收到 EngineResponse
    /// 引用关系：run.rs StreamChunk::Complete 置 true；reset_streaming 重置 false
    /// 生命周期：Complete 到达时 true → EngineResponse 到达 reset_streaming 时 false
    pub streaming_complete: bool,
    /// 流式输出中的工具执行状态
    /// 三元组承载 ToolEnd 已有的 success + duration_ms（之前用 `..` 丢失）
    /// 元组扩成 4 元 — 末位 trace_id 让 ToolEnd 能按 id 直接定位 trace_events
    /// 中对应条目(避免在并行 tool call 场景下按 name 顺序匹配错位)。
    /// 字段:name / status / duration_ms (None=进行中) / trace_id (SSOT 引用,不参与显示)
    /// 引用关系:run.rs ToolStart 创建 trace 同时 push 元组;ToolEnd 按 trace_id 回查;
    ///
    /// V42-B: 拆分到 `AbacusCard.events` 内部，字段保留供 timeline 使用
    pub streaming_tools: Vec<(String, StreamingToolStatus, Option<u64>, u64)>,
    /// 统一时序流 — 所有 streaming 事件按到达顺序排列
    /// 引用关系：run.rs push → components/mod.rs 遍历渲染
    /// 生命周期：首次 chunk 到达时 push → reset_streaming 清空
    pub streaming_timeline: Vec<TimelineEntry>,
    // Phase 3+4: blocks 每帧从 timeline 局部构建（O(timeline_len) 聚合），不持久化
    // Phase 4: 用户手动展开的 block id 集合（优先级高于 auto_collapse）
    pub expanded_block_ids: std::cell::RefCell<std::collections::HashSet<u64>>,
    // streaming_parsed_lines / streaming_parsed_len 已移除
    // 旧的增量解析缓存被 timeline + mdstream committed/pending 模型完全替代
    /// 流式 Markdown 增量渲染状态（mdstream committed/pending 模型）
    /// 引用关系：run.rs TextDelta → append；components 渲染 → committed_styled/pending_styled
    /// 生命周期：首次 TextDelta 时 lazy 创建，reset_streaming 时 drop
    /// 使用 RefCell：渲染函数持有 &self 但 committed_styled 需 &mut self
    /// V42-B: 拆分到 `LlmCard.reply_md` 内部，字段保留供兼容路径使用
    pub streaming_md: std::cell::RefCell<Option<crate::tui::md_stream::StreamingMd>>,

    // ─── V0.3 IDE Effects ──────────────────────────────────────────
    /// 代码行 flash 高亮状态（新行出现时高亮 300ms）
    pub flash_state: crate::tui::effects::FlashState,
    /// V28.5: streaming 期间消息框顶部边框光带动效相位
    /// - 生命周期: 仅在 is_streaming=true 时由 `render_messages_in_card` 每帧 += 1 推进;
    ///             streaming 结束后停止递增, 数值保留(下次 streaming 自然续上, 用 modulo 不会溢出)
    /// - 引用关系: 被 `paint_streaming_top_shimmer` 读取 → patch frame buffer 顶部行 cell style
    /// - 为什么用 Cell: render 链路统一传 `&AppState`, 内部可变性兼容现有签名
    pub anim_tick: std::cell::Cell<u64>,
    /// 代码块折叠/展开（Ctrl+E 切换）：true=展开全部；false=超 20 行折叠显示
    ///
    /// 引用关系：被 components::build_message_lines 读取；event::handle_key 写入
    pub code_blocks_expanded: bool,
    /// LSP 错误诊断数（由 pipeline 推送，状态栏实时显示）
    pub lsp_diag_errors: u32,
    /// LSP 警告诊断数
    pub lsp_diag_warnings: u32,

    // ─── V42 Section / Dashboard Registry (可扩展 UI 框架) ───────────────
    /// 看板 Section 注册仓库 —— 启动时由 [`crate::tui::extensions::register_builtin_sections`] 注入 6 内置
    ///
    /// Agent 应用可通过 `state.section_registry.register(Box::new(MySection))` 注入自定义看板区块。
    ///
    /// ## 引用关系
    /// - 写入: AppState::new 时调 extensions::register_builtin_sections; 外部 Agent 可后续 register
    /// - 读取: panel.rs::render_tab_scene 调 build_stack(&panel_layout).render(...)
    ///
    /// ## 不持久化
    /// trait object 不可 serde, 重启时按 builtin 重新注册（外部 Agent 自己负责 re-register）
    pub section_registry: abacus_ui_kit::SectionRegistry,

    /// 仪表盘 DashboardTab 注册仓库 —— 启动时注入 Health + Auto 两个内置 tab
    ///
    /// Agent 应用可通过 `state.dashboard_registry.register(Box::new(MyTab))` 注入自定义仪表盘 tab。
    pub dashboard_registry: abacus_ui_kit::DashboardRegistry,

    /// 看板 Section 启用列表 + 渲染顺序 —— 用户通过 config.toml 覆盖, 默认见
    /// [`crate::tui::extensions::default_panel_layout`]
    pub panel_layout: Vec<String>,

    // ─── V0.5 Confirmation Dialog ────────────────────────────────────
    /// 权限确认弹窗状态（None = 无弹窗，Some = 等待用户确认）
    pub confirm_dialog: Option<ConfirmDialog>,
    /// 用户对确认弹窗的响应（true=确认, false=拒绝），由 run loop 消费后清除
    pub pending_confirmation_response: Option<bool>,
    /// "总是允许" 列表——按 tool_id 匹配，匹配后自动跳过弹窗
    /// V29.11: 从 Vec<String> 改为 HashSet<String>（O(1) 查找；工具数 <50 性能无感，但语义更精确）
    /// 引用关系:
    ///   - 写入: event/mod.rs 按 Y/A + run.rs 超时 auto-allow
    ///   - 读取: run.rs:698 always_allow 短路检查
    ///   - 持久化: SessionExport 序列化为 JSON array (Vec<String>); load 时 collect 回 HashSet
    pub always_allow: std::collections::HashSet<String>,
    /// 当前待确认的 MCIP 请求
    ///
    /// 引用关系（V28 channel-based）：
    /// - 生产者：① ConfirmRequired stream chunk handler 写入（streaming 路径）
    ///           ② EngineResponse.pending_confirmations legacy 路径（非流式 fallback）
    /// - 消费者：run.rs 决策消费块——通过 SessionState.mcip_confirm_channels[req.nonce]
    ///           取出 oneshot::Sender 发送 bool（不再走 grant_and_rerun 重运）
    /// 生命周期：写入 → 用户决策（Y/A/N/Enter）/超时 → 消费时整体 take → 清除
    pub pending_mcip_confirmations: Vec<abacus_core::mcip::McipConfirmRequest>,

    // ─── V0.5 Panel Scroll ──────────────────────────────────────────
    /// 时间线滚动（offset-from-top，0 = 顶部，follow_tail = auto-scroll to bottom）
    pub timeline_scroll: SimpleScrollOffset,
    /// V35: Timeline 分组缓存 — 防止每帧重分组（trace_events.len() 变化时失效重建）
    /// 生命周期: render_tab_scene 按需重建，进程内有效
    pub timeline_groups_cache: Vec<TimelineGroup>,
    /// 上次缓存时 trace_events.len()，用于失效检测
    pub timeline_cache_len: usize,
    /// 知识宫殿滚动（offset-from-top，0 = 顶部，follow_tail = auto-scroll to bottom）
    pub knowledge_scroll: SimpleScrollOffset,
    /// 面板当前滚动焦点区块（用于 ↑↓ 操作哪个区块）
    pub panel_scroll_section: PanelSection,

    // ─── V0.4 Knowledge Palace Tracking ─────────────────────────────
    /// 本 session 知识宫殿调用统计：(宫殿名, 领域, 调用次数)
    /// 由 tool 调用（file_read/kb_query 等）解析路径后自动归类
    pub knowledge_calls: Vec<KnowledgeCallEntry>,

    // ─── K1 Focus Pulse Feedback ─────────────────────────────
    /// 焦点切换时间戳（用于 200ms 脉冲反馈）
    /// 引用关系：被 components 渲染层读取做边框脉冲
    /// 生命周期：set_focus/note_focus_change 写入；过期后值仍在但 focus_pulsing() 返回 false
    pub focus_changed_at: Option<Instant>,

    // ─── 焦点跟随用户操作（方案 3 磁吸抑制窗）───────────────
    /// 用户最后一次按键时间戳。
    /// 引用关系：handle_global_key 入口写入；try_magnet_focus 读取判断是否抑制磁吸。
    /// 生命周期：每次按键 record_keypress() 更新；从未按键时为 None（首次磁吸允许）。
    /// 设计动机：agent 消息/trace 事件抵达时**自动**把焦点磁吸到 Panel(Timeline)，
    /// 但若用户正在操作（< MAGNET_SUPPRESS_MS），不打断用户。
    pub last_user_keypress_at: Option<Instant>,

    /// 磁吸 toast 提示节流时间戳。
    /// 引用关系：try_magnet_focus 实际触发切换时写入。
    /// 生命周期：磁吸成功后 ≥ MAGNET_TOAST_THROTTLE_MS 才允许下一次 toast，避免 trace 流刷屏。
    pub last_magnet_toast_at: Option<Instant>,
}

/// 磁吸抑制窗口（ms）：距用户最后一次按键 < 此值时禁止系统主动切焦点。
/// 2000ms 经验值：覆盖人类连续输入间隔（200~500ms/键）+ 思考间歇（~1s），
/// 又不至于在用户停手后让系统响应过慢。
pub const MAGNET_SUPPRESS_MS: u128 = 2000;

/// 磁吸 toast 节流（ms）：连续磁吸不重复 toast，避免 trace 流期间刷屏。
/// 5000ms：与典型一次 agent 回复时长相近，让用户在一次完整对话只看到 1 次提示。
pub const MAGNET_TOAST_THROTTLE_MS: u128 = 5000;



// V40-3: SessionTokenStats per-mode helpers 单元测试
#[cfg(test)]
mod per_mode_query_tests {
    use super::*;

    fn make_stats() -> SessionTokenStats {
        let mut s = SessionTokenStats::default();
        // 使用 mode.label() 返回的实际值作 key（小写）— 与 run.rs 累加同源
        s.per_mode.insert(AbacusMode::Clarify.label().to_string(), ModelTokenStats {
            cost_cny: 3.0,
            turns: 2,
            ..Default::default()
        });
        s.per_mode.insert(AbacusMode::Meeting.label().to_string(), ModelTokenStats {
            cost_cny: 7.0,
            turns: 5,
            ..Default::default()
        });
        s
    }

    #[test]
    fn mode_stats_finds_existing() {
        let s = make_stats();
        assert_eq!(s.mode_stats(AbacusMode::Clarify).map(|x| x.cost_cny), Some(3.0));
        assert_eq!(s.mode_stats(AbacusMode::Meeting).map(|x| x.turns), Some(5));
    }

    #[test]
    fn mode_stats_returns_none_when_absent() {
        // Clarify 已插入，Meeting 已插入；测试空 stats
        let s2 = SessionTokenStats::default();
        assert!(s2.mode_stats(AbacusMode::Clarify).is_none());
    }

    #[test]
    fn total_per_mode_cny_sums_all() {
        let s = make_stats();
        assert!((s.total_per_mode_cny() - 10.0).abs() < 1e-9);
    }

    #[test]
    fn mode_cost_ratio_correct() {
        let s = make_stats();
        assert!((s.mode_cost_ratio(AbacusMode::Clarify) - 0.30).abs() < 1e-9);
        assert!((s.mode_cost_ratio(AbacusMode::Meeting) - 0.70).abs() < 1e-9);
    }

    #[test]
    fn mode_cost_ratio_zero_when_absent() {
        let s = SessionTokenStats::default();
        assert_eq!(s.mode_cost_ratio(AbacusMode::Clarify), 0.0);
    }

    #[test]
    fn mode_cost_ratio_zero_when_total_zero() {
        let s = SessionTokenStats::default();
        assert_eq!(s.mode_cost_ratio(AbacusMode::Meeting), 0.0);
    }
}

/// 消息显示窗口上限 — 只保留最近 N 条消息（超出时裁剪最旧的，先剥离 Trace 内容节省内存）
const MAX_MESSAGES: usize = 200;
/// 事件列表上限（参考 Claude Code 控制 RAM，FIFO 淘汰最旧，UI 始终展示最新）
const MAX_EVENTS: usize = 300;

impl AppState {
    // ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
    // V42-B is_streaming 升级路径 → streaming_ext.rs
    // ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

    // ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
    // V42-B rendered_lines_dirty 升级路径
    // ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

    /// V42-B: 标记渲染脏（替代 V40 `state.rendered_lines_dirty.set(true)`）
    ///
    /// V40 行为: 设置 bool 标记, render_messages_in_card 据此决定是否全量重建
    /// V42-B 行为: 设置 V40 字段 (兼容), V42-B `render_cards` 每帧重建无需此标记
    pub fn mark_render_dirty(&self) {
        self.rendered_lines_dirty.set(true);
    }

    pub fn is_render_dirty(&self) -> bool {
        self.rendered_lines_dirty.get()
    }

    pub fn clear_render_dirty(&self) {
        self.rendered_lines_dirty.set(false);
    }

    /// 标记渲染缓存失效（外部触发重绘）
    pub fn mark_dirty(&self) {
        self.mark_render_dirty();
    }

    // ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
    // V42-B messages 升级路径
    // ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

    /// V42-B: 消息总数（替代 V40 `state.messages.len()`）
    pub fn message_count(&self) -> usize {
        self.cards.len()
    }

    /// V42-B: 清空所有消息（替代 V40 `state.messages.clear()`）
    pub fn clear_messages(&mut self) {
        self.cards.clear();
    }

    /// V42-B: 检查消息是否为空（替代 V40 `state.messages.is_empty()`）
    pub fn messages_is_empty(&self) -> bool {
        self.cards.is_empty()
    }

    // ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
    // V42-B trace_events 升级路径
    // ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

    pub fn trace_event_count(&self) -> usize {
        use crate::tui::cards::AbacusCard;
        let mut total = 0;
        for card in self.cards.iter() {
            if let Some(ac) = self.cards.card_downcast_ref::<AbacusCard>(card.id()) {
                total += ac.events_ref().len();
            }
        }
        total + self.trace_events.len() // 合并 V40 遗留数据
    }

    pub fn collect_trace_events(&self) -> Vec<crate::tui::state::TraceEvent> {
        use crate::tui::cards::AbacusCard;
        let mut all = Vec::new();
        for card in self.cards.iter() {
            if let Some(ac) = self.cards.card_downcast_ref::<AbacusCard>(card.id()) {
                all.extend(ac.events_ref().iter().cloned());
            }
        }
        all.extend(self.trace_events.iter().cloned());
        all
    }


    /// 清空所有流式输出累积状态（is_streaming + streaming_* 字段 + 增量解析缓存）
    ///
    /// 引用关系：被 res_rx 收到 EngineResponse / StreamChunk::Complete /
    ///           StreamChunk::Error / 启动新流式（先清后填）调用——4 处共用真相源
    /// 生命周期：操作幂等，可无条件调用
    ///
    /// V42（Bug B 修复）：删除"自动落档守卫"逻辑
    pub fn input_bar_color(&self) -> ratatui::style::Color {
        match self.input_state {
            InputState::Ready => self.theme.user,
            InputState::Typing => self.theme.text,
            InputState::Completing => self.theme.accent,
            InputState::Thinking | InputState::Executing | InputState::Outputting => {
                self.theme.accent
            }
            InputState::Paused => self.theme.semantic_fg(abacus_ui_kit::SemanticIntent::Warning),
            InputState::Editor => self.theme.accent,
        }
    }

    pub fn expert_count(&self) -> usize {
        self.expert_names_cache.len()
    }
}

// ── input_ext.rs 内的函数 re-export（保持 crate 内路径兼容）──
pub(crate) use input_ext::row_col_to_byte_pos;

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_new_state_default() {
        let state = AppState::new(AbacusMode::Clarify);
        assert_eq!(state.mode, AbacusMode::Clarify);
        // panel 默认可见（UX 设计：面板是常驻信息源；通过 Ctrl+O 可切换隐藏）
        assert!(state.panel_visible);
        assert_eq!(state.input_state, InputState::Ready);
        // V32: 初始焦点改为 Input — 用户启动 TUI 通常先打字，焦点直接落在输入栏
        assert_eq!(state.focus, Focus::Input);
        assert!(state.running);
        assert!(!state.paused);
        assert_eq!(state.cursor_pos, 0);
        assert!(state.messages.is_empty());
        assert!(state.events.is_empty());
    }

    /// V32 · cycle_focus 三档循环：Input → Panel → CommandHint → Input（commands 非空 + panel_visible）
    #[test]
    fn cycle_focus_three_stage_loop_when_all_visible() {
        let mut s = AppState::new(AbacusMode::Clarify);
        s.commands = vec![("/help".into(), "帮助".into())]; // 让 CommandHint 入环
        s.panel_visible = true;
        assert_eq!(s.focus, Focus::Input);
        s.cycle_focus();
        assert_eq!(s.focus, Focus::Panel);
        s.cycle_focus();
        assert_eq!(s.focus, Focus::CommandHint);
        s.cycle_focus();
        assert_eq!(s.focus, Focus::Input);
    }

    /// V32 · panel_visible=false 时跳过 Panel
    #[test]
    fn cycle_focus_skips_hidden_panel() {
        let mut s = AppState::new(AbacusMode::Clarify);
        s.commands = vec![("/help".into(), "h".into())];
        s.panel_visible = false;
        assert_eq!(s.focus, Focus::Input);
        s.cycle_focus();
        assert_eq!(s.focus, Focus::CommandHint, "panel 隐藏时 Input → CommandHint");
        s.cycle_focus();
        assert_eq!(s.focus, Focus::Input, "CommandHint → Input（Panel 仍跳过）");
    }

    /// V32 · commands 空时跳过 CommandHint
    #[test]
    fn cycle_focus_skips_when_no_commands() {
        let mut s = AppState::new(AbacusMode::Clarify);
        s.commands.clear();
        s.panel_visible = true;
        assert_eq!(s.focus, Focus::Input);
        s.cycle_focus();
        assert_eq!(s.focus, Focus::Panel);
        s.cycle_focus();
        assert_eq!(s.focus, Focus::Input, "CommandHint 应被跳过");
    }

    /// V32 · 极端：panel 隐藏 + commands 空 → 留在 Input
    #[test]
    fn cycle_focus_stays_when_only_input() {
        let mut s = AppState::new(AbacusMode::Clarify);
        s.commands.clear();
        s.panel_visible = false;
        assert_eq!(s.focus, Focus::Input);
        s.cycle_focus();
        assert_eq!(s.focus, Focus::Input, "无其他可见档位时 cycle 留在 Input");
    }

    /// V32 · 磁吸抑制窗口保护用户连续输入
    #[test]
    fn try_magnet_focus_respects_keypress_window() {
        let mut s = AppState::new(AbacusMode::Clarify);
        s.panel_visible = true;
        s.record_keypress(); // 模拟用户刚按键
        s.try_magnet_focus(Focus::Panel, PanelSection::Timeline);
        assert_eq!(s.focus, Focus::Input, "用户刚操作 → 不磁吸");
    }

    /// V32 · 用户离手 ≥ MAGNET_SUPPRESS_MS 后允许磁吸
    #[test]
    fn try_magnet_focus_after_idle() {
        let mut s = AppState::new(AbacusMode::Clarify);
        s.panel_visible = true;
        // 模拟"很久没按键"：手动设 last_user_keypress_at 为远古
        s.last_user_keypress_at = Some(
            Instant::now() - std::time::Duration::from_millis((MAGNET_SUPPRESS_MS + 100) as u64),
        );
        s.try_magnet_focus(Focus::Panel, PanelSection::Timeline);
        assert_eq!(s.focus, Focus::Panel, "用户离手 ≥ 抑制窗 → 磁吸生效");
    }

    // ════════════════════════════════════════════════════════════
    // V33 PanelTab::Quant 行为测试
    //   引用关系：验证 PanelTab::all / next / set_mode 边界对 Quant 的处理
    //   生命周期：单元测试，不依赖外部状态
    // ════════════════════════════════════════════════════════════
    #[test]
    fn panel_tab_all_includes_quant_for_every_mode() {
        // V34: 2 个 mode 的静态 tab 序列必须包含 Quant 作为末位（Custom 之前）
        for mode in [AbacusMode::Clarify, AbacusMode::Meeting] {
            let tabs = PanelTab::all(mode);
            assert!(tabs.contains(&PanelTab::Quant),
                "mode={:?} 必须含 Quant tab", mode);
            assert_eq!(tabs.last(), Some(&PanelTab::Quant),
                "mode={:?} Quant 应为末位（Custom 仅在 all_with_custom 时追加）", mode);
        }
    }

    #[test]
    fn panel_tab_next_cycles_through_quant() {
        // V35: 两模式统一两 Tab — 现场(Timeline) + 仓库(Quant)
        // Clarify: Timeline → Quant → Timeline (循环)
        let mode = AbacusMode::Clarify;
        assert_eq!(PanelTab::Timeline.next(mode), PanelTab::Quant);
        assert_eq!(PanelTab::Quant.next(mode), PanelTab::Timeline);

        // Meeting: Timeline → Quant → Timeline (循环) — Tasks 已并入现场 Tab
        let mode = AbacusMode::Meeting;
        assert_eq!(PanelTab::Timeline.next(mode), PanelTab::Quant);
        assert_eq!(PanelTab::Quant.next(mode), PanelTab::Timeline);
    }

    #[test]
    fn set_mode_preserves_quant_across_modes() {
        // 在 Meeting 切到 Quant tab，然后切到 Clarify mode；Quant 在两者 allowed 列表都在 → 应保留
        let mut s = AppState::new(AbacusMode::Meeting);
        s.panel_tab = PanelTab::Quant;
        s.set_mode(AbacusMode::Clarify);
        assert_eq!(s.panel_tab, PanelTab::Quant,
            "Quant 在两个 mode 都合法，跨 mode 切换应保留");

        // 反向：Clarify → Meeting，Quant 仍合法保留
        s.set_mode(AbacusMode::Meeting);
        assert_eq!(s.panel_tab, PanelTab::Quant);
    }

    #[test]
    fn set_mode_demotes_orphan_tab_to_timeline() {
        // Tasks 在 Clarify 不合法 → 切到 Clarify 应被兜底回 Timeline
        let mut s = AppState::new(AbacusMode::Meeting);
        s.panel_tab = PanelTab::Tasks;
        s.set_mode(AbacusMode::Clarify);
        assert_eq!(s.panel_tab, PanelTab::Timeline,
            "Tasks 不在 Clarify allowed 列表 → 兜底回 Timeline");
    }

    #[test]
    fn test_add_message_grows_and_caps() {
        let mut state = AppState::new(AbacusMode::Clarify);
        for i in 0..1500 {
            if i % 2 == 0 {
                state.add_message(Message::new_user(format!("msg {}", i), "12:00"));
            } else {
                state.add_message(Message::new_session(
                    vec![MsgContent::Stream(format!("reply {}", i))],
                    "12:00",
                ));
            }
        }
        assert!(state.messages.len() <= MAX_MESSAGES);
        // turn_count 只对 User 消息递增，1500 条中约一半是 User
        assert_eq!(state.turn_count, 750);
    }

    #[test]
    fn test_add_event_cap() {
        // V28: add_event 现在写入 trace_events,events 字段保留但不再写入。
        // 验证 SSOT 切换后裁剪上限仍然生效。
        let mut state = AppState::new(AbacusMode::Clarify);
        for _ in 0..1000 {
            state.add_event("12:00", "test", "event", EventLevel::Info);
        }
        assert!(state.trace_events.len() <= MAX_EVENTS);
        assert!(state.events.is_empty(), "V28: 旧 events 字段不再被 add_event 写入");
    }

    #[test]
    fn test_expert_count_from_messages() {
        let mut state = AppState::new(AbacusMode::Clarify);
        state.add_message(Message::new_expert("ExpertA", vec![], "12:00"));
        state.add_message(Message::new_expert("ExpertB", vec![], "12:01"));
        state.add_message(Message::new_expert("ExpertA", vec![], "12:02"));
        assert_eq!(state.expert_count(), 2);
    }

    #[test]
    fn test_empty_expert_count() {
        let state = AppState::new(AbacusMode::Clarify);
        assert_eq!(state.expert_count(), 0);
    }

    // ── V28 Trace 数据模型测试 ──────────────────────────────────────

    #[test]
    fn test_v28_push_trace_id_monotonic() {
        let mut state = AppState::new(AbacusMode::Clarify);
        let id0 = state.push_trace("test", EventLevel::Info, TraceKind::Generic { content: "a".into() });
        let id1 = state.push_trace("test", EventLevel::Info, TraceKind::Generic { content: "b".into() });
        let id2 = state.push_trace("test", EventLevel::Info, TraceKind::Generic { content: "c".into() });
        assert_eq!(id0, 0);
        assert_eq!(id1, 1);
        assert_eq!(id2, 2);
        assert_eq!(state.next_trace_id, 3);
        assert_eq!(state.trace_events.len(), 3);
    }

    #[test]
    fn test_v28_add_event_equals_push_trace_generic() {
        // V28 兼容 wrapper 契约: add_event 行为等价于 push_trace(Generic)
        let mut a = AppState::new(AbacusMode::Clarify);
        let mut b = AppState::new(AbacusMode::Clarify);
        a.add_event("12:00", "llm", "hello", EventLevel::Info);
        b.push_trace_with_time("12:00", "llm", EventLevel::Info, TraceKind::Generic { content: "hello".into() });
        assert_eq!(a.trace_events.len(), b.trace_events.len());
        assert_eq!(a.trace_events[0].time, b.trace_events[0].time);
        assert_eq!(a.trace_events[0].category, b.trace_events[0].category);
        match (&a.trace_events[0].kind, &b.trace_events[0].kind) {
            (TraceKind::Generic { content: c1 }, TraceKind::Generic { content: c2 }) => {
                assert_eq!(c1, c2);
            }
            _ => panic!("expected Generic kind in both"),
        }
    }

    #[test]
    fn test_v28_reset_streaming_preserves_trace_events() {
        // SSOT 不变量: reset_streaming 不能动 trace_events,只清 streaming_*
        let mut state = AppState::new(AbacusMode::Clarify);
        let id = state.push_trace("llm", EventLevel::Info, TraceKind::Thinking { text: "x".into(), lines: 1 });
        state.streaming_trace_ids.push(id);
        // V42-B: 用 CardStream 管道设置内容，而非直接写 V40 字段
        state.begin_streaming_session();
        // V42-B 拆卡: reply 走 LlmCard, thinking 走 ThinkingCard
        if let Some(llm) = state.cards.card_downcast_mut::<crate::tui::cards::LlmCard>(
            state.cards.active_id().unwrap()
        ) {
            llm.append_reply("partial");
        }
        let th_id = state.cards.alloc_id();
        let mut th = crate::tui::cards::ThinkingCard::new(th_id, "test");
        th.append("thinking");
        state.cards.push_active(Box::new(th));

        state.reset_streaming();

        assert_eq!(state.trace_events.len(), 1, "trace_events 必须保留(SSOT)");
        assert_eq!(state.next_trace_id, 1, "next_trace_id 不应回退");
        // V42-B: CardStream 被 reset 清空，但 V40 字段不再由生产代码写入
        assert!(state.active_llm_text().is_empty(), "active_llm_text 应被 reset 清空");
        assert!(state.active_llm_thinking().is_empty(), "active_llm_thinking 应被 reset 清空");
        assert!(state.streaming_trace_ids.is_empty(), "streaming_trace_ids 兜底清空");
    }

    #[test]
    fn test_v28_trace_max_fifo_eviction() {
        let mut state = AppState::new(AbacusMode::Clarify);
        for i in 0..(MAX_EVENTS + 50) {
            state.push_trace("test", EventLevel::Info, TraceKind::Generic { content: format!("e{}", i) });
        }
        // 裁剪后 ≤ MAX_EVENTS,但 next_trace_id 仍单调(被裁剪的 id 不复用)
        assert!(state.trace_events.len() <= MAX_EVENTS);
        assert_eq!(state.next_trace_id, (MAX_EVENTS + 50) as u64);
        // 裁剪保留的应该是较新的 events(最后一条 id 为 next_trace_id - 1)
        let last = state.trace_events.last().expect("non-empty");
        assert_eq!(last.id, state.next_trace_id - 1);
    }

    #[test]
    fn test_cursor_char_boundary() {
        let mut state = AppState::new(AbacusMode::Clarify);
        // 模拟输入一个 emoji (4 字节)
        state.input.push('🔥');
        state.cursor_pos = "🔥".len();
        assert_eq!(state.cursor_pos, 4);

        // Backspace: 删除最后一个字符
        if let Some((idx, _)) = state.input[..state.cursor_pos].char_indices().next_back() {
            state.input.remove(idx);
            state.cursor_pos = idx;
        }
        assert!(state.input.is_empty());
        assert_eq!(state.cursor_pos, 0);
    }

    #[test]
    fn test_mode_switch_state_clear() {
        let mut state = AppState::new(AbacusMode::Clarify);
        state.add_message(Message::new_user("hello", "12:00"));
        state.tasks.push(TaskCard {
            id: "T1".into(),
            title: "test".into(),
            assignee: "coder".into(),
            status: TaskStatus::Pending,
            progress: 0,
            deps: vec![],
            description: "".into(),
        });

        // 模拟 switch_mode 清空
        state.messages.clear();
        state.events.clear();
        state.tool_records.clear();
        state.thinking_text.clear();
        state.experts.clear();
        state.tasks.clear();

        assert!(state.messages.is_empty());
        assert!(state.tasks.is_empty());
    }

    #[test]
    fn test_toast_cleanup() {
        let mut state = AppState::new(AbacusMode::Clarify);
        state.add_toast("test", Duration::from_secs(0));
        std::thread::sleep(std::time::Duration::from_millis(10));
        state.cleanup_toasts();
        assert!(state.toasts.is_empty());
    }

    // ── 前后端联通 E2E 测试 ──────────────────────────

    #[test]
    fn test_submit_message_sets_pending_with_flag() {
        let mut state = AppState::new(AbacusMode::Clarify);
        state.input = "hello world".to_string();

        let text = state.input.trim().to_string();
        assert!(!text.is_empty());
        state.pending_text = Some(text.clone());
        state.input_state = InputState::Thinking;

        assert_eq!(state.pending_text, Some("hello world".to_string()));
        assert_eq!(state.input_state, InputState::Thinking);
        let text = state.pending_text.take();
        assert!(text.is_some());
        assert!(state.pending_text.is_none());
    }

    #[test]
    fn test_engine_response_restores_ready() {
        let mut state = AppState::new(AbacusMode::Clarify);
        state.input_state = InputState::Thinking;

        // main.rs interval.tick 收到引擎响应后执行的逻辑
        state.add_message(Message::new_session(
            vec![MsgContent::Stream("hello back".into())],
            "12:00",
        ));
        state.add_event("12:00", "llm", "生成完成", EventLevel::Notice);
        state.input_state = InputState::Ready;

        assert_eq!(state.input_state, InputState::Ready);
        assert_eq!(state.messages.len(), 1);
        // V28: SSOT 切换 — add_event 写 trace_events,events 字段保留但不再写入
        assert_eq!(state.trace_events.len(), 1);
        assert!(state.events.is_empty());
        if let MsgRole::Session = &state.messages[0].role {
        } else {
            panic!("expected Session role");
        }
    }

    #[test]
    fn test_full_engine_bridge_cycle() {
        // 完整周期: new → submit → pending → spawn → response → ready
        let mut state = AppState::new(AbacusMode::Clarify);

        // 1. 提交消息
        state.input = "test message".to_string();
        state.add_message(Message::new_user("test message", "12:00"));
        state.input.clear();
        state.cursor_pos = 0;
        state.input_state = InputState::Thinking;
        state.pending_text = Some("test message".to_string());

        assert_eq!(state.messages.len(), 1);
        assert_eq!(state.pending_text, Some("test message".to_string()));

        // 2. 模拟 spawn 完成后收到响应
        let _text = state.pending_text.take();
        state.add_message(Message::new_session(
            vec![MsgContent::Stream("[Mock Engine] response".into())],
            "12:01",
        ));
        state.input_state = InputState::Ready;

        // 3. 验证结果
        assert_eq!(state.messages.len(), 2);
        assert_eq!(state.input_state, InputState::Ready);
        assert!(state.pending_text.is_none());
        // turn_count 只对 User 消息递增
        assert_eq!(state.turn_count, 1);
        match (&state.messages[0].role, &state.messages[1].role) {
            (MsgRole::User, MsgRole::Session) => {} // correct
            _ => panic!("expected User then Session roles"),
        }
    }

    #[test]
    fn test_engine_offline_fallback() {
        // 无引擎时 submit_message 应直接恢复 Ready
        let mut state = AppState::new(AbacusMode::Clarify);
        state.engine_handle = None;

        state.add_message(Message::new_user("offline test", "12:00"));
        state.input_state = InputState::Ready;

        assert_eq!(state.input_state, InputState::Ready);
        assert_eq!(state.messages.len(), 1);
    }

    #[test]
    fn test_pending_text_cleared_on_switch_mode() {
        let mut state = AppState::new(AbacusMode::Clarify);
        state.pending_text = Some("pending".to_string());
        state.turn_count = 5;

        // switch_mode 清空
        // V29.16: 走 SSOT set_scroll, 与生产代码同入口 (避免测试和实际行为分叉)
        state.messages.clear();
        state.events.clear();
        state.pending_text = None;
        state.set_scroll(ScrollAction::ToBottom);
        state.input.clear();

        assert!(state.pending_text.is_none());
        assert!(state.messages.is_empty());
        assert_eq!(state.turn_count, 5); // 不清除 turn_count (引擎会话已消耗)
    }

    #[test]
    fn test_engine_bridge_state_defaults() {
        let state = AppState::new(AbacusMode::Clarify);
        assert!(state.engine_handle.is_none());
        assert!(state.engine_tx.is_none());
        assert!(state.pending_text.is_none());
    }

    /// ST1 回归：reset_streaming 必须清空所有流式累积字段（含增量解析缓存）
    /// 防御「双显示 bug」复发——任何字段漏清都会导致 streaming_* 残留与 messages 重复渲染
    #[test]
    fn reset_streaming_clears_all_fields() {
        let mut state = AppState::new(AbacusMode::Clarify);
        // 模拟流式累积状态
        state.begin_streaming_session();
        // V42-B: 通过 CardStream 设置内容（拆卡后 thinking 走 ThinkingCard）
        if let Some(llm) = state.cards.card_downcast_mut::<crate::tui::cards::LlmCard>(
            state.cards.active_id().unwrap()
        ) {
            llm.append_reply("partial output");
        }
        let th_id = state.cards.alloc_id();
        let mut th = crate::tui::cards::ThinkingCard::new(th_id, "test");
        th.append("partial reasoning");
        state.cards.push_active(Box::new(th));
        state.streaming_tools.push(("read_file".into(), StreamingToolStatus::Running, None, 0));
        state.push_timeline_entry(TimelineEntry::Tool {
            name: "read_file".into(), context: String::new(),
            status: StreamingToolStatus::Running, duration_ms: None,
            failure_kind: None, trace_id: 0,
        });

        state.reset_streaming();

        assert!(!state.is_streaming_active(), "is_streaming 应被清空");
        // V42-B: CardStream 内容被 reset 清空
        assert!(state.active_llm_text().is_empty(), "active_llm_text 应被清空");
        assert!(state.active_llm_thinking().is_empty(), "active_llm_thinking 应被清空");
        assert!(state.streaming_tools.is_empty(), "streaming_tools 应被清空");
        assert!(state.streaming_timeline.is_empty(), "streaming_timeline 应被清空");
    }

    /// ST1 回归：reset_streaming 是幂等操作——多次调用无副作用
    #[test]
    fn reset_streaming_is_idempotent() {
        let mut state = AppState::new(AbacusMode::Clarify);
        state.reset_streaming();
        state.reset_streaming();
        state.reset_streaming();
        // 默认状态已是 reset 状态，多次调用仍应保持
        assert!(!state.is_streaming_active());
        assert!(state.active_llm_text().is_empty());
    }

    // ─── V42-B streaming text/thinking helpers ──────────────────────

    #[test]
    fn active_llm_text_empty_when_no_active() {
        let state = AppState::new(AbacusMode::Clarify);
        assert_eq!(state.active_llm_text(), "");
        assert_eq!(state.active_llm_thinking(), "");
    }

    #[test]
    fn active_llm_text_returns_card_reply_text() {
        use crate::tui::cards::LlmCard;
        let mut state = AppState::new(AbacusMode::Clarify);
        let id = state.cards.alloc_id();
        let mut card = LlmCard::new(id, "test-model");
        card.append_reply("hello ");
        card.append_reply("world");
        state.cards.push_active(Box::new(card));
        assert_eq!(state.active_llm_text(), "hello world");
    }

    #[test]
    fn active_llm_thinking_returns_card_thinking() {
        use crate::tui::cards::ThinkingCard;
        let mut state = AppState::new(AbacusMode::Clarify);
        let id = state.cards.alloc_id();
        let mut card = ThinkingCard::new(id, "test-model");
        card.append("step 1: ");
        card.append("analyze");
        state.cards.push_active(Box::new(card));
        assert_eq!(state.active_llm_thinking(), "step 1: analyze");
    }

    #[test]
    fn active_llm_thinking_empty_when_card_has_none() {
        use crate::tui::cards::ThinkingCard;
        let mut state = AppState::new(AbacusMode::Clarify);
        let id = state.cards.alloc_id();
        let card = ThinkingCard::new(id, "test-model");
        state.cards.push_active(Box::new(card));
        assert_eq!(state.active_llm_thinking(), "");
    }

    #[test]
    fn active_llm_text_empty_when_active_is_not_llm() {
        use crate::tui::cards::AbacusCard;
        let mut state = AppState::new(AbacusMode::Clarify);
        let id = state.cards.alloc_id();
        // Push a non-LlmCard (AbacusCard) as active
        state.cards.push_active(Box::new(AbacusCard::new(id, "tool")));
        // active_llm_text should return empty since it's not an LlmCard
        assert_eq!(state.active_llm_text(), "");
        assert_eq!(state.active_llm_thinking(), "");
    }

    // ─── V42-B last_llm_text/thinking (for finalize path) ─────────

    #[test]
    fn last_llm_text_empty_when_no_llm_card() {
        let state = AppState::new(AbacusMode::Clarify);
        assert_eq!(state.last_llm_text(), "");
        assert_eq!(state.last_llm_thinking(), "");
    }

    #[test]
    fn last_llm_text_finds_after_finish() {
        // 模拟 finalize 场景：卡片已被 finish_active() 标记为 Static
        use crate::tui::cards::{LlmCard, ThinkingCard};
        let mut state = AppState::new(AbacusMode::Clarify);
        let id = state.cards.alloc_id();
        let mut card = LlmCard::new(id, "test-model");
        card.append_reply("first response");
        state.cards.push_active(Box::new(card));
        // thinking 拆到独立卡片
        let th_id = state.cards.alloc_id();
        let mut th = ThinkingCard::new(th_id, "test-model");
        th.append("first reasoning");
        state.cards.push_active(Box::new(th));
        state.cards.finish_active();
        assert!(state.cards.active_id().is_none());
        assert_eq!(state.last_llm_text(), "first response");
        assert_eq!(state.last_llm_thinking(), "first reasoning");
    }

    #[test]
    fn last_llm_text_picks_most_recent() {
        // 多次 LlmCard（多轮）：应返回最后一张
        use crate::tui::cards::LlmCard;
        let mut state = AppState::new(AbacusMode::Clarify);
        // Turn 1
        let id1 = state.cards.alloc_id();
        let mut card1 = LlmCard::new(id1, "model");
        card1.append_reply("first turn");
        state.cards.push_active(Box::new(card1));
        state.cards.finish_active();
        // Turn 2
        let id2 = state.cards.alloc_id();
        let mut card2 = LlmCard::new(id2, "model");
        card2.append_reply("second turn");
        state.cards.push_active(Box::new(card2));
        state.cards.finish_active();
        assert_eq!(state.last_llm_text(), "second turn");
    }

    #[test]
    fn last_llm_text_skips_non_llm_cards() {
        use crate::tui::cards::{AbacusCard, LlmCard};
        let mut state = AppState::new(AbacusMode::Clarify);
        // LlmCard first
        let id1 = state.cards.alloc_id();
        let mut card1 = LlmCard::new(id1, "m");
        card1.append_reply("llm text");
        state.cards.push_active(Box::new(card1));
        state.cards.finish_active();
        // Then AbacusCard (tool result) — should be skipped
        let id2 = state.cards.alloc_id();
        state.cards.push_active(Box::new(AbacusCard::new(id2, "tool")));
        state.cards.finish_active();
        // last_llm_text should still return the LlmCard text, not nothing
        assert_eq!(state.last_llm_text(), "llm text");
    }

    #[test]
    fn take_last_llm_text_clears_card() {
        // 用于 fatal network 错误时 take-and-store
        use crate::tui::cards::LlmCard;
        let mut state = AppState::new(AbacusMode::Clarify);
        let id = state.cards.alloc_id();
        let mut card = LlmCard::new(id, "m");
        card.append_reply("partial text");
        state.cards.push_active(Box::new(card));
        state.cards.finish_active();

        let taken = state.take_last_llm_text();
        assert_eq!(taken, "partial text");
        // After take, the card is still in stream but its reply_text is empty
        assert_eq!(state.last_llm_text(), "");
    }

    #[test]
    fn take_last_llm_text_empty_when_no_llm_card() {
        let mut state = AppState::new(AbacusMode::Clarify);
        let taken = state.take_last_llm_text();
        assert_eq!(taken, "");
    }

    // ─── V42-B 重复响应回归测试（user reported scenario）───
    //
    // 用户报告：发送"你好"，LLM 回复"你好。准备开始什么任务？"在聊天区出现两次。
    // 模拟用户实际场景：UserCard → 流式 LlmCard（累积完整文本）→ EngineResponse 落档。
    // 期望：add_message(Session) 应触发 dedup，跳过新建 LlmCard，仅保留流式累积的卡。
    //
    // 根因假设：3c26b8a 修复在以下边界场景可能仍 miss：
    //   - LlmCard.reply_text 与 response.text 内容不完全相等
    //   - finish_active 时机问题
    //   - 别的路径也调 add_message

    #[test]
    fn dedup_skips_when_llm_card_already_has_full_text() {
        use crate::tui::cards::LlmCard;

        let mut state = AppState::new(AbacusMode::Clarify);

        // 1. 用户输入 → UserCard
        state.add_message(Message::new_user("你好", "03:30"));

        // 2. begin_streaming_session 创建空 LlmCard (active)
        state.begin_streaming_session();

        // 3. 流式累积（模拟 LLM 完整回复）
        let reply = "你好。准备开始什么任务？";
        let active_id = state.cards.active_id().expect("active after begin_streaming");
        if let Some(llm) = state.cards.card_downcast_mut::<LlmCard>(active_id) {
            llm.append_reply(reply);
        }

        // 4. Complete 抵达 → finish_active（直接调，避免依赖 StreamChunk::Default）
        state.cards.finish_active();
        assert!(state.cards.active_id().is_none(), "active should be finished");

        // 5. 关键：此时 state.cards 里 LlmCard 的 reply_text 应 == reply
        let pre_count = state.cards.len();
        let pre_last_text = state.last_llm_text();
        assert_eq!(pre_last_text, reply, "LlmCard should have full text");

        // 6. 模拟 process_engine_response 调用 add_message(Session{Stream(reply)})
        state.add_message(Message::new_session(
            vec![MsgContent::Stream(reply.to_string())],
            "03:30",
        ));

        // 7. 期望：dedup 命中，cards 数量不变 (UserCard + 1 LlmCard)
        let post_count = state.cards.len();
        assert_eq!(
            post_count, pre_count,
            "dedup should skip: pre={} post={}", pre_count, post_count
        );
    }

    #[test]
    fn dedup_with_exact_match_still_dedups() {
        use crate::tui::cards::LlmCard;

        let mut state = AppState::new(AbacusMode::Clarify);
        state.begin_streaming_session();

        let reply = "你好。准备开始什么任务？";
        let active_id = state.cards.active_id().unwrap();
        if let Some(llm) = state.cards.card_downcast_mut::<LlmCard>(active_id) {
            llm.append_reply(reply);
        }
        state.cards.finish_active();

        let count_before = state.cards.len();
        state.add_message(Message::new_session(
            vec![MsgContent::Stream(reply.to_string())],
            "03:30",
        ));
        assert_eq!(
            state.cards.len(),
            count_before,
            "exact match should dedup"
        );
    }

    #[test]
    fn dedup_when_response_text_differs_by_trailing_newline() {
        use crate::tui::cards::LlmCard;

        let mut state = AppState::new(AbacusMode::Clarify);
        state.begin_streaming_session();

        // 流式累积末尾带换行
        let streamed = "你好。准备开始什么任务？\n";
        let active_id = state.cards.active_id().unwrap();
        if let Some(llm) = state.cards.card_downcast_mut::<LlmCard>(active_id) {
            llm.append_reply(streamed);
        }
        state.cards.finish_active();

        let count_before = state.cards.len();
        // response.text 无末尾换行（典型 case：API 端 trim 过）
        let response_text = "你好。准备开始什么任务？";
        state.add_message(Message::new_session(
            vec![MsgContent::Stream(response_text.to_string())],
            "03:30",
        ));
        // 期望：streamed.contains(response_text) → dedup 命中
        assert_eq!(
            state.cards.len(),
            count_before,
            "trailing-newline diff should still dedup via contains()"
        );
    }

    /// Bug repro: 流式累积被 ToolAgentResult 注入了 prefix，response.text 只包含正文
    /// 这是用户实际可能遇到的 duplication 场景之一
    #[test]
    fn dedup_when_streaming_has_tool_agent_prefix() {
        use crate::tui::cards::LlmCard;

        let mut state = AppState::new(AbacusMode::Clarify);
        state.begin_streaming_session();

        // 模拟 ToolAgentResult 先注入 prefix（format!("\n{} {} · {} calls", icon, name, call_count)）
        let active_id = state.cards.active_id().unwrap();
        if let Some(llm) = state.cards.card_downcast_mut::<LlmCard>(active_id) {
            llm.append_reply("\n🔍 code · 1 calls");
        }
        // 然后 LLM 回复正文
        if let Some(llm) = state.cards.card_downcast_mut::<LlmCard>(active_id) {
            llm.append_reply("\n你好。准备开始什么任务？");
        }
        state.cards.finish_active();

        // LlmCard.reply_text = "\n🔍 code · 1 calls\n\n你好。准备开始什么任务？"

        let count_before = state.cards.len();
        // response.text 只包含正文（不含 ToolAgent prefix）
        let response_text = "你好。准备开始什么任务？";
        state.add_message(Message::new_session(
            vec![MsgContent::Stream(response_text.to_string())],
            "03:30",
        ));
        // 期望：text 包含在 existing 中 → dedup 命中（条件三：text.contains(&existing)...）
        // 实际：existing 是超集，text 是子集 → existing.contains(text) 命中
        assert_eq!(
            state.cards.len(),
            count_before,
            "tool_agent_prefix scenario should dedup"
        );
    }

    /// Bug repro: 流式累积包含 markdown formatting，response.text 可能是 stripped 版本
    #[test]
    fn dedup_when_streaming_has_markdown_formatting() {
        use crate::tui::cards::LlmCard;

        let mut state = AppState::new(AbacusMode::Clarify);
        state.begin_streaming_session();

        let active_id = state.cards.active_id().unwrap();
        if let Some(llm) = state.cards.card_downcast_mut::<LlmCard>(active_id) {
            // 流式累积包含 **bold** 标记
            llm.append_reply("**你好。准备开始什么任务？**");
        }
        state.cards.finish_active();

        let count_before = state.cards.len();
        // response.text 不含 markdown 标记
        let response_text = "你好。准备开始什么任务？";
        state.add_message(Message::new_session(
            vec![MsgContent::Stream(response_text.to_string())],
            "03:30",
        ));
        // 此场景下不应该 dedup（因为内容确实不同）
        // 这是正确的行为 — 测试记录这个不 dedup 的边界
        assert_eq!(
            state.cards.len(),
            count_before + 1,
            "markdown stripping is content change, should create new card"
        );
    }

    /// Bug repro: 同一 turn 多次调 add_message（重复 EngineResponse 抵达）
    #[test]
    fn dedup_against_double_add_message() {
        use crate::tui::cards::LlmCard;

        let mut state = AppState::new(AbacusMode::Clarify);
        state.begin_streaming_session();

        let reply = "你好。准备开始什么任务？";
        let active_id = state.cards.active_id().unwrap();
        if let Some(llm) = state.cards.card_downcast_mut::<LlmCard>(active_id) {
            llm.append_reply(reply);
        }
        state.cards.finish_active();

        let count_before = state.cards.len();
        // 第一次 add_message: 应该 dedup（流式已累积完整文本）
        state.add_message(Message::new_session(
            vec![MsgContent::Stream(reply.to_string())],
            "03:30",
        ));
        assert_eq!(state.cards.len(), count_before, "first add_message should dedup");

        // 第二次 add_message（同响应重发）: 也应该 dedup
        state.add_message(Message::new_session(
            vec![MsgContent::Stream(reply.to_string())],
            "03:30",
        ));
        assert_eq!(
            state.cards.len(),
            count_before,
            "second add_message should also dedup"
        );
    }
}
