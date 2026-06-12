//! TUI 通用工具 —— 终端宽度感知字符串处理
//!
//! 设计动机：
//! Rust 字符串有三种"长度"：byte (`len()`) / char (`chars().count()`) / display column。
//! 终端 UI 必须用 display column，否则 CJK 全角字符（每个占 2 列）下：
//! - 列宽计算偏小 → 表格列对不齐
//! - padding 字符不足 → 右对齐错位
//! - gap 算少 → fill 空间不够
//!
//! 与 `abacus-cli::tui::util` 平行：CLI 内已有同等 helper，
//! 本模块向第三方 Agent 应用 crate（不依赖 abacus-cli）暴露等价 API。

use unicode_width::UnicodeWidthStr;

/// 计算字符串显示列宽（终端列数）。
///
/// CJK 全角字符 = 2 列；ASCII / Latin = 1 列；零宽字符 = 0 列。
/// 等价于 `UnicodeWidthStr::width(s)` 的 re-export。
///
/// ## 为何不直接用 `s.chars().count()`
///
/// `chars().count()` 数 Unicode scalar，CJK 全角字符算 1 个；
/// 但终端显示 CJK 全角字符占 **2 列**。混用两者会导致
/// 表格列、padding 字符、卡片 header `fill_w` 计算全部错位。
#[inline]
pub fn display_width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// 单字符显示列宽。
///
/// 控制字符 None → 0；ASCII = 1；CJK 全角 = 2。
/// 用于增量计算光标位置等单字符场景。
#[inline]
pub fn char_width(c: char) -> usize {
    unicode_width::UnicodeWidthChar::width(c).unwrap_or(0)
}

/// 左对齐 padding 到指定显示列宽。
///
/// 若 `display_width(s) >= target_w` 则原样返回；否则在右侧追加空格
/// 使总显示宽度 = `target_w`。不截断超长字符串。
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
    }
}
