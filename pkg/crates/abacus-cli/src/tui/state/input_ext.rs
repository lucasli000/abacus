//! AppState 输入相关方法提取
//!
//! 光标计算、textarea 同步、内联补全计算。

use std::cell::RefCell;

use super::AppState;
use crate::tui::state::TuiTextArea;

impl AppState {
    /// 从 cursor_pos 重新计算 cursor_line / cursor_col（O(n)，仅在输入变更时调用）
    /// cursor_col 使用 display width（unicode-width），非 char count
    pub fn recalculate_cursor(&mut self) {
        let before = &self.input[..self.cursor_pos.min(self.input.len())];
        self.cursor_line = before.matches('\n').count();
        let line_start = before.rfind('\n').map(|i| i + 1).unwrap_or(0);
        self.cursor_col = before[line_start..]
            .chars()
            .map(|c| unicode_width::UnicodeWidthChar::width(c).unwrap_or(1))
            .sum();
    }

    /// 清空输入框 + 重置光标 + 同步 textarea
    pub fn clear_input(&mut self) {
        self.input.clear();
        self.cursor_pos = 0;
        self.cursor_line = 0;
        self.cursor_col = 0;
        self.sync_to_textarea();
    }

    /// V42-B+: 从 tui-textarea 同步光标到 state 字段
    ///
    /// 独立函数避免 &self vs &mut self 冲突（textarea 在 RefCell 中）。
    /// 调用方需持有 &mut AppState，传入各字段的可变引用。
    pub(crate) fn sync_from_textarea(
        textarea: &RefCell<TuiTextArea<'static>>,
        input: &str,
        cursor_pos: &mut usize,
        cursor_line: &mut usize,
        cursor_col: &mut usize,
    ) {
        let ta = textarea.borrow();
        let (row, col) = ta.cursor();
        let new_cursor_pos = row_col_to_byte_pos(input, row, col);
        if *cursor_pos != new_cursor_pos {
            *cursor_pos = new_cursor_pos;
        }
        *cursor_line = row;
        *cursor_col = col;
    }

    /// 文本变化专用 sync（比 sync_from_textarea 更高效，跳过 cursor 重算）
    /// 仅在 textarea 文本实际变化时调用（如 textarea.input() 返回 true 后）
    pub(crate) fn sync_text_from_textarea(
        textarea: &RefCell<TuiTextArea<'static>>,
        input: &mut String,
    ) {
        let ta = textarea.borrow();
        let new_input: String = ta.lines().join("\n");
        if *input != new_input {
            input.clear();
            input.push_str(&new_input);
        }
    }

    /// V42-B+: 从 state.input 同步到 tui-textarea
    ///
    /// 外部代码直接修改 state.input 后调用（如 navigate_history、accept_completion），
    /// 确保 textarea 内容与 state.input 一致。
    pub(crate) fn sync_to_textarea(&self) {
        let mut ta = self.textarea.borrow_mut();
        let current = ta.lines().join("\n");
        if current != self.input {
            let lines: Vec<String> = if self.input.is_empty() {
                vec![String::new()]
            } else {
                self.input.lines().map(|s| s.to_string()).collect()
            };
            ta.select_all();
            ta.cut();
            for (i, line) in lines.iter().enumerate() {
                if i > 0 {
                    ta.insert_newline();
                }
                ta.insert_str(line);
            }
        }
        // 同步光标
        let (target_row, target_col) = byte_pos_to_row_col(&self.input, self.cursor_pos);
        let (cur_row, cur_col) = ta.cursor();
        if (cur_row, cur_col) != (target_row, target_col) {
            use tui_textarea::CursorMove;
            ta.move_cursor(CursorMove::Top);
            for _ in 0..target_row {
                ta.move_cursor(CursorMove::Down);
            }
            ta.move_cursor(CursorMove::Head);
            for _ in 0..target_col {
                ta.move_cursor(CursorMove::Forward);
            }
        }
    }

