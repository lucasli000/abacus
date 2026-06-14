//! Abacus TUI Theme — 设计规范品牌黑 #0F172A 系列 + 10 套终端主题
//!
//! 设计规范来源: ABACUS-TUI-DESIGN-SPEC.md v1.0

use ratatui::style::{Color, Modifier, Style};

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// 设计规范主色常量 (HEX → RGB)
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// 品牌黑 — 设计规范核心色
pub mod brand {
    use super::Color;

    /// 深色模式前景 #0F172A — 边框、按钮、图标、选中态
    pub const FG_DARK: Color = Color::Rgb(15, 23, 42);
    /// 深色模式背景 #1E293B — 按钮背景、高亮块
    pub const BG_DARK: Color = Color::Rgb(30, 41, 59);
    /// active 态 #334155
    pub const ACTIVE: Color = Color::Rgb(51, 65, 85);
}

/// 模式专属色 — 仅用于模式标识，不可混用
/// 色盲友好设计：蓝/橙/红 三色红绿色盲可清晰区分
pub mod mode_color {
    use super::Color;

    /// Chat 模式 #2563EB 蓝 — Chat标题、用户消息标识、Chat按钮
    pub const CHAT: Color = Color::Rgb(37, 99, 235);
    /// Team 模式 #EA580C 橙 — Team标题、角色标识、Team按钮
    pub const TEAM: Color = Color::Rgb(234, 88, 12);
    /// Meeting 模式 #DC2626 红 — Meeting标题、专家标识、Meeting按钮
    pub const MEETING: Color = Color::Rgb(220, 38, 38);
}

/// Z轴层级常量
pub mod z_index {
    pub const GLOBAL_BG: u8 = 0;
    pub const CARD_SHADOW: u8 = 1;
    pub const CARD_BG: u8 = 2;
    pub const CARD_BORDER: u8 = 3;
    pub const CARD_CONTENT: u8 = 4;
    pub const STATE_HIGHLIGHT: u8 = 5;
    pub const FLOATING: u8 = 6;
    pub const MODAL: u8 = 7;
    pub const OVERLAY: u8 = 8;
    pub const CURSOR: u8 = 9;
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// Theme struct — 13 色终端调色板
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

#[derive(Clone)]
pub struct Theme {
    /// 主题名（与 from_name 接受的字符串严格对应）
    /// 引用关系：settings 弹窗主题切换、cycle_next、Theme::all_names
    /// 生命周期：构造时绑定 static 字符串，主题切换时整体替换
    pub name: &'static str,
    pub primary: Color,
    pub accent: Color,
    pub text: Color,
    pub muted: Color,
    pub success: Color,
    pub error: Color,
    pub user: Color,
    pub session: Color,
    pub expert: Color,
    /// V42-B 新增：Abacus 角色专属色（卡片化重构）
    /// 用于 AbacusCard 边框 + header
    /// 视觉契约：紫色系（与 user 蓝 / session 绿 / gold 金 / accent 各自独立, 不混色）
    /// 引用关系：abacus-cli/src/tui/cards/abacus.rs 渲染 AbacusCard 时使用
    pub abacus: Color,
    pub border: Color,
    pub gold: Color,
    pub surface: Color,
    pub bg: Color,
    /// 模式专属色（运行时覆盖）
    pub mode: Color,
    /// 提升面（surface 上一级）— 弹窗/选中/悬浮内容
    /// 引用关系：被 ConfirmDialog/SettingsModal/Toast 渲染填底
    /// 生命周期：随主题切换更新，渲染期只读
    pub elevated: Color,
    /// 三种模式的标识色（替代静态 mod_color，跟随主题）
    pub mode_chat: Color,
    pub mode_team: Color,
    pub mode_meeting: Color,
    /// 模型品牌色（仅染 TopBar 模型 chip，不污染全局 accent）
    pub model_chip: Color,
    // ── V42-B+ 语义色：Markdown ──
    pub markdown_heading: Color,
    pub markdown_link: Color,
    pub markdown_code: Color,
    pub markdown_blockquote: Color,
    pub markdown_emph: Color,
    pub markdown_strong: Color,
    pub markdown_hr: Color,
    pub markdown_list: Color,
    // ── V42-B+ 语义色：Syntax ──
    pub syntax_comment: Color,
    pub syntax_keyword: Color,
    pub syntax_function: Color,
    pub syntax_variable: Color,
    pub syntax_string: Color,
    pub syntax_number: Color,
    pub syntax_type: Color,
    pub syntax_operator: Color,
    // ── V42-B+ 语义色：Diff ──
    pub diff_added: Color,
    pub diff_removed: Color,
    pub diff_context: Color,
    pub diff_added_bg: Color,
    pub diff_removed_bg: Color,
    pub diff_context_bg: Color,
    // ── V42-B+ 语义色：Thinking ──
    pub thinking_bg: Color,
    pub thinking_border: Color,
}

impl Theme {
    /// 从 13 个基础色派生出所有语义色（markdown / syntax / diff / thinking）
    ///
    /// 每个主题构造函数末尾调用 `.with_semantic_colors()`，新主题自动继承。
    /// 特定主题可覆盖个别语义色后再调 `with_semantic_colors()`。
    pub fn with_semantic_colors(mut self) -> Self {
        // Markdown（参考 OpenTUI smoke-theme.json）
        self.markdown_heading = self.accent;
        self.markdown_link = self.accent;
        self.markdown_code = self.gold;
        self.markdown_blockquote = self.muted;
        self.markdown_emph = self.error;
        self.markdown_strong = self.gold;
        self.markdown_hr = self.muted;
        self.markdown_list = self.accent;
        // Syntax
        self.syntax_comment = self.muted;
        self.syntax_keyword = self.primary;
        self.syntax_function = self.accent;
        self.syntax_variable = self.user;
        self.syntax_string = self.success;
        self.syntax_number = self.gold;
        self.syntax_type = self.primary;
        self.syntax_operator = self.accent;
        // Diff
        self.diff_added = self.success;
        self.diff_removed = self.error;
        self.diff_context = self.muted;
        self.diff_added_bg = blend(self.success, self.bg, 0.15);
        self.diff_removed_bg = blend(self.error, self.bg, 0.15);
        self.diff_context_bg = self.surface;
        // Thinking
        self.thinking_bg = self.elevated;
        self.thinking_border = self.accent;
        self
    }
}

impl Theme {
    pub fn init() -> Self {
        let name = std::env::var("ABACUS_THEME").unwrap_or_else(|_| "light".into());
        let mut theme = from_name(&name);
        match ColorCapability::detect() {
            ColorCapability::TrueColor => {},
            ColorCapability::Ansi256 => {
                theme.strip_to_ansi256();
            }
            ColorCapability::Ansi16 | ColorCapability::NoColor => {
                theme.strip_all_color();
            }
        }
        theme
    }

    fn strip_to_ansi256(&mut self) {
        let map = |c: &Color| -> Color {
            if let Color::Rgb(r, g, b) = *c {
                Color::Indexed(ansi256_fallback(r, g, b))
            } else { *c }
        };
        self.primary = map(&self.primary);
        self.accent = map(&self.accent);
        self.text = map(&self.text);
        self.muted = map(&self.muted);
        self.success = map(&self.success);
        self.error = map(&self.error);
        self.user = map(&self.user);
        self.session = map(&self.session);
        self.expert = map(&self.expert);
        self.border = map(&self.border);
        self.gold = map(&self.gold);
        self.surface = map(&self.surface);
        self.bg = map(&self.bg);
        self.mode = map(&self.mode);
        self.elevated = map(&self.elevated);
        self.mode_chat = map(&self.mode_chat);
        self.mode_team = map(&self.mode_team);
        self.mode_meeting = map(&self.mode_meeting);
        self.model_chip = map(&self.model_chip);
        // V42-B+ 语义色
        self.markdown_heading = map(&self.markdown_heading);
        self.markdown_link = map(&self.markdown_link);
        self.markdown_code = map(&self.markdown_code);
        self.markdown_blockquote = map(&self.markdown_blockquote);
        self.markdown_emph = map(&self.markdown_emph);
        self.markdown_strong = map(&self.markdown_strong);
        self.markdown_hr = map(&self.markdown_hr);
        self.markdown_list = map(&self.markdown_list);
        self.syntax_comment = map(&self.syntax_comment);
        self.syntax_keyword = map(&self.syntax_keyword);
        self.syntax_function = map(&self.syntax_function);
        self.syntax_variable = map(&self.syntax_variable);
        self.syntax_string = map(&self.syntax_string);
        self.syntax_number = map(&self.syntax_number);
        self.syntax_type = map(&self.syntax_type);
        self.syntax_operator = map(&self.syntax_operator);
        self.diff_added = map(&self.diff_added);
        self.diff_removed = map(&self.diff_removed);
        self.diff_context = map(&self.diff_context);
        self.diff_added_bg = map(&self.diff_added_bg);
        self.diff_removed_bg = map(&self.diff_removed_bg);
        self.diff_context_bg = map(&self.diff_context_bg);
        self.thinking_bg = map(&self.thinking_bg);
        self.thinking_border = map(&self.thinking_border);
    }

