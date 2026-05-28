//! Block detail rendering
//!
//! 从 mod.rs 提取的 block detail 渲染函数集合。
//! 包含：render_block_detail, try_render_bash_exec, group_consecutive_tool_runs,
//! render_single_trace_event, render_merged_tool_run, format_duration_ms,
//! format_duration_ms_padded, extract_tool_param_summary, try_render_edit_diff,
//! render_simple_diff, render_block_detail_with_limit
//!
//! 引用关系：被 mod.rs 中 build_message_lines 及 Trace 展开分支调用
//! 生命周期：纯渲染函数集，无持久状态

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::tui::markdown;
use crate::tui::state::{BlockKind, TraceEvent, TraceKind, ToolStatus};
use crate::tui::theme::{TextRole, Theme};

/// 按 BlockKind 渲染 Block detail 内容(默认无软上限,保持旧 V12 行为)
///
/// V12: 替代之前所有 BlockKind 共用 plain Caption 的"一坨文本"展示
/// V28 (T5): 重构为 `render_block_detail_with_limit` 的薄 wrapper, max_lines=0 表示不限
///
/// 引用关系：被 build_message_lines 内 Block 展开分支调用
/// 生命周期：单次展开渲染，无持久缓存（detail 已固化在 message.parts）
pub(super) fn render_block_detail<'a>(detail: &str, kind: &BlockKind, theme: &Theme, max_width: usize) -> Vec<Line<'a>> {
    render_block_detail_with_limit(detail, kind, theme, 0, max_width)
}

/// V29.11: bash_exec 工具的 args 渲染为 shell 命令视图
///
/// 触发条件: tool name 是 bash_exec / bash.exec
/// 渲染:
///   $ command-text              ← theme.accent + 加粗
///   (workdir: /path/to/dir)    ← theme.muted, 仅在 args 含 workdir 时
/// 不限行: 命令本身通常 1-3 行, 不需要额外折叠
pub(super) fn try_render_bash_exec<'a>(
    name: &str,
    args_json: &str,
    theme: &Theme,
    _max_total_lines: usize,
) -> Option<Vec<Line<'a>>> {
    let lower = name.to_lowercase();
    // 2026-05-28: ToolId 直接用原始名 "bash_exec"（去掉了 filengine_ 前缀）
    if lower.as_str() != "bash_exec" {
        return None;
    }
    let json: serde_json::Value = serde_json::from_str(args_json).ok()?;
    let command = json.get("command").and_then(|v| v.as_str()).unwrap_or("(empty)");
    let workdir = json.get("workdir").and_then(|v| v.as_str());

    let mut lines: Vec<Line<'static>> = Vec::new();
    // 命令行 — 多行命令每行都加 $ / > 前缀
    let cmd_lines: Vec<&str> = command.lines().collect();
    for (i, l) in cmd_lines.iter().enumerate() {
        let prefix = if i == 0 { "$ " } else { "> " };
        lines.push(Line::from(vec![
            Span::styled(
                format!("{}{}", prefix, l),
                Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
            ),
        ]));
    }
    // workdir 提示(可选)
    if let Some(wd) = workdir {
        lines.push(Line::from(vec![
            Span::styled(
                format!("  (workdir: {})", wd),
                theme.text_style(TextRole::Caption),
            ),
        ]));
    }
    Some(lines)
}

// ═══ V29.12: 连续同名 ToolCall 合并展示 ═══════════════════════════════════
//
// 设计:
//   - 纯渲染层分组,trace_events 数据不改。timeline panel、/copy 不受影响。
//   - 仅相邻且同名的 ToolCall 归为一组 (run),中间插入其他 kind 则断开。
//   - 单条 run 照原渲染 (不丢信息); 多条合并为 `⚙ name ×N · 状态 · 总耗时` header
//     + 每条调用的关键参数单行摘要。
//
// 引用关系:
//   - 被 build_message_lines → Trace 展开态调用
//   - 引用 render_single_trace_event (渲染单条) / render_merged_tool_run (渲染合并组)
// 生命周期: 单次渲染帧