    /// 根据当前输入计算最佳内联补全候选
    ///
    /// 优先级：斜杠命令 > 历史记录
    pub fn compute_inline_suggestion(&self) -> Option<String> {
        let input = self.input.trim();
        if input.is_empty() {
            return None;
        }
        let lower = input.to_lowercase();

        // 采纳后抑制 — 如果 input 已完全匹配一个命令名，不再建议
        if input.starts_with('/') {
            let all_names = crate::tui::slash_commands::all_command_names();
            let exact = all_names.iter().any(|n| format!("/{}", n).to_lowercase() == lower);
            if exact { return None; }
        }

        // 优先级 1: 斜杠命令补全（至少输入 /+1字符 才触发）
        if input.starts_with('/') && input.len() > 1 {
            let all_names = crate::tui::slash_commands::all_command_names();
            let mut matches: Vec<String> = all_names.iter()
                .map(|n| format!("/{}", n))
                .filter(|c| {
                    let cl = c.to_lowercase();
                    cl.starts_with(&lower) && cl.len() > lower.len()
                })
                .collect();
            matches.sort();
            if let Some(best) = matches.first() {
                return Some(best.clone());
            }
        }

        // 优先级 2: 历史记录补全
        if !self.input_history.is_empty() {
            let exact_history = self.input_history.iter()
                .any(|h| h.trim().to_lowercase() == lower);
            if exact_history { return None; }

            if let Some(h) = self.input_history.iter()
                .rev()
                .find(|h| {
                    let hl = h.trim().to_lowercase();
                    hl.starts_with(&lower) && hl.len() > lower.len()
                })
            {
                return Some(h.trim().to_string());
            }
        }

        None
    }

    /// 计算所有匹配的 inline 候选（用于 Tab 循环）
    ///
    /// 返回排序后的全部候选列表（斜杠命令优先，历史次之）。
    /// 第一项与 `compute_inline_suggestion()` 结果一致。
    pub fn compute_all_inline_candidates(&self) -> Vec<String> {
        let input = self.input.trim();
        if input.is_empty() {
            return Vec::new();
        }
        let lower = input.to_lowercase();
        let mut candidates: Vec<String> = Vec::new();

        // 斜杠命令（至少 /+1字符）
        if input.starts_with('/') && input.len() > 1 {
            let all_names = crate::tui::slash_commands::all_command_names();
            let mut cmd_matches: Vec<String> = all_names.iter()
                .map(|n| format!("/{}", n))
                .filter(|c| {
                    let cl = c.to_lowercase();
                    cl.starts_with(&lower) && cl.len() > lower.len()
                })
                .collect();
            cmd_matches.sort();
            candidates.extend(cmd_matches);
        }

        // 历史记录
        let hist_matches: Vec<String> = self.input_history.iter()
            .rev()
            .filter(|h| {
                let hl = h.trim().to_lowercase();
                hl.starts_with(&lower) && hl.len() > lower.len()
            })
            .take(10)
            .map(|h| h.trim().to_string())
            .collect();
        for h in hist_matches {
            if !candidates.contains(&h) {
                candidates.push(h);
            }
        }

        candidates
    }
}

/// 字节偏移 → (row, col) 转换
fn byte_pos_to_row_col(input: &str, byte_pos: usize) -> (usize, usize) {
    let mut row = 0usize;
    let mut col = 0usize;
    let mut byte_offset = 0usize;
    for ch in input.chars() {
        if byte_offset >= byte_pos {
            break;
        }
        if ch == '\n' {
            row += 1;
            col = 0;
        } else {
            col += 1;
        }
        byte_offset += ch.len_utf8();
    }
    (row, col)
}

/// (row, col) → 字节偏移 转换
///
/// 用于将 tui-textarea 的光标位置 (row, col) 转换为 state.cursor_pos（字节偏移）。
pub(crate) fn row_col_to_byte_pos(input: &str, row: usize, col: usize) -> usize {
    let mut current_row = 0usize;
    let mut current_col = 0usize;
    let mut byte_offset = 0usize;
    for ch in input.chars() {
        if current_row == row && current_col == col {
            return byte_offset;
        }
        if ch == '\n' {
            if current_row == row {
                return byte_offset;
            }
            current_row += 1;
            current_col = 0;
        } else {
            current_col += 1;
        }
        byte_offset += ch.len_utf8();
    }
    byte_offset
}
