//! Abacus TUI Event — 事件处理 + 全局快捷键
//!
//! 设计规范来源: ABACUS-TUI-DESIGN-SPEC.md v1.0
//!
//! ## 快捷键优先级（从高到低）
//! - 双击 `Ctrl+C` — 退出
//! - `Esc` — 关闭 popup / 取消选中 / 暂停切换（链式优先级见 dispatch_esc）
//! - `Ctrl+/` — 打开快捷键帮助
//! - `Tab/Shift+Tab` — 看板焦点时切换 Tab 页；否则触发补全
//! - `Ctrl+B` — 焦点切换（看板 ↔ 命令提示框，显式兜底）
//! - `[/]` — 看板 Tab 切换（看板可见时）
//! - `Enter` — 发送消息 | `Shift+Enter`/`Ctrl+Enter` — 换行
//! - `Alt+1..9` — 补全弹窗可见时直选第 N 项
//!
//! ## 焦点系统（V32 多触发器）
//! - **输入栏始终可接收字符**，不参与焦点循环
//! - **意图前置**（auto_route_focus）：用户敲 ↑↓/Tab/`/` 时焦点自动跟过去
//! - **事件磁吸**（try_magnet_focus）：agent 消息/trace 抵达且用户离手 ≥ 2s 时切到 Panel/Timeline
//! - **Slash 自动补全**：输入栏首位 `/` 起头任意字符自动弹候选弹窗（无需 Tab）

// V30 复制修复后不再手写 OSC 52，改走 tui::clipboard::set_text；std::io::Write 不再需要。
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use tracing::info;

use crate::tui::state::{AppState, Focus, InputState, Message, MsgContent, ScrollAction, AbacusMode, TextSelection};

