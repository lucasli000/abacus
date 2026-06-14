# OpenCode TUI 设计学习笔记（深度版 v2）

> 学习源：https://github.com/anomalyco/opencode + https://github.com/anomalyco/opentui
> 学习日期：2026-06-14
> 关键改变：从"截图分析" → "源码分析"，发现真正的差异不在视觉，而在**架构**

---

## 1. 最关键发现：OpenCode 的"scrollback buffer"是根本性差异

### 1.1 AbacusCode 现状（state/mod.rs:1389, cards/writer.rs 等）

```rust
// AbacusCode 自己维护消息队列
pub struct CardStream {
    cards: Vec<Box<dyn MessageCard>>,        // 自己存消息
    id_to_idx: HashMap<u64, usize>,
    next_id: u64,
    active: Option<u64>,
    collapse_overrides: HashMap<u64, CardCollapse>,
}

// 每帧 render_cards 遍历全部
pub fn render_cards(f: &mut Frame, state: &AppState, area: Rect, _focus: Focus) {
    for card in state.cards.iter() {
        let id = card.id();
        // ... 手动算高度、滚动、clip、渲染
        y = y.saturating_add(actual_h);
    }
}
```

**问题**：
1. 自己实现滚动 buffer（FIFO 裁剪逻辑 line 80）
2. 自己实现 dirty 标记（`rendered_lines_dirty`、`stream_cursor`）
3. 自己实现 markdown 增量解析（`streaming_md.borrow_mut()`）
4. 自己实现消息去重（`push_session_message` 的 dedup）
5. 每帧遍历所有卡 → 长会话性能差

### 1.2 OpenCode 现状（packages/opencode/src/cli/cmd/tui.ts:131-148, opentui split-mode-demo）

```ts
// OpenCode 用 OpenTUI 内置的 scrollback surface
const surface = this.renderer.createScrollbackSurface({
  startOnNewLine: true,
})

const renderable = new MarkdownRenderable(surface.renderContext, {
  content: "",                   // 增量设置
  streaming: true,                // 流式模式
  internalBlockMode: "top-level",
  tableOptions: { widthMode: "content" },
  treeSitterClient: this.treeSitterClient,   // ← tree-sitter 内置
  syntaxStyle: SURFACE_SYNTAX_STYLE,
})

// 流式 chunk 抵达：直接覆盖 content，OpenTUI 自动 diff
renderable.content = run.content
run.surface.render()  // ← OpenTUI 自己算 dirty

// 提交已稳定的行到 scrollback（这是关键模式）
run.surface.commitRows(0, targetRows)   // ← 推到底层 buffer
```

**好处**：
1. **OpenTUI 内置** scrollback buffer、滚动、clipping —— AbacusCode 不需要写
2. **OpenTUI 内置** MarkdownRenderable：tree-sitter 高亮、表格、链接、emoji、CJK、流式
3. **OpenTUI 内置** CodeRenderable：tree-sitter 语法高亮、自动语言识别
4. **OpenTUI 内置** dirty diff：流式 chunk 只重新渲染变化的部分
5. **OpenTUI 内置** TextTableRenderable：表格渲染（footer 用）

### 1.3 根本性差距

**OpenCode 与 AbacusCode 的差距不是"设计"**，而是**底层框架的能力差距**：

| 能力 | OpenTUI（OpenCode 用） | ratatui（AbacusCode 用） |
|---|---|---|
| Markdown 流式渲染 | ✅ `MarkdownRenderable` + tree-sitter | ❌ 自己写 `md_stream.rs` |
| Code syntax 高亮 | ✅ `CodeRenderable` + tree-sitter | ❌ `syntax.rs` 简单正则 |
| Scrollback buffer | ✅ `createScrollbackSurface` | ❌ 自己维护 `CardStream` |
| 流式 dirty diff | ✅ 内置 | ❌ `stream_cursor` 标记 |
| 自动布局 (Flexbox) | ✅ `flexDirection/gap/padding` | ❌ `Constraint` 手算 |
| 主题切换 | ✅ `themeMode` 事件 + 自动重绘 | ⚠️ 手动遍历 |
| 多行编辑器 | ✅ `TextareaRenderable` | ❌ 自己写 cursor_pos/line/col |
| Mouse 集成 | ✅ `useMouse: true` | ⚠️ 部分支持 |
| Resize 处理 | ✅ 内置 `CliRenderEvents.RESIZE` | ⚠️ `terminal.size()` 轮询 |
| `screenMode` 模式切换 | ✅ split-footer / fullscreen / main-screen | ❌ 不存在 |