    fn strip_all_color(&mut self) {
        // V6 完善：16 色终端下保留语义色映射到 ANSI16，
        // 仅结构色（bg/surface/elevated/border）Reset 以继承终端默认
        // V9 完善：text 也 Reset，避免在浅底终端下白字白底不可见（让终端默认前景接管）
        self.primary = Color::Blue;
        self.accent = Color::Cyan;
        self.text = Color::Reset;
        self.muted = Color::DarkGray;
        self.success = Color::Green;
        self.error = Color::Red;
        self.user = Color::Blue;
        self.session = Color::Magenta;
        self.expert = Color::Cyan;
        self.gold = Color::Yellow;
        self.border = Color::Reset;
        self.surface = Color::Reset;
        self.bg = Color::Reset;
        self.mode = Color::Blue;
        self.elevated = Color::Reset;
        self.mode_chat = Color::Blue;
        self.mode_team = Color::Yellow;
        self.mode_meeting = Color::Red;
        self.model_chip = Color::Cyan;
        // V42-B+ 语义色（16 色终端降级）
        self.markdown_heading = Color::Cyan;
        self.markdown_link = Color::Cyan;
        self.markdown_code = Color::Yellow;
        self.markdown_blockquote = Color::DarkGray;
        self.markdown_emph = Color::Red;
        self.markdown_strong = Color::Yellow;
        self.markdown_hr = Color::DarkGray;
        self.markdown_list = Color::Cyan;
        self.syntax_comment = Color::DarkGray;
        self.syntax_keyword = Color::Blue;
        self.syntax_function = Color::Cyan;
        self.syntax_variable = Color::Blue;
        self.syntax_string = Color::Green;
        self.syntax_number = Color::Yellow;
        self.syntax_type = Color::Blue;
        self.syntax_operator = Color::Cyan;
        self.diff_added = Color::Green;
        self.diff_removed = Color::Red;
        self.diff_context = Color::DarkGray;
        self.diff_added_bg = Color::Reset;
        self.diff_removed_bg = Color::Reset;
        self.diff_context_bg = Color::Reset;
        self.thinking_bg = Color::Reset;
        self.thinking_border = Color::Cyan;
    }

    /// 按当前模式更新 mode 专属色
    pub fn set_mode_color(&mut self, mode: &str) {
        // 跟随当前主题的 mode_* 字段（V3 完善）
        // V31: "clarify" 是 "chat" 的重命名，共用 mode_chat 色值
        self.mode = match mode {
            "clarify" | "chat" => self.mode_chat,
            "team" => self.mode_team,
            "meeting" => self.mode_meeting,
            _ => self.mode_chat,
        };
    }

    /// V4 完善：仅染 model_chip，保留主题主色稳定（不再覆盖 accent/primary/user/mode）
    /// 模型品牌色仅出现于 TopBar 模型 chip；整体主题不被“污染”
    pub fn apply_model_brand(&mut self, model_name: &str) {
        let lower = model_name.to_lowercase();
        let chip_rgb = if lower.starts_with("deepseek") {
            (170, 180, 220)   // 冷灰蓝
        } else if lower.starts_with("gpt") || lower.starts_with("o1") || lower.starts_with("o3") {
            (170, 210, 185)   // 冷灰绿
        } else if lower.starts_with("claude") || lower.starts_with("sonnet")
            || lower.starts_with("opus") || lower.starts_with("haiku")
        {
            (215, 185, 170)   // 暖灰橙
        } else if lower.starts_with("qwen") {
            (215, 185, 160)   // 暖灰金
        } else if lower.starts_with("gemini") {
            (175, 190, 220)   // 冷灰蓝
        } else if lower.starts_with("llama") {
            (175, 185, 220)   // 紫灰蓝
        } else if lower.starts_with("mistral") {
            (215, 185, 175)   // 暖灰橙
        } else {
            return;
        };
        // V4：仅染 chip 通道；主题 accent/primary/user/mode 不被污染
        self.model_chip = Color::Rgb(chip_rgb.0, chip_rgb.1, chip_rgb.2);
    }

    /// 切换主题（返回 true 表示成功；未知主题名返回 false 不修改）
    pub fn switch_theme(&mut self, name: &str) -> bool {
        if !Self::all_names().contains(&name) {
            return false;
        }
        *self = from_name(name);
        true
    }

    /// 全部可用主题名（cycle_next + settings 校验共用单一真相）
    /// 引用关系：switch_theme / cycle_next / settings 弹窗
    /// 生命周期：&'static — 进程级常量
    pub fn all_names() -> &'static [&'static str] {
        &[
            "brand", "light", "apple", "google",
            "monokai", "dracula", "nord", "gruvbox",
            "catppuccin", "tokyo-night", "solarized-dark", "one-dark",
        ]
    }

    /// 循环切换到下一个主题（settings Enter / `/theme cycle` 共用）
    /// 当前主题不在列表则回退到首个；否则环形 +1
    pub fn cycle_next(current: &str) -> &'static str {
        let names = Self::all_names();
        let idx = names.iter().position(|n| *n == current).unwrap_or(0);
        names[(idx + 1) % names.len()]
    }
}

pub fn from_name(name: &str) -> Theme {
    match name {
        "brand" => Theme::brand(),
        "light" => Theme::light(),
        "monokai" => Theme::monokai(),
        "dracula" => Theme::dracula(),
        "nord" => Theme::nord(),
        "gruvbox" => Theme::gruvbox(),
        "catppuccin" => Theme::catppuccin_mocha(),
        "tokyo-night" => Theme::tokyo_night(),
        "solarized-dark" => Theme::solarized_dark(),
        "one-dark" => Theme::one_dark(),
        "apple" => Theme::apple(),
        "google" => Theme::google(),
        // 未知主题名 fallback 到默认 light（白色系），与 Theme::init 默认值保持一致
        _ => Theme::light(),
    }
}

