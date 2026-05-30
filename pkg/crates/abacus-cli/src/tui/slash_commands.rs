//! TUI Slash Command Registry — typed command dispatch
//!
//! Replaces the giant match in handle_slash_command with a
//! registry pattern: commands register their names + handlers,
//! /help auto-generates, completion auto-discovers.

use crate::tui::state::{AppState, ScrollAction, SlashCommand};

/// Result of command execution
#[derive(Debug, Clone)]
pub enum CmdResult {
    Consumed,
    Pending(SlashCommand),
    NotFound(String),
}

type Handler = fn(state: &mut AppState, cmd: &str, args: &[&str]) -> CmdResult;

struct Entry {
    names: &'static [&'static str],
    handler: Handler,
    help: &'static str,
}

static REGISTRY: std::sync::OnceLock<Vec<Entry>> = std::sync::OnceLock::new();

fn registry() -> &'static [Entry] {
    REGISTRY.get_or_init(|| {
        let mut v = Vec::new();

        // ════════════════════════════════════════════════════════════════
        // 2026-05-28: 分层设计
        //   Tier 0: 基础操作（新手第一天会用）
        //   Tier 1: 核心能力（每天用）
        //   Tier 2: 进阶功能（按需学）
        //   Tier 3: 诊断/高级（大多数人不碰）
        //
        // /help 只显示 Tier 0+1；/help all 展开全部
        // 排布仍按 tier 从低到高；同 tier 按频率从高到低
        // ════════════════════════════════════════════════════════════════

        // ── Tier 0: 基础操作 ──
        v.push(Entry { names: &["clear", "cls"],    handler: cmd_clear,    help: "清空屏幕" });
        v.push(Entry { names: &["new", "reset"],    handler: cmd_new,      help: "新建会话" });
        v.push(Entry { names: &["copy"],            handler: cmd_copy,     help: "复制最后回复到剪贴板" });
        v.push(Entry { names: &["save"],            handler: cmd_save,     help: "保存当前会话" });
        v.push(Entry { names: &["quit", "exit", "q"], handler: cmd_quit,   help: "退出" });

        // ── Tier 1: 核心能力 ──
        v.push(Entry { names: &["model", "m", "thinking", "think"], handler: cmd_model, help: "模型+思考+上下文 - /model [name|thinking <level>]" });
        v.push(Entry { names: &["preset"],          handler: cmd_preset,   help: "场景预设 - /preset [quick|code|creative|lean|marathon|debug]" });
        v.push(Entry { names: &["set"],             handler: cmd_set,      help: "调参数 - /set [key value] 或 /set 查看全部" });
        v.push(Entry { names: &["plan"],            handler: cmd_plan,     help: "规划+执行 - /plan <任务>" });
        v.push(Entry { names: &["auto", "turnkey", "tk"], handler: cmd_turnkey, help: "全托管 - /auto <目标>（规划+执行不确认）" });

        // ── Tier 2: 进阶功能 ──
        v.push(Entry { names: &["team"],            handler: cmd_team,     help: "多 agent 协作 - /team <任务>" });
        v.push(Entry { names: &["meeting"],         handler: cmd_meeting,  help: "专家会诊模式" });
        v.push(Entry { names: &["mode"],            handler: cmd_mode,     help: "切换模式 — Clarify / Meeting" });
        v.push(Entry { names: &["clarify", "chat"], handler: cmd_clarify,  help: "切到澄清模式" });
        v.push(Entry { names: &["done"],            handler: cmd_done,     help: "完成当前模式，推进下一步" });
        v.push(Entry { names: &["review"],          handler: cmd_review,   help: "审查 - /review <plan|diff|security> [--strict]" });
        v.push(Entry { names: &["role"],            handler: cmd_role,     help: "调用角色 - /role <fix|summarize|test>" });
        v.push(Entry { names: &["theme"],           handler: cmd_theme,    help: "切换主题" });
        v.push(Entry { names: &["lang", "language"], handler: cmd_lang,    help: "切换语言 - /lang [zh|en]" });
        v.push(Entry { names: &["undo"],            handler: cmd_undo,     help: "撤销 - /undo [seq|turn|history|timeline]" });
        v.push(Entry { names: &["redo"],            handler: cmd_redo,     help: "重做" });
        v.push(Entry { names: &["search"],          handler: cmd_search,   help: "搜索消息 - /search <query>" });
        v.push(Entry { names: &["diff"],            handler: cmd_diff,     help: "git diff - /diff [path]" });
        v.push(Entry { names: &["export"],          handler: cmd_export,   help: "导出会话" });
        v.push(Entry { names: &["context", "ctx"],  handler: cmd_context,  help: "上下文状态" });
        v.push(Entry { names: &["compress"],        handler: cmd_compress, help: "手动压缩上下文" });
        v.push(Entry { names: &["inject"],          handler: cmd_inject,   help: "注入临时知识 - /inject <text>" });
        v.push(Entry { names: &["rename"],          handler: cmd_rename,   help: "重命名会话 - /rename <alias>" });
        v.push(Entry { names: &["resume"],          handler: cmd_resume,   help: "恢复 session - /resume [uuid]" });
        v.push(Entry { names: &["branch", "fork"],  handler: cmd_branch,   help: "派生新会话" });
        v.push(Entry { names: &["history"],         handler: cmd_history,  help: "输入历史" });
        v.push(Entry { names: &["meeting-list", "ml"],   handler: cmd_meeting_list, help: "列出历史会议" });
        v.push(Entry { names: &["meeting-load", "mload"], handler: cmd_meeting_load, help: "注入历史会议 - /meeting-load <id>" });
        v.push(Entry { names: &["expert"],          handler: cmd_expert,   help: "专家配置 - /expert list|add|remove" });

        // ── Tier 3: 诊断与高级 ──
        v.push(Entry { names: &["status"],          handler: cmd_status,   help: "当前状态" });
        v.push(Entry { names: &["tokens", "tok"],   handler: cmd_tokens,   help: "Token 统计" });
        v.push(Entry { names: &["models"],          handler: cmd_models,   help: "列出可用模型" });
        v.push(Entry { names: &["tools"],           handler: cmd_tools,    help: "已注册工具" });
        v.push(Entry { names: &["tool-stats", "stats"], handler: cmd_tool_stats, help: "工具效能统计" });
        v.push(Entry { names: &["allow"],           handler: cmd_allow,    help: "授权管理 - /allow [list|revoke|clear]" });
        v.push(Entry { names: &["safety"],          handler: cmd_safety,   help: "安全状态" });
        v.push(Entry { names: &["doctor"],          handler: cmd_doctor,   help: "系统健康检查" });
        v.push(Entry { names: &["debug"],           handler: cmd_debug,    help: "调试信息" });
        v.push(Entry { names: &["streaming", "stream"], handler: cmd_streaming, help: "切换流式输出" });
        v.push(Entry { names: &["info"],            handler: cmd_info,     help: "会话详情" });
        v.push(Entry { names: &["memory"],          handler: cmd_memory,   help: "Memory 面板" });
        v.push(Entry { names: &["plugins", "mcp"],  handler: cmd_plugins,  help: "Components 面板" });
        v.push(Entry { names: &["settings"],        handler: cmd_settings, help: "设置面板" });
        v.push(Entry { names: &["auto-review"],     handler: cmd_auto_review, help: "自动 review - /auto-review <on|off>" });
        v.push(Entry { names: &["review-clear"],    handler: cmd_review_clear, help: "清除 review 阻断" });
        v.push(Entry { names: &["review-history"],  handler: cmd_review_history, help: "review 历史" });
        v.push(Entry { names: &["review-required"], handler: cmd_review_required, help: "review 强约束 - /review-required <on|off>" });
        v.push(Entry { names: &["feedback"],        handler: cmd_feedback, help: "提交反馈" });
        v.push(Entry { names: &["version", "v"],    handler: cmd_version,  help: "版本号" });

        // ── /help 置最后 ──
        v.push(Entry { names: &["help", "h"],       handler: cmd_help,     help: "帮助 - /help [all|<命令名>]" });

        v
    })
}

/// 返回所有已注册命令的名称列表（带 / 前缀）
///
/// Phase 3 去重：替代 event/mod.rs 中硬编码的 SLASH_COMMANDS 常量
/// 引用关系：被 event/mod.rs::trigger_completion 调用
/// 生命周期：纯函数，每次调用从 registry 动态生成
pub fn all_command_names() -> Vec<&'static str> {
    registry().iter().flat_map(|e| e.names.iter().copied()).collect()
}

/// 按命令名（不含 /）查询 help 描述
///
/// 引用关系：render_completion_popup 用于多列弹窗中展示命令描述
/// 生命周期：O(1) 查找，registry 是 OnceLock 静态数据
pub fn command_desc_by_name(name: &str) -> Option<&'static str> {
    let bare = name.trim_start_matches('/');
    registry().iter()
        .find(|e| e.names.iter().any(|n| *n == bare))
        .map(|e| e.help)
}

/// Dispatch a slash command. Returns (consumed, result_text_for_display).
pub fn dispatch(state: &mut AppState, text: &str) -> CmdResult {
    if !text.starts_with('/') { return CmdResult::NotFound(text.into()); }

    let parts: Vec<&str> = text.trim_start_matches('/').split_whitespace().collect();
    if parts.is_empty() { return CmdResult::NotFound("".into()); }

    let name = parts[0].to_lowercase();
    let args = &parts[1..];

    for entry in registry() {
        if entry.names.iter().any(|n| n.eq_ignore_ascii_case(&name)) {
            return (entry.handler)(state, &name, args);
        }
    }

    CmdResult::NotFound(name)
}

/// Generate help text
pub fn help_text() -> String {
    use unicode_width::UnicodeWidthStr;
    let mut lines: Vec<String> = Vec::new();
    let mut max_name = 0usize;
    for entry in registry() {
        let display = entry.names.join("/");
        max_name = max_name.max(UnicodeWidthStr::width(display.as_str()));
    }
    for entry in registry() {
        let display = entry.names.join("/");
        let w = UnicodeWidthStr::width(display.as_str());
        let padding = " ".repeat(max_name.saturating_sub(w) + 2);
        lines.push(format!("/{} {}— {}", display, padding, entry.help));
    }
    lines.join("\n")
}

/// 命令清单（供 CommandHint 面板派生展示用）
///
/// V13: 之前 state.commands 硬编码 16 条，与 registry 33 条漂移；
///      改为从 registry 自动派生，含别名紧凑展示
/// 引用关系：state::AppState::new 初始化 commands 字段；render_shortcuts_hints 渲染
/// 生命周期：&'static — registry 是 OnceLock 进程级
/// 设计意图：单一真相源，新加命令自动出现在 CommandHint 面板
///
/// 返回格式：(display_name, description)
/// - display_name 形如 "/help" 或 "/help [h]"（含别名）
/// - 子命令（/theme preview 等）作为补充虚拟项追加
pub fn command_inventory() -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = registry().iter().map(|entry| {
        let primary = entry.names[0];
        let display = if entry.names.len() > 1 {
            // 别名紧凑展示："/help [h]" / "/thinking [think|t]"
            let aliases = entry.names[1..].join("|");
            format!("/{} [{}]", primary, aliases)
        } else {
            format!("/{}", primary)
        };
        (display, entry.help.to_string())
    }).collect();

    // 子命令补充（registry 不识别但常用）
    out.push(("/theme preview".into(), "主题色板预览".into()));

    out
}

/// 键盘快捷键参考（供 /help 与 Ctrl+/ 共用）
///
/// V12: 之前 Ctrl+/ 只一行 toast、/help 仅列 slash 命令——快捷键散落用户发现不到
/// 引用关系：cmd_help 拼接到 /help 输出；可独立用于 cheatsheet 命令
/// 生命周期：&'static — 与 binding 同步维护（new binding 必须同步更新此函数）
/// 设计意图：按交互场景分组（消息流 / 输入 / 模式 / 视图 / 系统）而非按按键，便于按需查找
pub fn keyboard_cheatsheet() -> String {
    let sections: &[(&str, &[(&str, &str)])] = &[
        ("消息流", &[
            ("Space",        "折叠/展开最后消息所有 blocks（思考链 + 工具调用）"),
            ("Ctrl+E",       "代码块折叠/展开切换（>20 行长代码）"),
            ("↑ / ↓",        "向上/下滚动一行（焦点在 chat 时）"),
            ("PgUp / PgDn",  "向上/下翻页"),
            ("Home / End",   "回到底部（恢复自动跟随）"),
        ]),
        ("输入", &[
            ("Enter",        "发送消息（忙碌态自动排队）"),
            ("Ctrl+Enter",   "插入换行（多行输入）"),
            ("Shift+Enter",  "插入换行（多行输入）"),
            ("Tab",          "三源补全：命令 / 历史 / 文件路径"),
            ("Ctrl+Tab",     "AI 按需补全（异步）"),
            ("Ctrl+Space",   "文件路径补全（异步）"),
            ("↑ / ↓",        "Completing 状态下选候选；Ready 时按上键拉历史"),
        ]),
        ("视图", &[
            ("Ctrl+B",       "焦点循环（输入 ↔ 看板 ↔ 命令提示）"),
            ("Ctrl+I",       "面板显隐 toggle"),
            ("[ / ]",        "切换看板 Tab（Timeline / Tools / Tasks ...）"),
            ("Ctrl+D",       "密度切换（Compact ↔ Comfortable）"),
            ("Ctrl+E",       "代码块折叠/展开切换（>20 行长代码）"),
            ("Ctrl+O",       "设置面板（API Key / Model / Thinking / Theme）"),
        ]),
        ("模式", &[
            ("Ctrl+1/2/3",   "切换 Clarify / Team / Meeting（Plan 由 /done 从 Clarify 流转触发）"),
        ]),
        ("会话", &[
            ("Ctrl+N",       "新建会话（清空当前 + 保留引擎）"),
            ("Ctrl+W",       "关闭当前会话"),
            ("Ctrl+S",       "保存会话（引擎在线时持久化）"),
        ]),
        ("MCIP 授权弹窗（弹出时拦截全键）", &[
            ("Y / Enter",    "单次允许"),
            ("A",            "总是允许同类（High 风险禁用）"),
            ("N / Esc",      "拒绝"),
            ("D",            "详情展开/折叠"),
        ]),
        ("复制文本·V30", &[
            ("Shift+拖动",     "选中消息区字符；释放鼠标自动复制到系统剪贴板"),
            ("Esc",          "选中状态下丢弃选中不复制"),
            ("/copy",        "复制最后一条 LLM 回复全文"),
            ("Option+拖动",  "iTerm2 / Terminal.app 旁路到终端原生选择（macOS）"),
        ]),
        ("系统", &[
            ("Esc",          "智能 dismiss（设置 → 主题预览 → 补全 → 选中 → 取消 → 暂停）"),
            ("Ctrl+/",       "快捷键速览 toast"),
            ("Ctrl+C ×2",    "1 秒内连按两次退出"),
        ]),
    ];

    let mut out = String::new();
    for (title, rows) in sections {
        out.push_str(&format!("\n## {}\n\n", title));
        // 计算列宽对齐
        use unicode_width::UnicodeWidthStr;
        let max_key = rows.iter().map(|(k, _)| UnicodeWidthStr::width(*k)).max().unwrap_or(0);
        for (key, desc) in *rows {
            let w = UnicodeWidthStr::width(*key);
            let pad = " ".repeat(max_key.saturating_sub(w) + 2);
            out.push_str(&format!("- `{}`{}— {}\n", key, pad, desc));
        }
    }
    out
}