/// 对 event_ids 按连续同名 ToolCall 分组。
///
/// 返回 `Vec<Vec<u64>>`,每组内 id 的对应 trace event 同名且相邻。
/// 非 ToolCall 类型单独成组 (len=1)。
pub(super) fn group_consecutive_tool_runs(
    event_ids: &[u64],
    trace_events: &[TraceEvent],
    trace_event_index: &std::collections::HashMap<u64, usize>,
) -> Vec<Vec<u64>> {
    let mut runs: Vec<Vec<u64>> = Vec::new();
    for id in event_ids {
        let tool_name = trace_event_index.get(id).and_then(|&i| trace_events.get(i)).and_then(|ev| {
            if let TraceKind::ToolCall { ref name, .. } = ev.kind { Some(name.as_str()) } else { None }
        });
        // 尝试追加到前一 run (必须都是 ToolCall 且同名)
        let append = if let (Some(prev_run), Some(cur_name)) = (runs.last(), tool_name) {
            // 前一 run 的首 id 对应的 tool name
            let prev_name = trace_event_index.get(&prev_run[0]).and_then(|&i| trace_events.get(i)).and_then(|ev| {
                if let TraceKind::ToolCall { ref name, .. } = ev.kind { Some(name.as_str()) } else { None }
            });
            prev_name == Some(cur_name)
        } else {
            false
        };
        if append {
            runs.last_mut().unwrap().push(*id);
        } else {
            runs.push(vec![*id]);
        }
    }
    runs
}

/// 渲染单条 trace event (非合并路径)。
///
/// 引用关系: 从 build_message_lines Trace 展开态调用
/// 生命周期: 单帧渲染,输出 push 到 `lines`
pub(super) fn render_single_trace_event<'a>(
    ev: &TraceEvent,
    bar: &Span<'a>,
    theme: &Theme,
    max_lines_think: usize,
    max_lines_tool: usize,
    max_width: usize,
    lines: &mut Vec<Line<'a>>,
) {
    match &ev.kind {
        TraceKind::Thinking { text, lines: l_count } => {
            // V29.12: 消息内不重复显示 time (timeline panel 已有),
            //   与 ToolCall (`⚙ name · ✓ · dur`) 风格对称
            lines.push(Line::from(vec![
                bar.clone(),
                Span::raw("  "),
                Span::styled("💭 ", Style::default().fg(theme.accent)),
                Span::styled(
                    format!("Thinking · {}行", l_count),
                    theme.text_style(TextRole::Caption),
                ),
            ]));
            // bar(1)+"  "(2)=3 chars overhead; max_width 已由调用方减去容器宽度开销
            let detail_lines = render_block_detail_with_limit(
                text, &BlockKind::Think, theme, max_lines_think, max_width.saturating_sub(3),
            );
            for dl in detail_lines {
                let mut spans: Vec<Span> = vec![bar.clone(), Span::raw("  ")];
                spans.extend(dl.spans);
                lines.push(Line::from(spans));
            }
        }
        TraceKind::ToolCall { name, args, output, status } => {
            let (status_icon, status_color) = match status {
                ToolStatus::Success => ("✓", theme.success),
                ToolStatus::Failed => ("✗", theme.error),
                ToolStatus::Running => ("⟳", theme.gold),
            };
            let dur_str = ev.duration_ms.map(|ms| format_duration_ms_padded(ms)).unwrap_or_default();
            lines.push(Line::from(vec![
                bar.clone(),
                Span::raw("  "),
                Span::styled("⚙ ", Style::default().fg(theme.gold)),
                Span::styled(name.clone(), Style::default().fg(theme.gold).add_modifier(Modifier::BOLD)),
                Span::raw(" · "),
                Span::styled(status_icon, Style::default().fg(status_color)),
                Span::styled(dur_str, theme.text_style(TextRole::Caption)),
            ]));
            if !args.is_empty() {
                // V29.11: 工具特化视图链
                // 将 output 传入 diff 渲染以提取 start_line（文件实际行号）
                let output_ref = output.as_deref();
                let arg_lines = try_render_edit_diff_with_output(
                    name, args, output_ref, theme, max_lines_tool,
                ).or_else(|| try_render_bash_exec(
                    name, args, theme, max_lines_tool,
                )).unwrap_or_else(|| render_block_detail_with_limit(
                    args, &BlockKind::ToolCall, theme, max_lines_tool, max_width.saturating_sub(3),
                ));
                for dl in arg_lines {
                    let mut spans: Vec<Span> = vec![bar.clone(), Span::raw("  ")];
                    spans.extend(dl.spans);
                    lines.push(Line::from(spans));
                }
            }
            if let Some(out) = output {
                if !out.is_empty() {
                    lines.push(Line::from(vec![
                        bar.clone(),
                        Span::raw("  "),
                        Span::styled("→", theme.text_style(TextRole::Caption)),
                    ]));
                    let out_lines = render_block_detail_with_limit(
                        out, &BlockKind::ToolCall, theme, max_lines_tool, max_width.saturating_sub(3),
                    );
                    for dl in out_lines {
                        let mut spans: Vec<Span> = vec![bar.clone(), Span::raw("  ")];
                        spans.extend(dl.spans);
                        lines.push(Line::from(spans));
                    }
                }
            }
        }
        TraceKind::Generic { content } => {
            lines.push(Line::from(vec![
                bar.clone(),
                Span::raw("  "),
                Span::styled(
                    format!("· {} · {}", ev.time, content),
                    theme.text_style(TextRole::Caption),
                ),
            ]));
        }
        TraceKind::Reply { tokens } => {
            lines.push(Line::from(vec![
                bar.clone(),
                Span::raw("  "),
                Span::styled(
                    format!("↩ reply · {} tok", tokens),
                    theme.text_style(TextRole::Caption),
                ),
            ]));
        }
    }
}

