//! 语法高亮引擎 — 基于 syntect
//!
//! 将代码文本转为带颜色的 ratatui Span 序列。
//! 支持 200+ 语言自动检测，使用内置的 Monokai 暗色主题。
//!
//! 引用关系：被 markdown.rs 的代码块渲染调用
//! 生命周期：全局单例（lazy_static），程序启动时初始化一次
//!
//! ## ⚠ 代码审查 @2025-01-23 (中等)
//! `pick_syntect_theme` 仅二选一（base16-ocean.light / base16-ocean.dark），
//! 用户切换到 Dracula/Nord/Gruvbox 等主题时，语法高亮仍用 base16-ocean.dark，
//! 可能与用户主题不完全协调。理想情况应基于用户主题 bg 动态适配所有 syntect
//! 内置主题 (inspired-github、Solarized 等 10+ 套)。

use once_cell::sync::Lazy;
use ratatui::style::{Color, Style};
use ratatui::text::Span;
use syntect::easy::HighlightLines;
use syntect::highlighting::{self, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

/// 全局语法高亮资源（加载一次，跨帧复用）
static SYNTAX_SET: Lazy<SyntaxSet> = Lazy::new(|| SyntaxSet::load_defaults_newlines());
static THEME_SET: Lazy<ThemeSet> = Lazy::new(ThemeSet::load_defaults);

/// 将一行代码高亮为 ratatui Span 列表
///
/// `lang`: 语言名（如 "rust", "python", "go"）。空字符串 = 纯文本。
/// 返回: 带颜色的 Span 列表，每个 token 一个 Span。
pub fn highlight_line(line: &str, lang: &str, user_theme: &crate::tui::theme::Theme) -> Vec<Span<'static>> {
    let syntax = if lang.is_empty() {
        SYNTAX_SET.find_syntax_plain_text()
    } else {
        SYNTAX_SET.find_syntax_by_token(lang)
            .or_else(|| SYNTAX_SET.find_syntax_by_extension(lang))
            .unwrap_or_else(|| SYNTAX_SET.find_syntax_plain_text())
    };

    // F3 完善：根据用户主题亮度选择 syntect 主题（亮底主题不再被深色代码块割裂）
    let theme_name = pick_syntect_theme(user_theme);
    let theme = &THEME_SET.themes[theme_name];
    let mut h = HighlightLines::new(syntax, theme);

    // 高亮单行
    let regions = match h.highlight_line(line, &SYNTAX_SET) {
        Ok(r) => r,
        Err(_) => return vec![Span::raw(line.to_string())],
    };

    regions.iter().map(|(style, text)| {
        let fg = syntect_to_ratatui_color(style.foreground);
        Span::styled(text.to_string(), Style::default().fg(fg))
    }).collect()
}

/// 高亮整段代码（多行），返回每行的 Span 列表
pub fn highlight_code(code: &str, lang: &str, user_theme: &crate::tui::theme::Theme) -> Vec<Vec<Span<'static>>> {
    let syntax = if lang.is_empty() {
        SYNTAX_SET.find_syntax_plain_text()
    } else {
        SYNTAX_SET.find_syntax_by_token(lang)
            .or_else(|| SYNTAX_SET.find_syntax_by_extension(lang))
            .unwrap_or_else(|| SYNTAX_SET.find_syntax_plain_text())
    };

    // F3 完善：与 highlight_line 一致的主题选择逻辑
    let theme_name = pick_syntect_theme(user_theme);
    let theme = &THEME_SET.themes[theme_name];
    let mut h = HighlightLines::new(syntax, theme);
    let mut result = Vec::new();

    for line in LinesWithEndings::from(code) {
        let regions = match h.highlight_line(line, &SYNTAX_SET) {
            Ok(r) => r,
            Err(_) => {
                result.push(vec![Span::raw(line.trim_end_matches('\n').to_string())]);
                continue;
            }
        };
        let spans: Vec<Span<'static>> = regions.iter().map(|(style, text)| {
            let fg = syntect_to_ratatui_color(style.foreground);
            // 去掉行尾换行
            let t = text.trim_end_matches('\n').to_string();
            Span::styled(t, Style::default().fg(fg))
        }).collect();
        result.push(spans);
    }
    result
}

/// syntect Color → ratatui Color 转换
fn syntect_to_ratatui_color(c: highlighting::Color) -> Color {
    // syntect 用 RGBA，ratatui 用 RGB
    if c.a == 0 {
        return Color::Reset; // 透明 = 使用终端默认色
    }
    Color::Rgb(c.r, c.g, c.b)
}

/// F3 完善：按用户主题 bg 亮度选择 syntect 主题
///
/// — 浅色 bg → base16-ocean.light（若存在）
/// — 深色 / Reset / 名称色 → base16-ocean.dark
fn pick_syntect_theme(user_theme: &crate::tui::theme::Theme) -> &'static str {
    if let Color::Rgb(r, g, b) = user_theme.bg {
        let luminance = (r as u32 * 299 + g as u32 * 587 + b as u32 * 114) / 1000;
        if luminance > 128 && THEME_SET.themes.contains_key("base16-ocean.light") {
            return "base16-ocean.light";
        }
    }
    "base16-ocean.dark"
}