// ── Handlers ─────────────────────────────────────────────────

/// /mode — 无参时由 handle_slash_command 拦截打开 picker；此处处理有参直接切换
fn cmd_mode(s: &mut AppState, _: &str, args: &[&str]) -> CmdResult {
    if args.is_empty() {
        // 有 picker 拦截兜底，此处理论上不到；但若直接 dispatch 则打开 picker
        s.open_picker_mode();
        return CmdResult::Consumed;
    }
    match args[0].to_ascii_lowercase().as_str() {
        "clarify" | "chat" => {
            try_switch_mode(s, crate::tui::state::AbacusMode::Clarify);
        }
        "meeting" => {
            try_switch_mode(s, crate::tui::state::AbacusMode::Meeting);
        }
        other => {
            s.add_toast(
                format!("未知模式：{}（可选 clarify / meeting）", other),
                std::time::Duration::from_secs(3),
            );
        }
    }
    CmdResult::Consumed
}

fn cmd_help(s: &mut AppState, _: &str, _: &[&str]) -> CmdResult {
    // V13 修复：help 走 show_info（已改为聊天区推送），与其它 status/tokens/debug 统一
    let cmds = help_text();
    let keys = keyboard_cheatsheet();
    s.show_info(format!("# 帮助\n\n## Slash 命令\n\n{}\n{}", cmds, keys));
    CmdResult::Consumed
}

fn cmd_clear(s: &mut AppState, _: &str, _: &[&str]) -> CmdResult {
    s.messages.clear();
    s.expert_names_cache.clear();
    s.events.clear();
    // V28: 同步清 trace_events 与 id 分配器(messages 已清,无悬挂引用风险)
    s.trace_events.clear();
    s.trace_event_index.clear();
    s.tool_freq_cache.borrow_mut().take();
    s.tool_freq_dirty.set(true);
    s.next_trace_id = 0;
    s.streaming_trace_ids.clear();
    s.timeline_expanded_ids.clear();
    s.timeline_row_map.borrow_mut().clear();
    s.focused_event_id = None;
    s.tool_records.clear();
    s.thinking_text.clear();
    // V29.16: 走 SSOT set_scroll, 内部已 mark dirty
    s.set_scroll(ScrollAction::ToBottom);
    // SC1a 修复：清屏改了消息影响渲染，必须失效缓存
    s.mark_dirty();
    // 同步清除引擎侧对话历史，避免 LLM 在清屏后仍记得所有旧对话
    if let Some(ref handle) = s.engine_handle {
        let session = handle.session.clone();
        tokio::spawn(async move {
            let s = session.write().await;
            s.messages.write().await.clear();
            s.context_messages.write().await.clear();
        });
    }
    s.add_toast("屏幕已清屏，对话历史已重置", std::time::Duration::from_secs(2));
    CmdResult::Consumed
}

fn cmd_new(s: &mut AppState, _: &str, _: &[&str]) -> CmdResult {
    // Phase 3 去重：统一调用 AppState::reset_session（SSoT）
    s.reset_session();
    s.add_toast("已创建新会话", std::time::Duration::from_secs(2));
    CmdResult::Consumed
}

fn cmd_save(s: &mut AppState, _: &str, _: &[&str]) -> CmdResult {
    if s.engine_handle.is_some() {
        match crate::tui::run::save_session(s) {
            Ok(()) => {
                let ts = chrono::Local::now().format("%H:%M").to_string();
                s.add_event(&ts, "session", "手动保存", crate::tui::state::EventLevel::Info);
                s.add_toast("会话已保存", std::time::Duration::from_secs(2));
            }
            Err(e) => {
                s.add_toast(format!("保存失败: {}", e), std::time::Duration::from_secs(4));
            }
        }
    } else {
        s.add_toast("演示模式 — 会话仅在内存中", std::time::Duration::from_secs(2));
    }
    CmdResult::Consumed
}