/// V29.12: 渲染合并的 tool call run (连续 ≥2 次同名调用)。
///
/// 视觉:
/// ```text
/// ⚙ fs_read ×3 · ✓ · 35ms
///      /path/to/a.rs
///      /path/to/b.rs
///      /path/to/c.rs
/// ```
///
/// 策略:
///   - Header: `⚙ name ×N · 聚合状态 · 总耗时`
///   - 聚合状态: 全成功 → ✓; 有失败 → `✓M ✗K`; 全运行中 → ⟳
///   - 每条调用提取关键参数摘要 (path 字段优先; 无则取 args 首行截断到 60 字符)
///   - 编辑类工具(fs_edit/fs_write)仍显示 diff,不做摘要退化
///
/// 引用关系: build_message_lines → group 分支
/// 生命周期: 单帧渲染
pub(super) fn render_merged_tool_run<'a>(
    run: &[u64],
    trace_events: &[TraceEvent],
    trace_event_index: &std::collections::HashMap<u64, usize>,
    bar: &Span<'a>,
    theme: &Theme,
    code_blocks_expanded: bool,
    expanded_event_ids: &std::collections::HashSet<u64>,
    max_width: usize,
    lines: &mut Vec<Line<'a>>,
) {
    // 收集本组 events（部分可能已被 FIFO 裁剪）
    let events: Vec<&TraceEvent> = run.iter()
        .filter_map(|id| trace_event_index.get(id).and_then(|&i| trace_events.get(i)))
        .collect();
    if events.is_empty() {
        // 全部过期: 显示占位提示
        lines.push(Line::from(vec![
            bar.clone(),
            Span::raw("  "),
            Span::styled(
                format!("[{} events 已过期]", run.len()),
                theme.text_style(TextRole::Hint),
            ),
        ]));
        return;
    }

    // 从首 event 取 tool name
    let tool_name = match &events[0].kind {
        TraceKind::ToolCall { ref name, .. } => name.clone(),
        _ => return, // 不应发生
    };

    // 聚合状态
    let mut ok = 0u32;
    let mut fail = 0u32;
    let mut running = 0u32;
    let mut total_dur_ms: u64 = 0;
    for ev in &events {
        if let TraceKind::ToolCall { status, .. } = &ev.kind {
            match status {
                ToolStatus::Success => ok += 1,
                ToolStatus::Failed => fail += 1,
                ToolStatus::Running => running += 1,
            }
        }
        if let Some(d) = ev.duration_ms { total_dur_ms += d; }
    }

    let status_text = if fail == 0 && running == 0 {
        "✓".to_string()
    } else if running > 0 {
        format!("⟳{}", running)
    } else {
        format!("✓{} ✗{}", ok, fail)
    };
    let status_color = if fail > 0 { theme.error } else if running > 0 { theme.gold } else { theme.success };

    let dur_str = {
        let d = format_duration_ms(total_dur_ms);
        if d.is_empty() { String::new() } else { format!("  {}", d) }
    };

    // Header: ⚙ name ×N · status · dur
    lines.push(Line::from(vec![
        bar.clone(),
        Span::raw("  "),
        Span::styled("⚙ ", Style::default().fg(theme.gold)),
        Span::styled(
            format!("{} ×{}", tool_name, events.len()),
            Style::default().fg(theme.gold).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" · "),
        Span::styled(status_text, Style::default().fg(status_color)),
        Span::styled(dur_str, theme.text_style(TextRole::Caption)),
    ]));

    // 判断是否为编辑类工具(需要 diff 而非摘要)
    let is_edit_tool = tool_name.contains("edit") || tool_name.contains("write");

    // 逐条摘要或 diff
    let fully_expanded = code_blocks_expanded || run.iter().any(|id| expanded_event_ids.contains(id));
    let max_lines_tool = if fully_expanded { 0 } else { 20 };

    for (ei, ev) in events.iter().enumerate() {
        if let TraceKind::ToolCall { args, output, status, .. } = &ev.kind {
            if is_edit_tool && !args.is_empty() {
                // 编辑工具: 尝试 diff 视图
                // 多条编辑间插入 path 标识以区分不同文件(避免 diff 连片)
                if events.len() > 1 {
                    let path_hint = extract_tool_param_summary(args);
                    lines.push(Line::from(vec![
                        bar.clone(),
                        Span::raw("  "),
                        Span::styled(
                            format!("{}. {}", ei + 1, path_hint),
                            Style::default().fg(theme.muted).add_modifier(Modifier::ITALIC),
                        ),
                    ]));
                }
                let arg_lines = try_render_edit_diff_with_output(
                    &tool_name, args, output.as_deref(), theme, max_lines_tool,
                ).unwrap_or_else(|| {
                    // fallback: 提取 path 摘要（单条时已有 path_hint 不重复）
                    vec![Line::from(Span::styled(
                        extract_tool_param_summary(args),
                        theme.text_style(TextRole::Caption),
                    ))]
                });
                for dl in arg_lines {
                    let mut spans: Vec<Span> = vec![bar.clone(), Span::raw("  ")];
                    spans.extend(dl.spans);
                    lines.push(Line::from(spans));
                }
            } else if !args.is_empty() {
                // 非编辑工具: 单行摘要
                let summary = extract_tool_param_summary(args);
                let (si, sc) = match status {
                    ToolStatus::Success => ("✓", theme.success),
                    ToolStatus::Failed => ("✗", theme.error),
                    ToolStatus::Running => ("⟳", theme.gold),
                };
                let item_dur = ev.duration_ms.map(|ms| {
                    let d = format_duration_ms(ms); if d.is_empty() { d } else { format!(" {}", d) }
                }).unwrap_or_default();
                lines.push(Line::from(vec![
                    bar.clone(),
                    Span::raw("  "),
                    Span::styled(si, Style::default().fg(sc)),
                    Span::raw(" "),
                    Span::styled(summary, theme.text_style(TextRole::Caption)),
                    Span::styled(item_dur, theme.text_style(TextRole::Hint)),
                ]));
            }
            // output 合并时省略 (避免过于冗长),展开后会走单条路径显示
            if fully_expanded {
                if let Some(out) = output {
                    if !out.is_empty() {
                        lines.push(Line::from(vec![
                            bar.clone(),
                            Span::raw("  "),
                            Span::styled("→", theme.text_style(TextRole::Caption)),
                        ]));
                        let out_lines = render_block_detail_with_limit(
                            out, &BlockKind::ToolCall, theme, max_lines_tool, max_width.saturating_sub(3),
                        );
                        for dl in out_lines {
                            let mut spans: Vec<Span> = vec![bar.clone(), Span::raw("  ")];
                            spans.extend(dl.spans);
                            lines.push(Line::from(spans));
                        }
                    }
                }
            }
        }
    }
}

