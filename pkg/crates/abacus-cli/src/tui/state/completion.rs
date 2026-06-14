//! CompletionEngine — 内联补全状态机
//!
//! 封装 inline_suggestion / inline_candidates / inline_candidate_idx 三个字段，
//! 提供 Tab 触发、接受、循环、重置的统一 API。
//!
//! ## 设计目标
//!
//! - 消除 event/mod.rs 中 Tab 处理的 4 条路径分支
//! - 补全逻辑与 AppState 解耦，可独立测试
//! - 为 tui-textarea Phase 5 提供干净的补全接口

/// 内联补全引擎
#[derive(Debug, Default)]
pub struct CompletionEngine {
    /// 当前 ghost text 建议（渲染时显示为灰色文本）
    pub suggestion: Option<String>,
    /// 所有匹配的候选（Tab 循环时使用）
    pub candidates: Vec<String>,
    /// 当前候选索引
    pub candidate_idx: usize,
}

/// Tab 操作的结果
#[derive(Debug)]
pub enum TabResult {
    /// 已接受建议/循环到下一个候选，调用方应更新 input
    Accepted(String),
    /// 触发了补全计算，有新的 ghost text
    SuggestionComputed(String),
    /// 无候选，应插入缩进
    InsertIndent,
}

impl CompletionEngine {
    pub fn new() -> Self {
        Self::default()
    }

    /// 是否有活跃的 ghost text 建议
    pub fn has_suggestion(&self) -> bool {
        self.suggestion.is_some()
    }

    /// 是否在循环候选模式
    pub fn is_cycling(&self) -> bool {
        !self.candidates.is_empty()
    }

    /// 重置所有状态（输入新字符时调用）
    pub fn reset(&mut self) {
        self.suggestion = None;
        self.candidates.clear();
        self.candidate_idx = 0;
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
            // 不应调用此方法
            return String::new();
        }
        self.candidate_idx = (self.candidate_idx + 1) % self.candidates.len();
        self.candidates[self.candidate_idx].clone()
    }
}