impl Theme {
    /// 品牌主题 — 黑灰白极简（高对比）
    /// 纯黑底 + 灰白文字层，accent 用冷灰蓝仅作提示不跳色
    pub fn brand() -> Self {
        Self {
            name: "brand",
            primary: Color::Rgb(200, 200, 210),     // 聚焦边框 — 亮灰
            accent: Color::Rgb(180, 190, 210),       // 提示色 — 冷灰
            text: Color::Rgb(240, 240, 245),         // 主文字 — 近白
            muted: Color::Rgb(130, 130, 140),        // 辅助文字 — 中灰
            success: Color::Rgb(140, 200, 170),      // 成功 — 青绿（V5 色相分离）
            error: Color::Rgb(230, 130, 140),        // 错误 — 珊瑚红（V5 色相分离）
            user: Color::Rgb(140, 180, 220),         // 用户标识 — 冷灰蓝（区别于 session 暖橙）
            session: Color::Rgb(220, 195, 165),      // 会话标识 — 暖灰金（与 user 冷蓝形成强对比）
            expert: Color::Rgb(190, 210, 200),       // 专家标识 — 灰绿
            abacus: Color::Rgb(168, 85, 247),
            border: Color::Rgb(70, 70, 80),          // 边框 — 深灰
            gold: Color::Rgb(220, 185, 130),         // 金色 — 暖金（V5 增暖度）
            surface: Color::Rgb(30, 30, 35),         // 卡片面 — 深灰（比 bg 亮一级）
            bg: Color::Rgb(18, 18, 22),              // 背景 — 近黑
            mode: mode_color::CHAT,
            elevated: Color::Rgb(45, 45, 50),
            mode_chat: Color::Rgb(140, 165, 215),
            mode_team: Color::Rgb(215, 175, 130),
            mode_meeting: Color::Rgb(215, 130, 145),
            model_chip: Color::Rgb(180, 190, 210),
            markdown_heading: Color::Rgb(0, 0, 0),
            markdown_link: Color::Rgb(0, 0, 0),
            markdown_code: Color::Rgb(0, 0, 0),
            markdown_blockquote: Color::Rgb(0, 0, 0),
            markdown_emph: Color::Rgb(0, 0, 0),
            markdown_strong: Color::Rgb(0, 0, 0),
            markdown_hr: Color::Rgb(0, 0, 0),
            markdown_list: Color::Rgb(0, 0, 0),
            syntax_comment: Color::Rgb(0, 0, 0),
            syntax_keyword: Color::Rgb(0, 0, 0),
            syntax_function: Color::Rgb(0, 0, 0),
            syntax_variable: Color::Rgb(0, 0, 0),
            syntax_string: Color::Rgb(0, 0, 0),
            syntax_number: Color::Rgb(0, 0, 0),
            syntax_type: Color::Rgb(0, 0, 0),
            syntax_operator: Color::Rgb(0, 0, 0),
            diff_added: Color::Rgb(0, 0, 0),
            diff_removed: Color::Rgb(0, 0, 0),
            diff_context: Color::Rgb(0, 0, 0),
            diff_added_bg: Color::Rgb(0, 0, 0),
            diff_removed_bg: Color::Rgb(0, 0, 0),
            diff_context_bg: Color::Rgb(0, 0, 0),
            thinking_bg: Color::Rgb(0, 0, 0),
            thinking_border: Color::Rgb(0, 0, 0),
        }.with_semantic_colors()
    }

    fn catppuccin_mocha() -> Self {
        Self {
            name: "catppuccin",
            primary: Color::Rgb(124, 92, 252),
            accent: Color::Rgb(137, 180, 250),
            text: Color::Rgb(205, 214, 244),
            muted: Color::Rgb(108, 112, 134),
            success: Color::Rgb(166, 227, 161),
            error: Color::Rgb(243, 139, 168),
            user: Color::Rgb(137, 220, 235),
            session: Color::Rgb(137, 180, 250),
            expert: Color::Rgb(166, 227, 161),
            abacus: Color::Rgb(203, 166, 247),
            border: Color::Rgb(69, 71, 90),
            gold: Color::Rgb(249, 226, 175),
            surface: Color::Rgb(49, 50, 68),
            bg: Color::Rgb(30, 30, 46),
            mode: mode_color::CHAT,
            elevated: Color::Rgb(69, 71, 90),
            mode_chat: Color::Rgb(137, 180, 250),
            mode_team: Color::Rgb(250, 179, 135),
            mode_meeting: Color::Rgb(243, 139, 168),
            model_chip: Color::Rgb(203, 166, 247),
            markdown_heading: Color::Rgb(0, 0, 0),
            markdown_link: Color::Rgb(0, 0, 0),
            markdown_code: Color::Rgb(0, 0, 0),
            markdown_blockquote: Color::Rgb(0, 0, 0),
            markdown_emph: Color::Rgb(0, 0, 0),
            markdown_strong: Color::Rgb(0, 0, 0),
            markdown_hr: Color::Rgb(0, 0, 0),
            markdown_list: Color::Rgb(0, 0, 0),
            syntax_comment: Color::Rgb(0, 0, 0),
            syntax_keyword: Color::Rgb(0, 0, 0),
            syntax_function: Color::Rgb(0, 0, 0),
            syntax_variable: Color::Rgb(0, 0, 0),
            syntax_string: Color::Rgb(0, 0, 0),
            syntax_number: Color::Rgb(0, 0, 0),
            syntax_type: Color::Rgb(0, 0, 0),
            syntax_operator: Color::Rgb(0, 0, 0),
            diff_added: Color::Rgb(0, 0, 0),
            diff_removed: Color::Rgb(0, 0, 0),
            diff_context: Color::Rgb(0, 0, 0),
            diff_added_bg: Color::Rgb(0, 0, 0),
            diff_removed_bg: Color::Rgb(0, 0, 0),
            diff_context_bg: Color::Rgb(0, 0, 0),
            thinking_bg: Color::Rgb(0, 0, 0),
            thinking_border: Color::Rgb(0, 0, 0),
        }.with_semantic_colors()
    }

    fn light() -> Self {
        // 默认主题（白色系）—— 参考 Apple Light Mode 系统色
        // 设计意图：① 真白底 #FAFAFA 微暖，比纯白柔和不灼眼
        //          ② 文字 #1D1D1F 微暖深，避免纯黑刺眼
        //          ③ 所有强调色（primary/accent/error/success）已加深至 ≥4:1
        //             保证白底下文本可读（WCAG AA）
        // 注：NoColor 终端时 strip_all_color 会把 bg 重置为 Reset，兼容性保留
        Self {
            name: "light",
            primary: Color::Rgb(0x00, 0x66, 0xCC),     // Light Blue（深蓝，按钮/边框聚焦可读）
            accent: Color::Rgb(0x00, 0x7A, 0xFF),       // Apple systemBlue Light Mode
            text: Color::Rgb(0x1D, 0x1D, 0x1F),         // 微暖深（比纯黑减刺）
            muted: Color::Rgb(0x6E, 0x6E, 0x73),        // Apple Gray Light（次要文本，对白底 ≈4.97:1）
            success: Color::Rgb(0x24, 0x8A, 0x3D),      // Apple Light systemGreen 强调版
            error: Color::Rgb(0xC7, 0x25, 0x2C),        // Light Red 加深（白底可读）
            user: Color::Rgb(0x1E, 0x40, 0xAF),         // 深蓝（用户气泡条）
            session: Color::Rgb(0x9D, 0x17, 0x4D),      // 深玫红（与 user 蓝形成强色相对比，避免同色调融合）
            expert: Color::Rgb(0x24, 0x8A, 0x3D),       // 与 success 同色系（专家=可信）
            abacus: Color::Rgb(124, 58, 237),
            border: Color::Rgb(0xD2, 0xD2, 0xD7),       // Apple Separator Light
            gold: Color::Rgb(0xBB, 0x7B, 0x00),         // Light Amber 加深
            surface: Color::Rgb(0xFF, 0xFF, 0xFF),       // 输入框纯白
            bg: Color::Rgb(0xFA, 0xFA, 0xFA),           // 真白底（微暖）—— 用户期望的"白色系"
            mode: mode_color::CHAT,
            elevated: Color::Rgb(0xF5, 0xF5, 0xF7),     // Apple Light System Gray（卡片底）
            mode_chat: Color::Rgb(0x00, 0x7A, 0xFF),
            mode_team: Color::Rgb(0xC2, 0x41, 0x0C),    // Light Orange 加深
            mode_meeting: Color::Rgb(0xC7, 0x25, 0x2C),
            model_chip: Color::Rgb(0x00, 0x7A, 0xFF),
            markdown_heading: Color::Rgb(0, 0, 0),
            markdown_link: Color::Rgb(0, 0, 0),
            markdown_code: Color::Rgb(0, 0, 0),
            markdown_blockquote: Color::Rgb(0, 0, 0),
            markdown_emph: Color::Rgb(0, 0, 0),
            markdown_strong: Color::Rgb(0, 0, 0),
            markdown_hr: Color::Rgb(0, 0, 0),
            markdown_list: Color::Rgb(0, 0, 0),
            syntax_comment: Color::Rgb(0, 0, 0),
            syntax_keyword: Color::Rgb(0, 0, 0),
            syntax_function: Color::Rgb(0, 0, 0),
            syntax_variable: Color::Rgb(0, 0, 0),
            syntax_string: Color::Rgb(0, 0, 0),
            syntax_number: Color::Rgb(0, 0, 0),
            syntax_type: Color::Rgb(0, 0, 0),
            syntax_operator: Color::Rgb(0, 0, 0),
            diff_added: Color::Rgb(0, 0, 0),
            diff_removed: Color::Rgb(0, 0, 0),
            diff_context: Color::Rgb(0, 0, 0),
            diff_added_bg: Color::Rgb(0, 0, 0),
            diff_removed_bg: Color::Rgb(0, 0, 0),
            diff_context_bg: Color::Rgb(0, 0, 0),
            thinking_bg: Color::Rgb(0, 0, 0),
            thinking_border: Color::Rgb(0, 0, 0),
        }.with_semantic_colors()
    }

