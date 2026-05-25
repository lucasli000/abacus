//! TUI 多语言支持 — 中英双语，跟随系统 LANG 环境变量
//!
//! ## 架构
//! 轻量静态翻译表 + OnceLock 全局语言选择（进程生命周期内不变）
//!
//! ## 依赖
//! - `std::sync::OnceLock`: 一次性初始化语言设置
//! - `std::env`: 读取 LANG/LC_ALL/ABACUS_LANG
//!
//! ## 引用关系
//! - 被所有 TUI 组件调用（bars.rs, overlays.rs, extras.rs, panel.rs, run.rs）
//! - `init_lang()` 在 main.rs 启动时调用一次
//!
//! ## 生命周期
//! - `init_lang()`: 进程启动时调用一次，写入 OnceLock
//! - `t(key)`: 任意时刻调用，返回 &'static str（零分配）
//! - 语言设置进程内不可变

use std::sync::OnceLock;

/// 支持的 UI 语言
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    /// 简体中文
    Zh,
    /// English
    En,
}

/// 全局语言设置（进程启动时一次性确定）
static LANGUAGE: OnceLock<Lang> = OnceLock::new();

/// 初始化语言设置。检测优先级：
/// 1. ABACUS_LANG 环境变量（显式覆盖）
/// 2. LANG / LC_ALL 系统环境变量
/// 3. 默认 En
///
/// 调用时机：main.rs 进程启动时调用一次
pub fn init_lang() {
    let lang_str = std::env::var("ABACUS_LANG")
        .or_else(|_| std::env::var("LC_ALL"))
        .or_else(|_| std::env::var("LANG"))
        .unwrap_or_default()
        .to_lowercase();

    let detected = if lang_str.starts_with("zh") {
        Lang::Zh
    } else {
        Lang::En
    };
    LANGUAGE.set(detected).ok();
}

/// 获取当前语言设置
#[inline]
pub fn lang() -> Lang {
    LANGUAGE.get().copied().unwrap_or(Lang::En)
}

