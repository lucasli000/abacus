//! TUI 通用工具——终端宽度感知字符串处理
//!
//! 设计动机：
//! Rust 字符串有三种"长度"：byte (`len()`) / char (`chars().count()`) / display column。
//! 终端 UI 必须用 display column，否则 CJK 全角字符（每个占 2 列）下：
//! - 列宽计算偏小 → 表格列对不齐
//! - padding 字符不足 → 右对齐错位
//! - gap 算少 → fill 空间不够
//!
//! 历史上散落在 markdown / components 至少 5 个独立现场（MD2/MD2b/I1/P2/status_bar），
//! 本模块统一收口防止第 6 现场。
//!
//! 引用关系：被 components/mod.rs、markdown.rs 调用
//! 生命周期：纯函数、无状态、无副作用

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// 计算字符串显示列宽（终端列数）。
///
/// CJK 全角字符 = 2 列；ASCII / Latin = 1 列；零宽字符 = 0 列。
/// 等价于 `UnicodeWidthStr::width(s)` 的 re-export，用项目自有 API 防止
/// 调用方再次写出 `s.chars().count()` 的错误模式。
#[inline]
pub fn display_width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// 单字符显示列宽（控制字符 None → 0；ASCII = 1；CJK 全角 = 2）。
///
/// 用于增量计算光标位置（cursor_col 累加）等单字符场景。
/// 控制字符遵循 unicode-width 库返回 None，本助手默认按 0 列处理——
/// 调用方若希望"未知字符 = 1 列"应自行 `or(1)` 兜底。
#[inline]
pub fn char_width(c: char) -> usize {
    UnicodeWidthChar::width(c).unwrap_or(0)
}

/// 左对齐 padding 到指定显示列宽。
///
/// 若 `display_width(s) >= target_w` 则原样返回；否则在右侧追加空格使总显示宽度 = `target_w`。
/// 不截断超长字符串——调用方需自行处理 overflow（如 `truncate_to_width`）。
///
/// 与 `format!("{:<N$}", s, N=w)` 的关键差异：format 用 char 数算 padding，
/// 对 CJK 文本会少补空格。本函数按 display column 算。
pub fn pad_to_width(s: &str, target_w: usize) -> String {
    let cur_w = display_width(s);
    if cur_w >= target_w {
        return s.to_string();
    }
    let pad = target_w - cur_w;
    let mut out = String::with_capacity(s.len() + pad);
    out.push_str(s);
    for _ in 0..pad {
        out.push(' ');
    }
    out
}

/// 截断字符串到 ≤ `max_w` 显示列宽（不带省略号）。
///
/// 在 char 边界处停止，确保返回有效 UTF-8。CJK 字符宽 2 时若加入会越界则跳过。
///
/// 实际 callsite（V33 审查更新）：
/// - slash_commands.rs:1044 — toast preview 限长 80 列
/// - components/mod.rs:3104 — markdown raw_text 截断到内容宽度
/// - components/mod.rs:3146 — markdown detail line 限宽
/// - markdown.rs:398 — 行内代码片段截断
pub fn truncate_to_width(s: &str, max_w: usize) -> String {
    let mut w = 0usize;
    let mut end = 0usize;
    for (i, ch) in s.char_indices() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        if w + cw > max_w {
            break;
        }
        w += cw;
        end = i + ch.len_utf8();
    }
    s[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_width() {
        assert_eq!(display_width("hello"), 5);
        assert_eq!(display_width(""), 0);
    }

    #[test]
    fn cjk_width_doubles() {
        assert_eq!(display_width("中文"), 4);
        assert_eq!(display_width("Esc 取消"), 8);
        // 对比 chars().count()——证明 chars 不可信
        assert_eq!("Esc 取消".chars().count(), 6);
    }

    #[test]
    fn pad_ascii() {
        assert_eq!(pad_to_width("hi", 5), "hi   ");
    }

    #[test]
    fn pad_cjk_uses_display_width() {
        // "中文" 占 4 列，pad 到 6 列应补 2 个空格
        assert_eq!(pad_to_width("中文", 6), "中文  ");
        // 关键：format!("{:<6}", "中文") 会补 4 空格（按 chars 算 2 → 加 4），错位
        // 这就是 helper 存在的意义
    }

    #[test]
    fn pad_no_op_when_overflow() {
        assert_eq!(pad_to_width("hello", 3), "hello");
    }

    #[test]
    fn truncate_at_char_boundary() {
        assert_eq!(truncate_to_width("中文hello", 4), "中文");
        assert_eq!(truncate_to_width("中文hello", 5), "中文h");
        // 宽度不够容纳一个 CJK 字符则停在它前面
        assert_eq!(truncate_to_width("中文", 1), "");
    }
}