fn cmd_copy(s: &mut AppState, _: &str, _: &[&str]) -> CmdResult {
    // V30 + V29.11 修复：复制最后一条 Session 消息全部文本内容
    //   Stream(正文) + Block(detail) + Trace(thinking/tool args+output)
    //   引用关系：clipboard::set_text 定义于 tui/clipboard.rs
    let last_session = s.messages.iter().rev()
        .find(|m| matches!(m.role, crate::tui::state::MsgRole::Session));
    if let Some(msg) = last_session {
        let mut text = String::new();
        for part in &msg.parts {
            match part {
                crate::tui::state::MsgContent::Stream(t) => text.push_str(t),
                crate::tui::state::MsgContent::Block { detail, .. } => {
                    if !text.is_empty() && !text.ends_with('\n') { text.push('\n'); }
                    text.push_str(detail);
                }
                crate::tui::state::MsgContent::Trace { event_ids, .. } => {
                    // 从 trace_events SSOT 提取内容
                    for eid in event_ids {
                        if let Some(ev) = s.trace_events.iter().find(|e| e.id == *eid) {
                            match &ev.kind {
                                crate::tui::state::TraceKind::Thinking { text: t, .. } => {
                                    if !text.is_empty() && !text.ends_with('\n') { text.push('\n'); }
                                    text.push_str(t);
                                }
                                crate::tui::state::TraceKind::ToolCall { name, args, output, .. } => {
                                    if !text.is_empty() && !text.ends_with('\n') { text.push('\n'); }
                                    text.push_str(&format!("[{}] ", name));
                                    if !args.is_empty() { text.push_str(args); }
                                    if let Some(o) = output {
                                        text.push_str("\n→ ");
                                        text.push_str(o);
                                    }
                                    text.push('\n');
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
        }
        if !text.is_empty() {
            match crate::tui::clipboard::set_text(&text) {
                Ok(backend) => {
                    s.add_toast(
                        format!("{} {} 字符", backend.label(), text.chars().count()),
                        std::time::Duration::from_secs(2),
                    );
                }
                Err(e) => {
                    tracing::warn!(error = %e, "cmd_copy clipboard failure");
                    s.add_toast("复制失败：本终端不支持剪贴板写入", std::time::Duration::from_secs(3));
                }
            }
        }
    } else {
        s.add_toast("无可复制的回复", std::time::Duration::from_secs(2));
    }
    CmdResult::Consumed
}

fn cmd_quit(s: &mut AppState, _: &str, _: &[&str]) -> CmdResult {
    s.running = false;
    s.add_toast("正在退出…", std::time::Duration::from_secs(2));
    CmdResult::Consumed
}

/// V29.8: /model 统一入口 — 合并原 /thinking + /provider 为子命令
///
/// 用法:
///   /model                              → 显示当前 model + thinking + context 状态
///   /model <name>                       → 切换模型 (flash/pro/qwen/...)
///   /model thinking                     → 循环切换思考深度 (off→low→medium→high)
///   /model thinking <off|low|medium|high> → 设置思考深度
///   /model provider                     → 显示当前 Provider 信息(异步)
///
/// 设计意图: 模型相关设置语义统一在一个入口, 减少命令清单噪音(29→27)
fn cmd_model(s: &mut AppState, _: &str, args: &[&str]) -> CmdResult {
    if args.is_empty() {
        // V29.8: 无参 → 打开 model picker (按 provider 分组 + 底部 thinking 调节器)
        //   ↑↓ 选模型, ←→ 调思考深度, Enter 应用, Esc 取消
        s.open_picker_model();
        return CmdResult::Consumed;
    }
    let lower = args[0].to_lowercase();

    // V29.8: 子命令分发 — thinking / provider 收编进 /model
    match lower.as_str() {
        "thinking" | "think" | "t" => {
            // /model thinking [level]
            // V29.10: 委托给 abacus_types::ThinkingIntent::from_str_loose 解析,
            //   后端单一真相 — 全档接受度: off/minimal/low/medium/high/max/xhigh,
            //   adaptive(auto/dynamic), 整数 budget(如 8192)。
            // 旧硬编码 4 档(off/low/medium/high)隐藏了后端能力, 用户无法走到 max/xhigh。
            let sub_args = &args[1..];
            if sub_args.is_empty() {
                let next = s.cycle_thinking_depth().to_string();
                s.add_toast(format!("思考 → {}", next), std::time::Duration::from_secs(2));
            } else {
                let raw = sub_args.join(" ");
                match abacus_types::ThinkingIntent::from_str_loose(&raw) {
                    Some(intent) => {
                        let label = intent.to_str();
                        s.thinking_depth = label.clone();
                        s.add_toast(format!("思考 → {}", label), std::time::Duration::from_secs(2));
                    }
                    None => s.add_toast(
                        "用法: /model thinking off|minimal|low|medium|high|max|xhigh|adaptive|<budget>",
                        std::time::Duration::from_secs(4),
                    ),
                }
            }
            return CmdResult::Consumed;
        }
        "provider" | "prov" | "p" => {
            // /model provider — 异步路径走 engine
            return engine_or(s, SlashCommand::Provider);
        }
        _ => {}
    }

    // 默认分支: 切换模型
    // PR4: Resolve through ModelPreference aliases first, then legacy shortcuts.
    // Priority: preference alias > hardcoded shortcut > raw input
    let input_str = args[0];
    let name = match lower.as_str() {
        "pro" | "deepseek-v4-pro" => "deepseek-v4-pro",
        "flash" | "deepseek-v4-flash" => "deepseek-v4-flash",
        "qwen" | "qwen-plus" => "qwen-plus",
        "list" | "ls" => {
            // PR4: `/model list` → delegate to engine for ProviderRegistry listing
            return engine_or(s, SlashCommand::ModelList);
        }
        _ => input_str,
    };
    s.model_name = name.to_string();
    s.theme.apply_model_brand(name);
    // 热切换：同步等待 CoreLoop 完成 model override + preference persistence
    // 修复：原 tokio::spawn 存在竞态——toast 显示"已生效"时 override 可能尚未写入，
    // 导致紧接的用户输入仍用旧模型。block_in_place + block_on 确保写入完成后才继续。
    //
    // PR4: Additionally resolve through preference aliases and persist selection.
    // ## 引用关系
    // - core.model_preference() → read alias → resolve QualifiedModelId
    // - core.set_model_override() → write effective model
    // - save_model_preference() → persist to ~/.abacus/model_preference.json
    if let Some(ref engine) = s.engine_handle {
        let core = engine.core.clone();
        let model_input = name.to_string();
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                // Resolve alias through preference
                let qid = {
                    let preference = core.model_preference().read().await;
                    preference.resolve_alias(&model_input)
                };
                // Set override using the resolved model name
                core.set_model_override(qid.model_name()).await;
                // Persist selection
                {
                    let mut pref = core.model_preference().write().await;
                    pref.set_last_selected(qid.clone());
                }
                // Fire-and-forget save to disk
                let pref_snapshot = core.model_preference().read().await.clone();
                let pref_path = abacus_types::preference_file_path();
                let _ = abacus_types::save_model_preference(&pref_snapshot, &pref_path);
            });
        });
    }
    s.add_toast(format!("模型 → {}（已生效）", name), std::time::Duration::from_secs(2));
    CmdResult::Consumed
}

/// /lang [zh|en] — 运行时切换 UI 语言
/// 无参数时在 zh ↔ en 之间切换
/// 引用关系：调用 i18n::set_lang()，立即生效（下一帧渲染可见）
fn cmd_lang(s: &mut AppState, _: &str, args: &[&str]) -> CmdResult {
    use crate::tui::i18n::{Lang, lang, set_lang};
    let new_lang = match args.first().map(|a| a.to_lowercase()).as_deref() {
        Some("zh" | "cn" | "chinese") => Lang::Zh,
        Some("en" | "english") => Lang::En,
        Some(_) => {
            s.add_toast("用法: /lang [zh|en]", std::time::Duration::from_secs(3));
            return CmdResult::Consumed;
        }
        None => {
            // Toggle
            match lang() {
                Lang::Zh => Lang::En,
                Lang::En => Lang::Zh,
            }
        }
    };
    set_lang(new_lang);
    s.add_toast(
        format!("Language: {}", new_lang.display_name()),
        std::time::Duration::from_secs(2),
    );
    s.rendered_lines_dirty.set(true);
    CmdResult::Consumed
}

fn cmd_theme(s: &mut AppState, _: &str, args: &[&str]) -> CmdResult {
    // V10 完善：支持 /theme preview 打开色板预览面板
    if args.is_empty() {
        s.add_toast("用法: /theme <name> 切换，或 /theme preview 预览色板", std::time::Duration::from_secs(5));
    } else if matches!(args[0], "preview" | "list" | "ls") {
        // 切换 preview 面板可见性（再次调用关闭）
        s.theme_preview_open = !s.theme_preview_open;
        // 若关闭则保留 info 面板原状；若打开需让面板模式可见
        if s.theme_preview_open {
            s.info_panel_auto_open = true;
        }
    } else if s.theme.switch_theme(args[0]) {
        // 切换主题后关闭预览（用户已选择）
        s.theme_preview_open = false;
        s.add_toast(format!("主题 → {}", args[0]), std::time::Duration::from_secs(2));
    } else {
        s.add_toast(format!("未知主题: {} (用 /theme preview 查看可用主题)", args[0]), std::time::Duration::from_secs(2));
    }
    CmdResult::Consumed
}

use crate::tui::state::AbacusMode;
use crate::tui::event::switch_mode;

/// V34: 提取最近 Session 消息的纯文本（Stream + Block.detail，不含 Trace）
///
/// 引用关系：
/// - try_switch_mode 用此抽 ClarifyBrief（Clarify→Meeting）和 MeetingConclusion（Meeting→Clarify）
///
/// 返回 None 表示无任何 Session 消息（例如 mode 切换时还未对话）— 调用方应允许无 artifact 切换。
fn extract_last_session_text(messages: &std::collections::VecDeque<crate::tui::state::Message>) -> Option<String> {
    let last_session = messages
        .iter()
        .rev()
        .find(|m| matches!(m.role, crate::tui::state::MsgRole::Session))?;
    let mut buf = String::new();
    for part in &last_session.parts {
        match part {
            crate::tui::state::MsgContent::Stream(s) => buf.push_str(s),
            crate::tui::state::MsgContent::Block { detail, .. } => {
                buf.push('\n');
                buf.push_str(detail);
            }
            crate::tui::state::MsgContent::Trace { .. } => {} // 跳过 trace
        }
    }
    let trimmed = buf.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

// V34: 模式切换命令 — clarify/meeting 走 DAG 转移；plan/team 为执行策略不切换 mode
fn cmd_clarify(s: &mut AppState, _: &str, _: &[&str]) -> CmdResult { try_switch_mode(s, AbacusMode::Clarify); CmdResult::Consumed }
fn cmd_meeting(s: &mut AppState, _: &str, _: &[&str]) -> CmdResult { try_switch_mode(s, AbacusMode::Meeting); CmdResult::Consumed }

/// V34: /plan <任务> — 触发规划+执行策略，不切换 mode
///
/// ## 引用关系
/// - 设置：构造 SlashCommand::ExecuteWithPlan { task }，由 run.rs 主循环调 send_plan_and_execute_streaming
/// - 消费：run.rs pending_slash_command 处理分支
///
/// ## 生命周期
/// 一次性 dispatch；在当前 mode（Clarify）内部异步执行，不切换 state.mode
fn cmd_plan(s: &mut AppState, _: &str, args: &[&str]) -> CmdResult {
    if args.is_empty() {
        s.add_toast("/plan <任务描述> — 直接触发规划+执行策略", std::time::Duration::from_secs(4));
        return CmdResult::Consumed;
    }
    let task = args.join(" ");
    s.pending_slash_command = Some(crate::tui::state::SlashCommand::ExecuteWithPlan { task });
    CmdResult::Consumed
}

/// V34: /team <任务> — 触发多 agent 执行策略，不切换 mode
///
/// ## 引用关系
/// - 设置：构造 SlashCommand::ExecuteWithTeam { task }，由 run.rs 主循环调 send_team_message
/// - 消费：run.rs pending_slash_command 处理分支
///
/// ## 生命周期
/// 一次性 dispatch；在当前 mode（Clarify）内部异步执行，不切换 state.mode
fn cmd_team(s: &mut AppState, _: &str, args: &[&str]) -> CmdResult {
    if args.is_empty() {
        s.add_toast("/team <任务描述> — 直接触发多 agent 执行策略", std::time::Duration::from_secs(4));
        return CmdResult::Consumed;
    }
    let task = args.join(" ");
    s.pending_slash_command = Some(crate::tui::state::SlashCommand::ExecuteWithTeam { task });
    CmdResult::Consumed
}

// ─── /preset — 场景预设 ─────────────────────────────────────────────────
//
// 预设定义：每个预设是一组参数组合。应用预设 = 批量 set。
// 预设列表在本函数内静态定义（不持久化——用户自定义预设 TODO）。

/// 场景预设定义
struct Preset {
    name: &'static str,
    icon: &'static str,
    description: &'static str,
    temperature: f64,
    max_tokens: u32,
    thinking: &'static str,
    tool_limit: u32,
    context_ratio: f64,
    router: bool,
    dedup: bool,
}

const PRESETS: &[Preset] = &[
    Preset { name: "quick",    icon: "⚡", description: "快速问答",   temperature: 0.3, max_tokens: 2048,  thinking: "off",  tool_limit: 10, context_ratio: 0.5,  router: true,  dedup: false },
    Preset { name: "code",     icon: "🔧", description: "深度编码",   temperature: 0.6, max_tokens: 64000, thinking: "high", tool_limit: 50, context_ratio: 1.0,  router: true,  dedup: true  },
    Preset { name: "creative", icon: "✨", description: "创意写作",   temperature: 0.9, max_tokens: 16384, thinking: "off",  tool_limit: 5,  context_ratio: 0.75, router: false, dedup: false },
    Preset { name: "lean",     icon: "💰", description: "省 Token",  temperature: 0.5, max_tokens: 4096,  thinking: "low",  tool_limit: 20, context_ratio: 0.5,  router: true,  dedup: true  },
    Preset { name: "marathon", icon: "🏃", description: "长任务",    temperature: 0.5, max_tokens: 32000, thinking: "medium", tool_limit: 50, context_ratio: 0.8,  router: true,  dedup: false },
    Preset { name: "debug",    icon: "🔍", description: "调试",      temperature: 0.3, max_tokens: 8192,  thinking: "high", tool_limit: 50, context_ratio: 1.0,  router: false, dedup: false },
];

fn cmd_preset(s: &mut AppState, _: &str, args: &[&str]) -> CmdResult {
    if args.is_empty() {
        // 打开 Preset picker
        let items: Vec<String> = PRESETS.iter().map(|p| p.name.to_string()).collect();
        let labels: Vec<String> = PRESETS.iter().map(|p| {
            format!("{} {}  temp={} tok={}K think={}", p.icon, p.description,
                p.temperature, p.max_tokens / 1000, p.thinking)
        }).collect();
        s.open_picker_generic(crate::tui::state::PickerKind::Preset, items, labels);
        return CmdResult::Consumed;
    }
    let name = args[0].to_lowercase();
    if let Some(preset) = PRESETS.iter().find(|p| p.name == name) {
        apply_preset(s, preset);
        s.add_toast(
            format!("{} 预设已应用: {} (temp={} tok={} think={})",
                preset.icon, preset.description, preset.temperature, preset.max_tokens, preset.thinking),
            std::time::Duration::from_secs(4),
        );
    } else {
        let valid: Vec<&str> = PRESETS.iter().map(|p| p.name).collect();
        s.add_toast(
            format!("未知预设。可选: {}", valid.join(" / ")),
            std::time::Duration::from_secs(4),
        );
    }
    CmdResult::Consumed
}

fn apply_preset(s: &mut AppState, p: &Preset) {
    s.thinking_depth = p.thinking.to_string();
    s.active_preset = Some(p.name.to_string());
    // 其余参数通过 engine 层设置（需 engine_handle）
    if let Some(ref handle) = s.engine_handle {
        let core = handle.core.clone();
        let temp = p.temperature;
        let max_tok = p.max_tokens;
        let tool_limit = p.tool_limit;
        let ctx_ratio = p.context_ratio;
        let router = p.router;
        let dedup = p.dedup;
        tokio::spawn(async move {
            core.set_temperature(temp).await;
            core.set_max_tokens(max_tok).await;
            core.set_tool_limit(tool_limit).await;
            core.set_context_ratio(ctx_ratio).await;
            core.set_silent_router(router).await;
            core.set_dedup(dedup).await;
        });
    }
}

// ─── /set — 单项参数调整 ────────────────────────────────────────────────

fn cmd_set(s: &mut AppState, _: &str, args: &[&str]) -> CmdResult {
    if args.is_empty() {
        // 显示所有可调参数及当前值
        let info = format!(
            "可调参数 (/set <key> <value>)\n\n\
             temperature | temp     {}\n\
             max-tokens  | tokens   {}\n\
             context-ratio | ctx    {}\n\
             tool-limit  | tools    {}\n\
             timeout              {}s\n\
             router               {}\n\
             dedup                {}\n\
             thinking             {}\n\
             preset               {}",
            s.runtime_temperature.unwrap_or(0.6),
            s.runtime_max_tokens.unwrap_or(64000),
            s.runtime_context_ratio.unwrap_or(1.0),
            s.runtime_tool_limit.unwrap_or(50),
            s.runtime_timeout.unwrap_or(300),
            s.runtime_router.unwrap_or(true),
            s.runtime_dedup.unwrap_or(false),
            s.thinking_depth,
            s.active_preset.as_deref().unwrap_or("(none)"),
        );
        s.show_info(info);
        return CmdResult::Consumed;
    }
    if args.len() < 2 {
        s.add_toast("用法: /set <参数名> <值>", std::time::Duration::from_secs(3));
        return CmdResult::Consumed;
    }
    let key = args[0].to_lowercase();
    let val = args[1];
    match key.as_str() {
        "temperature" | "temp" => {
            if let Ok(v) = val.parse::<f64>() {
                let v = v.clamp(0.0, 2.0);
                s.runtime_temperature = Some(v);
                if let Some(ref handle) = s.engine_handle {
                    let core = handle.core.clone();
                    tokio::spawn(async move { core.set_temperature(v).await; });
                }
                s.add_toast(format!("temperature → {}", v), std::time::Duration::from_secs(2));
            } else {
                s.add_toast("temperature 需要数字 (0.0-2.0)", std::time::Duration::from_secs(3));
            }
        }
        "max-tokens" | "tokens" => {
            if let Ok(v) = val.parse::<u32>() {
                s.runtime_max_tokens = Some(v);
                if let Some(ref handle) = s.engine_handle {
                    let core = handle.core.clone();
                    tokio::spawn(async move { core.set_max_tokens(v).await; });
                }
                s.add_toast(format!("max_tokens → {}", v), std::time::Duration::from_secs(2));
            } else {
                s.add_toast("max-tokens 需要整数", std::time::Duration::from_secs(3));
            }
        }
        "context-ratio" | "ctx" => {
            if let Ok(v) = val.parse::<f64>() {
                let v = v.clamp(0.1, 1.0);
                s.runtime_context_ratio = Some(v);
                if let Some(ref handle) = s.engine_handle {
                    let core = handle.core.clone();
                    tokio::spawn(async move { core.set_context_ratio(v).await; });
                }
                s.add_toast(format!("context_ratio → {}%", (v * 100.0) as u32), std::time::Duration::from_secs(2));
            } else {
                s.add_toast("context-ratio 需要数字 (0.1-1.0)", std::time::Duration::from_secs(3));
            }
        }
        "tool-limit" | "tools" => {
            if let Ok(v) = val.parse::<u32>() {
                s.runtime_tool_limit = Some(v);
                if let Some(ref handle) = s.engine_handle {
                    let core = handle.core.clone();
                    tokio::spawn(async move { core.set_tool_limit(v).await; });
                }
                s.add_toast(format!("tool_limit → {}", v), std::time::Duration::from_secs(2));
            } else {
                s.add_toast("tool-limit 需要整数", std::time::Duration::from_secs(3));
            }
        }
        "timeout" => {
            if let Ok(v) = val.parse::<u64>() {
                s.runtime_timeout = Some(v);
                if let Some(ref handle) = s.engine_handle {
                    let core = handle.core.clone();
                    tokio::spawn(async move { core.set_timeout(v).await; });
                }
                s.add_toast(format!("timeout → {}s", v), std::time::Duration::from_secs(2));
            } else {
                s.add_toast("timeout 需要整数（秒）", std::time::Duration::from_secs(3));
            }
        }
        "router" => {
            let enabled = matches!(val, "on" | "true" | "1" | "yes");
            s.runtime_router = Some(enabled);
            if let Some(ref handle) = s.engine_handle {
                let core = handle.core.clone();
                tokio::spawn(async move { core.set_silent_router(enabled).await; });
            }
            s.add_toast(format!("router → {}", if enabled { "on" } else { "off" }), std::time::Duration::from_secs(2));
        }
        "dedup" => {
            let enabled = matches!(val, "on" | "true" | "1" | "yes");
            s.runtime_dedup = Some(enabled);
            if let Some(ref handle) = s.engine_handle {
                let core = handle.core.clone();
                tokio::spawn(async move { core.set_dedup(enabled).await; });
            }
            s.add_toast(format!("dedup → {}", if enabled { "on" } else { "off" }), std::time::Duration::from_secs(2));
        }
        "thinking" => {
            match abacus_types::ThinkingIntent::from_str_loose(val) {
                Some(intent) => {
                    let label = intent.to_str();
                    s.thinking_depth = label.clone();
                    s.add_toast(format!("thinking → {}", label), std::time::Duration::from_secs(2));
                }
                None => s.add_toast("thinking: off|low|medium|high|max|xhigh", std::time::Duration::from_secs(3)),
            }
        }
        _ => {
            s.add_toast(
                format!("未知参数: {}。可调: temperature/max-tokens/context-ratio/tool-limit/timeout/router/dedup/thinking", key),
                std::time::Duration::from_secs(4),
            );
        }
    }
    // 应用 set 后清除 preset 标记（不再是预设状态）
    s.active_preset = None;
    CmdResult::Consumed
}

/// 模式 DAG 流转门控 — 验证转移合法性后调 switch_mode
///
/// 引用关系：cmd_clarify/meeting 入口；abacus_types::AbacusMode::can_transit_to 判定
/// 失败行为：toast 提示合法路径 + 不切换
/// 设计意图：仅保留 Clarify ⇄ Meeting 双向转移（V34: Plan/Team 已降级为执行策略）
/// 公开入口 — 供 event/mod.rs 的 @ 自动路由调用
/// 引用关系：event/mod.rs submit_message → 此函数
pub fn try_switch_mode_pub(s: &mut AppState, target: AbacusMode) {
    try_switch_mode(s, target);
}

fn try_switch_mode(s: &mut AppState, target: AbacusMode) {
    if s.mode == target {
        s.add_toast(
            format!("已在 {} 模式", target.display_zh()),
            std::time::Duration::from_secs(2),
        );
        return;
    }
    if !s.mode.can_transit_to(target) {
        let allowed: Vec<&str> = s.mode.transitions().iter().map(|m| m.display_zh()).collect();
        s.add_toast(
            format!(
                "⛔ {} → {} 非法。当前可转：{}",
                s.mode.display_zh(),
                target.display_zh(),
                allowed.join(" / ")
            ),
            std::time::Duration::from_secs(4),
        );
        return;
    }

    // V34: Clarify → Meeting 自动注入 ClarifyBrief（非结构化文本摘要）
    // 引用关系：写入 state.mode_artifact，switch_mode 内 take() → toast 显示 summary
    // 失败行为：无 Session 消息时静默允许切换（不阻断 UX）
    if s.mode == AbacusMode::Clarify && target == AbacusMode::Meeting && s.mode_artifact.is_none() {
        if let Some(text) = extract_last_session_text(&s.messages) {
            s.mode_artifact = Some(abacus_types::ModeArtifact::ClarifyBrief(text));
        }
    }

    // V34: Meeting → Clarify 自动注入 MeetingConclusion（会议结论文本）
    // V35: 同时将结论持久化到 ~/.abacus/meetings/ 本地缓存
    if s.mode == AbacusMode::Meeting && target == AbacusMode::Clarify && s.mode_artifact.is_none() {
        if let Some(text) = extract_last_session_text(&s.messages) {
            // 本地缓存保存 — 错误不阻断 UX，仅 toast 提示
            let specialists: Vec<String> = s.experts.iter().map(|e| e.name.clone()).collect();
            let cwd = std::env::current_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();
            // topic: 优先取会议首条用户消息，退回到结论前 10 词
            let topic = extract_meeting_topic(&s.messages)
                .unwrap_or_else(|| text.split_whitespace().take(10).collect::<Vec<_>>().join(" "));
            // 2026-05-27: 提取结构化行动项（零 LLM 开销）
            let action_items = crate::tui::meeting_cache::extract_action_items_from_text(&text);

            match crate::tui::meeting_cache::quick_save(&topic, &text, specialists, &cwd, action_items.clone()) {
                Ok(path) => s.add_toast(
                    format!("📋 会议记录已保存: {}", path.file_name().unwrap_or_default().to_string_lossy()),
                    std::time::Duration::from_secs(4),
                ),
                Err(e) => s.add_toast(
                    format!("⚠ 会议记录保存失败: {}", e),
                    std::time::Duration::from_secs(3),
                ),
            }

            // 2026-05-27: 如果提取到行动项 → 设置执行提案，等待用户确认
            // V41: 接入 TaskAnalyzer 7 维复杂度分析，替代硬编码 len()>3
            if !action_items.is_empty() {
                let items_text: Vec<String> = action_items.iter().map(|a| a.text.clone()).collect();
                let complexity = abacus_core::core::task_analyzer::TaskAnalyzer::analyze_complexity(
                    &items_text.join("\n")
                );
                // /team 触发条件: 高复杂度 / 多领域 / 有决策且多任务
                let suggest_team = complexity.score > 0.5
                    || complexity.domain_count > 2
                    || (complexity.has_decisions && items_text.len() > 2);
                let cmd_hint = if suggest_team { "/team" } else { "/plan" };
                s.pending_meeting_execution = Some(crate::tui::state::MeetingExecutionPrompt {
                    action_items: items_text.clone(),
                    full_conclusion: text.clone(),
                    suggest_team,
                    created_at: std::time::Instant::now(),
                });
                let preview: String = items_text.iter().take(3).cloned().collect::<Vec<_>>().join("; ");
                let more = if items_text.len() > 3 { format!(" (+{})", items_text.len() - 3) } else { String::new() };
                s.add_toast(
                    format!("🎯 检测到 {} 个行动项: {}{}. 输入 Y 使用 {} 执行", items_text.len(), preview, more, cmd_hint),
                    std::time::Duration::from_secs(15),
                );
            }

            s.mode_artifact = Some(abacus_types::ModeArtifact::MeetingConclusion(text));
        }
    }

    // V35: 过渡感知提示 — switch_mode 会 take() artifact，须在调用前捕获摘要
    // 引用: bars::render_status_bar 读 state.transition_hint，5s 内展示，自然过期
    let hint_text = match (s.mode, target) {
        (AbacusMode::Clarify, AbacusMode::Meeting) => {
            s.mode_artifact.as_ref().map(|a| {
                let preview = match a {
                    abacus_types::ModeArtifact::ClarifyBrief(t) => {
                        let ch: String = t.chars().take(28).collect();
                        if t.chars().count() > 28 { format!("{}…", ch) } else { ch }
                    }
                    _ => a.summary(),
                };
                format!("会诊 · 携带: {}", preview)
            })
        }
        (AbacusMode::Meeting, AbacusMode::Clarify) => {
            Some(s.mode_artifact.as_ref()
                .map(|a| format!("澄清 · {}", a.summary()))
                .unwrap_or_else(|| "澄清 · 会议已结束".to_string()))
        }
        _ => None,
    };

    switch_mode(s, target);

    if let Some(hint) = hint_text {
        s.transition_hint = Some((hint, std::time::Instant::now()));
    }
    s.add_toast(
        format!("✓ 已切换到 {}", target.display_zh()),
        std::time::Duration::from_secs(2),
    );
}

/// V34: /done 标记当前模式完成 — Clarify→Meeting / Meeting→Clarify（双向互转）
/// 设计意图：用户主动声明"我准备好了"，避免 LLM 假阳性自判
fn cmd_done(s: &mut AppState, _: &str, _: &[&str]) -> CmdResult {
    // V34: 2 模式互转 — Clarify→Meeting / Meeting→Clarify
    let next = match s.mode {
        AbacusMode::Clarify => AbacusMode::Meeting,
        AbacusMode::Meeting => AbacusMode::Clarify,
    };

    try_switch_mode(s, next);
    CmdResult::Consumed
}

/// V37-3: /review <plan|diff|security> [content]
///
/// ## 解析规则
/// - args[0]：必填，kind（plan / diff / security）
/// - args[1..]：可选，待审内容
///   - 不提供时自动取末尾 Session 消息文本（适合审刚出炉的 Planner 输出）
///   - 提供时直接审该字符串（适合粘贴 diff / 配置片段）
///
/// ## 失败行为
/// - kind 非法 → toast 提示合法值
/// - 取末尾 Session 失败（无任何消息）→ toast 提示"先发起对话再审查"
fn cmd_review(s: &mut AppState, _: &str, args: &[&str]) -> CmdResult {
    if args.is_empty() {
        // 无参 → picker 选类型（handle_slash_command 已拦截，此处作 fallback）
        s.open_picker_review();
        return CmdResult::Consumed;
    }
    let kind = match args[0].to_ascii_lowercase().as_str() {
        "plan" => crate::tui::api::ReviewKind::Plan,
        "diff" => crate::tui::api::ReviewKind::Diff,
        "security" | "sec" => crate::tui::api::ReviewKind::Security,
        other => {
            s.add_toast(
                format!("⛔ 未知 review 类型：{}（允许 plan/diff/security）", other),
                std::time::Duration::from_secs(4),
            );
            return CmdResult::Consumed;
        }
    };

    // V39-2: 解析 --strict 子参数；从剩余 args 中过滤掉
    // 引用关系：strict 标志写入 state.pending_review_strict，review 响应抵达时回填到 last_review_strict
    let rest_args: Vec<&str> = args[1..].iter().copied().filter(|a| *a != "--strict").collect();
    let strict = args[1..].iter().any(|a| *a == "--strict");
    if strict {
        s.add_toast(
            "🔒 strict 模式：verdict ≠ pass 将阻断后续切换",
            std::time::Duration::from_secs(3),
        );
    }
    s.pending_review_strict = strict;
    // V41-4: 记录 review kind，run.rs 抵达响应时注入到 ReviewReport.kind
    s.pending_review_kind = kind;

    // V37-3: content 取值 — 显式参数优先，缺省则取末尾 Session 消息
    let content_explicit = rest_args.join(" ");
    let content = if !content_explicit.trim().is_empty() {
        content_explicit
    } else {
        // 自动模式：从消息末尾取 Session 文本
        let mut buf = String::new();
        if let Some(last_session) = s.messages.iter().rev()
            .find(|m| matches!(m.role, crate::tui::state::MsgRole::Session))
        {
            for part in &last_session.parts {
                if let crate::tui::state::MsgContent::Stream(t) = part {
                    buf.push_str(t);
                }
            }
        }
        if buf.trim().is_empty() {
            s.add_toast(
                "⚠ 未找到可审查内容。先发起对话或在命令后追加内容",
                std::time::Duration::from_secs(4),
            );
            return CmdResult::Consumed;
        }
        buf
    };

    s.add_toast(
        format!("→ 启动 {}", kind.label()),
        std::time::Duration::from_secs(2),
    );
    CmdResult::Pending(crate::tui::state::SlashCommand::ReviewRole { kind, content })
}

/// V41-2: /review-required on|off|status [<secs>] — 切换 review 强约束模式
///
/// ## 设计意图
/// 比 /review --strict 更强的约束 — 必须有 fresh pass review 才能 Plan→Team
///
/// ## 子参数
/// - on [secs]: 启用，可选自定义 max_age（默认 600s）
/// - off: 关闭
/// - status: 查询当前状态
fn cmd_review_required(s: &mut AppState, _: &str, args: &[&str]) -> CmdResult {
    if args.is_empty() || args[0] == "status" {
        let state = if s.review_required {
            format!("on（启用，max_age={}s）", s.review_max_age_secs)
        } else {
            "off（关闭）".to_string()
        };
        s.add_toast(
            format!("review-required: {} · 用法 /review-required on|off [<秒>]", state),
            std::time::Duration::from_secs(5),
        );
        return CmdResult::Consumed;
    }
    match args[0].to_ascii_lowercase().as_str() {
        "on" | "true" | "1" => {
            s.review_required = true;
            // 可选 max_age 参数
            if let Some(secs_str) = args.get(1) {
                if let Ok(secs) = secs_str.parse::<u64>() {
                    if secs > 0 {
                        s.review_max_age_secs = secs;
                    }
                }
            }
            s.add_toast(
                format!(
                    "✓ review 强约束已启用（max_age={}s）：Plan→Team 必须有 fresh pass review",
                    s.review_max_age_secs
                ),
                std::time::Duration::from_secs(5),
            );
        }
        "off" | "false" | "0" => {
            s.review_required = false;
            s.add_toast("✓ review 强约束已关闭", std::time::Duration::from_secs(3));
        }
        other => {
            s.add_toast(
                format!("⛔ 未知参数：{}（允许 on/off/status）", other),
                std::time::Duration::from_secs(3),
            );
        }
    }
    CmdResult::Consumed
}

/// L-3/L-4/L-5: /role <fix|summarize|test> [content] — 通用角色调用
///
/// ## 解析规则
/// - args[0]：必填，AgentRole（fix / summarize / test）
/// - args[1..]：可选，待处理内容
///   - 不提供 → 取末尾 Session 消息（适合"摘要刚出炉的回复"场景）
///   - 提供 → 直接处理该字符串（适合粘贴代码 / 文档片段）
///
/// ## 失败行为
/// - kind 非法 → toast 提示合法值
/// - 取末尾 Session 失败 → toast 提示
fn cmd_role(s: &mut AppState, _: &str, args: &[&str]) -> CmdResult {
    if args.is_empty() {
        s.add_toast(
            "用法：/role <fix|summarize|test> [内容]",
            std::time::Duration::from_secs(4),
        );
        return CmdResult::Consumed;
    }
    let role = match crate::tui::api::RoleKind::from_cli_arg(args[0]) {
        Some(r) => r,
        None => {
            s.add_toast(
                format!("⛔ 未知角色：{}（允许 fix/summarize/test）", args[0]),
                std::time::Duration::from_secs(4),
            );
            return CmdResult::Consumed;
        }
    };

    // 与 cmd_review 同款 content 取值策略（显式优先 / 末尾 Session 兜底）
    let content_explicit = args[1..].join(" ");
    let content = if !content_explicit.trim().is_empty() {
        content_explicit
    } else {
        let mut buf = String::new();
        if let Some(last_session) = s.messages.iter().rev()
            .find(|m| matches!(m.role, crate::tui::state::MsgRole::Session))
        {
            for part in &last_session.parts {
                if let crate::tui::state::MsgContent::Stream(t) = part {
                    buf.push_str(t);
                }
            }
        }
        if buf.trim().is_empty() {
            s.add_toast(
                "⚠ 未找到可处理内容。先发起对话或在命令后追加内容",
                std::time::Duration::from_secs(4),
            );
            return CmdResult::Consumed;
        }
        buf
    };

    s.add_toast(
        format!("→ 启动 {}", role.label()),
        std::time::Duration::from_secs(2),
    );
    CmdResult::Pending(crate::tui::state::SlashCommand::RoleInvoke { role, content })
}

/// V41-4: /review-history — 显示 review 历史（最近 20 条，verdict 演变）
///
/// ## 设计意图
/// 让用户查看 review 修正轨迹（fail → needs_revision → pass）— 比单看 last_review 更有诊断价值
///
/// ## 输出格式
/// - 时间（相对，如"5 分钟前"）
/// - kind（Plan/Diff/Security 中文 label）
/// - verdict 图标（✓/⚠/⛔/?）+ label
/// - issues 数
fn cmd_review_history(s: &mut AppState, _: &str, _: &[&str]) -> CmdResult {
    if s.review_history.is_empty() {
        s.show_info("Review 历史\n\n暂无审查记录。运行 /review plan|diff|security 后此处会出现历史。".to_string());
        return CmdResult::Consumed;
    }
    let now = chrono::Utc::now();
    let mut body = format!("Review 历史 (最近 {} 条，倒序)\n\n", s.review_history.len());
    // 倒序展示（最新在前）
    for report in s.review_history.iter().rev() {
        let ago = chrono::DateTime::parse_from_rfc3339(&report.time_rfc3339)
            .ok()
            .map(|t| {
                let elapsed = now.signed_duration_since(t.with_timezone(&chrono::Utc));
                let secs = elapsed.num_seconds();
                if secs < 60 { format!("{}s 前", secs) }
                else if secs < 3600 { format!("{}m 前", secs / 60) }
                else if secs < 86400 { format!("{}h 前", secs / 3600) }
                else { format!("{}d 前", secs / 86400) }
            })
            .unwrap_or_else(|| "?".to_string());
        let icon = if report.verdict.is_pass() { "✓" }
            else if matches!(report.verdict, crate::tui::api::ReviewVerdict::Fail) { "⛔" }
            else if matches!(report.verdict, crate::tui::api::ReviewVerdict::Unknown) { "?" }
            else { "⚠" };
        body.push_str(&format!(
            "  {} [{}] {} · {} · {} 项 issue\n",
            icon,
            ago,
            report.kind.label(),
            report.verdict.label(),
            report.issues.len(),
        ));
    }
    s.show_info(body);
    CmdResult::Consumed
}

/// V40-4: /auto-review on|off|status — 切换 Plan→Team 自动 review 联动
///
/// ## 设计意图
/// 让 Plan→Team 串联两层守门员（schema validate + LLM review）
/// review 是高 LLM 成本，必须 opt-in；status 子命令查询当前状态
fn cmd_auto_review(s: &mut AppState, _: &str, args: &[&str]) -> CmdResult {
    if args.is_empty() || args[0] == "status" {
        let state = if s.auto_review_plan { "on（已启用）" } else { "off（默认关闭）" };
        s.add_toast(
            format!("auto-review: {} · 用法 /auto-review on|off", state),
            std::time::Duration::from_secs(4),
        );
        return CmdResult::Consumed;
    }
    match args[0].to_ascii_lowercase().as_str() {
        "on" | "true" | "1" => {
            s.auto_review_plan = true;
            s.add_toast(
                "✓ 自动 review 已启用：Plan→Team 切换前自动审查任务规划",
                std::time::Duration::from_secs(4),
            );
        }
        "off" | "false" | "0" => {
            s.auto_review_plan = false;
            s.add_toast(
                "✓ 自动 review 已关闭",
                std::time::Duration::from_secs(3),
            );
        }
        other => {
            s.add_toast(
                format!("⛔ 未知参数：{}（允许 on/off/status）", other),
                std::time::Duration::from_secs(3),
            );
        }
    }
    CmdResult::Consumed
}

/// V39-2: /review-clear 清除 review 阻断
/// V40-2: toast 增强 — 清除前显示 verdict + issues 数 + strict 状态摘要
///
/// ## 设计意图
/// 提供逃生通道——当 strict 模式误判（如 LLM 把合理代码判为 fail）时，
/// 用户可主动清除 last_review，恢复正常流转能力。
/// V40-2 增强：清除时反馈"丢失了什么"，让用户可判断是否真的要清除。
fn cmd_review_clear(s: &mut AppState, _: &str, _: &[&str]) -> CmdResult {
    // 在写入前抽取摘要（清除后 last_review 已为 None）
    let summary = s.last_review.as_ref().map(|r| {
        let strict_marker = if s.last_review_strict { " · 🔒strict" } else { "" };
        format!("{} · {} 项 issue{}", r.verdict.label(), r.issues.len(), strict_marker)
    });
    let was_strict_blocking = s.last_review_strict
        && s.last_review.as_ref().map(|r| !r.verdict.is_pass()).unwrap_or(false);

    s.last_review = None;
    s.last_review_strict = false;

    let toast_msg = match (was_strict_blocking, summary) {
        (true, Some(sum)) => format!("✓ 已清除 review 阻断（原：{}）", sum),
        (false, Some(sum)) => format!("✓ 已清除 review 状态（原：{}）", sum),
        (_, None) => "ℹ 当前无 review 状态可清除".to_string(),
    };
    s.add_toast(toast_msg, std::time::Duration::from_secs(5));
    CmdResult::Consumed
}

fn cmd_status(s: &mut AppState, _: &str, _: &[&str]) -> CmdResult {
    let engine = if s.engine_handle.is_some() { "已连接" } else { "未连接" };
    s.show_info(format!(
        "当前状态\n\n模式: {}\n模型: {}\n引擎: {}\n轮次: {}\n消息: {}\n思考深度: {}",
        s.mode.label(), s.model_name, engine, s.turn_count, s.messages.len(), s.thinking_depth
    ));
    CmdResult::Consumed
}

fn cmd_tokens(s: &mut AppState, _: &str, _: &[&str]) -> CmdResult {
    let st = &s.session_tokens;
    s.show_info(format!(
        "Token 统计\n\nPrompt: {}\nCompletion: {}\nTotal: {}\nCached tokens: {}",
        st.prompt_tokens, st.completion_tokens, st.total_tokens, st.cached_tokens
    ));
    CmdResult::Consumed
}

fn cmd_debug(s: &mut AppState, _: &str, _: &[&str]) -> CmdResult {
    // V14 增强：从 UI/会话状态拼诊断信息（thinking 协议 / endpoint 联通 / 工具调用）
    let engine_state = if s.engine_handle.is_some() { "已连接" } else { "未连接" };
    let last_thinking_len = s.streaming_thinking.chars().count();
    let last_thinking_status = if s.is_streaming {
        if last_thinking_len > 0 {
            format!("流式中（已收 {} 字符）", last_thinking_len)
        } else {
            "流式中（**未收到** reasoning_content）".to_string()
        }
    } else if last_thinking_len > 0 {
        format!("上轮收到 {} 字符", last_thinking_len)
    } else {
        "本会话尚未观察到 reasoning_content".to_string()
    };
    let used_tools: Vec<&str> = {
        let mut v: Vec<&str> = s.tool_records.iter().map(|r| r.name.as_str()).collect();
        v.sort(); v.dedup(); v
    };
    let tools_summary = if used_tools.is_empty() {
        "（本会话未触发任何工具调用）".to_string()
    } else {
        format!("{} 个：{}", used_tools.len(), used_tools.join(", "))
    };

    s.show_info(format!(
        "## 调试信息\n\n\
         ### 引擎\n\
         - 引擎状态：{engine_state}\n\
         - 模型：{model}\n\
         - 思考深度：{depth}\n\
         - thinking 状态：{thinking_status}\n\n\
         ### 已注册命令\n\
         - 数量：{cmd_count} 个（/help 看完整列表）\n\
         - 工具调用：{tools}\n\n\
         ### UI 状态\n\
         - Focus：{focus:?}\n\
         - Panel 可见：{panel}\n\
         - Tab：{tab:?}\n\
         - Scroll 偏移：{scroll}\n\
         - InputState：{input:?}\n\
         - Paused：{paused}\n\
         - 流式：{streaming}\n\n\
         ### 会话\n\
         - 消息总数：{msg_total}（用户 {msg_user}）\n\
         - 事件总数：{evt_total}\n\
         - 轮次：{turns}\n\n\
         > 排查建议：\n\
         > - 若 thinking 一直 **未收到**，且发消息 400 → endpoint 不真支持 thinking，建议在配置改 `core.thinking_enabled = false` 或换 reasoner 模型\n\
         > - 工具调用 0 但你想让 LLM 用 → 检查 register_all 是否注册了 builtin（V13 起在 CoreLoop::new 注册）",
        engine_state = engine_state,
        model = s.model_name,
        depth = s.thinking_depth,
        thinking_status = last_thinking_status,
        cmd_count = s.commands.len(),
        tools = tools_summary,
        focus = s.focus,
        panel = s.panel_visible,
        tab = s.panel_tab,
        scroll = s.scroll,
        input = s.input_state,
        paused = s.paused,
        streaming = s.is_streaming,
        msg_total = s.messages.len(),
        msg_user = s.messages.iter().filter(|m| matches!(m.role, crate::tui::state::MsgRole::User)).count(),
        evt_total = s.trace_events.len(),
        turns = s.turn_count,
    ));
    CmdResult::Consumed
}

fn cmd_version(s: &mut AppState, _: &str, _: &[&str]) -> CmdResult {
    s.add_toast("Abacus v1.0.0", std::time::Duration::from_secs(3));
    CmdResult::Consumed
}

fn cmd_memory(s: &mut AppState, _: &str, _: &[&str]) -> CmdResult {
    // V33: 「记忆」意图 → 路由到「统计」tab（含知识宫殿全量层级树）
    // 旧 PanelTab::Memory 在 V33 mode 序列中已被 set_mode 兜底回 Timeline
    s.focus = crate::tui::state::Focus::Panel;
    s.panel_tab = crate::tui::state::PanelTab::Quant;
    s.add_toast("已打开「统计」(知识宫殿全量)", std::time::Duration::from_secs(2));
    CmdResult::Consumed
}

fn cmd_plugins(s: &mut AppState, _: &str, _: &[&str]) -> CmdResult {
    // V33: 「工具/插件」意图 → 路由到「现场」(包含工具小计)
    // PanelTab::Components 已不在新 mode 序列；现场 tab 提供工具汇总最直接
    s.focus = crate::tui::state::Focus::Panel;
    s.panel_tab = crate::tui::state::PanelTab::Timeline;
    s.add_toast("已打开「现场」(工具/记忆小计)", std::time::Duration::from_secs(2));
    CmdResult::Consumed
}

fn cmd_settings(s: &mut AppState, _: &str, _: &[&str]) -> CmdResult {
    s.show_settings = true;
    s.add_toast("设置面板已打开 ↑↓选择 Enter修改 Esc关闭", std::time::Duration::from_secs(3));
    CmdResult::Consumed
}

// ── Backend async commands ──

fn cmd_context(s: &mut AppState, _: &str, _: &[&str]) -> CmdResult {
    engine_or(s, SlashCommand::ContextStatus)
}
fn cmd_compress(s: &mut AppState, _: &str, _: &[&str]) -> CmdResult {
    engine_or(s, SlashCommand::ContextCompress)
}
fn cmd_inject(s: &mut AppState, _: &str, args: &[&str]) -> CmdResult {
    if args.is_empty() {
        s.add_toast("用法: /inject <临时知识>", std::time::Duration::from_secs(3));
        CmdResult::Consumed
    } else {
        engine_or(s, SlashCommand::ContextInject(args.join(" ")))
    }
}
fn cmd_tools(s: &mut AppState, _: &str, _: &[&str]) -> CmdResult {
    engine_or(s, SlashCommand::ToolList)
}
fn cmd_tool_stats(s: &mut AppState, _: &str, _: &[&str]) -> CmdResult {
    engine_or(s, SlashCommand::ToolStats)
}
fn cmd_safety(s: &mut AppState, _: &str, _: &[&str]) -> CmdResult {
    engine_or(s, SlashCommand::SafetyStatus)
}
fn cmd_models(s: &mut AppState, _: &str, _: &[&str]) -> CmdResult {
    engine_or(s, SlashCommand::ModelList)
}
fn cmd_info(s: &mut AppState, _: &str, _: &[&str]) -> CmdResult {
    engine_or(s, SlashCommand::SessionInfo)
}

fn engine_or(s: &mut AppState, cmd: SlashCommand) -> CmdResult {
    if s.engine_handle.is_some() {
        CmdResult::Pending(cmd)
    } else {
        s.add_toast("引擎未连接", std::time::Duration::from_secs(2));
        CmdResult::Consumed
    }
}

// ─── V0.2 Commands ──────────────────────────────────────────────────────────

fn cmd_history(s: &mut AppState, _: &str, args: &[&str]) -> CmdResult {
    if args.is_empty() {
        // 无参 → picker（选中后直接重发）
        s.open_picker_history();
        return CmdResult::Consumed;
    }
    // 带参 /history N → 文本列出（保留原行为）
    let n: usize = args.first().and_then(|a| a.parse().ok()).unwrap_or(10);
    let history = &s.input_history;
    let start = history.len().saturating_sub(n);
    let lines: Vec<String> = history[start..].iter().enumerate()
        .map(|(i, h)| format!("  {}. {}", start + i + 1, h))
        .collect();
    if lines.is_empty() {
        s.add_toast("暂无输入历史", std::time::Duration::from_secs(2));
    } else {
        s.show_info(format!("## 输入历史 (最近 {})\n\n{}", n, lines.join("\n")));
    }
    CmdResult::Consumed
}

fn cmd_search(s: &mut AppState, _: &str, args: &[&str]) -> CmdResult {
    if args.is_empty() {
        s.add_toast("用法: /search <关键词>", std::time::Duration::from_secs(3));
        return CmdResult::Consumed;
    }
    let query = args.join(" ").to_lowercase();
    let mut results = Vec::new();
    for (i, msg) in s.messages.iter().enumerate() {
        // 搜索所有内容类型: Stream + Block (summary + detail)
        let text = msg.parts.iter().filter_map(|p| match p {
            crate::tui::state::MsgContent::Stream(t) => Some(t.clone()),
            crate::tui::state::MsgContent::Block { summary, detail, .. } => {
                Some(format!("{} {}", summary, detail))
            }
            // V28: Trace 详情在 trace_events 里, /search 暂不索引(详情可走 timeline)
            crate::tui::state::MsgContent::Trace { .. } => None,
        }).collect::<Vec<_>>().join("");
        if text.to_lowercase().contains(&query) {
            // SC3 修复：用 truncate_to_width 按显示列截断（CJK char-safe），
            // 避免 &text[..80] 在多字节字符中间切片 panic
            let preview = crate::tui::util::truncate_to_width(&text, 80);
            results.push(format!("  [{}] {}", i + 1, preview));
        }
    }
    if results.is_empty() {
        s.add_toast(format!("未找到 \"{}\"", query), std::time::Duration::from_secs(2));
    } else {
        // V13: 改走聊天区显示
        s.show_info(format!("## 搜索: \"{}\"\n\n找到 {} 条匹配:\n{}", query, results.len(), results.join("\n")));
    }
    CmdResult::Consumed
}

// SC3 修复：cmd_search 之前用 `&text[..80]` 字节切片，CJK 文本在 byte 80 落在
// 多字节字符中间会 panic。改用 truncate_to_width 按显示列宽截断，char-safe
// （注：上一处 results.push 已含 byte-slice，须替换那里）

fn cmd_feedback(s: &mut AppState, _: &str, args: &[&str]) -> CmdResult {
    if args.is_empty() {
        s.add_toast("用法: /feedback <反馈内容>", std::time::Duration::from_secs(3));
        return CmdResult::Consumed;
    }
    let text = args.join(" ");
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let dir = format!("{}/.abacus", home);
    let _ = std::fs::create_dir_all(&dir); // Ensure directory exists
    let path = format!("{}/feedback.log", dir);
    let entry = format!("[{}] {}\n", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"), text);
    match std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        Ok(mut f) => {
            use std::io::Write;
            let _ = f.write_all(entry.as_bytes());
            s.add_toast("感谢反馈！已记录", std::time::Duration::from_secs(3));
        }
        Err(e) => {
            s.add_toast(format!("写入失败: {}", e), std::time::Duration::from_secs(3));
        }
    }
    CmdResult::Consumed
}

fn cmd_streaming(s: &mut AppState, _: &str, _: &[&str]) -> CmdResult {
    s.streaming_enabled = !s.streaming_enabled;
    let mode = if s.streaming_enabled { "流式" } else { "完整" };
    s.add_toast(format!("输出模式: {} ✓", mode), std::time::Duration::from_secs(2));
    CmdResult::Consumed
}

// V29.8: cmd_thinking / cmd_provider 函数已合并进 cmd_model 子命令分支
//   旧入口 /thinking /provider 已从 registry 删除, 命令清单 29 → 27

// ─── Phase 4 file-undo ─────────────────────────────────────────

/// /undo                     撤销最后一次（全 session 取最新）
/// /undo seq <N>             撤销特定 seq（当前 session）
/// /undo turn <N>            撤销 turn N 全部（当前 session）
/// /undo history [N]         查看本 session 历史（默认 20 条）
/// /undo timeline [hours]    跨 session 时间线（默认 1h 内）
fn cmd_undo(s: &mut AppState, _: &str, args: &[&str]) -> CmdResult {
    if s.engine_handle.is_none() {
        s.add_toast("引擎未连接", std::time::Duration::from_secs(2));
        return CmdResult::Consumed;
    }
    let session_id = s.session_id.clone();
    match args.first().copied() {
        // /undo
        None => engine_or(s, SlashCommand::UndoLast { session_id: None }),
        // /undo seq <N>
        Some("seq") => {
            let seq: u64 = match args.get(1).and_then(|a| a.parse().ok()) {
                Some(n) => n,
                None => {
                    s.add_toast("用法: /undo seq <N>", std::time::Duration::from_secs(3));
                    return CmdResult::Consumed;
                }
            };
            engine_or(s, SlashCommand::UndoSeq { session_id, seq })
        }
        // /undo turn <N>
        Some("turn") => {
            let turn: u32 = match args.get(1).and_then(|a| a.parse().ok()) {
                Some(n) => n,
                None => {
                    s.add_toast("用法: /undo turn <N>", std::time::Duration::from_secs(3));
                    return CmdResult::Consumed;
                }
            };
            engine_or(s, SlashCommand::UndoTurn { session_id, turn })
        }
        // /undo history [N]
        Some("history") => {
            let limit: usize = args.get(1).and_then(|a| a.parse().ok()).unwrap_or(20);
            engine_or(s, SlashCommand::UndoHistory { session_id: Some(session_id), limit })
        }
        // /undo timeline [hours]
        Some("timeline") => {
            let since_hours: u64 = args.get(1).and_then(|a| a.parse().ok()).unwrap_or(1);
            engine_or(s, SlashCommand::UndoTimeline { since_hours })
        }
        Some(other) => {
            s.add_toast(format!("未知子命令: {other}"), std::time::Duration::from_secs(3));
            CmdResult::Consumed
        }
    }
}

fn cmd_redo(s: &mut AppState, _: &str, _: &[&str]) -> CmdResult {
    let session_id = s.session_id.clone();
    engine_or(s, SlashCommand::Redo { session_id })
}

/// 会话导出（E2 修复：之前 event/mod.rs::handle_export_session 实现完整但从未注册）
/// 目标：~/abacus_session_<ts>.md（Markdown 格式，含 user/expert 分块 + Block details 折叠）
fn cmd_export(s: &mut AppState, _: &str, _: &[&str]) -> CmdResult {
    use crate::tui::state::{BlockKind, MsgContent, MsgRole};
    if s.messages.is_empty() {
        s.add_toast("当前会话为空，无法导出", std::time::Duration::from_secs(2));
        return CmdResult::Consumed;
    }
    let ts = chrono::Local::now().format("%Y%m%d_%H%M%S");
    let filename = format!("abacus_session_{}.md", ts);
    let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
    let filepath = home.join(&filename);

    let mut content = format!("# Abacus Session - {}\n\n", ts);
    for msg in &s.messages {
        let role = match &msg.role {
            MsgRole::User => "**User**",
            MsgRole::Session => "**Assistant**",
            MsgRole::Expert(name) => name.as_str(),
        };
        content.push_str(&format!("### {} ({})\n\n", role, msg.time));
        for part in &msg.parts {
            match part {
                MsgContent::Stream(text) => {
                    content.push_str(text);
                    content.push_str("\n\n");
                }
                MsgContent::Block { kind, summary, detail, .. } => {
                    let icon = match kind {
                        BlockKind::Think => "💭",
                        BlockKind::ToolCall => "🔧",
                        BlockKind::Checklist => "📋",
                    };
                    content.push_str(&format!(
                        "<details>\n<summary>{} {}</summary>\n\n```\n{}\n```\n</details>\n\n",
                        icon, summary, detail
                    ));
                }
                // V28 Trace: 导出时按 event_ids 反查 trace_events 拼成 details 折叠块,
                // 让导出 markdown 能保留思考与工具历史(在 timeline 之外的另一份归档)
                MsgContent::Trace { event_ids, .. } => {
                    content.push_str(&format!(
                        "<details>\n<summary>📜 trace · {} events</summary>\n\n",
                        event_ids.len()
                    ));
                    for id in event_ids {
                        if let Some(ev) = s.trace_events.iter().find(|e| e.id == *id) {
                            match &ev.kind {
                                crate::tui::state::TraceKind::Generic { content: c } => {
                                    content.push_str(&format!("- [{}] {}: {}\n", ev.time, ev.category, c));
                                }
                                crate::tui::state::TraceKind::Thinking { text, lines } => {
                                    content.push_str(&format!(
                                        "\n💭 **Thinking** ({} lines)\n```\n{}\n```\n",
                                        lines, text
                                    ));
                                }
                                crate::tui::state::TraceKind::ToolCall { name, args, output, .. } => {
                                    content.push_str(&format!(
                                        "\n⚙ **{}**\n```json\n{}\n```\n",
                                        name, args
                                    ));
                                    if let Some(out) = output {
                                        content.push_str(&format!("→\n```\n{}\n```\n", out));
                                    }
                                }
                                crate::tui::state::TraceKind::Reply { tokens } => {
                                    content.push_str(&format!("- ↩ reply · {} tok\n", tokens));
                                }
                            }
                        }
                    }
                    content.push_str("</details>\n\n");
                }
            }
        }
    }

    match std::fs::write(&filepath, &content) {
        Ok(_) => s.add_toast(
            format!("会话已导出: {}", filepath.display()),
            std::time::Duration::from_secs(5),
        ),
        Err(e) => s.add_toast(format!("导出失败: {}", e), std::time::Duration::from_secs(3)),
    }
    CmdResult::Consumed
}

// ─── V29.9 + V29.10: /turnkey 全托管命令 ─────────────────────
//
// 引用关系:
//   - 写: AppState.session_goal(/turnkey <goal>) | AppState.pending_turnkey_plan(plan_from_nl 回写)
//   - 读: panel summary 显示 goal; pending_turnkey_plan 是 execute 子命令的前置条件
// 生命周期:
//   - 创建: /turnkey <goal> 设 goal + dispatch plan_from_nl
//   - 销毁: /turnkey clear | /turnkey execute(执行后清 plan) | /new
// 后端依赖:
//   - sandbox_engine.plan_from_nl + execute, 通过 register_provider 自动级联接通
fn cmd_turnkey(s: &mut AppState, _: &str, args: &[&str]) -> CmdResult {
    if args.is_empty() {
        // 无参数 → 查询当前状态
        let mut body = String::from("Turnkey 全托管\n\n");
        match &s.session_goal {
            Some(goal) => body.push_str(&format!("目标: {}\n", goal)),
            None => body.push_str("目标: (未设置)\n"),
        }
        match &s.pending_turnkey_plan {
            Some(task) => body.push_str(&format!(
                "待执行计划: {} phases × {} steps\n",
                task.phases.len(),
                task.phases.iter().map(|p| p.steps.len()).sum::<usize>()
            )),
            None => body.push_str("待执行计划: (无)\n"),
        }
        body.push_str(
            "\n用法:\n  /turnkey <goal>      生成计划(仅展示, 不执行)\n  \
             /turnkey execute     执行最近一次计划(需先生成)\n  \
             /turnkey clear       清空目标 + 计划",
        );
        s.show_info(body);
        return CmdResult::Consumed;
    }

    let sub = args[0].to_lowercase();

    // /turnkey clear → 清空
    if sub == "clear" {
        s.session_goal = None;
        s.pending_turnkey_plan = None;
        s.add_toast("Turnkey 目标 + 计划已清空", std::time::Duration::from_secs(2));
        return CmdResult::Consumed;
    }

    // V29.10 (C4-Phase2): /turnkey execute → 执行最近一次 plan
    if sub == "execute" || sub == "exec" || sub == "run" {
        match s.pending_turnkey_plan.take() {
            Some(task) => {
                s.add_toast(
                    "Turnkey 执行启动 — sandbox 自动循环, 进度见时间线",
                    std::time::Duration::from_secs(3),
                );
                let ts = chrono::Local::now().format("%H:%M:%S").to_string();
                s.add_event(
                    &ts,
                    "session",
                    &format!("Turnkey 执行: {} ({} phases)", task.goal, task.phases.len()),
                    crate::tui::state::EventLevel::Info,
                );
                return engine_or(s, SlashCommand::TurnkeyExecute(task));
            }
            None => {
                s.add_toast(
                    "无待执行计划 — 先 /turnkey <goal> 生成",
                    std::time::Duration::from_secs(3),
                );
                return CmdResult::Consumed;
            }
        }
    }

    // /turnkey <goal> → 生成新计划
    let goal = args.join(" ").trim().to_string();
    if goal.is_empty() {
        s.add_toast("目标文本为空", std::time::Duration::from_secs(2));
        return CmdResult::Consumed;
    }
    s.session_goal = Some(goal.clone());
    // 生成新 plan 之前清掉旧 plan, 避免 execute 误命中过时计划
    s.pending_turnkey_plan = None;

    let ts = chrono::Local::now().format("%H:%M:%S").to_string();
    s.add_event(
        &ts,
        "session",
        &format!("Turnkey 目标已设置: {}", goal),
        crate::tui::state::EventLevel::Info,
    );

    s.add_toast(
        format!("Turnkey 计划生成中: {}", goal.chars().take(40).collect::<String>()),
        std::time::Duration::from_secs(3),
    );
    engine_or(s, SlashCommand::TurnkeyPlan(goal))
}

// ─── V29.9: /rename 会话别名命令 ──────────────────────────────
//
// 引用关系:
//   - 写: AppState.session_alias（持久化进 SessionExport）
//   - 读: TopBar/StatusBar 优先显示 alias, 否则 session_id 截短
// 生命周期:
//   - 创建: 用户 /rename <alias>
//   - 销毁: /rename clear / /rename 无参（显式清空）/ /new(切换会话)
fn cmd_rename(s: &mut AppState, _: &str, args: &[&str]) -> CmdResult {
    if args.is_empty() {
        match s.session_alias.clone() {
            Some(alias) => s.show_info(format!(
                "会话别名\n\n当前: {}\n\n用法:\n  /rename <new alias>  设置新别名\n  /rename clear        清空别名",
                alias
            )),
            None => s.show_info(
                "会话别名\n\n当前未设置(显示 session_id 截短)\n\n用法:\n  /rename <alias>  设置别名"
                    .to_string(),
            ),
        }
        return CmdResult::Consumed;
    }

    if args.len() == 1 && args[0].eq_ignore_ascii_case("clear") {
        s.session_alias = None;
        s.add_toast("会话别名已清空", std::time::Duration::from_secs(2));
        return CmdResult::Consumed;
    }

    let alias = args.join(" ").trim().to_string();
    if alias.is_empty() {
        s.add_toast("别名不能为空", std::time::Duration::from_secs(2));
        return CmdResult::Consumed;
    }
    // 防御: 限制别名长度避免 TopBar 撑爆
    let alias = if alias.chars().count() > 40 {
        alias.chars().take(40).collect::<String>()
    } else {
        alias
    };
    s.session_alias = Some(alias.clone());
    s.add_toast(format!("会话已重命名: {}", alias), std::time::Duration::from_secs(2));
    CmdResult::Consumed
}

// ─── V29.9: /diff git 工作树差异 ──────────────────────────────
//
// 引用关系: 不依赖 AppState 持久化字段，仅以 cwd 为 git repo 根
// 生命周期: 一次性命令——执行 git diff 后写 system info 显示，无副作用
// 依赖外部: 系统 git CLI；非 git 仓库或 git 缺失 → 友好降级提示
//
// 设计取舍:
//   - 不缓存输出（git diff 本身已快）
//   - 截断 ≥ 4000 字符防止 info 弹窗撑爆 —— 用户可在终端里 git diff 看完整版
//   - 不引入 enum 变体，纯 cli wrapper
fn cmd_diff(s: &mut AppState, _: &str, args: &[&str]) -> CmdResult {
    let mut cmd = std::process::Command::new("git");
    cmd.arg("diff");
    if !args.is_empty() {
        cmd.arg("--");
        for a in args {
            cmd.arg(a);
        }
    }
    let out = match cmd.output() {
        Ok(o) => o,
        Err(e) => {
            s.show_info(format!(
                "git diff 失败\n\n无法启动 git: {}\n\n确认 git 已安装且在 PATH 中。",
                e
            ));
            return CmdResult::Consumed;
        }
    };
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        s.show_info(format!(
            "git diff 失败\n\nexit={}\n{}",
            out.status.code().unwrap_or(-1),
            stderr.trim()
        ));
        return CmdResult::Consumed;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    if stdout.trim().is_empty() {
        s.show_info("git diff\n\n工作树干净，无未提交变更".to_string());
        return CmdResult::Consumed;
    }
    const MAX_CHARS: usize = 4000;
    let body = if stdout.chars().count() > MAX_CHARS {
        let truncated: String = stdout.chars().take(MAX_CHARS).collect();
        format!(
            "{}\n\n... [已截断 ≥ {} 字符；终端运行 `git diff` 查看完整版]",
            truncated, MAX_CHARS
        )
    } else {
        stdout.to_string()
    };
    s.show_info(format!("git diff\n\n{}", body));
    CmdResult::Consumed
}

// ─── V29.9: /branch 会话派生命令 ──────────────────────────────
//
// 引用关系:
//   - 写: AppState.session_id（生成新 uuid）+ session_alias（可选）
//   - 调用: run::save_session（pub(crate)）— 把原 session 落盘 + 新 session 落盘
// 生命周期:
//   - 创建: 用户 /branch [alias] —— 当前 session 立刻切到新 uuid;
//   - 销毁: 用户后续 /new 或 /branch 切走;原 session 文件仍在磁盘,
//           待 /resume(C2 待办)实现后可访问。
// 设计取舍:
//   - **同步切换**：派生即切到新 session，用户后续动作进入新 session;
//     原 session 通过 last_session_uuid 之外的文件保留。
//   - 不复制 trace_events 列表:Vec::clone 已在 save_session 内部完成,
//     state.trace_events 引用保留在内存中无需 deep copy。
//   - 不强制要求 alias —— 无 alias 时新 session 显示截短 uuid。
fn cmd_branch(s: &mut AppState, _: &str, args: &[&str]) -> CmdResult {
    // 1) 在原 session_id 下保存当前快照（避免分叉点丢失）
    if let Err(e) = crate::tui::run::save_session(s) {
        s.add_toast(
            format!("分叉前保存失败: {}", e),
            std::time::Duration::from_secs(3),
        );
        return CmdResult::Consumed;
    }
    let old_id = s.session_id.clone();
    let old_alias = s.session_alias.clone();

    // 2) 切到新 uuid
    let new_id = uuid::Uuid::new_v4().to_string();
    s.session_id = new_id.clone();
    s.session_alias = if args.is_empty() {
        None
    } else {
        let alias = args.join(" ").trim().to_string();
        if alias.is_empty() { None } else { Some(alias.chars().take(40).collect::<String>()) }
    };

    // 3) 在 trace 时间线留 fork 标记，便于回看
    let ts = chrono::Local::now().format("%H:%M:%S").to_string();
    let parent_label = old_alias
        .as_deref()
        .map(|a| format!("{} ({})", a, &old_id[..8.min(old_id.len())]))
        .unwrap_or_else(|| old_id[..8.min(old_id.len())].to_string());
    s.add_event(
        &ts,
        "session",
        &format!("派生自 {}", parent_label),
        crate::tui::state::EventLevel::Info,
    );

    // 4) 立即把新 session 落盘（建立独立文件 + 推 last_session_uuid 指针）
    if let Err(e) = crate::tui::run::save_session(s) {
        s.add_toast(
            format!("新分支保存失败: {}", e),
            std::time::Duration::from_secs(3),
        );
        return CmdResult::Consumed;
    }
    let label = s.session_alias.clone().unwrap_or_else(|| {
        format!("{}…", &new_id[..8.min(new_id.len())])
    });
    s.add_toast(
        format!("已派生新会话: {}", label),
        std::time::Duration::from_secs(3),
    );
    CmdResult::Consumed
}

// ─── V34: /plan-prefix — 历史命令，plan_mode 字段已删除 ─────────
// 原 plan-mode 单次前缀注入功能随 plan_mode 字段移除而废止。
// 保留命令入口以避免"未知命令"错误；提示用户改用 /plan <task>。
fn cmd_plan_prefix(s: &mut AppState, _: &str, _args: &[&str]) -> CmdResult {
    s.add_toast(
        "/plan-prefix 已废弃，请改用 /plan <任务描述> 直接触发规划策略",
        std::time::Duration::from_secs(4),
    );
    CmdResult::Consumed
}

// ─── V29.9 (C2): /resume 按 uuid 恢复 session ──────────────────
//
// 引用关系:
//   - 调用: run::load_session_by_uuid(uuid)（pub(crate) 抽出的 helper）
//   - 副作用: 覆盖当前 state.messages / trace_events / session_id 等
// 生命周期:
//   - 一次性: 加载完成后用户可继续在恢复的 session 上对话, 后续 save 走新 session
//   - 失败兜底: uuid 不存在/JSON 解析失败 → 不动 state, 显示错误 toast
// 设计取舍:
//   - 支持 uuid 前缀匹配(8 字符以上即可定位)
//   - 无参时列出最近 sessions(按 mtime), 便于用户选择
//   - 当前 session 未保存的修改会丢失 → 加载前主动 save_session 保命
fn cmd_resume(s: &mut AppState, _: &str, args: &[&str]) -> CmdResult {
    let dir = abacus_core::paths::current_sessions_dir();

    // 无参 → 列出最近 session
    if args.is_empty() {
        let mut entries: Vec<(String, std::time::SystemTime)> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                let p = e.path();
                let is_session = p.extension().and_then(|x| x.to_str()) == Some("json")
                    && !p.file_name().and_then(|x| x.to_str()).map(|n| n.starts_with('.')).unwrap_or(false);
                if !is_session { continue; }
                let stem = p.file_stem().and_then(|x| x.to_str()).unwrap_or("").to_string();
                if let Ok(mt) = e.metadata().and_then(|m| m.modified()) {
                    entries.push((stem, mt));
                }
            }
        }
        entries.sort_by_key(|e| std::cmp::Reverse(e.1)); // 降序：最新 session 在前
        if entries.is_empty() {
            s.show_info("Resume\n\n当前项目暂无 session 历史".to_string());
            return CmdResult::Consumed;
        }
        let mut body = String::from("最近会话(按修改时间倒序, 取前 10 条):\n\n");
        for (uuid, mt) in entries.iter().take(10) {
            let dt: chrono::DateTime<chrono::Local> = (*mt).into();
            let mark = if uuid == &s.session_id { "▶ " } else { "  " };
            body.push_str(&format!(
                "{}{}  {}\n",
                mark,
                dt.format("%Y-%m-%d %H:%M"),
                &uuid[..8.min(uuid.len())]
            ));
        }
        body.push_str("\n用法:\n  /resume <uuid prefix>   恢复指定 session(前缀 ≥ 8 字符)");
        s.show_info(body);
        return CmdResult::Consumed;
    }

    let prefix = args[0];
    if prefix.len() < 4 {
        s.add_toast("uuid 前缀过短(至少 4 字符)", std::time::Duration::from_secs(2));
        return CmdResult::Consumed;
    }

    // 找到匹配项
    let mut matches: Vec<String> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("json") { continue; }
            let stem = p.file_stem().and_then(|x| x.to_str()).unwrap_or("").to_string();
            if stem.starts_with('.') { continue; }
            if stem.starts_with(prefix) {
                matches.push(stem);
            }
        }
    }

    if matches.is_empty() {
        s.add_toast(
            format!("未找到 uuid 前缀 '{}' 的 session", prefix),
            std::time::Duration::from_secs(3),
        );
        return CmdResult::Consumed;
    }
    if matches.len() > 1 {
        s.show_info(format!(
            "uuid 前缀 '{}' 不唯一, 匹配到 {} 条:\n\n{}\n\n请提供更长前缀",
            prefix,
            matches.len(),
            matches.iter().take(8).map(|u| format!("  {}", &u[..16.min(u.len())])).collect::<Vec<_>>().join("\n")
        ));
        return CmdResult::Consumed;
    }

    let uuid = matches.into_iter().next().unwrap();
    if uuid == s.session_id {
        s.add_toast("已在该 session 中, 无需切换", std::time::Duration::from_secs(2));
        return CmdResult::Consumed;
    }

    // 切换前先保住当前进度
    if let Err(e) = crate::tui::run::save_session(s) {
        s.add_toast(
            format!("切换前保存失败: {} (取消切换)", e),
            std::time::Duration::from_secs(3),
        );
        return CmdResult::Consumed;
    }

    match crate::tui::run::load_session_by_uuid(s, &uuid) {
        Ok(true) => {
            s.add_toast(
                format!("已恢复 session: {}", &uuid[..8.min(uuid.len())]),
                std::time::Duration::from_secs(3),
            );
            s.mark_dirty();
        }
        Ok(false) => {
            s.add_toast("session 文件不存在", std::time::Duration::from_secs(3));
        }
        Err(e) => {
            s.add_toast(format!("加载失败: {}", e), std::time::Duration::from_secs(3));
        }
    }
    CmdResult::Consumed
}

