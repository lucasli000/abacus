//! V30 系统剪贴板写入抽象
//!
//! ## 设计动机
//! 旧路径仅 OSC 52 转义序列（`\x1b]52;c;<base64>\x07`），在 macOS Terminal.app 完全失效，
//! iTerm2 默认禁用，只在 ssh + 现代 emulator 组合下可靠。引入 arboard 走平台原生 API
//! （NSPasteboard / X11 selection / Wayland wl-clipboard / Windows clipboard），失败时
//! 仍 fallback 到 OSC 52，最大化兼容覆盖。
//!
//! ## 引用关系
//! - 写入端：tui/event/mod.rs（Shift+Drag 释放时复制）/ tui/slash_commands.rs cmd_copy
//! - 读取端：无（只写不读，无 paste 用例）
//!
//! ## 生命周期
//! - `set_text` 内部每次新建 arboard::Clipboard 实例（句柄随 fn return drop）
//! - 不持有全局静态——避免 X11 server 长连接引发 ABACUS 退出后剪贴板丢失（X11 协议特性）
//!
//! ## 失败语义
//! - arboard 失败 → 自动尝试 OSC 52
//! - OSC 52 失败（write! 出错）→ 返回 Err，调用方 toast 提示
//! - 两者都失败 → 极少数无 GUI 终端 + 不支持 OSC 52 的环境（如 dumb terminal），
//!   功能性退化到无；现状下用户至少能看到 toast 错误提示
//!
//! ## ⚠ 代码审查 @2025-01-23
//! Phase 3 已完成去重：base64 实现统一到 tui::util::base64_encode，
//! 本模块通过 `use super::util::base64_encode` 引用 SSoT 实现。

use std::io::Write;

/// 把文本写入系统剪贴板。优先走平台原生，fallback OSC 52。
///
/// 返回 Ok 表示至少有一条路径成功；返回 Err 表示两条路径都失败。
pub fn set_text(text: &str) -> Result<ClipboardBackend, String> {
    // Path A: arboard 平台原生
    if let Ok(mut cb) = clipboard::Clipboard::new() {
        if cb.set_text(text.to_string()).is_ok() {
            return Ok(ClipboardBackend::Native);
        }
    }
    // Path B: OSC 52 fallback
    let encoded = base64_encode(text);
    let mut stdout = std::io::stdout();
    if write!(stdout, "\x1b]52;c;{}\x07", encoded).is_ok() && stdout.flush().is_ok() {
        return Ok(ClipboardBackend::Osc52);
    }
    Err("clipboard write failed: both arboard and OSC 52 unreachable".into())
}

/// 已使用的剪贴板路径（toast 显示给用户区分行为）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardBackend {
    /// arboard 平台原生 API（首选，所有桌面环境可靠）
    Native,
    /// OSC 52 终端转义序列（fallback，仅部分 emulator 支持写）
    Osc52,
}

impl ClipboardBackend {
    pub fn label(&self) -> &'static str {
        match self {
            ClipboardBackend::Native => "已复制",
            ClipboardBackend::Osc52 => "已复制 (OSC52)",
        }
    }
}

/// Phase 3 去重：base64 实现已统一到 util::base64_encode
/// 引用关系：调用 super::util::base64_encode（SSoT）
use super::util::base64_encode;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_encode_basic() {
        assert_eq!(base64_encode(""), "");
        assert_eq!(base64_encode("a"), "YQ==");
        assert_eq!(base64_encode("ab"), "YWI=");
        assert_eq!(base64_encode("abc"), "YWJj");
        assert_eq!(base64_encode("Hello"), "SGVsbG8=");
    }

    #[test]
    fn base64_encode_unicode() {
        // UTF-8 byte sequence — 与 util::base64_encode 输出对齐
        let s = "你好";
        let result = base64_encode(s);
        assert_eq!(result, "5L2g5aW9");
    }
}
