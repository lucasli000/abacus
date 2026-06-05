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

/// 标准 base64 编码（无外部依赖）
///
/// 引用关系：被 clipboard.rs（OSC 52 fallback）和 event/mod.rs（遗留引用）统一调用
/// 生命周期：纯函数、无状态、无副作用
///
/// Phase 3 去重：合并原 event/mod.rs::base64_encode_inner + clipboard.rs::base64_encode
/// 两处独立实现为单一 SSoT
pub fn base64_encode(input: &str) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = input.as_bytes();
    let mut out = String::with_capacity((bytes.len() + 2) / 3 * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((triple >> 18) & 0x3F) as usize] as char);
        out.push(TABLE[((triple >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(triple & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

/// 通用 word-wrap：按 max_width 列宽拆分文本，返回 (start_byte, end_byte) 切片列表。
/// 断点策略：空格/连字符/宽字符后断行；无合适断点时强制断。
///
/// 引用关系：被 components/mod.rs word-wrap 路径调用（3 处：markdown rendered line、
///   streaming thinking visible 切片、streaming text 超宽行）
/// 生命周期：纯函数、无状态、无副作用
pub fn word_wrap_segments(text: &str, max_width: usize) -> Vec<(usize, usize)> {
    if text.is_empty() {
        return vec![];
    }
    let mut segments = Vec::new();
    let mut start = 0;
    let mut remaining = text;
    while !remaining.is_empty() {
        let mut width = 0usize;
        let mut take = 0usize;
        let mut last_break = 0usize;
        for (i, ch) in remaining.char_indices() {
            let cw = UnicodeWidthChar::width(ch).unwrap_or(1);
            if width + cw > max_width {
                break;
            }
            width += cw;
            take = i + ch.len_utf8();
            if ch == ' ' || ch == '-' || ch == '/'
                || UnicodeWidthChar::width(ch).unwrap_or(0) > 1
            {
                last_break = take;
            }
        }
        if take == 0 {
            // 单字符超宽（不应发生），强制取 1 char
            take = remaining.chars().next().map(|c| c.len_utf8()).unwrap_or(1);
        } else if take < remaining.len() && last_break > 0 && last_break > take / 2 {
            take = last_break;
        }
        segments.push((start, start + take));
        remaining = &remaining[take..];
        start += take;
    }
    segments
}

/// 安全取字符串前缀：截到最多 `n` 个字符（不是字节），在 UTF-8 字符边界上截断。
///
/// 解决 `&s[..n.min(s.len())]` 在多字节 UTF-8 序列中间切分会 panic 的问题。
/// 历史上 `session_id`（UUID）触发此 bug——若 session_id 改为非 ASCII 来源（用户输入、
/// i18n）且长度恰好 8 字节横穿多字节序列，会 panic。
///
/// # Examples
/// ```
/// use abacus_cli::tui::util::safe_prefix;
/// assert_eq!(safe_prefix("hello world", 5), "hello");
/// assert_eq!(safe_prefix("你好世界", 2), "你好");
/// assert_eq!(safe_prefix("", 8), "");
/// assert_eq!(safe_prefix("abc", 100), "abc");
/// ```
pub fn safe_prefix(s: &str, n: usize) -> &str {
    if s.is_empty() || n == 0 {
        return "";
    }
    // chars() 迭代器本身就是按字符边界推进的，所以 take(n) 一定在合法边界停下
    let end_byte = s
        .char_indices()
        .nth(n)
        .map(|(idx, _)| idx)
        .unwrap_or(s.len());
    &s[..end_byte]
}

#[cfg(test)]
mod safe_prefix_tests {
    use super::*;

    #[test]
    fn safe_prefix_ascii() {
        assert_eq!(safe_prefix("hello world", 5), "hello");
        assert_eq!(safe_prefix("abc", 100), "abc");
    }

    #[test]
    fn safe_prefix_cjk() {
        // "你好世界" = 4 字符 × 3 字节 = 12 字节
        // 截 2 字符 → "你好" (6 字节)，不是 panic
        assert_eq!(safe_prefix("你好世界", 2), "你好");
        assert_eq!(safe_prefix("你好世界", 3), "你好世");
    }

    #[test]
    fn safe_prefix_emoji() {
        // 4-byte emoji: "🦀" is 1 char / 4 bytes
        // n=1 → "🦀" (4 bytes)，n=2 → "🦀🦀" (8 bytes)
        assert_eq!(safe_prefix("🦀🦀x", 2), "🦀🦀");
        assert_eq!(safe_prefix("🦀🦀x", 3), "🦀🦀x");
    }

    #[test]
    fn safe_prefix_edge_cases() {
        assert_eq!(safe_prefix("", 8), "");
        assert_eq!(safe_prefix("x", 0), "");
        assert_eq!(safe_prefix("x", 1), "x");
    }

    /// 直接演示 `&s[..n.min(s.len())]` 模式在 CJK 字符串上 panic
    #[test]
    fn demo_old_pattern_panics_on_cjk() {
        // 模拟原 buggy 代码：n=1 切在 "好" 中间（3 字节 = 1 字符宽度，但 [..3] 跨字符）
        let s = "你好世界";
        // [..3] 切在 "好" 字符中间（"好" 本身 3 字节 [3..6]）
        // 所以 s[..3] 是 "你" + 0 字节 的"好" → 切在"你"末尾，正好合法边界
        // 实际会 panic 的：s[..4]（"你" 3 字节 + "好" 第 1 字节）→ 切在"好"中
        let result = std::panic::catch_unwind(|| &s[..4]);
        assert!(result.is_err(), "old pattern should panic on CJK mid-codepoint");
    }
}