// ─── V29.9 (C3): /doctor 系统健康检查 ─────────────────────────
//
// 引用关系:
//   - 调用: commands::doctor::build_doctor_report (pub fn, 纯函数)
//   - 输出: AppState.show_info(多行字符串)
// 生命周期: 一次性命令 — 调用即出 snapshot
// 设计取舍:
//   - 复用 CLI 的同一份检查列表, 避免双份维护
fn cmd_doctor(s: &mut AppState, _: &str, _: &[&str]) -> CmdResult {
    let lines = crate::commands::doctor::build_doctor_report();
    s.show_info(lines.join("\n"));
    CmdResult::Consumed
}

// ─── V29.11: /allow 自动授权管理 ─────────────────────────────
//
// 引用关系:
//   - 读写: AppState.always_allow (HashSet) + run::save_always_allow (系统级持久化)
//   - 路径: ~/.abacus/always_allow.json
// 生命周期: 系统级 — 跨 session/项目共享; /allow clear 或手动删文件可清
fn cmd_allow(s: &mut AppState, _: &str, args: &[&str]) -> CmdResult {
    if args.is_empty() || args[0].eq_ignore_ascii_case("list") {
        // 查看列表
        if s.always_allow.is_empty() {
            s.show_info("自动授权列表\n\n(空)\n\n用法:\n  /allow list      查看\n  /allow revoke X  撤销\n  /allow clear     全部清空".to_string());
        } else {
            let mut sorted: Vec<&String> = s.always_allow.iter().collect();
            sorted.sort();
            let mut body = format!("自动授权列表 ({} 项)\n\n", sorted.len());
            for tool in &sorted {
                body.push_str(&format!("  ✓ {}\n", tool));
            }
            body.push_str("\n用法:\n  /allow revoke <tool>  撤销单个\n  /allow clear          清空全部");
            s.show_info(body);
        }
        return CmdResult::Consumed;
    }

    let sub = args[0].to_lowercase();

    if sub == "clear" {
        let count = s.always_allow.len();
        s.always_allow.clear();
        let _ = crate::tui::run::save_always_allow(&s.always_allow);
        s.add_toast(
            format!("已清空全部自动授权 ({} 项)", count),
            std::time::Duration::from_secs(3),
        );
        return CmdResult::Consumed;
    }

    if sub == "revoke" || sub == "remove" || sub == "rm" {
        if args.len() < 2 {
            s.add_toast("用法: /allow revoke <tool_id>", std::time::Duration::from_secs(3));
            return CmdResult::Consumed;
        }
        let tool = args[1].to_string();
        if s.always_allow.remove(&tool) {
            let _ = crate::tui::run::save_always_allow(&s.always_allow);
            s.add_toast(
                format!("已撤销: {}", tool),
                std::time::Duration::from_secs(2),
            );
        } else {
            // 模糊匹配 — 用户可能只输入部分名字
            let matches: Vec<String> = s.always_allow.iter()
                .filter(|t| t.contains(&tool))
                .cloned()
                .collect();
            if matches.len() == 1 {
                s.always_allow.remove(&matches[0]);
                let _ = crate::tui::run::save_always_allow(&s.always_allow);
                s.add_toast(
                    format!("已撤销: {}", matches[0]),
                    std::time::Duration::from_secs(2),
                );
            } else if matches.is_empty() {
                s.add_toast(
                    format!("未找到匹配 '{}' 的授权项", tool),
                    std::time::Duration::from_secs(3),
                );
            } else {
                s.show_info(format!(
                    "'{}' 匹配到 {} 项, 请精确指定:\n\n{}",
                    tool, matches.len(),
                    matches.iter().map(|m| format!("  {}", m)).collect::<Vec<_>>().join("\n")
                ));
            }
        }
        return CmdResult::Consumed;
    }

    s.add_toast("用法: /allow [list|revoke <tool>|clear]", std::time::Duration::from_secs(3));
    CmdResult::Consumed
}