/// 统一耗时格式化（M+S 展示）。
///
/// | 输入 ms | 输出 |
/// |---------|------|
/// | 0 | "" |
/// | 120 | "120ms" |
/// | 1500 | "1.5s" |
/// | 65000 | "1m5s" |
/// | 130000 | "2m10s" |
///
/// 引用关系: 被 trace 渲染 / streaming tools / timeline / 合并组 header 共用
/// 生命周期: 纯函数,无状态
pub(super) fn format_duration_ms(ms: u64) -> String {
    // 0 = 无耗时数据（聚合场景下多个 None 求和为 0）→ 不显示
    // 调用方如需区分 "瞬间完成 (Some(0))" vs "无数据 (None)"，
    // 应在 .map() 外层处理 None → 不调用本函数
    if ms == 0 {
        return String::new();
    }
    if ms < 1000 {
        return format!("{}ms", ms);
    }
    let total_secs = ms / 1000;
    let frac_ms = ms % 1000;
    if total_secs < 60 {
        // < 1 分钟: 显示秒 + 小数 (如 1.5s / 45s)
        if frac_ms >= 100 {
            format!("{}.{}s", total_secs, frac_ms / 100)
        } else {
            format!("{}s", total_secs)
        }
    } else {
        // ≥ 1 分钟: M分S秒 (如 1m5s / 2m10s)
        let mins = total_secs / 60;
        let secs = total_secs % 60;
        if secs > 0 {
            format!("{}m{}s", mins, secs)
        } else {
            format!("{}m", mins)
        }
    }
}