**结论**：ratatui 是底层 terminal 渲染库（和 ncurses 类似），OpenTUI 是**完整 TUI 应用框架**。AbacusCode 在 ratatui 上**重新发明**了 OpenTUI 已经造好的 80% 轮子。

---

## 2. 真正的"前端可用性"差异拆解

### 2.1 Markdown 渲染（最直观的差距）

**OpenCode**：
- 用 `MarkdownRenderable` + `tree-sitter` markdown parser
- 增量流式解析（`streaming: true`）
- 自动处理：表格、代码块、链接、标题、列表、引用、emoji、CJK
- 自动处理：稳定后才 commit 到 scrollback（不抖动）

**AbacusCode**：
- `pkg/crates/abacus-cli/src/tui/md_stream.rs` 自己实现
- `pkg/crates/abacus-cli/src/tui/markdown.rs` 自己实现
- 增量 markdown 解析可能漏掉边缘 case（已经在 markdown-demo 里看到 fence 重复解析 bug）

**代码对比**：
```rust
// AbacusCode (md_stream.rs)
// 自己维护 pending/committed 状态
pub struct StreamingMd {
    pending: String,
    committed: String,
}
```

```ts
// OpenCode (opentui)
// 一行调 OpenTUI 即可
const renderable = new MarkdownRenderable(ctx, {
    content: "",
    streaming: true,
    internalBlockMode: "top-level",
})
renderable.content = "## Hello\n\nThis is **markdown**."
```

### 2.2 Code syntax 高亮

**OpenCode**：
- tree-sitter 完整支持（rust/python/typescript/...）
- `streaming: true` 在 chunk 抵达时增量解析
- 颜色来自 theme 的 `syntaxKeyword/syntaxString/syntaxComment` 等

**AbacusCode**：
- `pkg/crates/abacus-cli/src/tui/syntax.rs` 简单关键字匹配
- 没有 streaming 模式（每次重渲染都重新高亮）
- 颜色有限

**影响**：AbacusCode 用户看到的代码块是纯文本或简单关键字高亮，OpenCode 用户看到的是 VSCode 级别的语法高亮。

### 2.3 输入编辑器（这是你没意识到的差异）

**OpenCode**：
```ts
this.composer = new TextareaRenderable(this.renderer, {
  width: "100%",
  minHeight: 3,
  flexGrow: 1,
  wrapMode: "word",
  showCursor: true,
  placeholder: "Type message or /command... (Enter = new line, Ctrl+Enter = send)",
  placeholderColor: ...,
  textColor: ...,
  focusedTextColor: ...,
  backgroundColor: ...,
  focusedBackgroundColor: ...,
  cursorColor: ...,
  onSubmit: this.handleComposerSubmit,
  keyBindings: [
    { name: "return", ctrl: true, action: "submit" },
    { name: "linefeed", ctrl: true, action: "submit" },
  ],
})
```

特性：
- ✅ 多行编辑（Enter = newline, Ctrl+Enter = submit）
- ✅ 自带 placeholder
- ✅ 自带 cursor 闪烁 + 颜色
- ✅ 自带 focused/unfocused 颜色切换
- ✅ 自带 keyBindings（不与全局冲突）
- ✅ 自带 wrap（字符级 / 词级）
- ✅ 自带 submit 回调