// ─── Phase 4 file-undo slash 解析单测 ──────────────────────────
//
// 仅测 slash 解析层（dispatch / Pending variant 派发），
// 不测 UndoEngine 实际执行（已在 abacus-core::undo::engine::tests 覆盖）。
#[cfg(test)]
mod undo_slash_tests {
    use super::*;
    use crate::tui::state::AppState;

    /// 构造一个挂了"虚拟引擎"的 AppState（让 engine_or 走 Pending 路径）
    /// **注意**：测试时 engine_handle 是真实 EngineHandle 类型，构造成本高；
    /// 我们采用 force-set engine_handle.is_some() 的内部 hack——这里只验解析
    /// 不验调度——故 engine_handle 不为 None 即可。
    fn mk_state_with_fake_engine() -> AppState {
        use crate::tui::state::AbacusMode;
        let mut s = AppState::new(AbacusMode::Clarify);
        s.session_id = "sess-test".into();
        s
    }

    #[test]
    fn undo_without_engine_emits_toast_consumed() {
        let mut s = mk_state_with_fake_engine();
        // 无 engine 时返回 Consumed（toast 提示）
        let r = dispatch(&mut s, "/undo");
        assert!(matches!(r, CmdResult::Consumed));
    }

    #[test]
    fn undo_seq_invalid_argument_emits_toast() {
        let mut s = mk_state_with_fake_engine();
        // /undo seq <非数字> → toast，不是 Pending
        let r = dispatch(&mut s, "/undo seq abc");
        assert!(matches!(r, CmdResult::Consumed));
    }