/// Phase 3 去重：base64_encode_inner 已统一到 util::base64_encode
/// 保留 pub 别名防止外部依赖（如有）break，下一 PR 可删此 wrapper
///
/// 引用关系：调用 crate::tui::util::base64_encode（SSoT）
/// 生命周期：纯函数 wrapper，无状态
#[inline]
pub fn base64_encode_inner(input: &str) -> String {
    crate::tui::util::base64_encode(input)
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// 斜杠命令解析器
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// V13: 应用 picker 选中项 — 调用对应 cmd handler with 单参数
///
/// 引用关系：picker 打开期间 Enter 触发；调用 slash_commands::dispatch 复用现成 handler
/// 生命周期：选完关闭 picker；输入框、焦点状态恢复正常
pub fn apply_picker_selection(state: &mut AppState) {
    use crate::tui::state::PickerKind;
    let Some(p) = state.picker.take() else { return; };
    let Some(value) = p.items.get(p.selected).cloned() else { return; };
    // V29.8: PickerKind::Thinking 走 "/model thinking <value>" (原 /thinking 已合并)
    let review_strict = p.review_strict; // 取出 strict flag（picker 即将被 take）
    let cmd = match p.kind {
        PickerKind::Model    => format!("/model {}", value),
        PickerKind::Theme    => format!("/theme {}", value),
        PickerKind::Thinking => format!("/model thinking {}", value),
        PickerKind::Mode     => format!("/{}", value),      // /clarify 或 /meeting
        PickerKind::Review   => {
            if review_strict {
                format!("/review {} --strict", value)
            } else {
                format!("/review {}", value)
            }
        }
        PickerKind::Resume   => format!("/resume {}", value),
        PickerKind::Meeting  => {
            // meeting    → /meeting （切换模式）
            // expert     → /expert list
            // meeting-list → /meeting-list
            match value.as_str() {
                "expert"       => "/expert list".to_string(),
                "meeting-list" => "/meeting-list".to_string(),
                _              => "/meeting".to_string(),  // default: switch mode
            }
        }
        PickerKind::Preset   => format!("/preset {}", value),
        PickerKind::History  => {
            // History 不走 slash dispatch，直接写入 input 并 submit
            state.picker = None; // 已被 take，确保清空
            state.input = value;
            state.cursor_pos = state.input.len();
            state.cursor_line = 0;
            state.cursor_col = state.input.len();
            state.input_state = crate::tui::state::InputState::Ready;
            state.rendered_lines_dirty.set(true);
            submit_message(state);
            return;
        }
    };
    // 复用 dispatch（已有 toast / state 副作用），不再走 picker 拦截分支
    let _ = crate::tui::slash_commands::dispatch(state, &cmd);
    // 清空输入框（用户已完成选择）
    state.input.clear();
    state.cursor_pos = 0;
    state.cursor_line = 0;
    state.cursor_col = 0;
    state.input_state = crate::tui::state::InputState::Ready;
    state.rendered_lines_dirty.set(true);
}

/// 解析并执行斜杠命令。返回 true 表示命令已消费（不发给引擎）。
///
/// V13: 拦截无参的 `/model` `/theme` `/thinking` 弹出参数 picker；
///      带参（"/model qwen"）走原 dispatch
pub fn handle_slash_command(state: &mut AppState, text: &str) -> bool {
    use crate::tui::slash_commands;
    let trimmed = text.trim();
    // 无参命令名 → 转 picker（用户体验：箭头选 + Enter 确认）
    match trimmed {
        "/model" | "/m"                => { state.open_picker_model();    return true; }
        "/theme"                       => { state.open_picker_theme();    return true; }
        "/thinking" | "/think" | "/t"  => { state.open_picker_thinking(); return true; }
        "/mode"                        => { state.open_picker_mode();     return true; }
        "/meeting" | "/meet"            => { state.open_picker_meeting();  return true; }
        "/review"                      => { state.open_picker_review();   return true; }
        "/resume"                      => { state.open_picker_resume();   return true; }
        "/history"                     => { state.open_picker_history();  return true; }
        "/preset"                      => {
            // 无参 /preset → 走 cmd_preset 内部逻辑打开 picker
            let _ = crate::tui::slash_commands::dispatch(state, "/preset");
            return true;
        }
        _ => {}
    }
    match slash_commands::dispatch(state, text) {
        slash_commands::CmdResult::Consumed => true,
        slash_commands::CmdResult::Pending(cmd) => {
            state.pending_slash_command = Some(cmd);
            state.add_toast("正在执行...", Duration::from_secs(2));
            true
        }
        slash_commands::CmdResult::NotFound(_) => false,
    }
}

// ── 斜杠命令处理函数 ──

/// L3-14: 共享会话重置逻辑（避免 /new、Ctrl+N、Ctrl+W 重复）
/// Phase 3 去重：委托 AppState::reset_session（SSoT）+ toast
fn reset_session_state(state: &mut AppState, toast: &str) {
    state.reset_session();
    state.add_toast(toast, Duration::from_secs(2));
}

fn handle_save_session(state: &mut AppState) {
    if state.engine_handle.is_some() {
        match crate::tui::run::save_session(state) {
            Ok(()) => {
                let ts = chrono::Local::now().format("%H:%M").to_string();
                state.add_event(&ts, "session", "手动保存", crate::tui::state::EventLevel::Info);
                state.add_toast("会话已保存", Duration::from_secs(2));
            }
            Err(e) => {
                state.add_toast(format!("保存失败: {}", e), Duration::from_secs(4));
            }
        }
    } else {
        state.add_toast("演示模式 — 会话仅在内存中", Duration::from_secs(2));
    }
}

// E1 清理：5 个死 handle_* 函数已被 slash_commands::cmd_* 替代
//   handle_model_command → cmd_model
//   handle_thinking_command → cmd_thinking（B1 修复后单一真相）
//   handle_theme_command → cmd_theme（B2 修复后支持 12 主题）
//   handle_turnkey_command → 已废弃（功能未上线）
//   handle_export_session → cmd_export（E2 接通了 /export 注册）

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// 全局快捷键处理
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// 处理全局快捷键 (优先级最高)
/// 返回 true 表示事件已消费，不需要继续分发到模式处理器
// ════════════════════════════════════════════════════════════
// K5 Esc 优先级链 — 单点分发器
// ════════════════════════════════════════════════════════════
// 优先级（高到低）：
//   1. ConfirmDialog 存在 → 在 handle_global_key 顶段已处理（不进入本函数）
//   2. settings 面板 → CloseSettings
//   3. Picker / ThemePreview
//   4. Completing 状态 → ExitCompletion
//   5. CancelSelection（有选中区间）
//   6. Thinking/Executing/Outputting → CancelOperation
//   7. V32 焦点不在 Input → ReturnToInput（统一回归输入态）
//   8. 其它 → TogglePause
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EscAction {
    CloseSettings,
    ClosePicker,
    /// 2026-05-28: 全屏编辑器关闭（保留 input 不提交）
    CloseEditor,
    CloseThemePreview,
    ExitCompletion,
    CancelOperation,
    /// V30 复制修复：选中状态存在时 Esc 取消选择不复制
    CancelSelection,
    /// V32 · 焦点回 Input（"统一回归"语义）：从 Panel/CommandHint 显式离开
    ReturnToInput,
    TogglePause,
}

pub fn dispatch_esc(state: &AppState) -> EscAction {
    if state.show_settings { return EscAction::CloseSettings; }
    // V13: picker 打开时 Esc 优先关闭（用户期望"取消选择"）
    if state.picker.is_some() { return EscAction::ClosePicker; }
    // 2026-05-28: 全屏编辑器打开时 Esc 关闭编辑器（保留内容）
    if state.input_state == InputState::Editor { return EscAction::CloseEditor; }
    // V10：theme preview 优先级低于 settings，高于 completion / pause
    if state.theme_preview_open { return EscAction::CloseThemePreview; }
    if state.input_state == InputState::Completing { return EscAction::ExitCompletion; }
    // V30 复制修复：selection 优先于 cancel-op / pause。
    // 设计说明：有选中区间时 Esc 需要能“丢弃选中不复制”，与鼠标释放自动复制互补。
    // 优先级设为高于 CancelOperation 会不适（重要于 taking precedence 临运行判断），
    // 所以放在 input_state pending 之后。
    if state.text_selection.is_some()
        && !matches!(state.input_state, InputState::Thinking | InputState::Executing | InputState::Outputting)
    {
        return EscAction::CancelSelection;
    }
    if matches!(state.input_state, InputState::Thinking | InputState::Executing | InputState::Outputting) {
        return EscAction::CancelOperation;
    }
    // V32 · 焦点不在 Input 时 Esc 优先回 Input：让 Esc 链一致——
    // 任何"非输入"状态 → Esc 一步到位回输入态，再按 Esc 才走 TogglePause
    if state.focus != crate::tui::state::Focus::Input {
        return EscAction::ReturnToInput;
    }
    EscAction::TogglePause
}

fn apply_esc_action(state: &mut AppState, action: EscAction) {
    match action {
        EscAction::CloseSettings => {
            state.show_settings = false;
            state.rendered_lines_dirty.set(true);
        }
        EscAction::ClosePicker => {
            state.picker = None;
            state.add_toast("已取消", Duration::from_millis(800));
        }
        EscAction::CloseEditor => {
            state.close_editor();
        }
        EscAction::CloseThemePreview => {
            state.theme_preview_open = false;
            state.rendered_lines_dirty.set(true);
        }
        EscAction::ExitCompletion => {
            cancel_completion(state);
        }
        EscAction::CancelOperation => {
            state.input_state = InputState::Ready;
            state.op_started_at = None;
            state.accumulated_elapsed = Duration::ZERO;
            // TT18: 走 SSoT helper（reset_streaming 是唯一 streaming 状态清理入口）
            state.reset_streaming();
            state.rendered_lines_dirty.set(true);
            state.add_toast("已取消当前请求", Duration::from_secs(2));
        }
        EscAction::CancelSelection => {
            // V30 复制修复：丢弃选中区间、不复制。
            state.text_selection = None;
            state.rendered_lines_dirty.set(true);
            state.add_toast("已取消选中", Duration::from_millis(800));
        }
        EscAction::ReturnToInput => {
            // V32 · 显式回归输入态：清楚的"我做完浏览/选命令了"语义
            state.set_focus(crate::tui::state::Focus::Input);
            state.add_toast("← 已回到输入栏", Duration::from_millis(800));
        }
        EscAction::TogglePause => {
            state.toggle_pause();
            state.add_toast(
                if state.paused { "已暂停" } else { "已恢复" },
                Duration::from_secs(1),
            );
        }
    }
}

/// 焦点跟随用户操作 · 方案 1 「意图前置」核心入口
///
/// 设计目标：用户期望 ↑↓/Tab/`/` 等键作用在哪个区域，焦点就先飞过去，
/// 让原有快捷键不再因为 focus 错位而"哑火"。原有 Ctrl+B 显式切换、
/// 鼠标点击、Enter 选命令等触发器全部保留——本函数仅做被动补强。
///
/// ## 触发表
/// | 按键 | 条件 | 动作 |
/// |---|---|---|
/// | `↑` / `↓` | panel_visible + focus≠Panel | set_focus(Panel) |
/// | `Tab` / `BackTab` | panel_visible + focus≠Panel | set_focus(Panel) |
/// | `/` | input 为空 + focus≠CommandHint | set_focus(CommandHint) |
///
/// ## 跳过条件
/// - Ctrl/Alt 修饰键 → 让显式快捷键路径处理
/// - `InputState::Completing` → 补全弹窗独占输入
/// - 非 `AbacusMode::Clarify` → 其他模式（meeting/team/setup）有独立焦点系统
/// - confirm_dialog 已在 line ~277 拦截，进不到这里
///
/// ## 优先级取舍
/// ↑↓ 默认归 Panel：用户最常见动作是滚 timeline；右下命令面板是次要的，
/// 想选完整命令列表仍可显式 Ctrl+B 切过去。
fn auto_route_focus(state: &mut AppState, code: KeyCode, mods: KeyModifiers) {
    // 仅 Chat 模式生效
    if !matches!(state.mode, AbacusMode::Clarify) {
        return;
    }
    // 修饰键走显式分支
    if mods.contains(KeyModifiers::CONTROL) || mods.contains(KeyModifiers::ALT) {
        return;
    }
    // 弹窗独占焦点：picker / confirm / completion 打开时不做焦点切换
    if state.input_state == InputState::Completing
        || state.picker.is_some()
        || state.confirm_dialog.is_some()
    {
        return;
    }

    match code {
        // ↑↓ 默认归 Panel：用户最常见动作是滚 timeline
        KeyCode::Up | KeyCode::Down => {
            if state.panel_visible && state.focus != Focus::Panel {
                state.set_focus(Focus::Panel);
            }
        }
        // V32 修正：Tab 在 focus=Input 时**不抢**，让 Tab 走输入栏补全（trigger_completion）
        // 仅在已经处于 Panel/CommandHint 浏览态时才把 Tab 路由到 Panel section 切换
        KeyCode::Tab | KeyCode::BackTab => {
            if state.focus != Focus::Input
                && state.panel_visible
                && state.focus != Focus::Panel
            {
                state.set_focus(Focus::Panel);
            }
            // focus == Input → 不动，让 handle_input_key 的 Tab 补全逻辑接管
        }
        KeyCode::Char('/') => {
            // 输入栏首字符 = `/` 时，用户即将编命令；提前把焦点放到命令面板
            // 让用户接下来按 ↑↓ 立刻能选命令。已经在编辑（input 非空）则不抢，
            // 避免误判 "abc/def" 这种正文里的斜杠。
            if state.input.is_empty() && state.focus != Focus::CommandHint {
                state.set_focus(Focus::CommandHint);
            }
        }
        // V32 · 任意可见字符（非 `/` 起头）→ 焦点归 Input
        // 用户开始打字 = 在编辑输入栏；之前若焦点磁吸到 Panel/CommandHint，回归输入态
        KeyCode::Char(c) if c.is_alphanumeric() || c.is_whitespace() => {
            if state.focus != Focus::Input {
                state.set_focus(Focus::Input);
            }
        }
        _ => {}
    }
}

pub fn handle_global_key(state: &mut AppState, code: KeyCode, mods: KeyModifiers) -> bool {
    // 焦点跟随用户操作 · 每次按键记录时间戳。
    // 给 try_magnet_focus 提供"用户最近是否在操作"的判定依据，2s 抑制窗内
    // agent 消息/trace 事件抵达不会抢焦点。
    state.record_keypress();

    // 双击 Ctrl+C: 退出程序 (间隔 < 1s)
    if code == KeyCode::Char('c') && mods.contains(KeyModifiers::CONTROL) {
        if let Some(last) = state.ctrl_c_last {
            if last.elapsed() < Duration::from_secs(1) {
                info!("用户退出 TUI (双击 Ctrl+C)");
                state.running = false;
                return true;
            }
        }
        state.ctrl_c_last = Some(Instant::now());
        state.add_toast("再按一次 Ctrl+C 退出", Duration::from_secs(2));
        return true;
    }

    // Ctrl+O: 打开/关闭设置
    if code == KeyCode::Char('o') && mods.contains(KeyModifiers::CONTROL) {
        state.show_settings = !state.show_settings;
        if state.show_settings {
            state.add_toast("设置面板已打开 ↑↓选择 Enter修改 Esc关闭", Duration::from_secs(3));
        }
        return true;
    }

    // Ctrl+T: 切换 Thinking/Tools 流式展示（默认隐藏，与 Claude Code Ctrl+O 等效）
    if code == KeyCode::Char('t') && mods.contains(KeyModifiers::CONTROL) {
        state.show_streaming_trace = !state.show_streaming_trace;
        let mode = if state.show_streaming_trace { "显示" } else { "隐藏" };
        state.add_toast(format!("Thinking/Tools 流式展示已{}", mode), Duration::from_secs(2));
        state.rendered_lines_dirty.set(true);
        return true;
    }

    // ── 权限确认弹窗键盘拦截（优先级最高）──
    // 响应类型：
    //   Y=单次允许, A=总是允许, N/Esc=拒绝, D=查看详情（英文 IME 字母快捷）
    //   ↑↓/Tab 切换选中, Enter 触发选中项（中文 IME fallback——字母键被 IME 拦截）
    if state.confirm_dialog.is_some() {
        let ts = chrono::Local::now().format("%H:%M").to_string();

        // V25：方向键/Tab 导航——中文 IME 下字母键被吞时的 fallback
        match code {
            KeyCode::Up | KeyCode::Left => {
                if let Some(ref mut dialog) = state.confirm_dialog {
                    let n = dialog.options.len();
                    if n > 0 { dialog.selected = (dialog.selected + n - 1) % n; }
                }
                return true;
            }
            KeyCode::Down | KeyCode::Right | KeyCode::Tab => {
                if let Some(ref mut dialog) = state.confirm_dialog {
                    let n = dialog.options.len();
                    if n > 0 { dialog.selected = (dialog.selected + 1) % n; }
                }
                return true;
            }
            _ => {}
        }

        match code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                // 单次允许（字母快捷，中文 IME 不可用时走 Enter 路径）
                let dialog = state.confirm_dialog.take().unwrap();
                state.add_event(&ts, "session", &format!("✓ 单次授权: {}", dialog.action), crate::tui::state::EventLevel::Notice);
                state.add_toast("✓ 已授权（本次）", Duration::from_secs(2));
                state.pending_confirmation_response = Some(true);
                return true;
            }
            KeyCode::Enter => {
                // V25：Enter 触发**当前选中**的选项，由 label 文本决定语义
                let (selected_label, is_high) = {
                    let d = state.confirm_dialog.as_ref().unwrap();
                    let lbl = d.options.get(d.selected).map(|o| o.label.clone()).unwrap_or_default();
                    (lbl, d.risk == crate::tui::state::ConfirmRisk::High)
                };
                if selected_label.contains("总是") && is_high {
                    state.add_toast("⚠ 破坏性操作仅支持单次授权", Duration::from_secs(2));
                    return true; // 保留弹窗让用户重新选
                }
                let dialog = state.confirm_dialog.take().unwrap();
                if selected_label.contains("总是") {
                    state.add_event(&ts, "session", &format!("✓ 总是授权: {}", dialog.action), crate::tui::state::EventLevel::Notice);
                    state.add_toast("✓ 已授权（总是允许同类）", Duration::from_secs(3));
                    // V29 (P0): 用 tool_id 而非 action 作 always_allow key, 与 run loop 短路检查一致
                    // V29.11: 系统级持久化 — insert 后立即落盘
                    state.always_allow.insert(dialog.tool_id.clone());
                    let _ = crate::tui::run::save_always_allow(&state.always_allow);
                    state.pending_confirmation_response = Some(true);
                } else if selected_label.contains("拒绝") {
                    state.add_event(&ts, "session", &format!("✗ 已拒绝: {}", dialog.action), crate::tui::state::EventLevel::Warning);
                    state.add_toast("✗ 已拒绝", Duration::from_secs(2));
                    state.pending_confirmation_response = Some(false);
                } else {
                    // 默认 = 单次允许
                    state.add_event(&ts, "session", &format!("✓ 单次授权: {}", dialog.action), crate::tui::state::EventLevel::Notice);
                    state.add_toast("✓ 已授权（本次）", Duration::from_secs(2));
                    state.pending_confirmation_response = Some(true);
                }
                return true;
            }
            KeyCode::Char('a') | KeyCode::Char('A') => {
                // B12: 放开所有风险等级的"总是允许"——用户可对高频安全命令授权
                let dialog = state.confirm_dialog.take().unwrap();
                state.add_event(&ts, "session", &format!("✓ 总是授权: {}", dialog.action), crate::tui::state::EventLevel::Notice);
                state.add_toast("✓ 已授权（总是允许同类操作）", Duration::from_secs(3));
                // V29 (P0): 用 dialog.tool_id 而非 dialog.action — action 含路径粒度太细,
                //   tool_id 才是 run loop `req.tool_id` 短路检查的同 key
                // V29.11: 系统级持久化
                state.always_allow.insert(dialog.tool_id.clone());
                let _ = crate::tui::run::save_always_allow(&state.always_allow);
                state.pending_confirmation_response = Some(true);
                return true;
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                // 拒绝
                let dialog = state.confirm_dialog.take().unwrap();
                state.add_event(&ts, "session", &format!("✗ 已拒绝: {}", dialog.action), crate::tui::state::EventLevel::Warning);
                state.add_toast("✗ 已拒绝", Duration::from_secs(2));
                state.pending_confirmation_response = Some(false);
                return true;
            }
            KeyCode::Char('d') | KeyCode::Char('D') => {
                // B7 修复：D 切换详情展开（折叠/全展开），不再用 toast 重复显示
                // V29 (P1): 用户按 D 主动查看详情 → timer 永久冻结("我在看, 别催")
                //   单向 false→true 不回退, 即使再按 D 收回也不恢复 timer
                //   设计意图: D 是用户介入的明确信号, 比 5s/10s 默认窗口更可靠
                if let Some(ref mut dialog) = state.confirm_dialog {
                    if dialog.details.is_empty() {
                        state.add_toast("无详情", Duration::from_secs(2));
                    } else {
                        dialog.details_expanded = !dialog.details_expanded;
                        if !dialog.interaction_paused {
                            dialog.interaction_paused = true;
                            state.add_toast("⏸ 倒计时已冻结(用户介入)", Duration::from_secs(2));
                        }
                    }
                }
                return true;
            }
            _ => return true, // 弹窗期间拦截所有其他按键
        }
    }

    // K5 完善：Esc 优先级链 — settings > completing > cancel-op > pause-toggle
    // 实际逻辑在 dispatch_esc + apply_esc_action 中统一管理
    if code == KeyCode::Esc {
        let action = dispatch_esc(state);
        apply_esc_action(state, action);
        return true;
    }

    // 设置面板键盘处理
    if state.show_settings {
        match code {
            KeyCode::Up => {
                state.settings_focus = state.settings_focus.saturating_sub(1);
                return true;
            }
            KeyCode::Down => {
                // B4：上限引用 SETTINGS_ITEM_COUNT 常量，避免与 fields 数组漂移
                let max_idx = AppState::SETTINGS_ITEM_COUNT.saturating_sub(1);
                state.settings_focus = (state.settings_focus + 1).min(max_idx);
                return true;
            }
            KeyCode::Enter => {
                match state.settings_focus {
                    0 => {
                        // B3：API Key 设计为只读——TUI 内修改有泄漏风险（屏幕共享、剪贴板）
                        // 提示具体路径让用户去更安全的入口操作
                        state.add_toast(
                            "API Key 只读：编辑 ~/.abacus/config.yaml 或导出 ABACUS_API_KEY",
                            Duration::from_secs(4),
                        );
                    }
                    1 => {
                        // B3：循环切换内置模型（与 /model 一致的热生效）
                        let next = state.cycle_model();
                        state.add_toast(format!("模型 → {}（已生效）", next), Duration::from_secs(2));
                    }
                    2 => {
                        // B1 修复：使用 AppState::cycle_thinking_depth 单一真相
                        let new_depth = state.cycle_thinking_depth().to_string();
                        state.add_toast(format!("思考深度 → {}", new_depth), Duration::from_secs(2));
                    }
                    3 => {
                        // B2+B5 修复：用 theme.name 字段循环遍历全部 12 个主题，
                        // 替代之前对 mode_color magic number 的错误判断
                        let next = crate::tui::theme::Theme::cycle_next(state.theme.name);
                        if state.theme.switch_theme(next) {
                            state.add_toast(format!("主题 → {}", next), Duration::from_secs(2));
                        }
                    }
                    4 => { state.show_settings = false; state.add_toast("设置已关闭", Duration::from_secs(2)); }
                    _ => {}
                }
                return true;
            }
            _ => return true, // 屏蔽其他输入
        }
    }

    // 焦点跟随用户操作（方案 1 · auto_route_focus）
    // 在 Tab/方向键依赖 focus 的逻辑之前，按按键意图前置切焦点，让快捷键不再哑火。
    auto_route_focus(state, code, mods);

    // Tab: 焦点在看板时切换 Tab 页（前进）；其他场景触发输入补全
    // Tab: 焦点在 Panel 时切换滚动区块（时间线 ↔ 知识宫殿）
    if code == KeyCode::Tab && !mods.contains(KeyModifiers::CONTROL) {
        if state.focus == Focus::Panel && state.panel_visible {
            state.panel_scroll_section = state.panel_scroll_section.toggle();
            return true;
        }
        return false; // 交给 handle_input_key 处理补全逻辑
    }

    // Shift+Tab: 同上反向
    if code == KeyCode::BackTab {
        if state.focus == Focus::Panel && state.panel_visible {
            state.panel_scroll_section = state.panel_scroll_section.toggle();
            return true;
        }
        return false;
    }

    // ↑↓: 焦点在 Panel 时滚动当前区块
    if state.focus == Focus::Panel && state.panel_visible {
        use crate::tui::state::PanelSection;
        match code {
            KeyCode::Up => {
                match state.panel_scroll_section {
                    PanelSection::Timeline => {
                        // V30 timeline 边界修复：以 last_timeline_visible 为 max_events 推导上限
                        // max_offset = total.saturating_sub(max_events)
                        let max_off = state.trace_events.len()
                            .saturating_sub(state.last_timeline_visible.get());
                        state.timeline_scroll_offset = (state.timeline_scroll_offset + 1).min(max_off);
                    }
                    PanelSection::Knowledge => {
                        state.knowledge_scroll_offset += 1;
                    }
                }
                state.rendered_lines_dirty.set(true); // 触发完整重绘
                return true;
            }
            KeyCode::Down => {
                match state.panel_scroll_section {
                    PanelSection::Timeline => {
                        state.timeline_scroll_offset = state.timeline_scroll_offset.saturating_sub(1);
                    }
                    PanelSection::Knowledge => {
                        state.knowledge_scroll_offset = state.knowledge_scroll_offset.saturating_sub(1);
                    }
                }
                state.rendered_lines_dirty.set(true); // 触发完整重绘
                return true;
            }
            _ => {}
        }
    }

    // V13: 焦点在 CommandHint 时
    //   ↑↓ 移动选中索引（自动滚动保持可见）
    //   Enter 把选中命令的"主名"填到输入框（去除 alias `[h]` 后缀），用户继续输参数
    if state.focus == Focus::CommandHint && !state.commands.is_empty() {
        let total = state.commands.len();
        match code {
            KeyCode::Up => {
                if state.cmd_selected > 0 {
                    state.cmd_selected -= 1;
                    // 自动滚动：选中超出可见区时上移
                    if state.cmd_selected < state.cmd_scroll * 2 {
                        state.cmd_scroll = state.cmd_selected / 2;
                    }
                }
                state.rendered_lines_dirty.set(true);
                return true;
            }
            KeyCode::Down => {
                if state.cmd_selected + 1 < total {
                    state.cmd_selected += 1;
                    // 简单自动滚动：选中越靠下，cmd_scroll 同步增长
                    let row = state.cmd_selected / 2;
                    if row > state.cmd_scroll + 4 {
                        state.cmd_scroll = row.saturating_sub(4);
                    }
                }
                state.rendered_lines_dirty.set(true);
                return true;
            }
            KeyCode::Enter => {
                // 把选中命令主名填入输入框（去掉 "[alias]" 后缀），尾随空格让用户输参数
                if let Some((display, _)) = state.commands.get(state.cmd_selected) {
                    // "/help [h]" → "/help "; "/theme preview" 保持原样 + 空格
                    let primary = display.split(' ').next().unwrap_or(display);
                    let prefix = if display.contains(" preview") || !display.contains('[') {
                        // 子命令含空格 / 无别名：用完整 display
                        display.clone()
                    } else {
                        primary.to_string()
                    };
                    state.input.clear();
                    state.input.push_str(&prefix);
                    state.input.push(' ');
                    state.cursor_pos = state.input.len();
                    state.recalculate_cursor();
                    // 切回输入焦点让用户立即可输参数
                    state.focus = crate::tui::state::Focus::Panel;
                    state.input_state = crate::tui::state::InputState::Typing;
                    state.add_toast(format!("已填充: {}", prefix), Duration::from_millis(800));
                    state.rendered_lines_dirty.set(true);
                }
                return true;
            }
            _ => {}
        }
    }

    // Ctrl+Tab: AI 按需补全（异步）
    if code == KeyCode::Tab && mods.contains(KeyModifiers::CONTROL) && mods.contains(KeyModifiers::SHIFT) {
        // Ctrl+Shift+Tab: 反向循环补全（在 Completing 状态下由下方 handle_input_key 处理）
        return false;
    }
    if code == KeyCode::Tab && mods.contains(KeyModifiers::CONTROL) {
        // AI 补全：提取当前 token 作为前缀
        let input = &state.input;
        if input.is_empty() {
            state.add_toast("AI 补全需要输入内容", Duration::from_secs(2));
            return true;
        }
        let cursor = state.cursor_pos.min(input.len());
        let before_cursor = &input[..cursor];
        let last_token_start = before_cursor.rfind(|c: char| c.is_whitespace())
            .map(|p| p + 1).unwrap_or(0);
        let token = &before_cursor[last_token_start..];
        if token.is_empty() {
            state.add_toast("AI 补全需要输入内容", Duration::from_secs(2));
            return true;
        }
        state.pending_ai_completion = Some(token.to_string());
        state.add_toast("AI 补全中...", Duration::from_secs(2));
        return true;
    }

    // Ctrl+Space: 文件路径补全（异步）
    if code == KeyCode::Char(' ') && mods.contains(KeyModifiers::CONTROL) {
        let input = &state.input;
        let cursor = state.cursor_pos.min(input.len());
        let before_cursor = &input[..cursor];
        let last_token_start = before_cursor.rfind(|c: char| c.is_whitespace())
            .map(|p| p + 1).unwrap_or(0);
        let token = &before_cursor[last_token_start..];
        if token.is_empty() {
            state.add_toast("文件补全需要输入内容", Duration::from_secs(2));
            return true;
        }
        state.pending_file_completion = Some(token.to_string());
        state.add_toast("文件补全中...", Duration::from_secs(2));
        return true;
    }

    // Ctrl+D: 密度切换（Compact ↔ Comfortable）
    if code == KeyCode::Char('d') && mods.contains(KeyModifiers::CONTROL) {
        state.compact = !state.compact;
        state.add_toast(
            if state.compact { "Compact 模式" } else { "Comfortable 模式" },
            Duration::from_secs(2),
        );
        state.rendered_lines_dirty.set(true);
        return true;
    }

    // Ctrl+E: 代码块折叠/展开切换
    //
    // 引用关系：写入 state.code_blocks_expanded；build_message_lines 读取
    if code == KeyCode::Char('e') && mods.contains(KeyModifiers::CONTROL) {
        state.code_blocks_expanded = !state.code_blocks_expanded;
        state.add_toast(
            if state.code_blocks_expanded { "代码块已展开" } else { "代码块已折叠（超 20 行）" },
            Duration::from_secs(2),
        );
        state.rendered_lines_dirty.set(true);
        return true;
    }

    // Ctrl+I: 面板显隐 toggle（V14：原 Ctrl+P 改 Ctrl+I；需 kitty 键盘协议区分 Tab）
    if code == KeyCode::Char('i') && mods.contains(KeyModifiers::CONTROL) {
        state.panel_visible = !state.panel_visible;
        state.add_toast(
            if state.panel_visible { "面板已显示" } else { "面板已隐藏" },
            Duration::from_secs(1),
        );
        return true;
    }

    // Ctrl+B: 焦点切换（看板 ↔ 命令提示框）
    if code == KeyCode::Char('b') && mods.contains(KeyModifiers::CONTROL) {
        // K2 完善：cycle_focus 内部判定 CommandHint 是否入环（chat + commands 非空）
        state.cycle_focus();
        if state.focus == Focus::Panel && !state.panel_visible {
            state.panel_visible = true;
        }
        return true;
    }

    // Ctrl+N: 新建会话（清空当前会话，保留引擎连接）
    if code == KeyCode::Char('n') && mods.contains(KeyModifiers::CONTROL) {
        reset_session_state(state, "已创建新会话");
        return true;
    }

    // Ctrl+W: 关闭当前会话（E3 修复：复用 reset_session_state 单一真相）
    if code == KeyCode::Char('w') && mods.contains(KeyModifiers::CONTROL) {
        reset_session_state(state, "会话已关闭");
        return true;
    }

    // Ctrl+S: 保存会话（引擎在线时持久化）
    if code == KeyCode::Char('s') && mods.contains(KeyModifiers::CONTROL) {
        handle_save_session(state);
        return true;
    }

    // Ctrl+/: 快捷键速览 toast（短）；完整参考用 /help
    if code == KeyCode::Char('/') && mods.contains(KeyModifiers::CONTROL) {
        state.add_toast(
            "Enter发送 Ctrl+Enter换行 Ctrl+B焦点 Esc取消/暂停 Space折叠块 [/]切Tab · /help 查看完整快捷键",
            Duration::from_secs(5),
        );
        return true;
    }

    // Ctrl+1/2: 模式切换 Clarify/Meeting（V34: Team/Plan 已降级为执行策略，无对应快捷键）
    if mods.contains(KeyModifiers::CONTROL) {
        match code {
            KeyCode::Char('1') => { switch_mode(state, AbacusMode::Clarify); return true; }
            KeyCode::Char('2') => { switch_mode(state, AbacusMode::Meeting); return true; }
            _ => {}
        }
    }

    // [ / ]: 看板 Tab 切换（含自定义 Tab，看板可见时生效）
    if state.panel_visible {
        let custom_count = state.custom_tabs.len();
        match code {
            KeyCode::Char('[') => {
                state.panel_tab = state.panel_tab.prev_with_custom(state.mode, custom_count);
                return true;
            }
            KeyCode::Char(']') => {
                state.panel_tab = state.panel_tab.next_with_custom(state.mode, custom_count);
                return true;
            }
            _ => {}
        }
    }

    false
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// 补全系统 — Tab 触发，三源（命令/历史/文件）
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