/// 同 format_duration_ms 但带前导双空格（用于 span 拼接场景）
pub(super) fn format_duration_ms_padded(ms: u64) -> String {
    let s = format_duration_ms(ms);
    if s.is_empty() { s } else { format!("  {}", s) }
}

/// 从 tool args JSON 中提取关键参数作为单行摘要。
///
/// 优先级: `path` → `file_path` → `url` → `query` → `command` → 首行截断60字符
pub(crate) fn extract_tool_param_summary(args_json: &str) -> String {
    // 辅助: UTF-8 安全截断 — 按 char 数而非字节切,避免多字节字符中间切断 panic
    fn truncate_chars(s: &str, max: usize) -> String {
        if s.chars().count() <= max { s.to_string() }
        else { format!("{}…", s.chars().take(max).collect::<String>()) }
    }

    if let Ok(obj) = serde_json::from_str::<serde_json::Value>(args_json) {
        // 按优先级尝试提取有意义的字段
        for key in &["path", "file_path", "url", "query", "command", "pattern", "selector"] {
            if let Some(val) = obj.get(*key).and_then(|v| v.as_str()) {
                return truncate_chars(val, 60);
            }
        }
        // fallback: 序列化首 60 字符
        let s = serde_json::to_string(&obj).unwrap_or_default();
        truncate_chars(&s, 60)
    } else {
        // 非 JSON: 首行截断
        let first_line = args_json.lines().next().unwrap_or("");
        truncate_chars(first_line, 60)
    }
}

/// V29.11: 编辑类工具（Edit/Write 等）的 args 渲染为 +/- diff 视图
///
/// 触发条件:
///   - tool name 落在白名单（含 mcp__filengine__file_edit / file_write 与裸名）
///   - args JSON 可解析且含 path + (old/new_string|content) 字段
/// 返回: Some(lines) 跳过默认 JSON pretty 渲染; None 退回默认路径
/// 引用关系: build_message_lines TraceKind::ToolCall args 分支前置
/// 生命周期: 单次渲染, 不缓存 (args/output 字符串已 owned 在 trace_events 内)
/// 设计取舍:
///   - 简单 diff (全旧 - / 全新 +) 而非 LCS 智能对比 — 编辑工具的 old/new
///     通常已是聚焦 chunk, 简单视图够用; 后续要更准可引 `similar` crate
/// output_json: 工具输出 JSON（可选）—— 从中提取 `start_line` 用于显示文件真实行号
/// fs_edit 在成功时返回 `{"edited":true,"path":"...","start_line":N}`
/// 无 output_json 或字段缺失时行号从 1 开始（相对编号）
pub(super) fn try_render_edit_diff(
    name: &str,
    args_json: &str,
    theme: &Theme,
    max_total_lines: usize,
) -> Option<Vec<Line<'static>>> {
    try_render_edit_diff_with_output(name, args_json, None, theme, max_total_lines)
}