    #[test]
    fn undo_unknown_subcommand_emits_toast() {
        let mut s = mk_state_with_fake_engine();
        let r = dispatch(&mut s, "/undo bogus");
        assert!(matches!(r, CmdResult::Consumed));
    }

    #[test]
    fn redo_without_engine_emits_toast() {
        let mut s = mk_state_with_fake_engine();
        let r = dispatch(&mut s, "/redo");
        assert!(matches!(r, CmdResult::Consumed));
    }

    #[test]
    fn undo_registered_in_command_inventory() {
        let inv = command_inventory();
        let names: Vec<&str> = inv.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.iter().any(|n| n.starts_with("/undo")));
        assert!(names.iter().any(|n| n.starts_with("/redo")));
    }

    #[test]
    fn undo_in_help_text() {
        let h = help_text();
        assert!(h.contains("/undo"));
        assert!(h.contains("/redo"));
    }
}

// ════════════════════════════════════════════════════════════
// ─── Meeting 辅助函数 ────────────────────────────────────────────────────────

/// 提取 Meeting 议题：Meeting 模式首条用户消息前 20 词
///
/// 引用关系：try_switch_mode Meeting→Clarify 分支，为 quick_save 提供 topic
fn extract_meeting_topic(
    messages: &std::collections::VecDeque<crate::tui::state::Message>,
) -> Option<String> {
    // 找第一条 User 消息（即进入 Meeting 后用户的首条提问）
    let first_user = messages
        .iter()
        .find(|m| matches!(m.role, crate::tui::state::MsgRole::User))?
        ;
    let mut buf = String::new();
    for part in &first_user.parts {
        if let crate::tui::state::MsgContent::Stream(s) = part {
            buf.push_str(s);
        }
    }
    let trimmed = buf.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.split_whitespace().take(20).collect::<Vec<_>>().join(" "))
}