    fn monokai() -> Self {
        Self {
            name: "monokai",
            primary: Color::Rgb(174, 129, 255),
            accent: Color::Rgb(102, 217, 239),
            text: Color::Rgb(248, 248, 242),
            muted: Color::Rgb(117, 113, 94),
            success: Color::Rgb(166, 226, 46),
            // TH1：原 (249,38,114) 对 bg 仅 3.93——提亮少量保证 ≥ 4.0
            error: Color::Rgb(255, 95, 145),
            user: Color::Rgb(102, 217, 239),
            session: Color::Rgb(174, 129, 255),
            expert: Color::Rgb(166, 226, 46),
            abacus: Color::Rgb(174, 129, 255),
            border: Color::Rgb(62, 61, 50),
            gold: Color::Rgb(230, 219, 116),
            surface: Color::Rgb(52, 52, 44),
            bg: Color::Rgb(39, 40, 34),
            mode: mode_color::CHAT,
            elevated: Color::Rgb(70, 70, 60),
            mode_chat: Color::Rgb(102, 217, 239),
            mode_team: Color::Rgb(253, 151, 31),
            mode_meeting: Color::Rgb(249, 38, 114),
            model_chip: Color::Rgb(174, 129, 255),
            markdown_heading: Color::Rgb(0, 0, 0),
            markdown_link: Color::Rgb(0, 0, 0),
            markdown_code: Color::Rgb(0, 0, 0),
            markdown_blockquote: Color::Rgb(0, 0, 0),
            markdown_emph: Color::Rgb(0, 0, 0),
            markdown_strong: Color::Rgb(0, 0, 0),
            markdown_hr: Color::Rgb(0, 0, 0),
            markdown_list: Color::Rgb(0, 0, 0),
            syntax_comment: Color::Rgb(0, 0, 0),
            syntax_keyword: Color::Rgb(0, 0, 0),
            syntax_function: Color::Rgb(0, 0, 0),
            syntax_variable: Color::Rgb(0, 0, 0),
            syntax_string: Color::Rgb(0, 0, 0),
            syntax_number: Color::Rgb(0, 0, 0),
            syntax_type: Color::Rgb(0, 0, 0),
            syntax_operator: Color::Rgb(0, 0, 0),
            diff_added: Color::Rgb(0, 0, 0),
            diff_removed: Color::Rgb(0, 0, 0),
            diff_context: Color::Rgb(0, 0, 0),
            diff_added_bg: Color::Rgb(0, 0, 0),
            diff_removed_bg: Color::Rgb(0, 0, 0),
            diff_context_bg: Color::Rgb(0, 0, 0),
            thinking_bg: Color::Rgb(0, 0, 0),
            thinking_border: Color::Rgb(0, 0, 0),
        }.with_semantic_colors()
    }

    fn dracula() -> Self {
        Self {
            name: "dracula",
            primary: Color::Rgb(189, 147, 249),
            accent: Color::Rgb(139, 233, 253),
            text: Color::Rgb(248, 248, 242),
            muted: Color::Rgb(98, 114, 164),
            success: Color::Rgb(80, 250, 123),
            error: Color::Rgb(255, 85, 85),
            user: Color::Rgb(139, 233, 253),
            session: Color::Rgb(189, 147, 249),
            expert: Color::Rgb(80, 250, 123),
            abacus: Color::Rgb(189, 147, 249),
            border: Color::Rgb(68, 71, 90),
            gold: Color::Rgb(241, 250, 140),
            surface: Color::Rgb(55, 56, 77),
            bg: Color::Rgb(40, 42, 54),
            mode: mode_color::CHAT,
            elevated: Color::Rgb(68, 71, 90),
            mode_chat: Color::Rgb(139, 233, 253),
            mode_team: Color::Rgb(255, 184, 108),
            mode_meeting: Color::Rgb(255, 85, 85),
            model_chip: Color::Rgb(189, 147, 249),
            markdown_heading: Color::Rgb(0, 0, 0),
            markdown_link: Color::Rgb(0, 0, 0),
            markdown_code: Color::Rgb(0, 0, 0),
            markdown_blockquote: Color::Rgb(0, 0, 0),
            markdown_emph: Color::Rgb(0, 0, 0),
            markdown_strong: Color::Rgb(0, 0, 0),
            markdown_hr: Color::Rgb(0, 0, 0),
            markdown_list: Color::Rgb(0, 0, 0),
            syntax_comment: Color::Rgb(0, 0, 0),
            syntax_keyword: Color::Rgb(0, 0, 0),
            syntax_function: Color::Rgb(0, 0, 0),
            syntax_variable: Color::Rgb(0, 0, 0),
            syntax_string: Color::Rgb(0, 0, 0),
            syntax_number: Color::Rgb(0, 0, 0),
            syntax_type: Color::Rgb(0, 0, 0),
            syntax_operator: Color::Rgb(0, 0, 0),
            diff_added: Color::Rgb(0, 0, 0),
            diff_removed: Color::Rgb(0, 0, 0),
            diff_context: Color::Rgb(0, 0, 0),
            diff_added_bg: Color::Rgb(0, 0, 0),
            diff_removed_bg: Color::Rgb(0, 0, 0),
            diff_context_bg: Color::Rgb(0, 0, 0),
            thinking_bg: Color::Rgb(0, 0, 0),
            thinking_border: Color::Rgb(0, 0, 0),
        }.with_semantic_colors()
    }

    fn nord() -> Self {
        Self {
            name: "nord",
            primary: Color::Rgb(136, 192, 208),
            accent: Color::Rgb(129, 161, 193),
            text: Color::Rgb(216, 222, 233),
            muted: Color::Rgb(110, 122, 142),  // 提亮 nord3 → 对 bg 满足 WCAG 次要文本(2.5+)，仍保留 Polar Night 冷灰调
            success: Color::Rgb(163, 190, 140),
            // TH1：原 Aurora Red (191,97,106) 对 bg 仅 3.05——提亮达 AA 读设阈值
            error: Color::Rgb(225, 135, 140),
            user: Color::Rgb(136, 192, 208),         // Frost Light（冰蓝）
            session: Color::Rgb(180, 142, 173),      // Aurora Purple（与 user 冰蓝形成色相对比，避免冷色融合）
            expert: Color::Rgb(163, 190, 140),
            abacus: Color::Rgb(180, 142, 173),
            border: Color::Rgb(59, 66, 82),
            gold: Color::Rgb(235, 203, 139),
            surface: Color::Rgb(59, 66, 82),
            bg: Color::Rgb(46, 52, 64),
            mode: mode_color::CHAT,
            elevated: Color::Rgb(67, 76, 94),
            mode_chat: Color::Rgb(129, 161, 193),
            mode_team: Color::Rgb(208, 135, 112),
            mode_meeting: Color::Rgb(191, 97, 106),
            model_chip: Color::Rgb(180, 142, 173),
            markdown_heading: Color::Rgb(0, 0, 0),
            markdown_link: Color::Rgb(0, 0, 0),
            markdown_code: Color::Rgb(0, 0, 0),
            markdown_blockquote: Color::Rgb(0, 0, 0),
            markdown_emph: Color::Rgb(0, 0, 0),
            markdown_strong: Color::Rgb(0, 0, 0),
            markdown_hr: Color::Rgb(0, 0, 0),
            markdown_list: Color::Rgb(0, 0, 0),
            syntax_comment: Color::Rgb(0, 0, 0),
            syntax_keyword: Color::Rgb(0, 0, 0),
            syntax_function: Color::Rgb(0, 0, 0),
            syntax_variable: Color::Rgb(0, 0, 0),
            syntax_string: Color::Rgb(0, 0, 0),
            syntax_number: Color::Rgb(0, 0, 0),
            syntax_type: Color::Rgb(0, 0, 0),
            syntax_operator: Color::Rgb(0, 0, 0),
            diff_added: Color::Rgb(0, 0, 0),
            diff_removed: Color::Rgb(0, 0, 0),
            diff_context: Color::Rgb(0, 0, 0),
            diff_added_bg: Color::Rgb(0, 0, 0),
            diff_removed_bg: Color::Rgb(0, 0, 0),
            diff_context_bg: Color::Rgb(0, 0, 0),
            thinking_bg: Color::Rgb(0, 0, 0),
            thinking_border: Color::Rgb(0, 0, 0),
        }.with_semantic_colors()
    }