/// 翻译查找——返回当前语言对应的静态字符串
///
/// 使用：`t("mode.clarify")` → "澄清" 或 "Clarify"
///
/// 未匹配的 key 返回 "???" 占位符（开发期可快速发现遗漏）
pub fn t(key: &'static str) -> &'static str {
    let zh = matches!(lang(), Lang::Zh);
    match key {
        // ── 模式名（中文生动短词，英文保持原样）──
        "mode.clarify" => if zh { "聊聊" } else { "Clarify" },
        "mode.meeting" => if zh { "会诊" } else { "Meeting" },
        "mode.plan" => if zh { "谋划" } else { "Plan" },
        "mode.team" => if zh { "干活" } else { "Team" },

        // ── 顶栏 ──
        "top.plan_tag" => "[PLAN]",

        // ── 状态栏提示 ──
        "hint.paused" => if zh { "⏸ 已暂停 · Esc 继续" } else { "⏸ Paused · Esc resume" },
        "hint.completing" => if zh { "Tab 候选 · Enter 确认 · Esc 取消" } else { "Tab select · Enter confirm · Esc cancel" },
        "hint.panel_focus" => if zh { "[ ] 切看板Tab · ↑↓ 滚动 · Esc 回输入" } else { "[ ] Switch tab · ↑↓ Scroll · Esc back" },
        "hint.cmd_focus" => if zh { "↑↓ 选命令 · Enter 填充 · Esc 回输入" } else { "↑↓ Select · Enter fill · Esc back" },
        "hint.input_default" => if zh { "Tab 缩进 · Ctrl+Tab AI补全 · Ctrl+I 面板" } else { "Tab indent · Ctrl+Tab AI · Ctrl+I panel" },
        "hint.esc_cancel" => if zh { "Esc取消" } else { "Esc cancel" },
        "hint.panel_hidden" => if zh { "Ctrl+I 显示看板 · Ctrl+B 切焦点" } else { "Ctrl+I panel · Ctrl+B focus" },
        "hint.panel_visible" => if zh { "Ctrl+B切焦点 · / 命令 · Tab补全" } else { "Ctrl+B focus · / cmd · Tab complete" },

        // ── 确认弹窗 ──
        "confirm.allow" => if zh { "允许" } else { "Allow" },
        "confirm.deny" => if zh { "拒绝" } else { "Deny" },
        "confirm.always" => if zh { "总是允许" } else { "Always allow" },

        // ── Toast 消息 ──
        "toast.timeout_allow" => if zh { "⏱ 超时 → session 内允许（不持久化）" } else { "⏱ Timeout → session allow (not persisted)" },
        "toast.timeout_reject" => if zh { "⏱ 超时 → 已拒绝（破坏性操作需显式确认）" } else { "⏱ Timeout → rejected (destructive ops need explicit confirm)" },
        "toast.auto_allow" => if zh { "自动放行" } else { "Auto-allowed" },

        // ── 事件日志（中文生动，英文保持原样）──
        "event.thinking" => if zh { "思考" } else { "Thinking" },
        "event.working" => if zh { "执行" } else { "Working" },
        "event.outputting" => if zh { "落笔" } else { "Outputting" },
        "event.ready" => if zh { "待命" } else { "Ready" },
        "event.gen_complete" => if zh { "搞定" } else { "Generation complete" },
        "event.wait_auth" => if zh { "请示" } else { "Awaiting auth" },
        "event.compacting" => if zh { "瘦身" } else { "Compacting" },
        "event.auto_allow_legacy" => if zh { "自动放行（已授权工具，legacy 路径）" } else { "Auto-allowed (authorized tool, legacy path)" },
        "event.authorized" => if zh { "已授权（继续执行）" } else { "Authorized (continuing)" },
        "event.denied" => if zh { "已拒绝" } else { "Denied" },

        // ── 面板标签 ──
        "panel.scene" => if zh { "现场" } else { "Scene" },
        "panel.tasks" => if zh { "任务" } else { "Tasks" },
        "panel.quant" => if zh { "统计" } else { "Quant" },
        "panel.custom" => if zh { "自定义" } else { "Custom" },

        // ── 面板字段 ──
        "field.confidence" => if zh { "置信度" } else { "Confidence" },
        "field.owner" => if zh { "负责人" } else { "Owner" },
        "field.deps" => if zh { "依赖" } else { "Dependencies" },
        "field.status" => if zh { "状态" } else { "Status" },

        // ── 空状态 ──
        "empty.experts" => if zh { " 暂无专家接入" } else { " No experts connected" },
        "empty.tasks" => if zh { " 暂无任务" } else { " No tasks" },
        "empty.invite_hint" => if zh { " 输入 /invite 邀请专家" } else { " Type /invite to add experts" },

        // ── 输入框 ──
        "input.placeholder" => if zh { "输入消息..." } else { "Type a message..." },
        "input.ready" => if zh { "Ready · Enter 发送" } else { "Ready · Enter to send" },

        // ── 通用标签 ──
        "label.expert" => if zh { " 专家 " } else { " Experts " },
        "label.kanban" => if zh { " 任务看板 " } else { " Task Board " },
        "label.settings" => if zh { " 设置 (Settings) " } else { " Settings " },
        "label.irreversible" => if zh { "此操作不可撤销" } else { "This action is irreversible" },
        "label.thinking_depth" => if zh { " 思考深度 " } else { " Thinking Depth " },

        // ── 面板 section headers ──
        "panel.team" => if zh { " 🧠 团队" } else { " 🧠 Team" },
        "panel.tasks_header" => if zh { " 📋 任务" } else { " 📋 Tasks" },
        "panel.participants" => if zh { " 🎙 参会者" } else { " 🎙 Participants" },
        "panel.decisions" => if zh { " 📝 决策" } else { " 📝 Decisions" },
        "panel.timeline" => if zh { " 📜 时间线" } else { " 📜 Timeline" },
        "panel.memory" => if zh { " 🧠 记忆" } else { " 🧠 Memory" },
        "panel.tools" => if zh { " 🔧 工具" } else { " 🔧 Tools" },
        "panel.stats" => if zh { " 📊 统计" } else { " 📊 Stats" },
        "panel.model_dist" => if zh { " 🤖 模型分布" } else { " 🤖 Model Distribution" },
        "panel.mode_dist" => if zh { " 🎭 模式分布" } else { " 🎭 Mode Distribution" },
        "panel.tool_calls" => if zh { " 🛠 工具调用" } else { " 🛠 Tool Calls" },
        "panel.turn_trend" => if zh { " 📈 轮次趋势" } else { " 📈 Turn Trends" },
        "panel.knowledge" => if zh { " 📚 知识宫殿" } else { " 📚 Knowledge Palace" },
        "panel.no_data" => if zh { "  (无数据)" } else { "  (no data)" },
        "panel.pipeline" => if zh { " ⚡ Pipeline" } else { " ⚡ Pipeline" },
        "panel.changes" => if zh { " 📁 变更" } else { " 📁 Changes" },
        "panel.knowledge_short" => if zh { " 📚 知识" } else { " 📚 Knowledge" },
        "panel.context" => if zh { " 📦 上下文" } else { " 📦 Context" },
        "stat.used" => if zh { "    已用 " } else { "    Used " },
        "stat.kv_cache" => if zh { "    KV缓存 " } else { "    KV Cache " },
        "stat.compress" => if zh { "    压缩 " } else { "    Compress " },
        "compress.phase" => if zh { "压缩上下文中..." } else { "compressing context..." },
        "compress.toast_start" => if zh { "上下文压缩中..." } else { "Compressing context..." },
        "compress.toast_done" => if zh { "压缩完成" } else { "Compression done" },
        "compress.event" => if zh { "压缩" } else { "compressed" },
        "compress.note" => if zh { "上下文压缩" } else { "Context compressed" },
        "stat.cache_hit" => if zh { "缓存" } else { "cache" },
        "timeline.thinking" => if zh { "推理" } else { "Reasoning" },
        "timeline.earlier" => if zh { "更早的步骤" } else { "earlier steps" },
        "panel.agenda" => if zh { "议程" } else { "Agenda" },
        "panel.theme_preview" => if zh { " 主题预览 " } else { " Theme Preview " },

        // ── 面板统计字段 ──
        "stat.mode" => if zh { "   模式  " } else { "   Mode  " },
        "stat.turns" => if zh { "   轮次  " } else { "   Turns  " },
        "stat.conv" => if zh { "   对话  " } else { "   Conv.  " },
        "stat.events" => if zh { "   事件  " } else { "   Events  " },
        "stat.input" => if zh { "   输入  " } else { "   Input  " },
        "stat.output" => if zh { "   输出  " } else { "   Output  " },
        "stat.total" => if zh { "   合计  " } else { "   Total  " },
        "stat.cost" => if zh { "   费用  " } else { "   Cost  " },
        "stat.system" => if zh { "  系统 " } else { "  System " },
        "stat.categories" => if zh { "  种类 " } else { "  Types " },
        "stat.calls" => if zh { "  调用 " } else { "  Calls " },
        "stat.entities" => if zh { "    实体 " } else { "    Entities " },
        "stat.experts" => if zh { "    专家  " } else { "    Experts  " },
        "stat.active_entities" => if zh { "   👥 激活实体" } else { "   👥 Active Entities" },
        "stat.knowledge" => if zh { "   📚 知识" } else { "   📚 Knowledge" },

        // ── Overlay 弹窗 ──
        "overlay.terminal_too_small" => if zh { "终端太小，请调大窗口" } else { "Terminal too small, please resize" },
        "overlay.completion_title" => if zh { " 补全 (↑↓/Tab 选择 · Enter 确认 · Alt+1-9 直选 · Esc 取消) " } else { " Complete (↑↓/Tab · Enter · Alt+1-9 · Esc) " },
        "overlay.model_picker" => if zh { " 🤖 选择模型 (↑↓ 选模型 · ←→ 调思考 · Enter 应用 · Esc 取消) " } else { " 🤖 Model (↑↓ select · ←→ thinking · Enter · Esc) " },
        "overlay.theme_picker" => if zh { " 🎨 选择主题 (↑↓ Tab 移动, Enter 切换, Esc 取消) " } else { " 🎨 Theme (↑↓ Tab · Enter · Esc) " },
        "overlay.thinking_picker" => if zh { " 💭 思考深度 (↑↓ Tab 移动, Enter 切换, Esc 取消) " } else { " 💭 Thinking (↑↓ Tab · Enter · Esc) " },
        "overlay.settings_hint" => if zh { "↑↓ 选择 · Enter 确认 · Esc 关闭" } else { "↑↓ Select · Enter confirm · Esc close" },
        "overlay.configured" => if zh { "✓ 已配置" } else { "✓ Configured" },
        "overlay.not_configured" => if zh { "✗ 未配置" } else { "✗ Not configured" },
        "overlay.model_cycle" => if zh { "Enter 循环 (4 内置)" } else { "Enter cycle (4 built-in)" },
        "overlay.theme_cycle" => if zh { "Enter 循环 (12 主题)" } else { "Enter cycle (12 themes)" },
        "overlay.close" => if zh { "5. 关闭" } else { "5. Close" },

        // ── 命令面板 ──
        "cmd.title_focused" => if zh { " ⌘ 命令 (↑↓ 选择 · Enter 填充 · 点击直填) " } else { " ⌘ Commands (↑↓ · Enter · Click) " },
        "cmd.title_unfocused" => if zh { " ⌘ 可用命令 (↑↓ 自动聚焦 · 点击直填) " } else { " ⌘ Commands (↑↓ auto-focus · Click) " },

        // ── 消息区 ──
        "msg.welcome" => if zh { "输入问题开始对话，/help 查看命令" } else { "Type to start, /help for commands" },

        // ── Toast ──
        "toast.screen_cleared" => if zh { "屏幕已清屏" } else { "Screen cleared" },
        "toast.new_session" => if zh { "已创建新会话" } else { "New session created" },
        "toast.session_saved" => if zh { "会话已保存" } else { "Session saved" },
        "toast.demo_mode" => if zh { "演示模式 — 会话仅在内存中" } else { "Demo mode — session in memory only" },
        "toast.copy_fail" => if zh { "复制失败：本终端不支持剪贴板写入" } else { "Copy failed: clipboard not supported" },
        "toast.nothing_to_copy" => if zh { "无可复制的回复" } else { "Nothing to copy" },
        "toast.exiting" => if zh { "正在退出…" } else { "Exiting…" },
        "toast.engine_connected" => if zh { "引擎已连接，输入消息即可对话" } else { "Engine connected, start typing" },
        "toast.connecting" => if zh { "正在连接引擎..." } else { "Connecting engine..." },
        "toast.first_setup" => if zh { "首次使用，请完成配置" } else { "First run, please configure" },
        "toast.config_saved" => if zh { "配置已保存，正在连接引擎" } else { "Config saved, connecting engine" },
        "toast.auth_granted" => if zh { "🔓 已授权，继续执行" } else { "🔓 Authorized, continuing" },
        "toast.auth_denied" => if zh { "🚫 已拒绝工具执行" } else { "🚫 Tool execution denied" },

        // ── Slash 命令描述 ──
        "cmd.help" => if zh { "显示所有命令" } else { "Show all commands" },
        "cmd.clear" => if zh { "清空屏幕" } else { "Clear screen" },
        "cmd.new" => if zh { "新建会话" } else { "New session" },
        "cmd.save" => if zh { "保存当前会话" } else { "Save session" },
        "cmd.copy" => if zh { "复制最后回复到剪贴板" } else { "Copy last reply to clipboard" },
        "cmd.quit" => if zh { "退出" } else { "Quit" },
        "cmd.model" => if zh { "模型设置" } else { "Model settings" },
        "cmd.theme" => if zh { "切换主题" } else { "Switch theme" },
        "cmd.clarify" => if zh { "切换到 聊聊 模式" } else { "Switch to Clarify mode" },
        "cmd.plan" => if zh { "切换到 谋划 模式" } else { "Switch to Plan mode" },
        "cmd.team" => if zh { "切换到 干活 模式" } else { "Switch to Team mode" },
        "cmd.meeting" => if zh { "切换到 会诊 模式" } else { "Switch to Meeting mode" },
        "cmd.done" => if zh { "标记完成，推进下一阶段" } else { "Mark done, advance to next stage" },

        // ── 未匹配：返回 key 本身（'static 保证）──
        other => other,
    }
}

/// 带格式化参数的翻译（返回 String）
///
/// 用法：`tf("toast.mode_switch", &[mode_name])` → "已切换到 澄清"
pub fn tf(key: &'static str, args: &[&str]) -> String {
    let zh = matches!(lang(), Lang::Zh);
    match key {
        "toast.mode_switch" => {
            let mode = args.first().copied().unwrap_or("?");
            if zh { format!("已切换到 {}", mode) } else { format!("Switched to {}", mode) }
        }
        "event.timeout_allow" => {
            let action = args.first().copied().unwrap_or("?");
            if zh { format!("⏱ 超时自动允许（session 内）: {}", action) }
            else { format!("⏱ Timeout auto-allowed (session): {}", action) }
        }
        "event.timeout_reject" => {
            let action = args.first().copied().unwrap_or("?");
            if zh { format!("⏱ 超时拒绝（破坏性）: {}", action) }
            else { format!("⏱ Timeout rejected (destructive): {}", action) }
        }
        "event.auto_allow_tool" => {
            let tool = args.first().copied().unwrap_or("?");
            if zh { format!("自动放行（always_allow）: {}", tool) }
            else { format!("Auto-allowed (always_allow): {}", tool) }
        }
        "event.auth_tools" => {
            let tools = args.first().copied().unwrap_or("?");
            if zh { format!("已授权（继续执行）: {}", tools) }
            else { format!("Authorized (continuing): {}", tools) }
        }
        "event.wait_auth_tool" => {
            let tool = args.first().copied().unwrap_or("?");
            if zh { format!("等待授权: {}", tool) }
            else { format!("Awaiting auth: {}", tool) }
        }
        "event.char_count" => {
            let count = args.first().copied().unwrap_or("0");
            if zh { format!("{}字", count) }
            else { format!("{} chars", count) }
        }
        _ => {
            // fallback: key 非 'static，不能调 t()；直接返回 key
            key.to_string()
        }




    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_is_english() {
        // OnceLock 未初始化时默认 En
        assert_eq!(lang(), Lang::En);
    }

    #[test]
    fn test_t_returns_key_for_unknown() {
        assert_eq!(t("nonexistent.key"), "nonexistent.key");
    }

    #[test]
    fn test_t_basic_keys() {
        // 默认 En（OnceLock 未 set 时）
        assert_eq!(t("mode.clarify"), "Clarify");
        assert_eq!(t("confirm.allow"), "Allow");
    }
}
