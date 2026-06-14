//! CompletionEngine — 统一补全状态机
//!
//! 封装内联补全（Tab 驱动）和弹窗补全（Ctrl+Space / Ctrl+Tab 驱动）的全部状态。
//!
//! ## 两类补全
//!
//! | 类型 | 触发 | 状态 | 行为 |
//! |------|------|------|------|
//! | 内联 | Tab | suggestion + candidates | ghost text + Tab 循环 |
//! | 弹窗 | Ctrl+Space / Ctrl+Tab | popup_candidates + popup_index | 弹窗列表 + 选中 |

/// 统一补全引擎
#[derive(Debug, Default)]
pub struct CompletionEngine {
    // ─── 内联补全（Tab 驱动）────────────────────────────
    /// 当前 ghost text 建议（渲染时显示为灰色文本）
    pub suggestion: Option<String>,
    /// 所有匹配的候选（Tab 循环时使用）
    pub candidates: Vec<String>,
    /// 当前候选索引
    pub candidate_idx: usize,

    // ─── 弹窗补全（Ctrl+Space / Ctrl+Tab 驱动）─────────
    /// 弹窗候选列表（文件路径 / AI 补全结果）
    pub popup_candidates: Vec<String>,
    /// 弹窗选中索引
    pub popup_index: usize,
    /// 弹窗补全前缀（用于替换；文件路径补全时为光标前的 token）
    pub popup_prefix: String,
}

impl CompletionEngine {
    pub fn new() -> Self {
        Self::default()
    }

    // ─── 内联补全 API ──────────────────────────────────

    /// 是否有活跃的 ghost text 建议
    pub fn has_suggestion(&self) -> bool {
        self.suggestion.is_some()
    }

    /// 是否在循环候选模式
    pub fn is_cycling(&self) -> bool {
        !self.candidates.is_empty()
    }

    /// 设置建议（由 compute 调用）
    pub fn set_suggestion(&mut self, suggestion: String) {
        self.suggestion = Some(suggestion);
    }

    /// 设置候选列表（首次 Tab 接受后）
    pub fn set_candidates(&mut self, candidates: Vec<String>) {
        if candidates.len() > 1 {
            self.candidates = candidates;
            self.candidate_idx = 0;
        } else {
            self.candidates.clear();
            self.candidate_idx = 0;
        }
    }

    /// 循环到下一个候选
    pub fn next_candidate(&mut self) -> String {
        if self.candidates.is_empty() {
            return String::new();
        }
        self.candidate_idx = (self.candidate_idx + 1) % self.candidates.len();
        self.candidates[self.candidate_idx].clone()
    }

    // ─── 弹窗补全 API ──────────────────────────────────

    /// 是否有弹窗候选
    pub fn has_popup(&self) -> bool {
        !self.popup_candidates.is_empty()
    }

    /// 设置弹窗候选（异步补全结果到达时调用）
    pub fn set_popup(&mut self, candidates: Vec<String>, prefix: String) {
        self.popup_candidates = candidates;
        self.popup_index = 0;
        self.popup_prefix = prefix;
    }

    /// 弹窗选中项前移
    pub fn popup_prev(&mut self) {
        if !self.popup_candidates.is_empty() {
            self.popup_index = if self.popup_index == 0 {
                self.popup_candidates.len() - 1
            } else {
                self.popup_index - 1
            };
        }
    }

    /// 弹窗选中项后移
    pub fn popup_next(&mut self) {
        if !self.popup_candidates.is_empty() {
            self.popup_index = (self.popup_index + 1) % self.popup_candidates.len();
        }
    }

    /// 弹窗 PageUp（-5）
    pub fn popup_page_up(&mut self) {
        self.popup_index = self.popup_index.saturating_sub(5);
    }

    /// 弹窗 PageDown（+5）
    pub fn popup_page_down(&mut self) {
        let max = self.popup_candidates.len().saturating_sub(1);
        self.popup_index = (self.popup_index + 5).min(max);
    }

    /// 弹窗直接选中（Alt+N）
    pub fn popup_select(&mut self, n: usize) {
        if n < self.popup_candidates.len() {
            self.popup_index = n;
        }
    }

    /// 获取当前弹窗选中项
    pub fn popup_selected(&self) -> Option<&str> {
        self.popup_candidates.get(self.popup_index).map(|s| s.as_str())
    }

    // ─── 全局重置 ──────────────────────────────────────

    /// 重置内联补全状态（输入新字符时调用）
    pub fn reset_inline(&mut self) {
        self.suggestion = None;
        self.candidates.clear();
        self.candidate_idx = 0;
    }

    /// 重置弹窗补全状态（取消/接受弹窗时调用）
    pub fn reset_popup(&mut self) {
        self.popup_candidates.clear();
        self.popup_index = 0;
        self.popup_prefix.clear();
    }

    /// 重置全部补全状态
    pub fn reset_all(&mut self) {
        self.reset_inline();
        self.reset_popup();
    }
}