    fn gruvbox() -> Self {
        Self {
            name: "gruvbox",
            primary: Color::Rgb(211, 134, 155),
            accent: Color::Rgb(131, 165, 152),
            text: Color::Rgb(235, 219, 178),
            muted: Color::Rgb(146, 131, 116),
            success: Color::Rgb(184, 187, 38),
            error: Color::Rgb(251, 73, 52),
            user: Color::Rgb(131, 165, 152),
            session: Color::Rgb(211, 134, 155),
            expert: Color::Rgb(184, 187, 38),
            abacus: Color::Rgb(177, 98, 134),
            border: Color::Rgb(60, 56, 54),
            gold: Color::Rgb(250, 189, 47),
            surface: Color::Rgb(60, 56, 54),
            bg: Color::Rgb(40, 40, 40),
            mode: mode_color::CHAT,
            elevated: Color::Rgb(80, 73, 69),
            mode_chat: Color::Rgb(131, 165, 152),
            mode_team: Color::Rgb(254, 128, 25),
            mode_meeting: Color::Rgb(251, 73, 52),
            model_chip: Color::Rgb(211, 134, 155),
            markdown_heading: Color::Rgb(0, 0, 0),
            markdown_link: Color::Rgb(0, 0, 0),
            markdown_code: Color::Rgb(0, 0, 0),
            markdown_blockquote: Color::Rgb(0, 0, 0),
            markdown_emph: Color::Rgb(0, 0, 0),
            markdown_strong: Color::Rgb(0, 0, 0),
            markdown_hr: Color::Rgb(0, 0, 0),
            markdown_list: Color::Rgb(0, 0, 0),
            syntax_comment: Color::Rgb(0, 0, 0),
            syntax_keyword: Color::Rgb(0, 0, 0),
            syntax_function: Color::Rgb(0, 0, 0),
            syntax_variable: Color::Rgb(0, 0, 0),
            syntax_string: Color::Rgb(0, 0, 0),
            syntax_number: Color::Rgb(0, 0, 0),
            syntax_type: Color::Rgb(0, 0, 0),
            syntax_operator: Color::Rgb(0, 0, 0),
            diff_added: Color::Rgb(0, 0, 0),
            diff_removed: Color::Rgb(0, 0, 0),
            diff_context: Color::Rgb(0, 0, 0),
            diff_added_bg: Color::Rgb(0, 0, 0),
            diff_removed_bg: Color::Rgb(0, 0, 0),
            diff_context_bg: Color::Rgb(0, 0, 0),
            thinking_bg: Color::Rgb(0, 0, 0),
            thinking_border: Color::Rgb(0, 0, 0),
        }.with_semantic_colors()
    }

    fn tokyo_night() -> Self {
        Self {
            name: "tokyo-night",
            primary: Color::Rgb(120, 160, 245),   // 主色保留蓝（仅高亮用）
            accent: Color::Rgb(100, 200, 255),    // accent 亮蓝
            text: Color::Rgb(230, 230, 230),      // 主文字 — 近白
            muted: Color::Rgb(150, 150, 150),     // 辅助文字 — 中灰
            success: Color::Rgb(100, 210, 180),   // 青绿
            error: Color::Rgb(240, 100, 120),     // 红
            user: Color::Rgb(100, 200, 255),      // 用户消息标识
            session: Color::Rgb(180, 180, 200),   // 引擎消息 — 浅灰白
            expert: Color::Rgb(100, 210, 180),    // 专家标识
            abacus: Color::Rgb(187, 154, 247),
            border: Color::Rgb(80, 80, 80),       // 边框 — 中灰
            gold: Color::Rgb(220, 180, 100),      // 金色
            surface: Color::Rgb(40, 40, 40),      // 输入框背景 — 深灰
            bg: Color::Rgb(20, 20, 20),           // 主背景 — 纯黑
            mode: mode_color::CHAT,
            elevated: Color::Rgb(60, 60, 60),
            mode_chat: Color::Rgb(122, 162, 247),
            mode_team: Color::Rgb(255, 158, 100),
            mode_meeting: Color::Rgb(247, 118, 142),
            model_chip: Color::Rgb(187, 154, 247),
            markdown_heading: Color::Rgb(0, 0, 0),
            markdown_link: Color::Rgb(0, 0, 0),
            markdown_code: Color::Rgb(0, 0, 0),
            markdown_blockquote: Color::Rgb(0, 0, 0),
            markdown_emph: Color::Rgb(0, 0, 0),
            markdown_strong: Color::Rgb(0, 0, 0),
            markdown_hr: Color::Rgb(0, 0, 0),
            markdown_list: Color::Rgb(0, 0, 0),
            syntax_comment: Color::Rgb(0, 0, 0),
            syntax_keyword: Color::Rgb(0, 0, 0),
            syntax_function: Color::Rgb(0, 0, 0),
            syntax_variable: Color::Rgb(0, 0, 0),
            syntax_string: Color::Rgb(0, 0, 0),
            syntax_number: Color::Rgb(0, 0, 0),
            syntax_type: Color::Rgb(0, 0, 0),
            syntax_operator: Color::Rgb(0, 0, 0),
            diff_added: Color::Rgb(0, 0, 0),
            diff_removed: Color::Rgb(0, 0, 0),
            diff_context: Color::Rgb(0, 0, 0),
            diff_added_bg: Color::Rgb(0, 0, 0),
            diff_removed_bg: Color::Rgb(0, 0, 0),
            diff_context_bg: Color::Rgb(0, 0, 0),
            thinking_bg: Color::Rgb(0, 0, 0),
            thinking_border: Color::Rgb(0, 0, 0),
        }.with_semantic_colors()
    }

    fn solarized_dark() -> Self {
        Self {
            name: "solarized-dark",
            primary: Color::Rgb(108, 113, 196),
            accent: Color::Rgb(38, 139, 210),
            text: Color::Rgb(131, 148, 150),
            muted: Color::Rgb(88, 110, 117),
            success: Color::Rgb(133, 153, 0),
            // TH1：原 Solarized Red (220,50,47) 对 base03 仅 3.25——提亮达 AA
            error: Color::Rgb(238, 100, 92),
            user: Color::Rgb(42, 161, 152),
            session: Color::Rgb(38, 139, 210),
            expert: Color::Rgb(133, 153, 0),
            abacus: Color::Rgb(108, 113, 196),
            border: Color::Rgb(7, 54, 66),
            gold: Color::Rgb(181, 137, 0),
            surface: Color::Rgb(7, 54, 66),
            bg: Color::Rgb(0, 43, 54),
            mode: mode_color::CHAT,
            elevated: Color::Rgb(14, 60, 72),
            mode_chat: Color::Rgb(38, 139, 210),
            mode_team: Color::Rgb(203, 75, 22),
            mode_meeting: Color::Rgb(220, 50, 47),
            model_chip: Color::Rgb(108, 113, 196),
            markdown_heading: Color::Rgb(0, 0, 0),
            markdown_link: Color::Rgb(0, 0, 0),
            markdown_code: Color::Rgb(0, 0, 0),
            markdown_blockquote: Color::Rgb(0, 0, 0),
            markdown_emph: Color::Rgb(0, 0, 0),
            markdown_strong: Color::Rgb(0, 0, 0),
            markdown_hr: Color::Rgb(0, 0, 0),
            markdown_list: Color::Rgb(0, 0, 0),
            syntax_comment: Color::Rgb(0, 0, 0),
            syntax_keyword: Color::Rgb(0, 0, 0),
            syntax_function: Color::Rgb(0, 0, 0),
            syntax_variable: Color::Rgb(0, 0, 0),
            syntax_string: Color::Rgb(0, 0, 0),
            syntax_number: Color::Rgb(0, 0, 0),
            syntax_type: Color::Rgb(0, 0, 0),
            syntax_operator: Color::Rgb(0, 0, 0),
            diff_added: Color::Rgb(0, 0, 0),
            diff_removed: Color::Rgb(0, 0, 0),
            diff_context: Color::Rgb(0, 0, 0),
            diff_added_bg: Color::Rgb(0, 0, 0),
            diff_removed_bg: Color::Rgb(0, 0, 0),
            diff_context_bg: Color::Rgb(0, 0, 0),
            thinking_bg: Color::Rgb(0, 0, 0),
            thinking_border: Color::Rgb(0, 0, 0),
        }.with_semantic_colors()
    }