// Phase 3 去重：SLASH_COMMANDS 硬编码数组已删除，
// 补全候选从 slash_commands::all_command_names() 动态获取（registry SSoT）

/// 触发补全：分析当前输入，生成候选列表。
/// 返回 true 表示有候选（进入 Completing 状态），false 表示无匹配。
/// 2026-05-28: 补全统一走 inline suggestion（不再弹窗）
///
/// 旧行为：设置 InputState::Completing + 弹窗候选列表
/// 新行为：计算最佳候选 → 写入 state.inline_suggestion → ghost text 渲染
///        用户 Tab 接受 / 继续输入覆盖 / Esc 清除
///
/// 返回 true 表示有候选（Tab 不应插入缩进）
pub fn trigger_completion(state: &mut AppState) -> bool {
    // 重算 inline suggestion（统一入口已涵盖斜杠命令 + 历史）
    let suggestion = state.compute_inline_suggestion();
    if suggestion.is_some() {
        state.inline_suggestion = suggestion;
        true
    } else {
        state.inline_suggestion = None;
        false
    }
}

/// 接受当前选中的补全候选。
///
/// 旗杆命令逻辑：
/// - 候选以 '/' 开头 且 completion_prefix 为空（根级命令，不是路径补全的参数部分）
///   → 仅 1 个候选时直接调 submit_message（一次 Enter 即执行），
///     多候选时只填充输入框，用户再按 Enter 确认（V32-2：防误选）
/// - 其他候选（文件路径、带参数的命令）→ 仅填充输入框，用户继续编辑
fn accept_completion(state: &mut AppState) {
    if state.completion_index >= state.completion_candidates.len() {
        cancel_completion(state);
        return;
    }
    let chosen = state.completion_candidates[state.completion_index].clone();
    // L3-17: 仅斜杠命令后加空格，文件路径不加
    let suffix = if chosen.starts_with('/') { " " } else { "" };
    // V32-2: 在 cancel_completion 清空列表前捕获候选数
    let unambiguous = state.completion_candidates.len() == 1;
    let is_slash_root_cmd = chosen.starts_with('/') && state.completion_prefix.is_empty();
    state.input = format!("{}{}{}", state.completion_prefix, chosen, suffix);
    state.cursor_pos = state.input.len();
    state.recalculate_cursor();
    cancel_completion(state);
    state.input_state = InputState::Typing;
    // V32-2: 仅单一候选时自动提交，避免多个候选时误选（如 /mode 选中 /model）
    if is_slash_root_cmd && unambiguous {
        submit_message(state);
    }
}
/// 取消补全，清除候选列表。
fn cancel_completion(state: &mut AppState) {
    state.completion_candidates.clear();
    state.completion_index = usize::MAX;
    state.completion_prefix.clear();
    state.inline_suggestion = None;
    if state.input_state == InputState::Completing {
        state.input_state = InputState::Typing;
    }
}

/// 历史导航：上箭头拉取更早的历史输入。
fn navigate_history_up(state: &mut AppState) {
    if state.input_history.is_empty() { return; }
    let idx = state.history_index.map(|i| i.saturating_add(1)).unwrap_or(0);
    if idx >= state.input_history.len() { return; }
    state.history_index = Some(idx);
    state.input = state.input_history[state.input_history.len() - 1 - idx].clone();
    state.cursor_pos = state.input.len();
    state.recalculate_cursor();
}

/// 历史导航：下箭头回到更新的历史。
fn navigate_history_down(state: &mut AppState) {
    match state.history_index {
        None | Some(0) => {
            state.history_index = None;
            state.input.clear();
            state.cursor_pos = 0;
            state.cursor_line = 0;
            state.cursor_col = 0;
        }
        Some(idx) => {
            let new_idx = idx - 1;
            state.history_index = Some(new_idx);
            state.input = state.input_history[state.input_history.len() - 1 - new_idx].clone();
            state.cursor_pos = state.input.len();
            state.recalculate_cursor();
        }
    }
}