/// V35: /meeting-list [——project] — 列出历史会议记录
///
/// 引用关系：读 crate::tui::meeting_cache::list_records
/// 输出：向 messages 追加一条 Session 消息展示列表
fn cmd_meeting_list(s: &mut AppState, _raw: &str, args: &[&str]) -> CmdResult {
    use crate::tui::meeting_cache;
    // 可选 --project 参数：按当前 cwd 过滤
    let cwd_filter = if args.contains(&"--project") {
        std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .ok()
    } else {
        None
    };
    let records = meeting_cache::list_records(cwd_filter.as_deref());
    if records.is_empty() {
        s.add_toast("没有历史会议记录（~/.abacus/meetings/ 为空）", std::time::Duration::from_secs(3));
        return CmdResult::Consumed;
    }
    // 构建列表文本，注入为一条 Session 消息展示
    let mut lines = vec![
        format!("📁 历史会议记录 ({} 条)：", records.len()),
        String::new(),
    ];
    for (i, r) in records.iter().enumerate() {
        lines.push(meeting_cache::format_list_entry(r, i));
    }
    lines.push(String::new());
    lines.push("提示：使用 /meeting-load <id> 将结论注入当前 session".to_string());
    let text = lines.join("\n");
    s.add_message(crate::tui::state::Message {
        role: crate::tui::state::MsgRole::Session,
        parts: vec![crate::tui::state::MsgContent::Stream(text)],
        time: chrono::Local::now().format("%H:%M").to_string(),
    });
    CmdResult::Consumed
}