    fn one_dark() -> Self {
        Self {
            name: "one-dark",
            primary: Color::Rgb(198, 120, 221),
            accent: Color::Rgb(97, 175, 239),
            text: Color::Rgb(171, 178, 191),
            muted: Color::Rgb(92, 99, 112),
            success: Color::Rgb(152, 195, 121),
            error: Color::Rgb(224, 108, 117),
            user: Color::Rgb(86, 182, 194),
            session: Color::Rgb(97, 175, 239),
            expert: Color::Rgb(152, 195, 121),
            abacus: Color::Rgb(198, 120, 221),
            border: Color::Rgb(50, 55, 65),
            gold: Color::Rgb(229, 192, 123),
            surface: Color::Rgb(40, 44, 52),
            bg: Color::Rgb(33, 37, 43),
            mode: mode_color::CHAT,
            elevated: Color::Rgb(50, 54, 62),
            mode_chat: Color::Rgb(97, 175, 239),
            mode_team: Color::Rgb(209, 154, 102),
            mode_meeting: Color::Rgb(224, 108, 117),
            model_chip: Color::Rgb(198, 120, 221),
            markdown_heading: Color::Rgb(0, 0, 0),
            markdown_link: Color::Rgb(0, 0, 0),
            markdown_code: Color::Rgb(0, 0, 0),
            markdown_blockquote: Color::Rgb(0, 0, 0),
            markdown_emph: Color::Rgb(0, 0, 0),
            markdown_strong: Color::Rgb(0, 0, 0),
            markdown_hr: Color::Rgb(0, 0, 0),
            markdown_list: Color::Rgb(0, 0, 0),
            syntax_comment: Color::Rgb(0, 0, 0),
            syntax_keyword: Color::Rgb(0, 0, 0),
            syntax_function: Color::Rgb(0, 0, 0),
            syntax_variable: Color::Rgb(0, 0, 0),
            syntax_string: Color::Rgb(0, 0, 0),
            syntax_number: Color::Rgb(0, 0, 0),
            syntax_type: Color::Rgb(0, 0, 0),
            syntax_operator: Color::Rgb(0, 0, 0),
            diff_added: Color::Rgb(0, 0, 0),
            diff_removed: Color::Rgb(0, 0, 0),
            diff_context: Color::Rgb(0, 0, 0),
            diff_added_bg: Color::Rgb(0, 0, 0),
            diff_removed_bg: Color::Rgb(0, 0, 0),
            diff_context_bg: Color::Rgb(0, 0, 0),
            thinking_bg: Color::Rgb(0, 0, 0),
            thinking_border: Color::Rgb(0, 0, 0),
        }.with_semantic_colors()
    }

    /// Apple Dark 主题 — macOS Sonoma 系统色彩（systemBlue / systemPurple / systemGreen）
    /// 设计意图：纯净深灰背景 · 柔和高对比 · 系统蓝强调；2026 现代审美
    /// 色调舒适：bg=systemGray6 微暖、text=苹果官网白（非纯白）减少灮眼
    pub fn apple() -> Self {
        Self {
            name: "apple",
            primary: Color::Rgb(0x0A, 0x84, 0xFF),     // systemBlue
            accent: Color::Rgb(0x5E, 0x5C, 0xE6),      // systemIndigo
            text: Color::Rgb(0xF5, 0xF5, 0xF7),        // 苹果官网白（微暖）
            muted: Color::Rgb(0x8E, 0x8E, 0x93),       // systemGray
            success: Color::Rgb(0x30, 0xD1, 0x58),     // systemGreen
            error: Color::Rgb(0xFF, 0x45, 0x3A),       // systemRed
            user: Color::Rgb(0x64, 0xD2, 0xFF),        // systemCyan
            session: Color::Rgb(0xBF, 0x5A, 0xF2),     // systemPurple
            expert: Color::Rgb(0x30, 0xD1, 0x58),      // systemGreen
            abacus: Color::Rgb(175, 82, 222),
            border: Color::Rgb(0x48, 0x48, 0x4A),      // systemGray3
            gold: Color::Rgb(0xFF, 0xD6, 0x0A),        // systemYellow
            surface: Color::Rgb(0x2C, 0x2C, 0x2E),     // systemGray5
            bg: Color::Rgb(0x1C, 0x1C, 0x1E),          // systemGray6（非纯黑，微暖）
            mode: mode_color::CHAT,
            elevated: Color::Rgb(0x3A, 0x3A, 0x3C),    // systemGray4
            mode_chat: Color::Rgb(0x0A, 0x84, 0xFF),
            mode_team: Color::Rgb(0xFF, 0x9F, 0x0A),   // systemOrange
            mode_meeting: Color::Rgb(0xFF, 0x45, 0x3A),
            model_chip: Color::Rgb(0xBF, 0x5A, 0xF2),  // systemPurple
            // V42-B+ 语义色（由 with_semantic_colors() 覆盖）
            markdown_heading: Color::Rgb(0, 0, 0), markdown_link: Color::Rgb(0, 0, 0),
            markdown_code: Color::Rgb(0, 0, 0), markdown_blockquote: Color::Rgb(0, 0, 0),
            markdown_emph: Color::Rgb(0, 0, 0), markdown_strong: Color::Rgb(0, 0, 0),
            markdown_hr: Color::Rgb(0, 0, 0), markdown_list: Color::Rgb(0, 0, 0),
            syntax_comment: Color::Rgb(0, 0, 0), syntax_keyword: Color::Rgb(0, 0, 0),
            syntax_function: Color::Rgb(0, 0, 0), syntax_variable: Color::Rgb(0, 0, 0),
            syntax_string: Color::Rgb(0, 0, 0), syntax_number: Color::Rgb(0, 0, 0),
            syntax_type: Color::Rgb(0, 0, 0), syntax_operator: Color::Rgb(0, 0, 0),
            diff_added: Color::Rgb(0, 0, 0), diff_removed: Color::Rgb(0, 0, 0),
            diff_context: Color::Rgb(0, 0, 0), diff_added_bg: Color::Rgb(0, 0, 0),
            diff_removed_bg: Color::Rgb(0, 0, 0), diff_context_bg: Color::Rgb(0, 0, 0),
            thinking_bg: Color::Rgb(0, 0, 0), thinking_border: Color::Rgb(0, 0, 0),
        }.with_semantic_colors()
    }