/// 将当前输入加入历史（提交时调用）。
pub fn record_input_history(state: &mut AppState, text: &str) {
    let trimmed = text.trim();
    if trimmed.is_empty() { return; }
    // 去重：已存在的相同条目移到末尾
    state.input_history.retain(|h| h != trimmed);
    state.input_history.push(trimmed.to_string());
    // 上限 100 条
    if state.input_history.len() > 100 {
        state.input_history.drain(0..state.input_history.len() - 100);
    }
    state.history_index = None;
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// 模式无关的通用按键处理
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// 处理键盘输入 (全局快捷键之后)
pub fn handle_input_key(state: &mut AppState, code: KeyCode, mods: KeyModifiers) {
    let is_busy = matches!(state.input_state, InputState::Thinking | InputState::Executing | InputState::Outputting);

    // V13: Picker 打开时拦截输入键（↑↓ 选择 / Enter 应用 / Esc 由 dispatch_esc 处理）
    //   Tab/Char 等也消化掉，避免 picker 打开时还能往输入框继续打字
    // V29.8: Model picker 加 ←→ 调 thinking 深度 (off↔low↔medium↔high)
    if state.picker.is_some() {
        match code {
            KeyCode::Up => {
                if let Some(p) = state.picker.as_mut() {
                    if p.selected > 0 { p.selected -= 1; }
                }
                state.rendered_lines_dirty.set(true);
                return;
            }
            KeyCode::Down => {
                if let Some(p) = state.picker.as_mut() {
                    if p.selected + 1 < p.items.len() { p.selected += 1; }
                }
                state.rendered_lines_dirty.set(true);
                return;
            }
            // V29.8: ←→ 调 thinking 深度 (仅 show_thinking_slider 时拦截)
            //   若 picker 不显示 slider, ←→ 直接吞掉(不影响输入框光标 — picker 期间输入被冻结)
            KeyCode::Left => {
                if let Some(p) = state.picker.as_ref() {
                    if p.show_thinking_slider {
                        // V29.10: 引用 AppState::THINKING_SLIDER_DEPTHS 单一真相
                        let depths = crate::tui::state::AppState::THINKING_SLIDER_DEPTHS;
                        let cur = depths.iter().position(|d| *d == state.thinking_depth).unwrap_or(3);
                        if cur > 0 {
                            state.thinking_depth = depths[cur - 1].to_string();
                        }
                    }
                }
                state.rendered_lines_dirty.set(true);
                return;
            }
            KeyCode::Right => {
                if let Some(p) = state.picker.as_ref() {
                    if p.show_thinking_slider {
                        // V29.10: 引用 AppState::THINKING_SLIDER_DEPTHS 单一真相
                        let depths = crate::tui::state::AppState::THINKING_SLIDER_DEPTHS;
                        let cur = depths.iter().position(|d| *d == state.thinking_depth).unwrap_or(3);
                        if cur + 1 < depths.len() {
                            state.thinking_depth = depths[cur + 1].to_string();
                        }
                    }
                }
                state.rendered_lines_dirty.set(true);
                return;
            }
            KeyCode::Char(' ') => {
                // Review picker：Space 切换 strict 模式
                if let Some(p) = state.picker.as_mut() {
                    if matches!(p.kind, crate::tui::state::PickerKind::Review) {
                        p.review_strict = !p.review_strict;
                        state.rendered_lines_dirty.set(true);
                    }
                }
                return;
            }
            KeyCode::Enter => {
                // 防键重复：picker 打开 150ms 内 Enter 无效
                let debounce_ok = state.picker.as_ref()
                    .map(|p| p.opened_at.elapsed().as_millis() >= 150)
                    .unwrap_or(true);
                if debounce_ok {
                    apply_picker_selection(state);
                }
                return;
            }
            // Tab 循环（与 ↓ 等价，方便单手操作）
            KeyCode::Tab => {
                if let Some(p) = state.picker.as_mut() {
                    p.selected = (p.selected + 1) % p.items.len().max(1);
                }
                state.rendered_lines_dirty.set(true);
                return;
            }
            // 其它键：吞掉，让用户专注 picker（Esc 由全局 dispatch_esc 处理）
            _ => return,
        }
    }

    // ── 2026-05-28: 全屏编辑器键盘拦截 ──
    // 引用关系：InputState::Editor 时接管所有输入
    // 生命周期：open_editor() 激活 → Ctrl+S/Esc 退出
    if state.input_state == InputState::Editor {
        handle_editor_key(state, code, mods);
        return;
    }

    // 忙碌态：允许打字（字符输入到 buffer），但禁止 Enter 发送
    // Paused 态仍完全阻塞
    if state.input_state == InputState::Paused {
        return;
    }

    // 忙碌态下 Enter → mid-turn signal 注入（实时通知 LLM）
    //
    // ## 设计变更（mid-turn user signal）
    // 旧行为：排队到 pending_inputs，等当前 turn 完成后作为新 turn 发送。
    // 新行为：写入 session.mid_turn_signals，pipeline 下次迭代时 drain 并注入为
    //   `[User update]` 格式 user message，LLM 可实时感知并自主决策是否调整。
    //
    // ## 引用关系
    // - 写入：此处通过 EngineHandle.session push
    // - 消费：abacus-core pipeline execute_loop 迭代间隙 drain
    // 忙碌态下斜杠命令仍本地执行（/clear 等不需要引擎参与，不应被 mid-turn 拦截）
    if is_busy && code == KeyCode::Enter && state.input.trim().starts_with('/') {
        submit_message(state);
        return;
    }

    // 授权弹窗激活时：支持输入框输入语义词确认/拒绝
    // 语义词识别优先于按键快捷键，支持 IME 环境
    if is_busy && code == KeyCode::Enter && state.confirm_dialog.is_some() {
        let text = state.input.trim().to_lowercase();
        if !text.is_empty() {
            let is_yes = matches!(
                text.as_str(),
                "y" | "yes" | "ok" | "allow" | "是" | "好" | "好的" | "允许" | "确认" | "同意" | "授权" | "肯定"
            );
            let is_no = matches!(
                text.as_str(),
                "n" | "no" | "deny" | "reject" | "否" | "不" | "拒绝" | "取消" | "不允许" | "不同意"
            );
            if is_yes || is_no {
                let dialog = state.confirm_dialog.take().unwrap();
                let ts = chrono::Local::now().format("%H:%M").to_string();
                state.input.clear();
                state.cursor_pos = 0;
                state.cursor_line = 0;
                state.cursor_col = 0;
                if is_yes {
                    state.add_event(&ts, "session", &format!("✓ 单次授权: {}", dialog.action), crate::tui::state::EventLevel::Notice);
                    state.add_toast("✓ 已授权（本次）", std::time::Duration::from_secs(2));
                    state.pending_confirmation_response = Some(true);
                } else {
                    state.add_event(&ts, "session", &format!("✗ 已拒绝: {}", dialog.action), crate::tui::state::EventLevel::Warning);
                    state.add_toast("✗ 已拒绝", std::time::Duration::from_secs(2));
                    state.pending_confirmation_response = Some(false);
                }
                return;
            }
        }
    }

    if is_busy && code == KeyCode::Enter {
        if !state.input.trim().is_empty() {
            let msg = state.input.trim().to_string();

            if state.input_state == InputState::Executing {
                // ── 压缩阶段：暂存消息，等压缩完成后自动发送 ──
                // 不走 mid-turn signal（压缩期间 turn 已结束，无 LLM 在接收）
                state.pending_compress_input = Some(msg);
                state.input.clear();
                state.cursor_pos = 0;
                state.cursor_line = 0;
                state.cursor_col = 0;
                state.add_toast("⏳ 已暂存，压缩完成后自动发送", Duration::from_secs(3));
            } else {
                // ── Thinking/Outputting：注入 mid-turn signal 让 LLM 实时感知 ──
                // 2026-05-27: 同时写入 TUI state.messages 让消息面板显示用户消息
                state.add_message(crate::tui::state::Message::new_user(
                    msg.clone(),
                    chrono::Local::now().format("%H:%M").to_string(),
                ));
                if let Some(ref handle) = state.engine_handle {
                    let session = handle.session.clone();
                    let msg_clone = msg.clone();
                    tokio::spawn(async move {
                        let s = session.read().await;
                        s.mid_turn_signals.lock().await.push(msg_clone);
                    });
                }
                state.input.clear();
                // EV7 修复：input.clear 后必须同步重置 cursor 三件套
                state.cursor_pos = 0;
                state.cursor_line = 0;
                state.cursor_col = 0;
                state.add_toast("已发送给 AI（工作中可感知）", Duration::from_secs(2));
            }
        }
        return;
    }

    // ── 补全状态下的按键 ──────────────────────────────────────
    if state.input_state == InputState::Completing {
        match code {
            KeyCode::Tab => {
                // Tab 循环选中下一候选
                if !state.completion_candidates.is_empty() {
                    state.completion_index = (state.completion_index + 1) % state.completion_candidates.len();
                }
                return;
            }
            KeyCode::BackTab => {
                // E5 修复：Shift+Tab 循环选中上一候选
                // 之前：wrapping_sub(1) % len 在 index=0 时退化为 usize::MAX % len，
                // 结果取决于 len（len=5 时 → 0 卡住；len=4 时 → 3）—— 不可预测
                // 用与 Up 键一致的显式 wrap-around 逻辑
                if !state.completion_candidates.is_empty() {
                    state.completion_index = if state.completion_index == 0 {
                        state.completion_candidates.len() - 1
                    } else {
                        state.completion_index - 1
                    };
                }
                return;
            }
            KeyCode::Up => {
                if !state.completion_candidates.is_empty() {
                    state.completion_index = if state.completion_index == 0 {
                        state.completion_candidates.len() - 1
                    } else {
                        state.completion_index - 1
                    };
                }
                return;
            }
            KeyCode::Down => {
                if !state.completion_candidates.is_empty() {
                    state.completion_index = (state.completion_index + 1) % state.completion_candidates.len();
                }
                return;
            }
            // L4-19: PageUp/PageDown 快速跳转补全候选
            KeyCode::PageUp => {
                state.completion_index = state.completion_index.saturating_sub(5);
                return;
            }
            KeyCode::PageDown => {
                let max = state.completion_candidates.len().saturating_sub(1);
                state.completion_index = (state.completion_index + 5).min(max);
                return;
            }
            KeyCode::Enter => {
                accept_completion(state);
                return;
            }
            KeyCode::Esc => {
                cancel_completion(state);
                return;
            }
            // V32 · Alt+1..9 直选：用户视觉上看到候选 1..N，按 Alt+数字直接确认
            // 比 ↓↓↓ Enter 快得多；Alt 修饰键避免与正常字符输入冲突
            KeyCode::Char(d) if mods.contains(KeyModifiers::ALT) && d.is_ascii_digit() && d != '0' => {
                let target = (d as u8 - b'1') as usize; // '1' → 0, '9' → 8
                if target < state.completion_candidates.len() {
                    state.completion_index = target;
                    accept_completion(state);
                }
                return;
            }
            // 字符键：插入后重算候选，不取消补全
            KeyCode::Char(c) => {
                let char_len = c.len_utf8();
                state.input.insert(state.cursor_pos, c);
                state.cursor_pos += char_len;
                if c == '\n' {
                    state.recalculate_cursor();
                } else {
                    state.cursor_col += unicode_width::UnicodeWidthChar::width(c).unwrap_or(1);
                }
                trigger_completion(state);
                return;
            }
            KeyCode::Backspace => {
                if state.cursor_pos > 0 {
                    if let Some((idx, _)) = state.input[..state.cursor_pos].char_indices().next_back() {
                        state.input.remove(idx);
                        state.cursor_pos = idx;
                    }
                    state.recalculate_cursor();
                }
                // 更新内联补全候选
                state.inline_suggestion = if state.input.is_empty() {
                    None
                } else {
                    state.compute_inline_suggestion()
                };
                if state.input.is_empty() {
                    cancel_completion(state);
                } else {
                    trigger_completion(state);
                }
                return;
            }
            // 其他键：取消补全，继续处理
            _ => {
                cancel_completion(state);
            }
        }
    }

    // 2026-05-28: Ctrl+E → 打开全屏编辑器
    if code == KeyCode::Char('e') && mods.contains(KeyModifiers::CONTROL) {
        state.open_editor();
        return;
    }

    // Enter: 发送消息  |  Shift+Enter / Ctrl+Enter: 换行
    if code == KeyCode::Enter {
        if mods.contains(KeyModifiers::SHIFT) || mods.contains(KeyModifiers::CONTROL) {
            state.input.insert(state.cursor_pos, '\n');
            state.cursor_pos += 1;
            state.recalculate_cursor();
        } else {
            submit_message(state);
        }
        return;
    }

    // Tab: 触发补全 — 优先接受内联建议（不阻塞输入），否则走原有的补全弹窗
    // 2026-05-28: Tab = inline 补全接受 + 连续 Tab 循环候选（fish shell 模式）
    //
    // 流程:
    //   首次 Tab: 接受 inline_suggestion → 填入 input
    //   连续 Tab: 从 inline_candidates 取下一个覆盖 input
    //   无候选时: 插入缩进
    //
    // 任何非 Tab 输入会在字符处理路径清空 inline_candidates（见 Char 分支）
    if code == KeyCode::Tab && !mods.contains(KeyModifiers::CONTROL) {
        // 路径 A: 已有 inline_candidates（连续 Tab 循环）
        if !state.inline_candidates.is_empty() {
            state.inline_candidate_idx = (state.inline_candidate_idx + 1) % state.inline_candidates.len();
            let next = state.inline_candidates[state.inline_candidate_idx].clone();
            state.input = next;
            state.cursor_pos = state.input.len();
            state.recalculate_cursor();
            state.inline_suggestion = None; // 循环中不显示 ghost text
            state.input_state = InputState::Typing;
            return;
        }

        // 路径 B: 首次 Tab — 接受 inline_suggestion 并构建候选列表
        if let Some(suggestion) = state.inline_suggestion.take() {
            let current_input = state.input.trim().to_string();
            if suggestion.len() > current_input.len() {
                // 构建全部候选列表（供连续 Tab 循环）
                let candidates = state.compute_all_inline_candidates();
                state.input = suggestion.clone();
                state.cursor_pos = state.input.len();
                state.recalculate_cursor();
                state.input_state = InputState::Typing;
                // 设置循环列表（包含当前已接受的作为第 0 项）
                if candidates.len() > 1 {
                    state.inline_candidates = candidates;
                    state.inline_candidate_idx = 0;
                }
                return;
            }
        }

        // 路径 C: 尝试触发补全（可能产生新 inline_suggestion）
        if trigger_completion(state) { return; }

        // 路径 D: 无任何候选 → Tab 插入缩进
        state.input.insert(state.cursor_pos, '\t');
        state.cursor_pos += 1;
        state.input_state = InputState::Typing;
        return;
    }

    // Up: 多行模式行间移光标 / 单行 history / 多行首行 no-op (V25 保护)
    // V25: 多行首行 Up 不再触发 navigate_history_up——避免误把整个多行 input 替换为历史命令
    //      单行场景下 Up = history 仍然合理(用户习惯)
    //      引用关系: navigate_history_up 会改 state.input,多行首行触发 = 整段被替换 = 数据丢失风险
    if code == KeyCode::Up {
        let has_newline = state.input.contains('\n');
        let on_first_line = state.cursor_line == 0;
        if has_newline && !on_first_line {
            // 多行非首行: 移光标到上一行同列
            // EV10 修复：按 char index 计算同列位置（不按 byte），
            // 避免 CJK 行 + ASCII 行混排时新 cursor_pos 落在多字节 char 中间
            // 触发 recalculate_cursor 的 slice panic
            let before = &state.input[..state.cursor_pos];
            let current_line_start = before.rfind('\n').map(|i| i + 1).unwrap_or(0);
            let col_chars = state.input[current_line_start..state.cursor_pos].chars().count();
            let prev_line_end = current_line_start.saturating_sub(1); // '\n' position
            let prev_before = &state.input[..prev_line_end];
            let prev_line_start = prev_before.rfind('\n').map(|i| i + 1).unwrap_or(0);
            let prev_line = &state.input[prev_line_start..prev_line_end];
            // 找到 char index col_chars 对应的 byte offset；超出则停在行末
            let target_byte = prev_line.char_indices()
                .nth(col_chars)
                .map(|(b, _)| b)
                .unwrap_or(prev_line.len());
            state.cursor_pos = prev_line_start + target_byte;
            state.recalculate_cursor();
        } else if !has_newline {
            // 单行: history navigate (用户习惯)
            navigate_history_up(state);
        }
        // V25: else 多行首行 → no-op,保护多行内容不被 history 替换
        if state.input.is_empty() {
            state.input_state = InputState::Ready;
        } else {
            state.input_state = InputState::Typing;
        }
        return;
    }

    // Down: 多行模式行间移光标 / 单行 history / 多行末行 no-op (V25 保护,与 Up 对称)
    if code == KeyCode::Down {
        let has_newline = state.input.contains('\n');
        let total_lines = state.input.matches('\n').count();
        let on_last_line = state.cursor_line >= total_lines;
        if has_newline && !on_last_line {
            // 多行非末行: 移光标到下一行同列
            // EV10 修复：与 Up 对称——按 char index 同列移动避免落入 char 中间字节
            let before = &state.input[..state.cursor_pos];
            let current_line_start = before.rfind('\n').map(|i| i + 1).unwrap_or(0);
            let col_chars = state.input[current_line_start..state.cursor_pos].chars().count();
            let after_cursor = &state.input[state.cursor_pos..];
            if let Some(next_newline) = after_cursor.find('\n') {
                let next_line_start = state.cursor_pos + next_newline + 1;
                let next_after = &state.input[next_line_start..];
                let next_line_end_in_after = next_after.find('\n').unwrap_or(next_after.len());
                let next_line = &next_after[..next_line_end_in_after];
                let target_byte = next_line.char_indices()
                    .nth(col_chars)
                    .map(|(b, _)| b)
                    .unwrap_or(next_line.len());
                state.cursor_pos = next_line_start + target_byte;
            }
            state.recalculate_cursor();
        } else if !has_newline {
            // 单行: history navigate
            navigate_history_down(state);
        }
        // V25: else 多行末行 → no-op,保护多行内容不被 history 替换
        if state.input.is_empty() {
            state.input_state = InputState::Ready;
        } else {
            state.input_state = InputState::Typing;
        }
        return;
    }

    // Backspace — 使用 char boundary 安全删除
    if code == KeyCode::Backspace {
        if state.cursor_pos > 0 {
            if let Some((idx, _)) = state.input[..state.cursor_pos].char_indices().next_back() {
                state.input.remove(idx);
                state.cursor_pos = idx;
            }
            state.recalculate_cursor();
        } else if !state.input.is_empty() {
            // cursor_pos == 0 但 input 非空：cursor 与 input 不同步（防御性修复）
            // 强制删除第一个字符
            if state.input.len() > 0 {
                let first_char_len = state.input.chars().next().map(|c| c.len_utf8()).unwrap_or(0);
                if first_char_len > 0 {
                    state.input.drain(..first_char_len);
                    state.cursor_pos = 0;
                    state.recalculate_cursor();
                }
            }
        }
        // 更新内联补全候选
        state.inline_suggestion = if state.input.is_empty() {
            None
        } else {
            state.compute_inline_suggestion()
        };
        if state.input.is_empty() {
            state.input_state = InputState::Ready;
        }
        state.rendered_lines_dirty.set(true);
        return;
    }

    // Left 光标移动（字符边界安全 + unicode-width 计算 display column）
    if code == KeyCode::Left {
        if state.cursor_pos > 0 {
            if let Some((idx, ch)) = state.input[..state.cursor_pos].char_indices().next_back() {
                state.cursor_pos = idx;
                if ch == '\n' {
                    state.recalculate_cursor();
                } else {
                    state.cursor_col = state.cursor_col.saturating_sub(unicode_width::UnicodeWidthChar::width(ch).unwrap_or(1));
                }
            }
        }
        return;
    }
    // Right 光标移动（unicode-width 计算 display column）
    if code == KeyCode::Right {
        if state.cursor_pos < state.input.len() {
            // 光标不在末尾：正常右移一字符
            let rest = &state.input[state.cursor_pos..];
            if let Some(ch) = rest.chars().next() {
                state.cursor_pos += ch.len_utf8();
                if ch == '\n' {
                    state.recalculate_cursor();
                } else {
                    state.cursor_col += unicode_width::UnicodeWidthChar::width(ch).unwrap_or(1);
                }
            }
        } else if let Some(suggestion) = state.inline_suggestion.take() {
            // 2026-05-28: 光标在末尾 + 有 inline suggestion → 采纳（fish shell 风格）
            state.input = suggestion;
            state.cursor_pos = state.input.len();
            state.recalculate_cursor();
            state.inline_candidates.clear();
            state.rendered_lines_dirty.set(true);
        }
        return;
    }

    // 字符输入 — 使用 char boundary 更新 cursor_pos + unicode-width
    if let KeyCode::Char(c) = code {
        let char_len = c.len_utf8();
        state.input.insert(state.cursor_pos, c);
        state.cursor_pos += char_len;
        state.input_state = InputState::Typing;
        if c == '\n' {
            state.recalculate_cursor();
        } else {
            state.cursor_col += unicode_width::UnicodeWidthChar::width(c).unwrap_or(1);
        }

        // 更新内联补全候选（不阻塞输入）+ 清空 Tab 循环状态
        state.inline_suggestion = state.compute_inline_suggestion();
        state.inline_candidates.clear();
        state.inline_candidate_idx = 0;

        // V32 · Slash 自动补全：用户在输入栏首位敲 `/` 起头的命令时自动弹候选
        //
        // 特殊路径：Picker 命令直接执行，跳过补全弹窗中间步骤
        // - /model / /m → 直接打开模型选择器（用户不需要再按一次 Enter）
        // - /theme       → 直接打开主题选择器
        // - /thinking /think → 直接打开思考深度选择器
        // 以上命令输入完成后即可开起 picker，无需经过补全候选列表确认。
        if state.input.starts_with('/') && state.cursor_pos == state.input.len() {
            let trimmed = state.input.trim().to_string(); // to_string() 释放借用，避免后续可变借用冲突
            let is_direct_picker = matches!(trimmed.as_str(),
                "/model" | "/m" | "/mode" | "/theme" | "/thinking" | "/think"
            );
            if is_direct_picker {
                if handle_slash_command(state, &trimmed) {
                    state.input.clear();
                    state.cursor_pos = 0;
                    state.cursor_line = 0;
                    state.cursor_col = 0;
                    state.input_state = InputState::Ready;
                }
                return;
            }
            let _ = trigger_completion(state);
        }
    }
}

/// 处理聚焦消息区时的滚动按键
/// 消息区行级滚动
///
/// scroll 语义：从底部向上偏移的行数（0 = 最底部，自动跟随）
/// Up/Down: 每次 3 行，PageUp/PageDown: 半屏（最少 5 行），Home/End: 回到底部
///
/// V29.5: 上限 clamp 到 last_total_lines - last_visible_h, 不再越过顶部进入空白区
/// V29.16: 写入收敛到 set_scroll(ScrollAction) 单一入口, clamp/dirty 内部统一处理
pub fn handle_chat_scroll_key(state: &mut AppState, code: KeyCode) {
    // V29.5: PageUp/PageDown 改半屏(最少 5 行), 大屏长步小屏短步, 一致体感
    // V29.16: clamp/max_scroll 计算下沉到 AppState::set_scroll, 此处只算 page_step
    let page_step = (state.last_visible_h.get() / 2).max(5);
    match code {
        KeyCode::Up => state.set_scroll(ScrollAction::Up(3)),
        KeyCode::Down => state.set_scroll(ScrollAction::Down(3)),
        KeyCode::PageUp => state.set_scroll(ScrollAction::Up(page_step)),
        KeyCode::PageDown => state.set_scroll(ScrollAction::Down(page_step)),
        KeyCode::Home | KeyCode::End => state.set_scroll(ScrollAction::ToBottom),
        KeyCode::Char(' ') => {
            // V12: Space 升级为"切换最后一条消息的所有 blocks"
            //   智能态：扫描所有块，任一已展开 → 全部折叠；全部折叠 → 全部展开
            //   设计意图：思考链通常是 Think + N ToolCall，逐个切换费时
            //
            // V29.11 (B4): 折叠锚定 — 用户在浏览历史时(scroll>0), 切换最后一条
            //   blocks 折叠会改变总行数; 不调整 scroll 会导致视野上方的 anchor msg
            //   被推出屏幕。算法:
            //     before_rows = estimate(last_msg, content_w)
            //     toggle blocks
            //     after_rows = estimate(last_msg, content_w)
            //     scroll += (after - before) — 让上方 anchor 视觉位置不动
            //   scroll==0 时不锚定 — 用户在 auto-follow 底部, 期望"看到最新内容"
            //   而非"卡在原行号"
            if !state.messages.is_empty() {
                let msg_idx = state.messages.len() - 1;
                let content_w = {
                    let w = state.last_content_width.get();
                    if w == 0 { 80 } else { w }  // V29.11: 首帧前 fallback 80
                };
                let should_anchor = state.scroll > 0;
                let before_rows = if should_anchor {
                    state.messages.get(msg_idx)
                        .map(|m| crate::tui::components::estimate_msg_rows(m, content_w))
                        .unwrap_or(0)
                } else { 0 };

                if let Some(msg) = state.messages.get_mut(msg_idx) {
                    // 先判断目标状态
                    let any_expanded = msg.parts.iter().any(|p| matches!(p,
                        crate::tui::state::MsgContent::Block { collapsed: false, .. }));
                    let target_collapsed = any_expanded; // 有任一展开 → 全部折叠
                    for part in &mut msg.parts {
                        if let crate::tui::state::MsgContent::Block { collapsed, .. } = part {
                            *collapsed = target_collapsed;
                        }
                    }

                    if should_anchor {
                        // V29.11: 切换后估算 + 调整 scroll, 保持上方 anchor 不动
                        // V29.16: 经 SSOT set_scroll(AnchorAdjust) — 内部判方向 + 标 dirty
                        let after_rows = crate::tui::components::estimate_msg_rows(msg, content_w);
                        state.set_scroll(ScrollAction::AnchorAdjust { after_rows, before_rows });
                    } else {
                        // 不锚定时仅折叠状态变了, 仍需重渲染
                        state.rendered_lines_dirty.set(true);
                    }
                }
            }
        }
        _ => {}
    }
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// 鼠标事件处理
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

// EV4: 删除陈旧的 #[allow(dead_code)]——run.rs:801 已实际调用，allow 是历史遗留
// （之前函数确为 dead 时加上的标注，迁移后未清理）。删除让 lint 重新生效，
// 将来真死代码可被 rustc 立刻提示
pub fn handle_mouse(state: &mut AppState, event: MouseEvent, terminal_cols: u16, terminal_rows: u16) {
    // E11：输入框高度跟随终端自适应，与渲染层一致
    let input_h = crate::tui::layout::chat_input_height(terminal_rows);
    match event.kind {
        MouseEventKind::ScrollUp => {
                // V29.7: 滚动优先级 — 消息区 > 看板 timeline > (输入框命令面板用键盘 ↑↓)
                //   旧设计 3 分支: in_panel / in_input / else, in_input 抢走输入区上方的滚轮事件
                //   新设计 2 分支: 仅"明确在 panel 列内"才滚 panel, 其它(含输入区)默认滚消息区
                //   命令面板滚动改走键盘 ↑↓ (handle_completing_key 路径), 鼠标不再触发
                //   设计意图: 用户主要交互对象是消息历史, 鼠标位置不应过度精确才能滚消息
                let pw = if state.panel_visible { terminal_cols * crate::tui::layout::panel_pct_for_width(terminal_cols) / 100 } else { 0 };
                let in_panel = state.panel_visible && event.column >= terminal_cols.saturating_sub(pw);
                let _ = input_h; // 仍保留计算便于未来需要时启用 in_input 分支

                if in_panel {
                    // 看板 timeline / knowledge: 鼠标显式在 panel 列内
                    use crate::tui::state::PanelSection;
                    match state.panel_scroll_section {
                        PanelSection::Timeline => {
                            // V30 timeline 边界修复：同键盘 ↑ 以 last_timeline_visible 推导上限
                            let max_off = state.trace_events.len()
                                .saturating_sub(state.last_timeline_visible.get());
                            state.timeline_scroll_offset = (state.timeline_scroll_offset + 2).min(max_off);
                        }
                        PanelSection::Knowledge => state.knowledge_scroll_offset += 2,
                    }
                    state.rendered_lines_dirty.set(true);
                } else {
                    // 默认消息区(含输入区上方区域): 向上浏览历史
                    // V29.5 (B1): 与键盘路径同 clamp 策略, 避免越过顶部
                    // V29.16: 走 SSOT — clamp/dirty 内部统一
                    state.set_scroll(ScrollAction::Up(3));
                }
            }
        MouseEventKind::ScrollDown => {
                // V29.7: 与 ScrollUp 同优先级 — 消息区 > timeline > 命令面板(键盘)
                let pw = if state.panel_visible { terminal_cols * crate::tui::layout::panel_pct_for_width(terminal_cols) / 100 } else { 0 };
                let in_panel = state.panel_visible && event.column >= terminal_cols.saturating_sub(pw);
                let _ = input_h;

                if in_panel {
                    use crate::tui::state::PanelSection;
                    match state.panel_scroll_section {
                        PanelSection::Timeline => state.timeline_scroll_offset = state.timeline_scroll_offset.saturating_sub(2),
                        PanelSection::Knowledge => state.knowledge_scroll_offset = state.knowledge_scroll_offset.saturating_sub(2),
                    }
                    state.rendered_lines_dirty.set(true);
                } else {
                    // V29.16: 走 SSOT — clamp/dirty 内部统一
                    state.set_scroll(ScrollAction::Down(3));
                }
            }
        MouseEventKind::Down(MouseButton::Left) => {
            // 消息区实际宽度（考虑面板）
            let pw = if state.panel_visible { terminal_cols * crate::tui::layout::panel_pct_for_width(terminal_cols) / 100 } else { 0 };
            let chat_cols = terminal_cols.saturating_sub(pw).max(40);

            // V30 复制修复：Shift+Click 开始文本选择。char_idx 现按 unicode 列反查写入。
            // 引用关系：screen_pos_to_msg_char 定义于 components/mod.rs。
            // 生命周期：selection 状态清除走 Up 分支（复制后 take()）或 Esc 取消
            if event.modifiers.contains(KeyModifiers::SHIFT) {
                let cached_rows = state.cached_msg_rows.borrow();
                if let Some((msg_idx, char_idx)) = crate::tui::components::screen_pos_to_msg_char(
                    event.row, event.column, terminal_rows, state.scroll, &state.messages, chat_cols,
                    cached_rows.as_slice(),
                ) {
                    state.text_selection = Some(TextSelection {
                        start_msg_idx: msg_idx,
                        start_char_idx: char_idx,
                        end_msg_idx: msg_idx,
                        end_char_idx: char_idx,
                    });
                    state.rendered_lines_dirty.set(true);
                }
                return;
            }

            // V25: 区域 hit-test —— 按 (panel 列 / input 行) 二维定位
            // 入参 input_h 来自函数顶部 chat_input_height,与渲染层一致
            // E10：terminal_cols < pw 时 underflow panic — 用 saturating_sub 防御
            let input_top = terminal_rows.saturating_sub(input_h + 1);
            let in_input_row = event.row >= input_top
                && event.row < terminal_rows.saturating_sub(1);
            let in_panel_col = state.panel_visible
                && event.column >= terminal_cols.saturating_sub(pw);

            if in_panel_col {
                // V25: 右侧栏 — 上半为 Panel,下半(input 行)为 CommandHint
                if in_input_row {
                    state.focus = Focus::CommandHint;
                } else {
                    state.focus = Focus::Panel;
                }
                state.note_focus_change();

                // V32 · 点击 shortcuts hints 行直接填充命令到输入框
                // 在 panel_col + input_row 区域（即 shortcuts_hints area），按 cmd_row_map 反查
                // 起始 cmd_idx，并按 column 决定双列布局下点的是左还是右列。
                if in_input_row {
                    let cmd_row_map = state.cmd_row_map.borrow();
                    if let Some((_, base_idx)) = cmd_row_map.iter().find(|(y, _)| *y == event.row) {
                        let base = *base_idx;
                        drop(cmd_row_map);
                        // 双列判定：shortcuts area 起点 + 中线
                        let shortcuts_x_start = terminal_cols.saturating_sub(pw);
                        let half_w = pw / 2;
                        let in_right_col = pw >= 22 && event.column >= shortcuts_x_start + half_w;
                        let target_idx = if in_right_col { base + 1 } else { base };
                        if let Some((display, _)) = state.commands.get(target_idx) {
                            // 复用 Enter 选中后的填入逻辑：拆 alias 后缀 → 填入 input + 尾随空格
                            let primary = display.split(' ').next().unwrap_or(display);
                            let prefix = if display.contains(" preview") || !display.contains('[') {
                                display.clone()
                            } else {
                                primary.to_string()
                            };
                            state.input.clear();
                            state.input.push_str(&prefix);
                            state.input.push(' ');
                            state.cursor_pos = state.input.len();
                            state.recalculate_cursor();
                            state.input_state = InputState::Typing;
                            state.add_toast(
                                format!("已填充: {}", prefix),
                                Duration::from_millis(800),
                            );
                            state.rendered_lines_dirty.set(true);
                            return;
                        }
                    }
                }

                // V28.1 (PR8): 点击 timeline 事件行 → 双重作用
                //   1. toggle 该 event 的 inline 展开(panel 内就地看摘要)
                //   2. 反查该 event 属于哪条 message,scroll 消息区跳转过去
                //   设计意图: panel 是"快速一瞥",消息区是"完整阅读"。点击同步两个视图,
                //   用户不必手动滚动找对应消息。展开消息内 trace 仍由用户主动控制。
                if !in_input_row
                    && state.panel_scroll_section == crate::tui::state::PanelSection::Timeline
                {
                    let row_map = state.timeline_row_map.borrow();
                    if let Some((_, eid)) = row_map.iter().find(|(y, _)| *y == event.row) {
                        let target_id = *eid;
                        drop(row_map);
                        // V28.4: 设全局 focused event,两边渲染同步高亮该 event
                        // 再次点击同一 event(已 focused)时不取消 focus,只 toggle 展开
                        // (用户期望: 多次点击切展开状态;切换 focus 用切到别的 event)
                        state.focused_event_id = Some(target_id);
                        // 1. timeline inline 展开切换
                        if state.timeline_expanded_ids.contains(&target_id) {
                            state.timeline_expanded_ids.remove(&target_id);
                        } else {
                            state.timeline_expanded_ids.insert(target_id);
                        }
                        // 2. 反查该 event 在哪条 Message::Trace.event_ids 里 → scroll 跳转
                        let target_msg_idx = state.messages.iter().position(|m| {
                            m.parts.iter().any(|p| matches!(p,
                                crate::tui::state::MsgContent::Trace { event_ids, .. }
                                    if event_ids.contains(&target_id)
                            ))
                        });
                        if let Some(idx) = target_msg_idx {
                            // content_width = chat_cols 减 border(2) + padding(1) + bar(1) + indent
                            // 与 screen_row_to_msg_idx 保持一致(saturating_sub(5).max(20))
                            let content_w = (chat_cols as usize).saturating_sub(5).max(20);
                            scroll_to_message(state, idx, content_w);
                            state.add_toast(
                                format!("↓ 已跳转到第 {} 条消息", idx + 1),
                                Duration::from_secs(2),
                            );
                        }
                        state.rendered_lines_dirty.set(true);
                    }
                }
            } else if in_input_row {
                // V25: 左侧 input 区点击 → 定位 cursor 到点击字符位置
                // input_block 内部布局: row 0 顶栏提示, row 1-2 文字行, row 3+ 底栏 hints
                // borders+padding 占 col 0-1 (border 1 + padding 1) 和 row 0 (border top)
                // → inner row 1 = visible[0], inner row 2 = visible[1]
                let inner_row = event.row.saturating_sub(input_top + 1);
                let target_visible: Option<usize> = match inner_row {
                    1 => Some(0),
                    2 => Some(1),
                    _ => None,
                };
                if let Some(visible_idx) = target_visible {
                    let display_lines: Vec<&str> = state.input.lines().collect();
                    let start = display_lines.len().saturating_sub(2);
                    let target_input_line = start + visible_idx;
                    if target_input_line < display_lines.len() {
                        let line = display_lines[target_input_line];
                        // 列 → char index (按 unicode 显示宽度,CJK 占 2)
                        let col_in_inner = event.column.saturating_sub(2);
                        let mut current_col: u16 = 0;
                        let mut target_char_idx = line.chars().count();
                        for (idx, c) in line.chars().enumerate() {
                            let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(1) as u16;
                            if current_col + w > col_in_inner {
                                target_char_idx = idx;
                                break;
                            }
                            current_col += w;
                        }
                        // char index → byte offset in line
                        let byte_offset = line.char_indices()
                            .nth(target_char_idx)
                            .map(|(b, _)| b)
                            .unwrap_or(line.len());
                        // 累加之前所有行的 byte 长度(每行 +1 for '\n')
                        let mut cursor_pos: usize = 0;
                        for (idx, l) in display_lines.iter().enumerate() {
                            if idx >= target_input_line { break; }
                            cursor_pos += l.len() + 1;
                        }
                        cursor_pos += byte_offset;
                        state.cursor_pos = cursor_pos;
                        state.recalculate_cursor();
                    }
                    // else: 点击虚拟空行(input 行数 < 2 时填充的 ""),不操作
                }
            }
            // V28.3: 消息区点击 — 反查 message_trace_row_map,
            // 如果命中 Trace/Block summary 行 → toggle_block 展开/折叠
            // 不命中则保留原有"消息区点击不切焦点"行为
            if !in_panel_col && !in_input_row {
                let row_map = state.message_trace_row_map.borrow();
                let hit = row_map.iter()
                    .find(|(y, _, _)| *y == event.row)
                    .map(|(_, m, p)| (*m, *p));
                drop(row_map);
                if let Some((msg_idx, part_idx)) = hit {
                    toggle_block(state, msg_idx, part_idx);
                }
            }
            // else: 消息区点击,不切焦点(原有行为,消息区不在 Focus enum 内)
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            // V30 复制修复：Shift+Drag 扩展选中区间；char_idx 随拖拽实时更新。
            // 未按 Shift 拖拽 → fall through 不动 selection（保留 V28 鼠标交互路径）。
            if event.modifiers.contains(KeyModifiers::SHIFT) {
                if state.text_selection.is_some() {
                    let pw = if state.panel_visible { terminal_cols * crate::tui::layout::panel_pct_for_width(terminal_cols) / 100 } else { 0 };
                    let chat_cols = terminal_cols.saturating_sub(pw).max(40);
                    let cached_rows = state.cached_msg_rows.borrow();
                    if let Some((msg_idx, char_idx)) = crate::tui::components::screen_pos_to_msg_char(
                        event.row, event.column, terminal_rows, state.scroll, &state.messages, chat_cols,
                        cached_rows.as_slice(),
                    ) {
                        if let Some(ref mut sel) = state.text_selection {
                            sel.end_msg_idx = msg_idx;
                            sel.end_char_idx = char_idx;
                        }
                        state.rendered_lines_dirty.set(true);
                    }
                }
            }
        }
        MouseEventKind::Up(MouseButton::Left) => {
            // V30 复制修复：
            //   - 释放时只要 selection 范围非空 → 自动复制，不再要求 Ctrl 修饰键。
            //   - selection 范围为空（仅点击未拖动）→ 丢弃选中状态，不复制。
            //   - 走 tui::clipboard::set_text 路径：优先 arboard 平台原生，fallback OSC 52。
            //   - Trace 块详情现在会被拼入复制文本（extract_selection_text 读 trace_events）。
            if let Some(sel) = state.text_selection.take() {
                let is_empty_range = sel.start_msg_idx == sel.end_msg_idx
                    && sel.start_char_idx == sel.end_char_idx;
                if !is_empty_range {
                    let text = extract_selection_text(&sel, &state.messages, &state.trace_events);
                    if !text.is_empty() {
                        match crate::tui::clipboard::set_text(&text) {
                            Ok(backend) => {
                                state.add_toast(
                                    format!("{} {} 字符", backend.label(), text.chars().count()),
                                    Duration::from_secs(2),
                                );
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "clipboard set_text failed");
                                state.add_toast("复制失败：本终端不支持剪贴板写入", Duration::from_secs(3));
                            }
                        }
                    }
                }
                state.rendered_lines_dirty.set(true);
            }
        }
        _ => {}
    }
}

/// V28.1 (跨视图跳转): 把消息区滚动到 target_idx 那条消息(让其底部对齐屏幕底部)
///
/// state.scroll 语义: 从底部往上偏移的"渲染行数"(非 msg 索引)。
/// 算法: 累加 target_idx 之后所有 msg 的估算高度,设为 scroll 值,
/// 这样 target_idx 的最后一行落到屏幕底部(用户滚轮可微调)。
///
/// 引用关系: 被 timeline 点击 hit-test 调用(event 所属 msg → 跳转)
/// 副作用: 设 state.scroll + mark_dirty;is_streaming 时不动(自动跟随底部)
pub fn scroll_to_message(state: &mut AppState, target_idx: usize, content_width: usize) {
    if state.is_streaming || target_idx >= state.messages.len() { return; }
    let last_idx = state.messages.len().saturating_sub(1);
    if target_idx >= last_idx {
        // 目标已在最底,scroll = 0(自动跟随)
        state.set_scroll(ScrollAction::ToBottom);
    } else {
        // 累加 target_idx+1 之后所有消息的估算行数 = 该位置距底部的行偏移
        // V29.10: estimate_msg_rows 已搬到 components 模块, 与 build_message_lines 同模块强约束
        // V29.16: 走 SSOT — clamp/dirty 内部统一
        let lines_below: usize = state.messages.iter()
            .skip(target_idx + 1)
            .map(|m| crate::tui::components::estimate_msg_rows(m, content_width))
            .sum();
        state.set_scroll(ScrollAction::Absolute(lines_below));
    }
}

// V29.10: estimated_msg_rows / screen_row_to_msg_idx 已移到 components/mod.rs
//   路径: crate::tui::components::estimate_msg_rows / screen_row_to_msg_idx
//   动机: 与 build_message_lines 同模块让「估算 vs 实际渲染」三处不一致问题在一个文件内可检轮

/// 从选择区域提取文本（含 stream + block detail）
/// V30 复制修复：提取选中文本，支持 char-级边界 + Trace 详情拼入。
///
/// ## 语义不变量
/// - char_idx 是 msg “Stream parts 拼接后”的字符偏移，与 screen_pos_to_msg_char 同语义
/// - 跨 msg 选中时：首 msg 从 start_char_idx 取到末；中间 msg 全部；末 msg 从 0 取到 end_char_idx
/// - Trace 详情：在 sel 跨过 Trace part 时拼入 events 的实际内容而非“[trace · N events]”占位符
///
/// ## 引用关系
/// - 调用方：handle_mouse Up 分支（selection 释放）
/// - 依赖：state.trace_events （SSOT）、state.messages
fn extract_selection_text(
    sel: &TextSelection,
    messages: &std::collections::VecDeque<Message>,
    trace_events: &[crate::tui::state::TraceEvent],
) -> String {
    use crate::tui::state::TraceKind;
    // 规范化 sel：按 (msg_idx, char_idx) 字典序计算起止
    let (s_msg, s_char, e_msg, e_char) =
        if (sel.start_msg_idx, sel.start_char_idx) <= (sel.end_msg_idx, sel.end_char_idx) {
            (sel.start_msg_idx, sel.start_char_idx, sel.end_msg_idx, sel.end_char_idx)
        } else {
            (sel.end_msg_idx, sel.end_char_idx, sel.start_msg_idx, sel.start_char_idx)
        };
    let mut text = String::new();
    let last = messages.len().saturating_sub(1);
    let e_msg = e_msg.min(last);
    for idx in s_msg..=e_msg {
        let Some(msg) = messages.get(idx) else { continue; };
        // Stream 拼接 + Block detail + Trace details
        let mut parts_flat = String::new();
        for part in &msg.parts {
            match part {
                MsgContent::Stream(s) => parts_flat.push_str(s),
                MsgContent::Block { summary, detail, .. } => {
                    if !parts_flat.is_empty() && !parts_flat.ends_with('\n') { parts_flat.push('\n'); }
                    parts_flat.push_str(&format!("[{}]\n", summary));
                    parts_flat.push_str(detail);
                    if !detail.ends_with('\n') { parts_flat.push('\n'); }
                }
                MsgContent::Trace { event_ids, .. } => {
                    if !parts_flat.is_empty() && !parts_flat.ends_with('\n') { parts_flat.push('\n'); }
                    for eid in event_ids {
                        if let Some(ev) = trace_events.iter().find(|e| e.id == *eid) {
                            match &ev.kind {
                                TraceKind::Generic { content } => {
                                    parts_flat.push_str(content);
                                    parts_flat.push('\n');
                                }
                                TraceKind::Thinking { text: t, .. } => {
                                    parts_flat.push_str("[thinking]\n");
                                    parts_flat.push_str(t);
                                    if !t.ends_with('\n') { parts_flat.push('\n'); }
                                }
                                TraceKind::ToolCall { name, args, output, .. } => {
                                    parts_flat.push_str(&format!("[tool: {}]\n", name));
                                    if !args.is_empty() {
                                        parts_flat.push_str("args: ");
                                        parts_flat.push_str(args);
                                        parts_flat.push('\n');
                                    }
                                    if let Some(o) = output {
                                        parts_flat.push_str("output: ");
                                        parts_flat.push_str(o);
                                        if !o.ends_with('\n') { parts_flat.push('\n'); }
                                    }
                                }
                                TraceKind::Reply { tokens } => {
                                    parts_flat.push_str(&format!("[reply · {} tokens]\n", tokens));
                                }
                            }
                        }
                    }
                }
            }
        }
        // 切片：首/末 msg 按 char_idx，中间 msg 全量
        let chars: Vec<char> = parts_flat.chars().collect();
        let start_in_msg = if idx == s_msg { s_char.min(chars.len()) } else { 0 };
        let end_in_msg = if idx == e_msg { e_char.min(chars.len()) } else { chars.len() };
        if start_in_msg < end_in_msg {
            let slice: String = chars[start_in_msg..end_in_msg].iter().collect();
            text.push_str(&slice);
            if idx < e_msg && !text.ends_with('\n') {
                text.push('\n');
            }
        }
    }
    text
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// 消息提交流程 (设计规范核心流程)
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// 全屏编辑器键盘处理
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// 2026-05-28: 全屏编辑器模式键盘事件处理
/// 引用关系：handle_input_key 中 InputState::Editor 时调用
/// 生命周期：编辑器打开期间每个 KeyPress 调用一次
fn handle_editor_key(state: &mut AppState, code: KeyCode, mods: KeyModifiers) {
    match code {
        // Ctrl+S → 提交消息
        KeyCode::Char('s') if mods.contains(KeyModifiers::CONTROL) => {
            state.close_editor();
            submit_message(state);
        }
        // Esc → 取消编辑器（保留 input 内容不提交）
        KeyCode::Esc => {
            state.close_editor();
        }
        // Enter → 插入换行（编辑器中 Enter 不提交！）
        KeyCode::Enter => {
            state.input.insert(state.cursor_pos, '\n');
            state.cursor_pos += 1;
            state.recalculate_cursor();
            editor_ensure_visible(state);
            state.rendered_lines_dirty.set(true);
        }
        // Tab → 插入 4 空格
        KeyCode::Tab => {
            for _ in 0..4 {
                state.input.insert(state.cursor_pos, ' ');
                state.cursor_pos += 1;
            }
            state.cursor_col += 4;
            state.rendered_lines_dirty.set(true);
        }
        // 光标移动
        KeyCode::Up => {
            let before = &state.input[..state.cursor_pos];
            if let Some(nl) = before.rfind('\n') {
                let current_col = state.cursor_col;
                let line_start = before[..nl].rfind('\n').map(|i| i + 1).unwrap_or(0);
                let prev_line = &before[line_start..nl];
                let target_col = current_col.min(
                    prev_line.chars().map(|c| unicode_width::UnicodeWidthChar::width(c).unwrap_or(1)).sum()
                );
                // 找到 prev_line 中 target_col display width 对应的 byte offset
                let mut col = 0;
                let mut byte_offset = line_start;
                for ch in prev_line.chars() {
                    if col >= target_col { break; }
                    col += unicode_width::UnicodeWidthChar::width(ch).unwrap_or(1);
                    byte_offset += ch.len_utf8();
                }
                state.cursor_pos = byte_offset;
                state.recalculate_cursor();
            }
            editor_ensure_visible(state);
            state.rendered_lines_dirty.set(true);
        }
        KeyCode::Down => {
            let after = &state.input[state.cursor_pos..];
            if let Some(nl) = after.find('\n') {
                let current_col = state.cursor_col;
                let next_line_start = state.cursor_pos + nl + 1;
                let next_line_end = state.input[next_line_start..].find('\n')
                    .map(|i| next_line_start + i)
                    .unwrap_or(state.input.len());
                let next_line = &state.input[next_line_start..next_line_end];
                let mut col = 0;
                let mut byte_offset = next_line_start;
                for ch in next_line.chars() {
                    if col >= current_col { break; }
                    col += unicode_width::UnicodeWidthChar::width(ch).unwrap_or(1);
                    byte_offset += ch.len_utf8();
                }
                state.cursor_pos = byte_offset;
                state.recalculate_cursor();
            }
            editor_ensure_visible(state);
            state.rendered_lines_dirty.set(true);
        }
        KeyCode::Left => {
            if state.cursor_pos > 0 {
                let prev_char_start = state.input[..state.cursor_pos]
                    .char_indices().next_back().map(|(i, _)| i).unwrap_or(0);
                state.cursor_pos = prev_char_start;
                state.recalculate_cursor();
            }
            state.rendered_lines_dirty.set(true);
        }
        KeyCode::Right => {
            if state.cursor_pos < state.input.len() {
                let next = state.input[state.cursor_pos..].chars().next()
                    .map(|c| c.len_utf8()).unwrap_or(0);
                state.cursor_pos += next;
                state.recalculate_cursor();
            }
            state.rendered_lines_dirty.set(true);
        }
        KeyCode::Home => {
            let before = &state.input[..state.cursor_pos];
            let line_start = before.rfind('\n').map(|i| i + 1).unwrap_or(0);
            state.cursor_pos = line_start;
            state.recalculate_cursor();
            state.rendered_lines_dirty.set(true);
        }
        KeyCode::End => {
            let after = &state.input[state.cursor_pos..];
            let line_end = after.find('\n')
                .map(|i| state.cursor_pos + i)
                .unwrap_or(state.input.len());
            state.cursor_pos = line_end;
            state.recalculate_cursor();
            state.rendered_lines_dirty.set(true);
        }
        // 字符插入
        KeyCode::Char(c) => {
            state.input.insert(state.cursor_pos, c);
            state.cursor_pos += c.len_utf8();
            state.recalculate_cursor();
            state.rendered_lines_dirty.set(true);
        }
        // 删除
        KeyCode::Backspace => {
            if state.cursor_pos > 0 {
                if let Some((idx, _)) = state.input[..state.cursor_pos].char_indices().next_back() {
                    state.input.remove(idx);
                    state.cursor_pos = idx;
                    state.recalculate_cursor();
                }
            }
            editor_ensure_visible(state);
            state.rendered_lines_dirty.set(true);
        }
        KeyCode::Delete => {
            if state.cursor_pos < state.input.len() {
                state.input.remove(state.cursor_pos);
            }
            state.rendered_lines_dirty.set(true);
        }
        // PgUp/PgDn — 滚动 ±(visible_h - 2) 并移动光标
        KeyCode::PageUp => {
            let page = state.editor_state.as_ref()
                .map(|e| e.last_visible_h.get().saturating_sub(2).max(1))
                .unwrap_or(18);
            // 光标上移 page 行
            for _ in 0..page {
                let before = &state.input[..state.cursor_pos];
                if before.rfind('\n').is_none() { break; }
                // 模拟 Up 一次
                let nl = before.rfind('\n').unwrap();
                let line_start = before[..nl].rfind('\n').map(|i| i + 1).unwrap_or(0);
                let prev_line = &before[line_start..nl];
                let target_col = state.cursor_col.min(
                    prev_line.chars().map(|c| unicode_width::UnicodeWidthChar::width(c).unwrap_or(1)).sum()
                );
                let mut col = 0;
                let mut byte_offset = line_start;
                for ch in prev_line.chars() {
                    if col >= target_col { break; }
                    col += unicode_width::UnicodeWidthChar::width(ch).unwrap_or(1);
                    byte_offset += ch.len_utf8();
                }
                state.cursor_pos = byte_offset;
                state.recalculate_cursor();
            }
            editor_ensure_visible(state);
            state.rendered_lines_dirty.set(true);
        }
        KeyCode::PageDown => {
            let page = state.editor_state.as_ref()
                .map(|e| e.last_visible_h.get().saturating_sub(2).max(1))
                .unwrap_or(18);
            let total_lines = state.input.matches('\n').count();
            for _ in 0..page {
                if state.cursor_line >= total_lines { break; }
                let after = &state.input[state.cursor_pos..];
                if let Some(nl) = after.find('\n') {
                    let next_line_start = state.cursor_pos + nl + 1;
                    let next_line_end = state.input[next_line_start..].find('\n')
                        .map(|i| next_line_start + i)
                        .unwrap_or(state.input.len());
                    let next_line = &state.input[next_line_start..next_line_end];
                    let mut col = 0;
                    let mut byte_offset = next_line_start;
                    for ch in next_line.chars() {
                        if col >= state.cursor_col { break; }
                        col += unicode_width::UnicodeWidthChar::width(ch).unwrap_or(1);
                        byte_offset += ch.len_utf8();
                    }
                    state.cursor_pos = byte_offset;
                    state.recalculate_cursor();
                } else {
                    break;
                }
            }
            editor_ensure_visible(state);
            state.rendered_lines_dirty.set(true);
        }
        _ => {}
    }
}

/// 编辑器滚动：确保光标在可见区域内（使用 last_visible_h 精确计算）
fn editor_ensure_visible(state: &mut AppState) {
    if let Some(ref mut ed) = state.editor_state {
        let cursor_line = state.cursor_line;
        let visible_h = ed.last_visible_h.get().max(3); // 用渲染侧记录的实际值
        if cursor_line < ed.scroll_top {
            ed.scroll_top = cursor_line;
        } else if cursor_line >= ed.scroll_top + visible_h {
            ed.scroll_top = cursor_line.saturating_sub(visible_h - 1);
        }
    }
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// 消息发送流程: 输入 → 非空校验 → 长度校验 → 斜杠命令拦截 → 禁用输入 → 提交引擎
/// 引擎调用由 main.rs 的事件循环通过 channel 异步执行
pub fn submit_message(state: &mut AppState) {
    let text = state.input.trim().to_string();
    if text.is_empty() {
        return;
    }

    // E7 修复：toast 写"字符"但 text.len() 是字节数；CJK 用户上限被压缩到 ~33K 字符。
    // 按字符数（chars().count()）统一限制
    let char_count = text.chars().count();
    if char_count > 100_000 {
        state.add_toast(
            format!("消息过长（{} 字符），最多 100,000 字符", char_count),
            Duration::from_secs(3),
        );
        return;
    }

    // 斜杠命令拦截：本地消费，不发给引擎
    if handle_slash_command(state, &text) {
        state.input.clear();
        state.cursor_pos = 0;
        state.cursor_line = 0;
        state.cursor_col = 0;
        state.input_state = InputState::Ready;
        return;
    }

    // 2026-05-27: Meeting 执行提案确认拦截
    // 当 pending_meeting_execution 存在且未超时时，拦截 Y/n 输入
    if let Some(ref prompt) = state.pending_meeting_execution.clone() {
        if prompt.created_at.elapsed() < Duration::from_secs(30) {
            let input_lower = text.trim().to_lowercase();
            if input_lower == "y" || input_lower == "yes" {
                // 用户确认：组装 goal 并触发执行
                let goal = prompt.action_items.join("; ");
                let suggest_team = prompt.suggest_team;
                state.pending_meeting_execution = None;
                state.pending_slash_command = if suggest_team {
                    Some(crate::tui::state::SlashCommand::ExecuteWithTeam { task: goal })
                } else {
                    Some(crate::tui::state::SlashCommand::ExecuteWithPlan { task: goal })
                };
                state.input.clear();
                state.cursor_pos = 0;
                state.cursor_line = 0;
                state.cursor_col = 0;
                state.input_state = InputState::Thinking;
                return;
            } else if input_lower == "n" || input_lower == "no" {
                // 用户拒绝
                state.pending_meeting_execution = None;
                state.add_toast("已取消自动执行".to_string(), Duration::from_secs(2));
                state.input.clear();
                state.cursor_pos = 0;
                state.cursor_line = 0;
                state.cursor_col = 0;
                state.input_state = InputState::Ready;
                return;
            }
            // 其他输入：清除提案，作为普通消息继续处理
            state.pending_meeting_execution = None;
        } else {
            // 超时：静默清除
            state.pending_meeting_execution = None;
        }
    }

    // 2026-05-27: 清除保留输入（如果有）
    state.preserved_input = None;

    state.add_message(crate::tui::state::Message::new_user(
        text.clone(),
        chrono::Local::now().format("%H:%M").to_string(),
    ));

    // 会话内容总结：用首条消息的前 30 字作为顶栏展示标签
    if state.session_summary.is_empty() {
        let summary = text.chars().take(30).collect::<String>();
        if summary.chars().count() < text.chars().count() {
            state.session_summary = format!("{}…", summary);
        } else {
            state.session_summary = summary;
        }
    }

    // V33.1: 关闭关键词自动模式切换——误报率过高。
    // 模式切换现在仅走显式命令 /clarify /plan /team /meeting + Ctrl+1/2/3

    state.input.clear();
    state.cursor_pos = 0;
    state.cursor_line = 0;
    state.cursor_col = 0;
    state.input_state = InputState::Thinking;
    state.op_started_at = Some(std::time::Instant::now());
    state.accumulated_elapsed = std::time::Duration::ZERO;
    state.processing_phase = "🤔 Thinking...".into();
    state.processing_step = 1;
    state.processing_total_steps = 4;

    let now = chrono::Local::now().format("%H:%M").to_string();
    state.add_event(now, "session", "用户发言", crate::tui::state::EventLevel::Info);
    info!(len = text.len(), "用户提交消息");

    // 记录输入历史（去重，上限 100）
    let trimmed_text = text.clone();
    record_input_history(state, &trimmed_text);

    // 如果有引擎连接，通过 pending_text 触发异步调用
    // main.rs 事件循环检测到 pending_text 后会 spawn 引擎调用
    if state.engine_handle.is_some() {
        state.pending_text = Some(text);
    } else {
        // 无引擎时直接恢复（TUI 处于演示模式）
        state.add_message(crate::tui::state::Message::new_session(
            vec![crate::tui::state::MsgContent::Stream(
                "(演示模式 — 未配置引擎连接)".into()
            )],
            chrono::Local::now().format("%H:%M").to_string(),
        ));
        state.input_state = InputState::Ready;
    }
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// 模式切换流程
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// 模式切换: 仅切换渲染布局，保留会话上下文
pub fn switch_mode(state: &mut AppState, mode: AbacusMode) {
    if state.mode == mode {
        return;
    }

    info!(from = ?state.mode.label(), to = ?mode.label(), "模式切换");
    // ST7 修复：流式中切 mode 不清累积，会在新 mode 下显示旧 mode 的 chunks（视觉混乱）。
    // 后台请求虽未 cancel（cancel token 未持久化到 state），但显示层先清，
    // 残留 chunks 抵达后会被 reset_streaming 后的判定丢弃显示
    if state.is_streaming || !state.streaming_text.is_empty() {
        state.reset_streaming();
        state.input_state = InputState::Ready;
    }

    // V28.7: mode-specific 状态在切换时清空，避免跨 mode 数据污染。
    // 引用关系：
    //   - state.experts → 仅 Meeting 模式 panel 议程 tab 渲染（render_panel_meeting_agenda）
    //                     非 Meeting 模式不可见，但若残留再切回 Meeting 会显示旧会议参会者
    //   - 数据来源：send_meeting_message 每轮 EngineResponse.meeting_experts 重写
    // 生命周期：模式切换 = 新会话上下文，experts 等待真实数据填充（下一条消息）
    // 不清理项：messages / events / tool_records（用户跨 mode 持续可见的历史）
    if !state.experts.is_empty() {
        state.experts.clear();
    }

    state.set_mode(mode);

    // V34: ModeArtifact 数据流转 — 进入新 mode 时取走上阶段产出
    // 引用关系：mode_artifact 由上阶段（Clarify /done 携带 ClarifyBrief / Meeting 结论）写入
    // 消费：本处取 take()，根据新 mode 加载到对应 state 字段
    // - ClarifyBrief → toast 提示（Meeting 入口可见）
    // - MeetingConclusion → toast 提示（Clarify 入口可见）
    // V34: PlanTasks 已删除（Plan 降级为执行策略，不通过 mode 切换传递 TaskSpec）
    if let Some(artifact) = state.mode_artifact.take() {
        let summary = artifact.summary();
        state.add_toast(
            format!("→ {} ({})", mode.display_zh(), summary),
            Duration::from_secs(4),
        );
    } else {
        state.add_toast(
            format!("切换到 {} 视图 — 消息和上下文已保留", mode.display_zh()),
            Duration::from_secs(2),
        );
    }
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// 消息块折叠/展开
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// V28 (T7): toggle_block 支持任意可折叠 part(Block 或 Trace),
/// part_idx 在 Block 与 Trace 之间共用计数空间,方便 hit-test 直接定位。
pub fn toggle_block(state: &mut AppState, msg_idx: usize, part_idx: usize) {
    let mut toggled = false;
    if let Some(msg) = state.messages.get_mut(msg_idx) {
        let mut bi = 0;
        for part in &mut msg.parts {
            match part {
                crate::tui::state::MsgContent::Block { collapsed, .. } => {
                    if bi == part_idx {
                        *collapsed = !*collapsed;
                        toggled = true;
                        break;
                    }
                    bi += 1;
                }
                crate::tui::state::MsgContent::Trace { collapsed, .. } => {
                    if bi == part_idx {
                        *collapsed = !*collapsed;
                        toggled = true;
                        break;
                    }
                    bi += 1;
                }
                _ => {}
            }
        }
    }
    // S3 修复：collapsed 翻转改变渲染行数，必须失效缓存——否则画面要等下次其他事件
    // 触发 dirty 才更新，用户感觉折叠"按了不响应"
    if toggled {
        state.mark_dirty();
    }
}

/// V28 (T7): 翻转 Trace 中某个 event 的"全展开"状态(per-event 细粒度,>30/20 行时全部显示)
/// part_idx 在 Block + Trace 共用计数空间(同 toggle_block);event_id 必须存在于该 Trace 的 event_ids
///
/// **状态**：架构预留（V33 审查留存）。当前 trace 事件展开通过整体 toggle 完成。
/// **Planned for V35+**：UI hit-test 接入鼠标 hover 单事件展开 + 键盘 Ctrl+E 聚焦展开。
/// 调用方注册点：mouse 事件 dispatch 在 events area 命中 trace event 子块时 → 调此函数。
#[allow(dead_code)] // V35+ hit-test 接入
pub fn toggle_trace_event(state: &mut AppState, msg_idx: usize, part_idx: usize, event_id: u64) {
    let mut toggled = false;
    if let Some(msg) = state.messages.get_mut(msg_idx) {
        let mut bi = 0;
        for part in &mut msg.parts {
            match part {
                crate::tui::state::MsgContent::Block { .. } => bi += 1,
                crate::tui::state::MsgContent::Trace { event_ids, expanded_event_ids, .. } => {
                    if bi == part_idx && event_ids.contains(&event_id) {
                        if expanded_event_ids.contains(&event_id) {
                            expanded_event_ids.remove(&event_id);
                        } else {
                            expanded_event_ids.insert(event_id);
                        }
                        toggled = true;
                        break;
                    }
                    bi += 1;
                }
                _ => {}
            }
        }
    }
    if toggled {
        state.mark_dirty();
    }
}

#[cfg(test)]
mod ev10_panic_tests {
    use super::*;
    use crate::tui::state::AbacusMode;
    use crossterm::event::{KeyCode, KeyModifiers};

    /// EV10 回归：构造 CJK + ASCII 混合多行场景，证明 Up/Down 不再 panic 在 char boundary
    /// 修复前路径：cursor_pos = prev_line_start + col(byte) → 落在 '中' 中间字节 → recalculate_cursor 切片 panic
    #[test]
    fn up_does_not_panic_at_char_boundary() {
        let mut s = AppState::new(AbacusMode::Clarify);
        // 第一行 CJK "中"(3 bytes), 第二行 ASCII "ab"
        s.input = "中\nab".to_string();
        s.cursor_pos = 6; // 'b' 之后（byte 6 = 'a'(4) + 'b'(5) end）
        s.recalculate_cursor();
        assert_eq!(s.cursor_line, 1);
        // 触发 Up：col_chars=2（"ab" 两个 char），上一行 "中" 只有 1 char
        // 预期：target_byte = 行末 = 3，new cursor_pos = 0+3 = 3 → '\n' 位置
        handle_input_key(&mut s, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(s.cursor_pos, 3, "应停在 '中' 之后即 \\n 位置");
        assert_eq!(s.cursor_line, 0);
    }

    #[test]
    fn down_does_not_panic_at_char_boundary() {
        let mut s = AppState::new(AbacusMode::Clarify);
        // 第一行 ASCII "ab", 第二行 CJK "中文"
        s.input = "ab\n中文".to_string();
        s.cursor_pos = 1; // 'a' 之后
        s.recalculate_cursor();
        assert_eq!(s.cursor_line, 0);
        // 触发 Down：col_chars=1，下一行 "中文" char index 1 → '文' 起始 byte=3
        // new cursor_pos = next_line_start(3) + 3 = 6 → '文' 起始（valid char boundary）
        handle_input_key(&mut s, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(s.cursor_pos, 6, "应落在 '文' char boundary");
        assert_eq!(s.cursor_line, 1);
    }

    #[test]
    fn up_from_ascii_to_cjk_overflow() {
        // 当前行 col_chars 超出上一行 char count 时停在行末
        let mut s = AppState::new(AbacusMode::Clarify);
        s.input = "中\nabcde".to_string();
        s.cursor_pos = 9; // 末尾 'e' 之后（3+1+5）
        s.recalculate_cursor();
        handle_input_key(&mut s, KeyCode::Up, KeyModifiers::NONE);
        // col_chars=5 但上一行只有 1 char → 停在行末 = 3
        assert_eq!(s.cursor_pos, 3);
    }
}

#[cfg(test)]
mod scroll_to_message_tests {
    //! V28.1 跨视图跳转: scroll_to_message 把 state.scroll 设到让 target_idx 可见
    //!
    //! 不变量:
    //! - target = 末尾 → scroll = 0(自动跟随底部)
    //! - target = 中间/前 → scroll = sum(target+1.. 各 msg 估算行数)
    //! - is_streaming 时 no-op(自动跟随优先)
    //! - target_idx 越界 → no-op
    use super::*;
    use crate::tui::state::{Message, MsgContent, AbacusMode};

    fn build_msg(text: &str) -> Message {
        Message::new_session(vec![MsgContent::Stream(text.into())], "12:00")
    }

    #[test]
    fn target_at_bottom_yields_zero_scroll() {
        let mut s = AppState::new(AbacusMode::Clarify);
        s.add_message(build_msg("a"));
        s.add_message(build_msg("b"));
        s.add_message(build_msg("c"));
        s.set_scroll(ScrollAction::Absolute(99)); // V29.16: 走 SSOT, 干扰值
        scroll_to_message(&mut s, 2, 80);
        assert_eq!(s.scroll, 0, "target 已是末尾 → scroll=0 自动跟随底部");
    }

    #[test]
    fn target_in_middle_jumps_to_msg() {
        let mut s = AppState::new(AbacusMode::Clarify);
        s.add_message(build_msg("一行短消息"));
        s.add_message(build_msg("中间消息"));
        s.add_message(build_msg("末尾消息"));
        scroll_to_message(&mut s, 0, 80);
        // target=0,后面 2 条消息(每条 estimated_msg_rows >= 2:1 行内容 + 1 行尾分隔)
        // 至少 4 行偏移
        assert!(s.scroll >= 2, "scroll 应反映后续消息的累计行数, 实得 {}", s.scroll);
    }

    #[test]
    fn out_of_bounds_is_noop() {
        let mut s = AppState::new(AbacusMode::Clarify);
        s.add_message(build_msg("hi"));
        s.set_scroll(ScrollAction::Absolute(5));
        scroll_to_message(&mut s, 99, 80);
        assert_eq!(s.scroll, 5, "越界 idx 应不动 scroll");
    }

    #[test]
    fn streaming_is_noop() {
        let mut s = AppState::new(AbacusMode::Clarify);
        s.add_message(build_msg("a"));
        s.add_message(build_msg("b"));
        s.set_scroll(ScrollAction::Absolute(7));
        s.is_streaming = true;
        scroll_to_message(&mut s, 0, 80);
        assert_eq!(s.scroll, 7, "is_streaming 时不动 scroll(自动跟随优先)");
    }
}

#[cfg(test)]
mod scroll_clamp_tests {
    //! V29.5: handle_chat_scroll_key 上限 clamp / PageUp 半屏
    //!
    //! 不变量:
    //! - last_total_lines == 0(首帧前) → 不限制(scroll 自由增长)
    //! - last_total_lines > visible_h → max_scroll = total - visible
    //! - PageUp 步长 = visible / 2(最少 5)
    //! - 越过 max_scroll 后被夹回(不是 panic, 不是空白)
    use super::*;
    use crate::tui::state::AbacusMode;
    use crossterm::event::KeyCode;

    #[test]
    fn pre_render_unrestricted() {
        // 首帧未渲染时 last_total_lines=0, 用户按 Up 不应被限制
        let mut s = AppState::new(AbacusMode::Clarify);
        s.last_total_lines.set(0);
        s.last_visible_h.set(0);
        for _ in 0..50 { handle_chat_scroll_key(&mut s, KeyCode::Up); }
        assert_eq!(s.scroll, 150, "首帧前 scroll 应自由增长(50×3=150)");
    }

    #[test]
    fn clamp_to_max_scroll() {
        // total=100, visible=20 → max_scroll = 80
        let mut s = AppState::new(AbacusMode::Clarify);
        s.last_total_lines.set(100);
        s.last_visible_h.set(20);
        s.set_scroll(ScrollAction::Absolute(75));
        handle_chat_scroll_key(&mut s, KeyCode::Up); // 75+3=78 < 80 ✓
        assert_eq!(s.scroll, 78);
        handle_chat_scroll_key(&mut s, KeyCode::Up); // 78+3=81 > 80, 夹回 80
        assert_eq!(s.scroll, 80);
        for _ in 0..10 { handle_chat_scroll_key(&mut s, KeyCode::Up); }
        assert_eq!(s.scroll, 80, "继续按 Up 不再越过 max_scroll");
    }

    #[test]
    fn page_up_half_screen() {
        // visible=40, page_step = 40/2 = 20
        let mut s = AppState::new(AbacusMode::Clarify);
        s.last_total_lines.set(1000);
        s.last_visible_h.set(40);
        handle_chat_scroll_key(&mut s, KeyCode::PageUp);
        assert_eq!(s.scroll, 20, "PageUp 应等于 visible/2 = 20");
    }

    #[test]
    fn page_step_minimum_5() {
        // visible=4 (极小屏), page_step max(4/2, 5) = 5
        let mut s = AppState::new(AbacusMode::Clarify);
        s.last_total_lines.set(1000);
        s.last_visible_h.set(4);
        handle_chat_scroll_key(&mut s, KeyCode::PageUp);
        assert_eq!(s.scroll, 5, "极窄屏下 PageUp 至少 5 行");
    }

    #[test]
    fn home_resets_to_zero() {
        let mut s = AppState::new(AbacusMode::Clarify);
        s.last_total_lines.set(100);
        s.last_visible_h.set(20);
        s.set_scroll(ScrollAction::Absolute(50));
        handle_chat_scroll_key(&mut s, KeyCode::Home);
        assert_eq!(s.scroll, 0);
    }

    #[test]
    fn down_saturates_at_zero() {
        let mut s = AppState::new(AbacusMode::Clarify);
        s.last_total_lines.set(100);
        s.last_visible_h.set(20);
        s.set_scroll(ScrollAction::Absolute(2));
        handle_chat_scroll_key(&mut s, KeyCode::Down);
        // 2 - 3 → saturating_sub → 0
        assert_eq!(s.scroll, 0);
    }

    #[test]
    fn content_fits_screen_disallows_scroll() {
        // total=10, visible=20 → max_scroll = 0(全部装得下), 不允许 Up
        let mut s = AppState::new(AbacusMode::Clarify);
        s.last_total_lines.set(10);
        s.last_visible_h.set(20);
        handle_chat_scroll_key(&mut s, KeyCode::Up);
        assert_eq!(s.scroll, 0, "内容装得下时 max_scroll=0, Up 无效");
    }
}

#[cfg(test)]
mod space_anchor_tests {
    //! V29.11 (B4): Space 切折叠时的视野锚定
    //!
    //! 不变量:
    //! - scroll==0 时不锚定(用户在 auto-follow 底部, 期望看最新内容)
    //! - scroll>0 时锚定: scroll 增减等于最后一条 msg 行数变化, anchor msg 视觉不动
    //! - 展开多行 → scroll 增加; 折叠少行 → scroll 减少
    //! - 极端: 折叠减幅 > scroll 时 saturating_sub 到 0, 不溢出
    use super::*;
    use crate::tui::state::{Message, MsgContent, BlockKind, AbacusMode};
    use crossterm::event::KeyCode;

    fn build_msg_with_block(stream_text: &str, block_detail: &str, collapsed: bool) -> Message {
        let parts = vec![
            MsgContent::Stream(stream_text.into()),
            MsgContent::Block {
                kind: BlockKind::Think,
                summary: "thinking".into(),
                collapsed,
                detail: block_detail.into(),
            },
        ];
        Message::new_session(parts, "12:00")
    }

    #[test]
    fn space_no_anchor_when_scroll_zero() {
        // scroll==0 (auto-follow 底部), Space 切换不调整 scroll
        let mut s = AppState::new(AbacusMode::Clarify);
        s.add_message(build_msg_with_block("hello", "line1\nline2\nline3\nline4\nline5", true));
        s.last_content_width.set(80);
        s.set_scroll(ScrollAction::ToBottom);
        handle_chat_scroll_key(&mut s, KeyCode::Char(' '));
        assert_eq!(s.scroll, 0, "scroll==0 时 Space 不锚定, 保持 0 auto-follow");
    }

    #[test]
    fn space_anchors_on_expand() {
        // scroll>0, 折叠态切到展开 → 行数+, scroll 应+(同等增量)
        let mut s = AppState::new(AbacusMode::Clarify);
        s.add_message(build_msg_with_block("hi", "L1\nL2\nL3\nL4\nL5", true));
        s.last_content_width.set(80);
        s.set_scroll(ScrollAction::Absolute(10));

        let before = crate::tui::components::estimate_msg_rows(&s.messages[0], 80);
        handle_chat_scroll_key(&mut s, KeyCode::Char(' '));
        let after = crate::tui::components::estimate_msg_rows(&s.messages[0], 80);
        assert!(after > before, "展开后行数应增加");
        assert_eq!(s.scroll, 10 + (after - before), "scroll 应同步增加 delta 行");
    }

    #[test]
    fn space_anchors_on_collapse() {
        // scroll>0, 展开态切到折叠 → 行数-, scroll 应-(同等减量)
        let mut s = AppState::new(AbacusMode::Clarify);
        s.add_message(build_msg_with_block("hi", "L1\nL2\nL3\nL4\nL5", false));
        s.last_content_width.set(80);
        s.set_scroll(ScrollAction::Absolute(100));

        let before = crate::tui::components::estimate_msg_rows(&s.messages[0], 80);
        handle_chat_scroll_key(&mut s, KeyCode::Char(' '));
        let after = crate::tui::components::estimate_msg_rows(&s.messages[0], 80);
        assert!(after < before, "折叠后行数应减少");
        assert_eq!(s.scroll, 100 - (before - after), "scroll 应同步减少 delta 行");
    }

    #[test]
    fn space_saturating_sub_on_collapse() {
        // scroll 比 delta 还小时 saturating 到 0, 不溢出
        let mut s = AppState::new(AbacusMode::Clarify);
        s.add_message(build_msg_with_block("hi", "L1\nL2\nL3\nL4\nL5\nL6\nL7\nL8", false));
        s.last_content_width.set(80);
        s.set_scroll(ScrollAction::Absolute(1));
        handle_chat_scroll_key(&mut s, KeyCode::Char(' '));
        // delta ≈ 8 行, 但 scroll 只有 1 → saturating 到 0
        assert_eq!(s.scroll, 0, "delta > scroll 时 saturating_sub 兜到 0");
    }

    #[test]
    fn space_fallback_width_when_zero() {
        // last_content_width == 0(首帧前) → 用 80 fallback, 锚定逻辑仍可跑
        let mut s = AppState::new(AbacusMode::Clarify);
        s.add_message(build_msg_with_block("hi", "L1\nL2\nL3", true));
        s.last_content_width.set(0); // 首帧前
        s.set_scroll(ScrollAction::Absolute(5));
        // 不应 panic
        handle_chat_scroll_key(&mut s, KeyCode::Char(' '));
    }
}

#[cfg(test)]
mod estimate_v29_12_tests {
    //! V29.12: estimate_msg_rows Block 分支按 BlockKind 分流的精度收敛
    //!
    //! 不变量:
    //! - Block.Think/Checklist: detail.lines().count() 等同于实际渲染行数(无 markdown reflow 时)
    //! - Block.ToolCall: 估算反映 JSON pretty-print 后的真实行数, 而非原文行数
    //! - Block.ToolCall: 超 400 行 → 截到 200 + 1 行被截断提示
    //! - Trace 展开: events × 5 (header 1 + detail 平均 4)
    use crate::tui::state::{Message, MsgContent, BlockKind};
    use crate::tui::components::estimate_msg_rows;

    fn block_msg(kind: BlockKind, detail: &str, collapsed: bool) -> Message {
        Message::new_session(
            vec![MsgContent::Block { kind, summary: "s".into(), collapsed, detail: detail.into() }],
            "12:00",
        )
    }

    #[test]
    fn think_uses_input_lines_count() {
        // Think 路径: detail 5 行 → 估算 = 1(role) + 1(arrow) + 5(detail) + 1(trailer) = 8
        let m = block_msg(BlockKind::Think, "L1\nL2\nL3\nL4\nL5", false);
        assert_eq!(estimate_msg_rows(&m, 80), 8);
    }

    #[test]
    fn toolcall_pretty_prints_json() {
        // ToolCall 路径: 单行 JSON {"a":1,"b":2} pretty 后应为 4 行
        // {  /  "a": 1,  /  "b": 2  /  }
        let m = block_msg(BlockKind::ToolCall, r#"{"a":1,"b":2}"#, false);
        // 1 + 1 + 4 + 1 = 7
        assert_eq!(estimate_msg_rows(&m, 80), 7);
    }

    #[test]
    fn toolcall_invalid_json_fallback() {
        // ToolCall 路径: 非 JSON 字符串走 fallback 用原文 lines
        let m = block_msg(BlockKind::ToolCall, "not-json\nline2", false);
        // 1 + 1 + 2 + 1 = 5
        assert_eq!(estimate_msg_rows(&m, 80), 5);
    }

    #[test]
    fn toolcall_truncates_over_400_lines() {
        // ToolCall 路径: pretty 后 > 400 行 → 截到 200 + 1
        let big_array: Vec<i64> = (0..500).collect();
        let json = serde_json::to_string(&big_array).unwrap();
        let m = block_msg(BlockKind::ToolCall, &json, false);
        // pretty 后是 502 行 (开头 [ + 500 items + 末尾 ]) > 400, 截到 200+1
        // 1 + 1 + 201 + 1 = 204
        assert_eq!(estimate_msg_rows(&m, 80), 204);
    }

    #[test]
    fn collapsed_block_only_header() {
        // 折叠态: header 1 行, 无 detail
        let m = block_msg(BlockKind::Think, "L1\nL2\nL3\nL4\nL5\nL6\nL7\nL8", true);
        // 1(role) + 1(arrow header) + 1(trailer) = 3
        assert_eq!(estimate_msg_rows(&m, 80), 3);
    }

    #[test]
    fn trace_expanded_uses_events_times_five() {
        // Trace 展开 3 个 events: 1(role) + 1(summary) + 3*5(events) + 1(trailer) = 18
        let m = Message::new_session(
            vec![MsgContent::Trace {
                event_ids: vec![1, 2, 3],
                collapsed: false,
                expanded_event_ids: Default::default(),
            }],
            "12:00",
        );
        assert_eq!(estimate_msg_rows(&m, 80), 18);
    }

    #[test]
    fn trace_collapsed_only_summary() {
        let m = Message::new_session(
            vec![MsgContent::Trace {
                event_ids: vec![1, 2, 3],
                collapsed: true,
                expanded_event_ids: Default::default(),
            }],
            "12:00",
        );
        // 1(role) + 1(summary) + 1(trailer) = 3
        assert_eq!(estimate_msg_rows(&m, 80), 3);
    }
}
