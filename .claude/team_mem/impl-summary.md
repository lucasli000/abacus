## 修改文件

- `pkg/crates/abacus-cli/src/tui/cards/llm.rs`：改进 LLM reply 部分的渲染，从估计行数占位符改为实际内容渲染
- `pkg/crates/abacus-cli/src/tui/cards/expert.rs`：修复重复的空行检查 bug，统一 reply 渲染逻辑
- `pkg/crates/abacus-ui-kit/src/theme.rs`：增强颜色能力检测逻辑，支持更多终端类型

## 设计决策

1. **llm.rs 修改**：
   - 移除了基于行数估算的占位符渲染逻辑
   - 改为直接渲染 `reply_text` 的实际内容
   - 当 `reply_text` 为空时显示 "(replying…)" 提示
   - 每行内容添加 1 字符前导 padding 避免贴左边框

2. **expert.rs 修改**：
   - 修复了重复的 `if lines.is_empty()` 检查 bug
   - 统一了 reply 渲染逻辑，与 llm.rs 保持一致
   - 当 `reply_text` 为空时显示 "(replying…)" 提示
   - 当 `reply_text` 非空时渲染实际内容

3. **theme.rs 修改**：
   - 优先检查 `COLORTERM` 环境变量（最可靠的信号）
   - 增加对现代终端的检测：xterm、screen、tmux 默认支持 TrueColor
   - 增加 `TERM_PROGRAM` 检测：支持 iTerm2、WezTerm、Alacritty、kitty、ghostty、Hyper 等
   - 增加 Windows Terminal 检测（`WT_SESSION` 环境变量）
   - Apple Terminal 支持 256 色

## 审查结果

- 安全审查：PASS（无安全问题）
- 样式审查：PASS（代码规范良好）
- 最终裁决：LGTM

## 已知风险

- 无