    /// Google Material Design 3 主题 — 深蓝灰底 + Google 品牌四色
    /// 设计意图：现代科技 · 清晰 · 高对比但不灮眼
    /// 色调舒适：bg=深蓝灰（非纯黑）、text=gray200（冷中性）
    pub fn google() -> Self {
        Self {
            name: "google",
            primary: Color::Rgb(0x42, 0x85, 0xF4),     // Google Blue
            accent: Color::Rgb(0x5F, 0x82, 0xCB),      // 蓝灰副色
            text: Color::Rgb(0xE8, 0xEA, 0xED),        // gray 200
            muted: Color::Rgb(0x9A, 0xA0, 0xA6),       // gray 500
            success: Color::Rgb(0x34, 0xA8, 0x53),     // Google Green
            error: Color::Rgb(0xEA, 0x43, 0x35),       // Google Red
            user: Color::Rgb(0x8A, 0xB4, 0xF8),        // Blue 300
            session: Color::Rgb(0xC5, 0x8A, 0xF9),     // Purple
            expert: Color::Rgb(0x81, 0xC9, 0x95),      // Green 300
            abacus: Color::Rgb(156, 39, 176),
            border: Color::Rgb(0x3C, 0x40, 0x43),      // gray 700
            gold: Color::Rgb(0xFB, 0xBC, 0x04),        // Google Yellow
            surface: Color::Rgb(0x2D, 0x2D, 0x3D),
            bg: Color::Rgb(0x1F, 0x1F, 0x2E),
            mode: mode_color::CHAT,
            elevated: Color::Rgb(0x3C, 0x3C, 0x50),
            mode_chat: Color::Rgb(0x42, 0x85, 0xF4),
            mode_team: Color::Rgb(0xFB, 0xBC, 0x04),
            mode_meeting: Color::Rgb(0xEA, 0x43, 0x35),
            model_chip: Color::Rgb(0xC5, 0x8A, 0xF9),
            markdown_heading: Color::Rgb(0, 0, 0),
            markdown_link: Color::Rgb(0, 0, 0),
            markdown_code: Color::Rgb(0, 0, 0),
            markdown_blockquote: Color::Rgb(0, 0, 0),
            markdown_emph: Color::Rgb(0, 0, 0),
            markdown_strong: Color::Rgb(0, 0, 0),
            markdown_hr: Color::Rgb(0, 0, 0),
            markdown_list: Color::Rgb(0, 0, 0),
            syntax_comment: Color::Rgb(0, 0, 0),
            syntax_keyword: Color::Rgb(0, 0, 0),
            syntax_function: Color::Rgb(0, 0, 0),
            syntax_variable: Color::Rgb(0, 0, 0),
            syntax_string: Color::Rgb(0, 0, 0),
            syntax_number: Color::Rgb(0, 0, 0),
            syntax_type: Color::Rgb(0, 0, 0),
            syntax_operator: Color::Rgb(0, 0, 0),
            diff_added: Color::Rgb(0, 0, 0),
            diff_removed: Color::Rgb(0, 0, 0),
            diff_context: Color::Rgb(0, 0, 0),
            diff_added_bg: Color::Rgb(0, 0, 0),
            diff_removed_bg: Color::Rgb(0, 0, 0),
            diff_context_bg: Color::Rgb(0, 0, 0),
            thinking_bg: Color::Rgb(0, 0, 0),
            thinking_border: Color::Rgb(0, 0, 0),
        }.with_semantic_colors()
    }
}

/// 256 色 / 16 色降级辅助
pub fn ansi256_fallback(r: u8, g: u8, b: u8) -> u8 {
    let ri = (r as u32 * 5 / 255).min(5);
    let gi = (g as u32 * 5 / 255).min(5);
    let bi = (b as u32 * 5 / 255).min(5);
    (16 + 36 * ri + 6 * gi + bi) as u8
}

/// 颜色混合：`blend(fg, bg, alpha)` → fg 与 bg 按 alpha 混合（0.0=纯bg，1.0=纯fg）
pub fn blend(fg: Color, bg: Color, alpha: f64) -> Color {
    if let (Color::Rgb(fr, fg_c, fb), Color::Rgb(br, bg_c, bb)) = (fg, bg) {
        let r = (fr as f64 * alpha + br as f64 * (1.0 - alpha)) as u8;
        let g = (fg_c as f64 * alpha + bg_c as f64 * (1.0 - alpha)) as u8;
        let b = (fb as f64 * alpha + bb as f64 * (1.0 - alpha)) as u8;
        Color::Rgb(r, g, b)
    } else {
        fg // fallback: 非 RGB 色直接返回前景
    }
}

/// 终端能力检测 — 主题降级用
pub enum ColorCapability {
    TrueColor,
    Ansi256,
    Ansi16,
    NoColor,
}

impl ColorCapability {
    pub fn detect() -> Self {
        // TH3 修复：尊重 NO_COLOR 行业标准（https://no-color.org/）—
        // 视障/CI/高对比度终端用户设置 NO_COLOR=任何值表示请求禁用色彩。
        // 必须最先检查，优先于 COLORTERM/TERM 能力探测。
        if std::env::var_os("NO_COLOR").is_some() {
            return Self::NoColor;
        }
        // 优先检查 COLORTERM（最可靠的信号）
        if let Ok(term) = std::env::var("COLORTERM") {
            if term == "truecolor" || term == "24bit" {
                return Self::TrueColor;
            }
        }
        // 检查 TERM
        if let Ok(term) = std::env::var("TERM") {
            if term.contains("256color") {
                return Self::Ansi256;
            }
            // 现代终端默认支持 TrueColor
            if term.contains("xterm") || term.contains("screen") || term.contains("tmux") {
                return Self::TrueColor;
            }
        }
        // 检查 TERM_PROGRAM（macOS 终端.app / iTerm2 / WezTerm 等）
        if let Ok(program) = std::env::var("TERM_PROGRAM") {
            match program.as_str() {
                "iTerm.app" | "WezTerm" | "Alacritty" | "kitty" | "ghostty" | "Hyper" => {
                    return Self::TrueColor;
                }
                "Apple_Terminal" => {
                    return Self::Ansi256; // Terminal.app 支持 256 色
                }
                _ => {}
            }
        }
        // 检查 Windows Terminal
        if std::env::var_os("WT_SESSION").is_some() {
            return Self::TrueColor;
        }
        Self::Ansi16
    }
}

// ═════════════════════════════════════════════════════════════
// Tier 1 — Typography（文本层次系统）
// ═════════════════════════════════════════════════════════════
// 提供语义化的文本样式角色：渲染层不再直接挑 fg + Modifier，
// 而是通过 TextRole 表达“这是什么类型的文字”，由 Theme 统一映射。
// 切换主题时所有 role 样式同步更新，视觉一致性自动保证。
// 引用关系：被 components/markdown/modes 等渲染层调用
// 生命周期：纯派生自 Theme 当前字段值

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TextRole {
    /// 一级标题（消息角色名 / TopBar logo）
    H1,
    /// 二级标题（区块标题）
    H2,
    /// 三级标题
    H3,
    /// 正文
    Body,
    /// 强调正文（用户消息加粗）
    BodyEmphasis,
    /// 次要文字（时间戳、辅助说明）
    Caption,
    /// 提示文字（输入框 placeholder）
    Hint,
    /// 内联代码
    InlineCode,
    /// 代码块行
    CodeBlock,
    /// 链接
    Link,
    /// 引用块
    Quote,
    /// 状态指示（spinner / Ready）
    Status,
}

impl Theme {
    /// 按 TextRole 派生样式（Tier 1）
    /// 调用方：components/render_*、markdown/*、modes/*
    /// 设计意图：统一管理 fg + Modifier 组合，消除“挑色 + 加 BOLD”的散乱
    /// 按 TextRole 派生样式（视觉层级设计）
    ///
    /// 层级（从亮到暗）：
    ///   L1: 回复正文 Body — 最亮，用户核心关注
    ///   L2: 标题/代码 — 正常亮度 + 修饰符
    ///   L3: 工具/状态 — muted 色（变淡但可读）
    ///   L4: Trace/折叠摘要 — muted + DIM（最暗，背景信息）
    pub fn text_style(&self, role: TextRole) -> Style {
        match role {
            // L1: 核心内容（最高对比度）
            TextRole::Body         => Style::default().fg(self.text),
            TextRole::BodyEmphasis => Style::default().fg(self.text).add_modifier(Modifier::BOLD),
            // L2: 结构标题 + 代码
            TextRole::H1           => Style::default().fg(self.text).add_modifier(Modifier::BOLD),
            TextRole::H2           => Style::default().fg(self.accent).add_modifier(Modifier::BOLD),
            TextRole::H3           => Style::default().fg(self.text).add_modifier(Modifier::BOLD | Modifier::ITALIC),
            TextRole::CodeBlock    => Style::default().fg(self.text),
            TextRole::InlineCode   => Style::default().fg(self.gold),
            TextRole::Link         => Style::default().fg(self.accent).add_modifier(Modifier::UNDERLINED),
            // L3: 辅助信息（变淡但不加 DIM，保持可读性）
            TextRole::Status       => Style::default().fg(self.muted),
            TextRole::Quote        => Style::default().fg(self.muted).add_modifier(Modifier::ITALIC),
            TextRole::Hint         => Style::default().fg(self.muted).add_modifier(Modifier::ITALIC),
            // L4: 背景信息（最淡——muted + DIM）
            TextRole::Caption      => Style::default().fg(self.muted).add_modifier(Modifier::DIM),
        }
    }
}

