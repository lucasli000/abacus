//! 流式 Markdown 渲染引擎 — mdstream committed/pending 模型
//!
//! 基于 mdstream crate 的 MdStream，实现增量 block 分割：
//! - committed blocks：已确定完成的段落，渲染结果可缓存
//! - pending block：当前未闭合的尾部块，每帧重渲染（但仅处理尾部文本）
//!
//! 引用关系：
//! - 写入：run.rs StreamChunk::TextDelta → append()
//! - 读取：components/mod.rs 流式消息渲染 → committed_styled() + pending_styled()
//! 生命周期：streaming 开始时 lazy 创建（首次 TextDelta），reset_streaming 时 drop

use crate::tui::markdown::{self, StyledLine};
use crate::tui::theme::Theme;
use mdstream::MdStream;

/// 流式 Markdown 状态管理器
///
/// 包装 mdstream::MdStream，将 committed/pending block 分割结果
/// 转为本项目 pulldown-cmark 渲染器可消费的 StyledLine 序列。
///
/// 引用关系：
/// - 被 AppState.streaming_md 持有（RefCell<Option<Self>>）
/// - run.rs TextDelta → append()
/// - components/mod.rs render → committed_styled() / pending_styled()
/// 生命周期：首次 TextDelta 创建 → streaming 期间追加 → reset_streaming 时 drop
pub struct StreamingMd {
    /// mdstream 核心状态机
    stream: MdStream,
    /// committed blocks 的已渲染缓存（仅新增 block 触发增量渲染）
    committed_lines: Vec<StyledLine>,
    /// 已渲染的 committed block 数量（用于增量：只渲染新 commit 的 blocks）
    rendered_committed_count: usize,
}

impl StreamingMd {
    pub fn new() -> Self {
        Self {
            stream: MdStream::new(mdstream::Options::default()),
            committed_lines: Vec::new(),
            rendered_committed_count: 0,
        }
    }

    /// 增量追加 delta 文本
    ///
    /// 内部调用 mdstream::MdStream::append 进行 block 边界检测。
    /// 新 committed blocks 会被追加到内部列表，pending 缓存失效。
    pub fn append(&mut self, delta: &str) {
        // mdstream append 返回 Update，但我们不直接使用——
        // 渲染时通过 snapshot_blocks() 获取最新状态
        let _update = self.stream.append(delta);
    }

    /// 获取已 committed 部分的渲染结果（增量缓存：仅新 block 重渲染）
    ///
    /// 返回引用切片，调用方 clone 后使用。
    pub fn committed_styled(&mut self, theme: &Theme, is_user: bool, max_width: usize) -> &[StyledLine] {
        // 获取当前所有 committed blocks
        let blocks = self.stream.snapshot_blocks();
        // 只有 Committed status 的才算（snapshot 包含 pending）
        let committed_blocks: Vec<_> = blocks.iter()
            .filter(|b| b.status == mdstream::BlockStatus::Committed)
            .collect();
        let current_count = committed_blocks.len();

        // 增量：只渲染新 committed 的 blocks
        if current_count > self.rendered_committed_count {
            for block in &committed_blocks[self.rendered_committed_count..] {
                let text = block.display_or_raw();
                let mut lines = markdown::render_markdown_bounded(text, theme, is_user, max_width);
                self.committed_lines.append(&mut lines);
            }
            self.rendered_committed_count = current_count;
        }

        &self.committed_lines
    }

    /// 获取 pending 部分（未闭合块）的渲染结果（每帧重算，但仅处理尾部文本）
    pub fn pending_styled(&mut self, theme: &Theme, is_user: bool, max_width: usize) -> Vec<StyledLine> {
        let blocks = self.stream.snapshot_blocks();
        // pending = 最后一个 Pending status 的 block
        let pending_block = blocks.iter()
            .find(|b| b.status == mdstream::BlockStatus::Pending);

        match pending_block {
            Some(block) => {
                let text = block.display_or_raw();
                if text.is_empty() {
                    vec![]
                } else {
                    markdown::render_markdown_bounded(text, theme, is_user, max_width)
                }
            }
            None => vec![],
        }
    }

    /// V40: 零拷贝一体化方法 — 返回 committed + pending 合并结果
    ///
    /// 优化：committed 部分通过 `extend_from_slice` 而非 `.to_vec()` + `.into_iter().chain()`
    /// 减少一次完整 Vec clone。内部先更新 committed 缓存（增量），再拼接 pending。
    ///
    /// 引用关系：components/mod.rs 流式消息渲染替代原有 committed_styled().to_vec() + pending_styled()
    /// 生命周期：每帧调用一次，committed 缓存跨帧复用
    pub fn all_styled(&mut self, theme: &Theme, is_user: bool, max_width: usize) -> Vec<StyledLine> {
        // 先更新 committed 缓存（增量渲染新 block）
        let blocks = self.stream.snapshot_blocks();
        let committed_blocks: Vec<_> = blocks.iter()
            .filter(|b| b.status == mdstream::BlockStatus::Committed)
            .collect();
        let current_count = committed_blocks.len();

        if current_count > self.rendered_committed_count {
            for block in &committed_blocks[self.rendered_committed_count..] {
                let text = block.display_or_raw();
                let mut lines = markdown::render_markdown_bounded(text, theme, is_user, max_width);
                self.committed_lines.append(&mut lines);
            }
            self.rendered_committed_count = current_count;
        }

        // 构建结果：committed 引用 + pending 新渲染
        let mut result = Vec::with_capacity(self.committed_lines.len() + 8);
        result.extend_from_slice(&self.committed_lines);

        // pending 部分
        let pending_block = blocks.iter()
            .find(|b| b.status == mdstream::BlockStatus::Pending);
        if let Some(block) = pending_block {
            let text = block.display_or_raw();
            if !text.is_empty() {
                let mut pending = markdown::render_markdown_bounded(text, theme, is_user, max_width);
                result.append(&mut pending);
            }
        }
        result
    }

    /// 重置（streaming 结束时调用）
    pub fn reset(&mut self) {
        self.stream.reset();
        self.committed_lines.clear();
        self.rendered_committed_count = 0;
    }

    /// 原始文本（用于落档 / fallback）
    pub fn raw_text(&self) -> &str {
        self.stream.buffer()
    }
}