/// 返回 `'static` 生命周期：所有 span 内容均为 owned String，可安全缓存在 AppState
pub(super) fn try_render_edit_diff_with_output(
    name: &str,
    args_json: &str,
    output_json: Option<&str>,
    theme: &Theme,
    max_total_lines: usize,
) -> Option<Vec<Line<'static>>> {
    let lower = name.to_lowercase();
    // 2026-05-28: ToolId 直接用原始名（fs_edit / fs_write）
    let is_edit = lower.as_str() == "fs_edit";
    let is_write = lower.as_str() == "fs_write";
    if !is_edit && !is_write { return None; }

    let json: serde_json::Value = serde_json::from_str(args_json).ok()?;
    let path = json.get("path")
        .or_else(|| json.get("file_path"))
        .and_then(|v| v.as_str())
        .unwrap_or("(unknown)");

    let mut lines: Vec<Line<'static>> = Vec::new();
    // 头行：📝 path + 下方一条极细分隔线（区分文件路径与 diff 内容）
    lines.push(Line::from(vec![
        Span::styled("📝 ", Style::default().fg(theme.accent)),
        Span::styled(
            path.to_string(),
            Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
        ),
    ]));

    let (old, new) = if is_edit {
        let o = json.get("old_string")
            .or_else(|| json.get("old_text"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let n = json.get("new_string")
            .or_else(|| json.get("new_text"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        (o.to_string(), n.to_string())
    } else {
        // Write 是新建/全量覆盖, 无旧内容
        let c = json.get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        (String::new(), c.to_string())
    };

    // 从工具输出提取 start_line（文件中的实际起始行号）
    // fs_edit 成功时返回 {"edited":true,"start_line":N}，无字段则从 1 开始（相对行号）
    let line_offset: usize = output_json
        .and_then(|o| serde_json::from_str::<serde_json::Value>(o).ok())
        .and_then(|v| v.get("start_line").and_then(|n| n.as_u64()))
        .map(|n| n as usize)
        .unwrap_or(1);

    render_simple_diff(&mut lines, &old, &new, line_offset, theme, max_total_lines);
    Some(lines)
}

/// LCS diff 渲染 — 基于 `similar` crate 的行级差分
///
/// 引用关系: 仅 try_render_edit_diff_with_output 内部调用
/// 限行算法: max_total_lines==0 不限; >0 时超额后截断 + 省略提示
/// 设计取舍:
///   - 用 similar::TextDiff::from_lines (Myers LCS, O(N·D))
///   - Equal 行显示为 context（theme.muted, 无前缀符号），但只保留变更临近 ±1 行
///   - Insert → `+ line` 绿; Delete → `- line` 红; 远离变更的 Equal 跳过
///   - Write (old 为空) 时 similar 全产出 Insert — 效果等同旧实现
fn render_simple_diff(
    lines: &mut Vec<Line<'static>>,
    old: &str,
    new: &str,
    line_offset: usize,  // 文件实际起始行号（1-based），从 fs_edit output.start_line 注入
    theme: &Theme,
    max_total_lines: usize,
) {
    use similar::{ChangeTag, TextDiff};

    let diff = TextDiff::from_lines(old, new);
    let mut rendered: Vec<Line<'static>> = Vec::new();
    let mut insert_count = 0usize;
    let mut delete_count = 0usize;

    // 收集所有变更 ops, 带上下文(±2 行 Equal)
    let changes: Vec<_> = diff.iter_all_changes().collect();
    let total_changes = changes.len();
    // 标记哪些 Equal 行要显示(距最近 Insert/Delete ≤2 行)
    let mut show_equal = vec![false; total_changes];
    for (i, c) in changes.iter().enumerate() {
        if c.tag() != ChangeTag::Equal {
            for offset in 1..=2 {
                if i >= offset && changes[i - offset].tag() == ChangeTag::Equal { show_equal[i - offset] = true; }
                if i + offset < total_changes && changes[i + offset].tag() == ChangeTag::Equal { show_equal[i + offset] = true; }
            }
        }
    }

    // 计算行号宽度（考虑起始偏移，如文件第 150 行需要 3 位宽）
    let old_line_count = old.lines().count();
    let new_line_count = new.lines().count();
    let max_line_num = (line_offset - 1 + old_line_count.max(new_line_count)).max(1);
    let num_width = if max_line_num >= 1000 { 4 } else if max_line_num >= 100 { 3 } else if max_line_num >= 10 { 2 } else { 1 };

    // 行号计数：从 line_offset 开始（文件实际行号，非相对行号）
    let mut old_line = line_offset.saturating_sub(1); // 下方 += 1 后为 line_offset
    let mut new_line = line_offset.saturating_sub(1);

    // 文件路径下方轻量分隔（同 code block ╭ 风格，无额外空行）
    lines.push(Line::from(vec![
        Span::styled(
            format!("{:─<w$}", "", w = num_width + 2),
            Style::default().fg(theme.border).add_modifier(Modifier::DIM),
        ),
    ]));

    let mut skipped_run = false;
    for (i, change) in changes.iter().enumerate() {
        let text = change.value().trim_end_matches('\n');
        match change.tag() {
            ChangeTag::Delete => {
                old_line += 1;
                if skipped_run {
                    // skip 指示符：右对齐行号列宽后显示 ⋯（视觉连续性）
                    rendered.push(Line::from(vec![Span::styled(
                        format!("{:>w$} ⋯", "", w = num_width),
                        theme.text_style(TextRole::Caption),
                    )]));
                    skipped_run = false;
                }
                // 删除行：行号(dim) + "─ "(error bold) + 内容(error)
                rendered.push(Line::from(vec![
                    Span::styled(format!("{:>w$} ", old_line, w = num_width), Style::default().fg(theme.muted)),
                    Span::styled("─ ", Style::default().fg(theme.error).add_modifier(Modifier::BOLD)),
                    Span::styled(text.to_string(), Style::default().fg(theme.error)),
                ]));
                delete_count += 1;
            }
            ChangeTag::Insert => {
                new_line += 1;
                if skipped_run {
                    rendered.push(Line::from(vec![Span::styled(
                        format!("{:>w$} ⋯", "", w = num_width),
                        theme.text_style(TextRole::Caption),
                    )]));
                    skipped_run = false;
                }
                // 新增行：行号(dim) + "+ "(success bold) + 内容(success)
                rendered.push(Line::from(vec![
                    Span::styled(format!("{:>w$} ", new_line, w = num_width), Style::default().fg(theme.muted)),
                    Span::styled("+ ", Style::default().fg(theme.success).add_modifier(Modifier::BOLD)),
                    Span::styled(text.to_string(), Style::default().fg(theme.success)),
                ]));
                insert_count += 1;
            }
            ChangeTag::Equal => {
                old_line += 1;
                new_line += 1;
                if show_equal[i] {
                    if skipped_run {
                        rendered.push(Line::from(vec![Span::styled(
                            format!("{:>w$} ⋯", "", w = num_width),
                            theme.text_style(TextRole::Caption),
                        )]));
                        skipped_run = false;
                    }
                    // 上下文行：行号(dim) + "· "(muted dim) + 内容(muted)
                    rendered.push(Line::from(vec![
                        Span::styled(format!("{:>w$} ", new_line, w = num_width), Style::default().fg(theme.muted).add_modifier(Modifier::DIM)),
                        Span::styled("· ", Style::default().fg(theme.muted).add_modifier(Modifier::DIM)),
                        Span::styled(text.to_string(), Style::default().fg(theme.muted).add_modifier(Modifier::DIM)),
                    ]));
                } else {
                    skipped_run = true;
                }
            }
        }
    }

    // 限行裁剪
    let shown = if max_total_lines > 0 && rendered.len() > max_total_lines {
        let mut truncated: Vec<Line<'static>> = rendered.into_iter().take(max_total_lines).collect();
        truncated.push(Line::from(vec![
            Span::styled(
                format!("{:>w$} ↳ 已截断（共 {} 行）", "", total_changes, w = num_width),
                theme.text_style(TextRole::Caption),
            ),
        ]));
        truncated
    } else {
        rendered
    };
    lines.extend(shown);

    // 统计 footer：紧凑 "+ N − N"，无斜杠
    if insert_count > 0 || delete_count > 0 {
        lines.push(Line::from(vec![
            Span::styled(
                format!("{:>w$} ", "", w = num_width),
                Style::default(),
            ),
            Span::styled(format!("+{} ", insert_count), Style::default().fg(theme.success).add_modifier(Modifier::DIM)),
            Span::styled(format!("−{}", delete_count), Style::default().fg(theme.error).add_modifier(Modifier::DIM)),
        ]));
    }
}

/// V28: 带行数软上限的 detail 渲染。`max_lines = 0` 表示不限(旧行为);
/// `max_lines > 0` 时超过则截到 max_lines 并追加 `↳ +N 行 Ctrl+E 展开全部` 提示行。
///
/// 注意:ToolCall 内部还有 400/200 行硬上限(超长 tool output 兜底),与软上限独立生效。
/// 调用方决定 max_lines:Trace 中 thinking=30, tool=20;Block 直接展开传 0。
///
/// 引用关系: 被 render_block_detail (传 0) 和 build_message_lines Trace 分支(传 30/20) 调用
pub(super) fn render_block_detail_with_limit<'a>(detail: &str, kind: &BlockKind, theme: &Theme, max_lines: usize, max_width: usize) -> Vec<Line<'a>> {
    let lines: Vec<Line<'a>> = match kind {
        BlockKind::Think => {
            // 走 markdown 渲染——空 bar，让思考块按结构化文本展示
            // T1修复：传入实际宽度，避免固定 80 宽导致表格溢出
            // max_width 已由调用方减去 bar+内嵌缩进开销
            let empty_bar = Span::raw("");
            let md_width = max_width.max(20);
            let styled_lines = markdown::render_markdown_bounded(detail, theme, false, md_width);
            styled_lines.iter()
                .map(|s| markdown::styled_line_to_ratatui(s, &empty_bar, theme))
                .collect()
        }
        BlockKind::ToolCall => {
            // 尝试 JSON pretty-print；失败则降级为 plain
            let pretty = serde_json::from_str::<serde_json::Value>(detail.trim())
                .ok()
                .and_then(|v| serde_json::to_string_pretty(&v).ok());
            let text = pretty.as_deref().unwrap_or(detail);
            // 限长保护：超过 400 行只显示前 200 + 截断提示
            let all_lines: Vec<&str> = text.lines().collect();
            let total = all_lines.len();
            let truncated = total > 400;
            let mut out: Vec<Line> = all_lines.iter()
                .take(if truncated { 200 } else { total })
                .map(|l| Line::from(vec![
                    Span::styled(l.to_string(), theme.text_style(TextRole::InlineCode)),
                ]))
                .collect();
            if truncated {
                out.push(Line::from(vec![
                    Span::styled(
                        format!("… (已截断，原始 {} 行；用 /export 导出完整)", total),
                        theme.text_style(TextRole::Hint),
                    ),
                ]));
            }
            out
        }
        BlockKind::Checklist => {
            detail.lines()
                .map(|l| Line::from(vec![
                    Span::styled(l.to_string(), theme.text_style(TextRole::Caption)),
                ]))
                .collect()
        }
    };

    // V28: 应用软上限 — 超出 max_lines 时截断并追加折叠提示行
    if max_lines > 0 && lines.len() > max_lines {
        let hidden = lines.len() - max_lines;
        let mut limited: Vec<Line<'a>> = lines.into_iter().take(max_lines).collect();
        limited.push(Line::from(vec![
            Span::styled(
                format!("↳ +{} 行  Ctrl+E 展开全部", hidden),
                theme.text_style(TextRole::Caption),
            ),
        ]));
        return limited;
    }
    lines
}