**AbacusCode**（event/mod.rs, state/mod.rs）：
```rust
// 自己维护
pub input: String,
pub cursor_pos: usize,
pub cursor_line: usize,
pub cursor_col: usize,
pub input_state: InputState,  // Ready / Thinking / Executing / Completing / ...

// handle_input_key 里大量 if-else 处理光标移动
fn handle_input_key(state: &mut AppState, code: KeyCode, mods: KeyModifiers) {
    match code {
        KeyCode::Char(c) => { /* handle CJK, paste, etc */ }
        KeyCode::Backspace => { /* CJK-aware backspace */ }
        KeyCode::Left => { /* move cursor */ }
        KeyCode::Right => { ... }
        // ... 几百行
    }
}
```

**缺失的能力**（用户能感知的）：
1. ❌ **多行输入**：AbacusCode 是单行 + 软 wrap（基于 char_width 估算），OpenCode 是原生多行 Textarea
2. ❌ **Placeholder 不闪烁**：OpenCode placeholder 是真正 hint，AbacusCode 只是输入前静态显示
3. ❌ **focused 颜色变化**：OpenCode 切换时背景色变化，AbacusCode 无
4. ❌ **真实 cursor 颜色控制**：OpenCode 自带，AbacusCode 用默认系统 cursor

### 2.4 命令面板（Cmd+K 风格）

**OpenCode**：未在源码中看到具体实现，但从 keymap-demo（80KB）推断有完整命令面板 + fuzzy search

**AbacusCode**：
- `/command` 拦截本地命令（state/mod.rs:2280）
- `picker` 用于选模型/agent，但 **没有 fuzzy search**
- 没有命令面板（ctrl+p/cmd+k 风格）

### 2.5 多 session（OpenCode 核心特性）

**OpenCode**：
- `Multi-session: Start multiple agents in parallel on the same project`（README.feature）
- 有 session list、session 切换、session 状态指示
- session_id 在 tui.ts 里作为参数传递

**AbacusCode**：
- `/new` 开新 session（slash_commands.rs:972）
- 没有 session list UI，没有 session 切换快捷键
- 没有并行 session

**影响**：OpenCode 用户可以在一个终端里跑 3 个 agent 同时工作，AbacusCode 用户必须串行。

### 2.6 主题系统（视觉一致性的根本）

**OpenCode**（来自 smoke-theme.json）：
- 50+ 个语义化色名
- 分 dark/light 两套
- 引用方式：defs.nord10 → theme.primary.dark
- 用户可以自定义主题文件

**AbacusCode**（pkg/crates/abacus-ui-kit/src/theme.rs）：
- 色板数量少（应该 < 20）
- 主要是 panel/border/text 等基础色
- 没有完整的 markdown/syntax/diff 色板

---

## 3. OpenTUI 关键 API 对照表（Rust ratatui 缺失的能力）

按 OpenCode 实际使用频率排序：

| OpenTUI API | 用途 | AbacusCode 状态 |
|---|---|---|
| `BoxRenderable` with `flexDirection/gap/padding` | 弹性布局 | ❌ 手算 Constraint |
| `TextareaRenderable` | 多行编辑器 | ❌ 手写 cursor 状态机 |
| `MarkdownRenderable` | markdown 流式渲染 | ⚠️ 自己实现，不完整 |
| `CodeRenderable` | 代码语法高亮 | ⚠️ 简单关键字匹配 |
| `TextTableRenderable` | 表格 | ❌ 自己实现 |
| `TextRenderable` | 单行/多行文本 | ✅ 类似 |
| `createScrollbackSurface` | 滚动历史区 | ❌ 自己实现 CardStream |
| `surface.commitRows()` | 行级 commit | ❌ 整体 push |
| `surface.render()` | 局部重绘 | ❌ 全量重绘 |
| `renderer.start()` | 渲染循环 | ✅ 类似 |
| `renderer.useMouse` | 鼠标支持 | ⚠️ 部分 |
| `screenMode: split-footer` | 分屏 | ❌ 不存在 |
| `renderer.footerHeight` | 底部栏高度 | ❌ 手算 |
| `renderer.writeToScrollback(write)` | 推送到 scrollback | ❌ 类似 |
| `CliRenderEvents.THEME_MODE` | 主题切换事件 | ❌ 不存在 |
| `CliRenderEvents.RESIZE` | resize 事件 | ⚠️ 轮询 |
| `setupCommonDemoKeys` | 标准快捷键集合 | ❌ 自己注册 |

