# Phase 14 Cleanup — V40 → V42-B Field Audit

**Source:** Originally embedded as `pub mod phase_14_audit` in
`crates/abacus-cli/src/tui/state/session_migrate.rs` (Reviewer C finding C-5).
Moved here on 2026-06-07 so it does not pollute compile time or mislead readers.

## 设计原则 (底层稳定)

- 本 Phase **不实际删除**任何旧字段
- V40 渲染路径 (`render_messages_in_card`) 仍使用这些字段
- 删除前需先切换 `modes/common.rs` 调 `render_cards` 而非 `render_messages_in_card`
- 然后逐字段删除 + 验证编译 + 跑测试

## 待清理字段清单 (V40 遗留, V42-B 用 CardStream 替代)

### 1. 消息存储

- `AppState::messages: VecDeque<Message>` (state/mod.rs:1058)
  - V42-B 替代: `AppState::cards: CardStream`
  - 引用点: `messages.rs` (1439 行), `state/mod.rs` (多处), `event/mod.rs` (drag 选中)
  - 清理前置: switch `modes/common.rs` to `render_cards`
- `AppState::trace_events: Vec<TraceEvent>` (state/mod.rs:1146)
  - V42-B 替代: `AbacusCard.events: Vec<TraceEvent>` (内置)
  - 引用点: `messages.rs` `build_message_lines`, `block_detail.rs`
  - 清理前置: `AbacusCard` 完全承载 trace events

### 2. 流式累积 (6 字段)

- `AppState::streaming_text: String` (state/mod.rs:1359)
  - V42-B 替代: `LlmCard.reply_text: String`
- `AppState::streaming_thinking: String` (state/mod.rs:1361)
  - V42-B 替代: `LlmCard.thinking: Option<String>`
- `AppState::streaming_text_started: bool` (state/mod.rs:1367)
- `AppState::streaming_thinking_started: bool` (state/mod.rs:1369)
- `AppState::streaming_tools: Vec<...>` (state/mod.rs:1382)
  - V42-B 替代: `AbacusCard.events`
- `AppState::is_streaming: bool` (state/mod.rs:1357)
  - V42-B 替代: `CardStream.active_id().is_some()`

### 3. 渲染缓存 (5 字段)

- `AppState::cached_lines: RefCell<Vec<Line<'static>>>` (state/mod.rs ~1065)
- `AppState::cached_base_lines: RefCell<Vec<Line<'static>>>`
- `AppState::cached_msg_rows: RefCell<Vec<u16>>`
- `AppState::rendered_lines_dirty: Cell<bool>`
- `AppState::message_trace_row_map: RefCell<Vec<(u16, usize, usize)>>`
  - V42-B 替代: `ScrollLayout.item_areas` (在 cards 内部)
  - 引用点: `messages.rs` (5 处), `event/mod.rs` (drag, click)

### 4. Diff 缓存

- `AppState::streaming_diff_cache: RefCell<HashMap<u64, Vec<Line<'static>>>>` (state/mod.rs:1206)
  - 引用点: `block_detail.rs` `render_single_trace_event`
  - 清理前置: `AbacusCard` `EditDiff` 重写完成后

## 清理步骤 (后续 Phase)

1. 切换 `modes/common.rs:84,98` 调用到 `components::render_cards`
2. 跑全测试 + 手动 smoke test, 确认 CardStream 渲染与 V40 视觉一致
3. 删除 `messages.rs` (1439 行) 整个文件
4. 删除 `msg_geometry.rs` (`estimate_msg_rows` 已被 `card_total_height` 替代)
5. 删除上述 10+ 字段, 修复编译错误
6. 跑全测试, 确认无 regression

## 估算

- 删除 1439 行 `messages.rs` + ~200 行 `msg_geometry.rs` = **~1640 行代码**
- 删除 10+ 字段定义 + 修复 30+ 处引用 = **~200 行变动**
- 简化 6 个流式累积字段的状态管理, 集中到 CardStream

## 风险评估

- **高风险**: `messages.rs` 是消息流核心, 误删会导致整个 TUI 渲染失败
- **缓解**: 实际清理在 Phase 14.1+ 分批做, 每批删除后跑全测试
- **测试**: `abacus-ui-kit` 90 测试 + 完整集成测试 1287 测试作为 safety net