// ═════════════════════════════════════════════════════════════
// Tier 2 — Semantic（状态色 + 强度三档）
// ═════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SemanticIntent {
    Info,
    Success,
    Warning,
    Danger,
    Active,
    Neutral,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Strength {
    Subtle,
    Default,
    Strong,
}

impl Theme {
    /// 按语义意图 + 强度派生样式（Tier 2）
    pub fn semantic_style(&self, intent: SemanticIntent, strength: Strength) -> Style {
        let fg = self.semantic_fg(intent);
        let modifier = match strength {
            Strength::Subtle  => Modifier::DIM,
            Strength::Default => Modifier::empty(),
            Strength::Strong  => Modifier::BOLD,
        };
        Style::default().fg(fg).add_modifier(modifier)
    }

    /// 语义意图的前景色
    pub fn semantic_fg(&self, intent: SemanticIntent) -> Color {
        match intent {
            SemanticIntent::Info     => self.accent,
            SemanticIntent::Success  => self.success,
            SemanticIntent::Warning  => self.gold,
            SemanticIntent::Danger   => self.error,
            SemanticIntent::Active   => self.primary,
            SemanticIntent::Neutral  => self.muted,
        }
    }

    /// 语义意图的背景色（toast/badge 背景填充用）
    pub fn semantic_bg(&self, intent: SemanticIntent) -> Color {
        // 与 fg 同色；调用方决定 fg/bg 映射（通常使用 auto_contrast_fg）
        self.semantic_fg(intent)
    }
}

// ═════════════════════════════════════════════════════════════
// Tier 5 — WCAG 对比度计算（色调舒适性静态校验）
// ═════════════════════════════════════════════════════════════

fn srgb_channel(c: u8) -> f64 {
    let c = c as f64 / 255.0;
    if c <= 0.03928 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
}

/// WCAG 2.1 相对亮度（sRGB → linear）
pub fn relative_luminance(c: Color) -> f64 {
    if let Color::Rgb(r, g, b) = c {
        0.2126 * srgb_channel(r) + 0.7152 * srgb_channel(g) + 0.0722 * srgb_channel(b)
    } else {
        // 非 Rgb 类型（Reset/Indexed/名称色）保守返回中性 0.5
        0.5
    }
}

/// WCAG 2.1 对比度（fg vs bg）：1.0 ~ 21.0；AA 要求 ≥ 4.5
pub fn wcag_contrast(fg: Color, bg: Color) -> f64 {
    let l1 = relative_luminance(fg);
    let l2 = relative_luminance(bg);
    let (light, dark) = if l1 > l2 { (l1, l2) } else { (l2, l1) };
    (light + 0.05) / (dark + 0.05)
}

#[cfg(test)]
mod theme_contrast_tests {
    use super::*;

    /// text/bg 需满足 WCAG AA ≥4.5
    fn assert_text_aa(theme: Theme, name: &str) {
        if let Color::Rgb(_, _, _) = theme.bg {
            let r = wcag_contrast(theme.text, theme.bg);
            assert!(r >= 4.5, "{}: text/bg contrast {:.2} < 4.5 (WCAG AA)", name, r);
        }
    }
    /// muted/bg 次级可读：≥2.5（宽容经典调色板）
    fn assert_muted_visible(theme: Theme, name: &str) {
        if let Color::Rgb(_, _, _) = theme.bg {
            let r = wcag_contrast(theme.muted, theme.bg);
            assert!(r >= 2.5, "{}: muted/bg contrast {:.2} < 2.5", name, r);
        }
    }
    /// primary/bg 焦点色可见：≥2.5
    fn assert_primary_visible(theme: Theme, name: &str) {
        if let Color::Rgb(_, _, _) = theme.bg {
            let r = wcag_contrast(theme.primary, theme.bg);
            assert!(r >= 2.5, "{}: primary/bg contrast {:.2} < 2.5", name, r);
        }
    }
    /// TH1：error/bg 关键警示色须达 WCAG AA ≥ 4.0（用户必须看清错误信息）
    fn assert_error_visible(theme: Theme, name: &str) {
        if let Color::Rgb(_, _, _) = theme.bg {
            let r = wcag_contrast(theme.error, theme.bg);
            assert!(r >= 4.0, "{}: error/bg contrast {:.2} < 4.0 (errors must be readable)", name, r);
        }
    }
    /// TH1：success/bg ≥ 3.0（次要语义色，可读即可）
    fn assert_success_visible(theme: Theme, name: &str) {
        if let Color::Rgb(_, _, _) = theme.bg {
            let r = wcag_contrast(theme.success, theme.bg);
            assert!(r >= 3.0, "{}: success/bg contrast {:.2} < 3.0", name, r);
        }
    }
    fn check_all(t: Theme, name: &str) {
        assert_text_aa(t.clone(), name);
        assert_muted_visible(t.clone(), name);
        assert_primary_visible(t.clone(), name);
        assert_error_visible(t.clone(), name);
        assert_success_visible(t, name);
    }

    #[test] fn brand()          { check_all(Theme::brand(), "brand"); }
    #[test] fn light()          { check_all(Theme::light(), "light"); }
    #[test] fn monokai()        { check_all(Theme::monokai(), "monokai"); }
    #[test] fn dracula()        { check_all(Theme::dracula(), "dracula"); }
    #[test] fn nord()           { check_all(Theme::nord(), "nord"); }
    #[test] fn gruvbox()        { check_all(Theme::gruvbox(), "gruvbox"); }
    #[test] fn catppuccin()     { check_all(Theme::catppuccin_mocha(), "catppuccin"); }
    #[test] fn tokyo_night()    { check_all(Theme::tokyo_night(), "tokyo-night"); }
    #[test] fn solarized_dark() { check_all(Theme::solarized_dark(), "solarized-dark"); }
    #[test] fn one_dark()       { check_all(Theme::one_dark(), "one-dark"); }
    #[test] fn apple()          { check_all(Theme::apple(), "apple"); }
    #[test] fn google()         { check_all(Theme::google(), "google"); }

    #[test]
    fn text_style_h1_uses_text_and_bold() {
        let t = Theme::apple();
        let s = t.text_style(TextRole::H1);
        assert_eq!(s.fg, Some(t.text));
        assert!(s.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn text_style_caption_dim() {
        let t = Theme::brand();
        let s = t.text_style(TextRole::Caption);
        assert_eq!(s.fg, Some(t.muted));
        assert!(s.add_modifier.contains(Modifier::DIM));
    }

    #[test]
    fn semantic_strong_carries_bold() {
        let t = Theme::apple();
        let s = t.semantic_style(SemanticIntent::Danger, Strength::Strong);
        assert_eq!(s.fg, Some(t.error));
        assert!(s.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn semantic_subtle_dim() {
        let t = Theme::google();
        let s = t.semantic_style(SemanticIntent::Success, Strength::Subtle);
        assert!(s.add_modifier.contains(Modifier::DIM));
    }

    /// TH3 回归：NO_COLOR=1 时 detect 应返回 NoColor，确保 init 走 strip_all_color 路径
    /// 注：测试操作进程级环境变量，可能与并发测试冲突；用 `serial_test`-style 顺序确保
    #[test]
    fn no_color_env_returns_no_color() {
        // 保存原值（如有）+ 设置后还原，避免污染其他测试
        let saved = std::env::var_os("NO_COLOR");
        std::env::set_var("NO_COLOR", "1");
        let cap = ColorCapability::detect();
        assert!(matches!(cap, ColorCapability::NoColor),
            "NO_COLOR=1 时 detect 应返回 NoColor");
        match saved {
            Some(v) => std::env::set_var("NO_COLOR", v),
            None => std::env::remove_var("NO_COLOR"),
        }
    }
}