---

## 4. 改造优先级（重新评估，按 ROI）

### P0：低成本视觉优化（今天可做）

1. **header 间呼吸感**（render.rs:164 加 1-2 行 gap）
2. **任务头 compact 化**（top bar 增加 tokens + context% + cost 行）
3. **快捷键提示栏**（input bar 旁边增加 dynamic hint bar）
4. **focused 输入框背景色变化**（state.theme 应用）
5. **placeholder 视觉强化**（现有 placeholder 加 placeholder color + cursor 提示）

### P1：能力补齐（中等成本，1-2 周）

1. **多行输入编辑器**
   - 引入 `tui-textarea` crate（ratatui 生态最成熟的 Textarea）
   - 替换 `state.input + cursor_pos/cursor_line/cursor_col`
   - 用户感受：从"软 wrap 单行" → "真实多行编辑器"
   - 估算：~500 行代码

2. **Scrollback buffer 抽象**
   - 把 `state.cards: CardStream` 重构为 `Scrollback` trait
   - 把渲染逻辑从 `render_cards` 拆出，独立滚动 buffer 管理
   - 引入 `commit_rows(start, end)` API
   - 用户感受：长会话（> 100 张卡）不再卡顿
   - 估算：~1500 行代码

3. **命令面板（Cmd+K）**
   - 全局 ctrl+p 打开
   - fuzzy search 全部 slash commands + 模型 + agent
   - 用户感受：无需记 / 命令，全键盘可达
   - 估算：~800 行代码

### P2：架构升级（高投入，需讨论）

1. **声明式 UI 层**
   - 类似 SolidJS 的响应式 diff 引擎
   - 现有 render.rs 重构为响应式
   - 估算：~3000 行代码

2. **Tree-sitter 集成**
   - 代码高亮质量提升
   - 引入 `tree-sitter` crate
   - 估算：~2000 行代码

3. **多 session 并行**
   - OpenCode 的核心特性
   - 架构大改（state.per_session 而不是单 state）
   - 估算：~5000 行代码

### P3：抄 OpenTUI 的轮子（最高投入，回报最不确定）

1. **Zig + 自研 TUI 框架**
   - 长期投资，但 ratatui 生态成熟，重造框架 ROI 不高
   - 不推荐（除非 ratatui 真的成为瓶颈）

---

## 5. 具体改造建议（针对 AbacusCode 当前截图）

你截图里看到的"重复响应 bug"和"无呼吸感"是**视觉问题**，但视觉下面藏着的**架构问题**是：

| 用户看到的 | 实际根因 |
|---|---|
| 两条 "你好。" 重复 | 流式 LlmCard 与非流式 LlmCard 同时存在（dedup 漏了） |
| header 紧贴 | render.rs 没有 gap |
| 无快捷键提示 | 缺乏 dynamic hint bar |
| 无 placeholder 闪烁 | input bar 实现简单 |
| 无 focused 颜色 | theme 未应用 |
| 无 markdown 流式高亮 | md_stream.rs 是简化版 |

**我的建议**：
1. **短期**（今天）：修呼吸感 + 修 dedup bug（已诊断）
2. **中期**（下周）：引入 `tui-textarea`，提升输入体验
3. **长期**（下月）：重构 `state.cards` 为 scrollback buffer 模式

---

## 6. 学习资源

- OpenCode 主仓库：https://github.com/anomalyco/opencode
- OpenTUI 文档：https://opentui.com/docs/getting-started
- OpenTUI 仓库：https://github.com/anomalyco/opentui
- OpenCode 官网：https://opencode.ai
- OpenCode `.opencode/plugins/tui-smoke.tsx`（31KB）—— 完整 TUI plugin 范例
- OpenTUI `split-mode-demo.ts`（36KB）—— OpenCode TUI 核心模式
- OpenTUI `split-footer-streaming-demo.ts`（22KB）—— 流式 markdown/code 模式
- OpenTUI `keymap-demo.ts`（80KB）—— 快捷键系统范例
- OpenCode `.opencode/plugins/smoke-theme.json`（50+ 色名）—— 主题范例