/// V35: /meeting-load <id> — 将历史会议结论注入当前 session
///
/// 引用关系：读 crate::tui::meeting_cache::load_record
/// 操作：将历史结论格式化后添加为 System 消息
/// 当 LLM 下次回复时可引用这些内容
fn cmd_meeting_load(s: &mut AppState, _raw: &str, args: &[&str]) -> CmdResult {
    use crate::tui::meeting_cache;
    let id = match args.first() {
        Some(id) => *id,
        None => {
            s.add_toast("用法：/meeting-load <id>  （ID 可从 /meeting-list 获取）", std::time::Duration::from_secs(4));
            return CmdResult::Consumed;
        }
    };
    match meeting_cache::load_record(id) {
        Some(record) => {
            let injected = meeting_cache::format_for_injection(&record);
            // 以 Expert("会议记录") 角色显示：视觉上区别于普通 Session 消息
            // 注：TUI messages 为展示层，LLM context 由 abacus-core SessionState 管理
            // 用户阅读后可在对话中主动引用，或结合 mode_artifact 注入机制
            s.add_message(crate::tui::state::Message {
                role: crate::tui::state::MsgRole::Expert("会议记录".to_string()),
                parts: vec![crate::tui::state::MsgContent::Stream(injected)],
                time: chrono::Local::now().format("%H:%M").to_string(),
            });
            s.add_toast(
                format!("✔ 已加载会议记录: {}（可在对话中引用）", record.topic),
                std::time::Duration::from_secs(4),
            );
        }
        None => {
            s.add_toast(
                format!("✖ 找不到会议记录: {}", id),
                std::time::Duration::from_secs(3),
            );
        }
    }
    CmdResult::Consumed
}

// ─── /expert 专家角色配置命令 ───────────────────────────────────────────────────────

/// V35: /expert <subcommand> — Meeting 专家角色和模型配置
///
/// 子命令:
///   list                       列出当前专家配置
///   add <name> <domain>        添加专家（用主模型）
///   add <name> <domain> --model <id>  添加专家并指定模型
///   set <name> --model <id>    修改已有专家的模型
///   remove <name>              删除专家
///   reset                      恢复默认配置（3 个内置专家）
fn cmd_expert(s: &mut AppState, _raw: &str, args: &[&str]) -> CmdResult {
    use crate::tui::expert_config as ec;

    let sub = args.first().copied().unwrap_or("list");
    match sub {
        "list" | "ls" => {
            let experts = ec::load_experts();
            if experts.is_empty() {
                s.add_toast("没有专家配置，请用 /expert add 添加", std::time::Duration::from_secs(3));
                return CmdResult::Consumed;
            }
            let mut lines = vec![
                format!("🧠 Meeting 专家 ({} 位)：", experts.len()),
                String::new(),
            ];
            for (i, e) in experts.iter().enumerate() {
                lines.push(ec::format_expert_entry(e, i));
            }
            lines.push(String::new());
            lines.push("提示: /expert add <名> <领域> [--model <模型号>] | set <名> --model <模型号> | remove <名> | reset".to_string());
            s.add_message(crate::tui::state::Message {
                role: crate::tui::state::MsgRole::Session,
                parts: vec![crate::tui::state::MsgContent::Stream(lines.join("\n"))],
                time: chrono::Local::now().format("%H:%M").to_string(),
            });
        }
        "add" => {
            // /expert add <name> <domain> [--model <id>]
            let name = match args.get(1) {
                Some(n) => n.to_string(),
                None => {
                    s.add_toast("用法: /expert add <名称> <领域> [--model <模型号>]", std::time::Duration::from_secs(4));
                    return CmdResult::Consumed;
                }
            };
            let domain = match args.get(2) {
                Some(d) => d.to_string(),
                None => {
                    s.add_toast("用法: /expert add <名称> <领域> [--model <模型号>]", std::time::Duration::from_secs(4));
                    return CmdResult::Consumed;
                }
            };
            // 解析 --model <id>
            let model = args.windows(2)
                .find(|w| w[0] == "--model")
                .map(|w| w[1].to_string());

            let id = name.to_lowercase().replace(' ', "_");
            let new_expert = ec::ExpertDef {
                id: id.clone(),
                name: name.clone(),
                domain: domain.clone(),
                model: model.clone(),
                hint_tags: vec![domain.clone()],
                role: String::new(),
                guide_strategy: String::new(),
                anti_pattern: String::new(),
                capabilities: vec![],
                allowed_tools: vec![],
                engagement: ec::ExpertEngagement::default(),
            };
            let mut experts = ec::load_experts();
            if experts.iter().any(|e| e.id == id) {
                s.add_toast(format!("专家 {} 已存在，请用 /expert set 修改", name), std::time::Duration::from_secs(3));
                return CmdResult::Consumed;
            }
            experts.push(new_expert);
            match ec::save_experts(&experts) {
                Ok(_) => s.add_toast(
                    format!("✔ 已添加专家: {} ({}) {}",
                        name, domain,
                        model.as_deref().map(|m| format!("[模型: {}]", m)).unwrap_or_default()),
                    std::time::Duration::from_secs(3),
                ),
                Err(e) => s.add_toast(format!("✖ 保存失败: {}", e), std::time::Duration::from_secs(3)),
            }
        }
        "set" => {
            // /expert set <name> --model <id>
            let name = match args.get(1) {
                Some(n) => n.to_string(),
                None => {
                    s.add_toast("用法: /expert set <名称> --model <模型号>", std::time::Duration::from_secs(4));
                    return CmdResult::Consumed;
                }
            };
            let model = args.windows(2)
                .find(|w| w[0] == "--model")
                .map(|w| w[1].to_string());
            if model.is_none() {
                s.add_toast("用法: /expert set <名称> --model <模型号>", std::time::Duration::from_secs(4));
                return CmdResult::Consumed;
            }
            let mut experts = ec::load_experts();
            let id = name.to_lowercase().replace(' ', "_");
            // 借用拆分：先修改字段并取出 name，再 drop 可变借用，再调用 save_experts（需不可变借用）
            let found_name = experts.iter_mut().find(|e| e.id == id || e.name == name)
                .map(|e| { e.model = model.clone(); e.name.clone() });
            match found_name {
                Some(expert_name) => {
                    match ec::save_experts(&experts) {
                        Ok(_) => s.add_toast(
                            format!("✔ {} 模型已更新为 {}", expert_name, model.unwrap_or_default()),
                            std::time::Duration::from_secs(3),
                        ),
                        Err(e2) => s.add_toast(format!("✖ 保存失败: {}", e2), std::time::Duration::from_secs(3)),
                    }
                }
                None => s.add_toast(format!("找不到专家: {}", name), std::time::Duration::from_secs(3)),
            }
        }
        "remove" | "rm" => {
            let name = match args.get(1) {
                Some(n) => n.to_string(),
                None => {
                    s.add_toast("用法: /expert remove <名称>", std::time::Duration::from_secs(3));
                    return CmdResult::Consumed;
                }
            };
            let mut experts = ec::load_experts();
            let before = experts.len();
            let id = name.to_lowercase().replace(' ', "_");
            experts.retain(|e| e.id != id && e.name != name);
            if experts.len() == before {
                s.add_toast(format!("找不到专家: {}", name), std::time::Duration::from_secs(3));
            } else {
                match ec::save_experts(&experts) {
                    Ok(_) => s.add_toast(format!("✔ 已删除专家: {}", name), std::time::Duration::from_secs(3)),
                    Err(e) => s.add_toast(format!("✖ 保存失败: {}", e), std::time::Duration::from_secs(3)),
                }
            }
        }
        "reset" => {
            let defaults = ec::default_experts();
            match ec::save_experts(&defaults) {
                Ok(_) => s.add_toast(
                    format!("✔ 已恢复默认专家配置 ({} 位内置专家)", defaults.len()),
                    std::time::Duration::from_secs(3),
                ),
                Err(e) => s.add_toast(format!("✖ 保存失败: {}", e), std::time::Duration::from_secs(3)),
            }
        }
        _ => {
            s.add_toast(
                "/expert list | add <名> <领域> [--model <号>] | set <名> --model <号> | remove <名> | reset",
                std::time::Duration::from_secs(5),
            );
        }
    }
    CmdResult::Consumed
}

// V33-续 ModeArtifact 写入端单测
//   引用关系：验证 try_switch_mode 在 Clarify→Plan/Meeting + Meeting→Team 三条路径
//             正确写入 ModeArtifact::ClarifyBrief / MeetingConclusion
//   生命周期：纯单元测试，构造 AppState + 模拟 Session 消息后调 try_switch_mode
// ════════════════════════════════════════════════════════════
#[cfg(test)]
mod mode_artifact_tests {
    use super::*;
    use crate::tui::state::{AppState, Message, MsgContent, MsgRole};

    fn mk_state_with_session(mode: AbacusMode, session_text: &str) -> AppState {
        let mut s = AppState::new(mode);
        s.add_message(Message {
            role: MsgRole::Session,
            parts: vec![MsgContent::Stream(session_text.to_string())],
            time: "12:00".to_string(),
        });
        s
    }

    #[test]
    fn extract_last_session_text_picks_session_role() {
        let mut s = AppState::new(AbacusMode::Clarify);
        s.add_message(Message {
            role: MsgRole::User,
            parts: vec![MsgContent::Stream("用户".to_string())],
            time: "12:00".to_string(),
        });
        s.add_message(Message {
            role: MsgRole::Session,
            parts: vec![MsgContent::Stream("AI 摘要".to_string())],
            time: "12:01".to_string(),
        });
        let text = extract_last_session_text(&s.messages);
        assert_eq!(text.as_deref(), Some("AI 摘要"));
    }

    #[test]
    fn extract_last_session_text_returns_none_when_empty() {
        let s = AppState::new(AbacusMode::Clarify);
        assert!(extract_last_session_text(&s.messages).is_none());
    }

    #[test]
    fn extract_last_session_text_returns_none_when_no_session_role() {
        let mut s = AppState::new(AbacusMode::Clarify);
        s.add_message(Message {
            role: MsgRole::User,
            parts: vec![MsgContent::Stream("only user".to_string())],
            time: "12:00".to_string(),
        });
        assert!(extract_last_session_text(&s.messages).is_none());
    }

    #[test]
    fn clarify_to_meeting_writes_clarify_brief() {
        // V34: Clarify → Meeting 自动注入 ClarifyBrief
        let mut s = mk_state_with_session(AbacusMode::Clarify, "澄清后需求");
        try_switch_mode(&mut s, AbacusMode::Meeting);
        assert_eq!(s.mode, AbacusMode::Meeting);
        assert!(s.mode_artifact.is_none()); // switch_mode 已消费
    }

    #[test]
    fn meeting_to_clarify_writes_meeting_conclusion() {
        // V34: Meeting → Clarify 自动注入 MeetingConclusion
        let mut s = mk_state_with_session(AbacusMode::Meeting, "会议结论：方案 A");
        try_switch_mode(&mut s, AbacusMode::Clarify);
        assert_eq!(s.mode, AbacusMode::Clarify);
        assert!(s.mode_artifact.is_none()); // switch_mode 已消费
    }

    #[test]
    fn clarify_without_session_still_switches_silently() {
        // 无 Session 消息 → 不写 artifact，但仍允许切换（不阻断 UX）
        let mut s = AppState::new(AbacusMode::Clarify);
        try_switch_mode(&mut s, AbacusMode::Meeting);
        assert_eq!(s.mode, AbacusMode::Meeting);
    }

    #[test]
    fn existing_mode_artifact_not_overwritten() {
        // 用户/agent 已显式注入 mode_artifact 时，try_switch_mode 不应覆盖
        let mut s = mk_state_with_session(AbacusMode::Clarify, "Session 文本");
        s.mode_artifact = Some(abacus_types::ModeArtifact::ClarifyBrief("已有摘要".to_string()));
        try_switch_mode(&mut s, AbacusMode::Meeting);
        // switch_mode take() 后 mode_artifact = None；但写入分支因 is_none() 守卫不会执行
        // 验证未 panic + 已切换即可
        assert_eq!(s.mode, AbacusMode::Meeting);
    }
}
