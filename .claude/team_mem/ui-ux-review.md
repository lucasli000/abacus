# UI/UX 细粒度审查报告

## 发现的问题

### 1. 输入框高度计算不一致（中等严重）✅ 已修复
**位置**：`event/mod.rs:1608` vs `modes/common.rs:47`
**问题**：鼠标事件处理使用 `chat_input_height(terminal_rows)`（默认2行），渲染层使用 `chat_input_height_adaptive(terminal_rows, input_lines)`（自适应高度）
**影响**：多行输入时，鼠标点击位置与实际输入框不匹配
**修复**：统一使用 `chat_input_height_adaptive`

### 2. 滚动位置不精确（低严重）✅ 已修复
**位置**：`cards/render.rs:90-94`
**问题**：滚动按卡片高度跳过，`scroll_offset` 不是卡片高度整数倍时会卡住
**影响**：滚动体验不流畅，可能出现跳动
**修复**：实现像素级滚动，支持部分卡片可见

### 3. 光标定位边界条件（低严重）✅ 已修复
**位置**：`components/bars.rs:371`
**问题**：`cursor_pos <= byte_end` 使用 `<=`，光标在行尾时可能定位到下一行
**影响**：光标在行尾时显示位置可能偏移
**修复**：改为 `cursor_pos < byte_end`，特殊处理行尾

### 4. 指针计算潜在溢出（低严重）✅ 已修复
**位置**：`components/bars.rs:478`
**问题**：`line_ptr - input_ptr` 无 saturating_sub 保护
**影响**：极端情况下可能 panic
**修复**：使用 `saturating_sub`

### 5. 输入框填充逻辑边界（低严重）✅ 已修复
**位置**：`components/bars.rs:439`
**问题**：`(end - start)` 可能大于 `text_area_h`，循环不执行
**影响**：输入框高度可能不正确
**修复**：添加边界检查

### 6. 状态指示与边框颜色分离（设计问题）✅ 已修复
**位置**：`components/bars.rs:246` vs `components/bars.rs:303-313`
**问题**：边框始终 `primary`，状态指示用 `accent/gold/success`，`input_bar_color()` 未使用
**影响**：状态变化时边框不响应，用户可能忽略状态变化
**修复**：让边框颜色跟随状态变化

### 7. 设置模态框背景遮罩（设计问题）✅ 已修复
**位置**：`components/overlays.rs:750-753`
**问题**：`bg + DIM` 在浅色主题下可能使背景变暗过多
**影响**：浅色主题下模态框背景可能不够清晰
**修复**：使用 `elevated` 颜色替代

### 8. 全屏编辑器光标列计算（低严重）✅ 已修复
**位置**：`components/overlays.rs:936`
**问题**：光标定位可能溢出
**影响**：极端情况下可能 panic
**修复**：使用 `saturating_add` 防止溢出

## 测试结果

- 编译：通过
- 测试：90/90 通过
