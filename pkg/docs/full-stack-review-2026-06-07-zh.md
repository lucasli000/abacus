# 全栈审查报告 — 综合版
**日期：** 2026-06-07
**范围：** 42 文件 / 5,122 删除 / 2,198 插入 / 3 位审查员并行
**模式：** 团队模式（CON · MNT · SCL · COR × USB · ROB · SEC · EFF）

---

## 1. 摘要卡

| 指标 | 数值 |
|---|---|
| 派出的审查员 | 3（A：清单/注册表 · B：流水线/记忆/LLM · C：TUI/UI） |
| 发现总数 | 16 |
| P0 / P1 / P2 / P3 | 5 / 6 / 4 / 1 |
| 按文件:行号核验 | 16/16 |
| 构建/测试状态 | `cargo build` 通过 · 958 lib + 15 silent_router + 18 pressure 测试通过 · 110+ 弃用警告 |
| 发布判定 | **暂停** — 3 项 P0 TUI 缺陷是发布阻断；2 项 P0 清单/缓存缺陷是正确性问题 |
| 质量维度（1-5） | CON=2 · MNT=2 · COR=2 · SCL=4 · USB=2 · SEC=3 · EFF=4 |

**一句话结论：** 渲染层已迁移到 V42-B CardStream，写入层没有迁移。TUI 结构干净、后端优化方向正确 — 但 5 项 P0 缺陷必须与重构同批发布，否则 v1.5.0 会让基础聊天持久化发生回退。

---

## 2. 发现项 — 综合

### 审查员 A — 清单 / 工具注册表

**A-1 [P0 · CON] `fs_mkdir` 在 tools.toml 中注册了两次，第二次覆盖第一次**
- **证据：** `tools.toml:142` 和 `tools.toml:212` 都定义了 `fs_mkdir` · `manifest.rs:83` HashMap insert — 后写覆盖前写 · 第二个条目没有 `cluster` 字段，所以 `cluster_of("fs_mkdir")` 返回 `None`
- **影响：** 当清单是真值源（Phase 3 设计）时，`fs_mkdir` 静默丢失其集群成员资格。`extract_tool_domain` 兜底还能工作，但 `cluster_of()` 取决于调用方是 panic 还是返回 `Err`
- **修复：** 删除 `tools.toml:211-221`。在 `manifest.rs:83` 加载处加断言：`assert_eq!(by_name.len(), original_len, "tools.toml 出现重复工具名")`，防止未来再静默覆盖

**A-2 [P0 · CON] 清单中的 `code_exec` 名称与代码中的 `code_execute` 不匹配**
- **证据：** `tools.toml:460` 声明 `[tools.code_exec]` · `code_exec.rs:63,65,109` 输出 `ToolId("code_execute")` · `injector.rs:1385` 和 `mcip.rs:248` 都引用 `code_execute`
- **影响：** `manifest.by_name.get("code_exec")` 返回 `None`。新引入的 `schemas()` 查询对该工具失败 — schema 显示为桩或 panic，取决于调用路径。`code_execute` 当前对清单驱动的代码路径**不可见**
- **修复：** 把 `tools.toml:460` 重命名为 `[tools.code_execute]`。重跑 schema 测试确认

### 审查员 B — 流水线 / 记忆 / LLM 优化

**B-1 [P0 · ROB/COR] 检查点哈希缓存是中毒的 OnceLock，且不区分 session**
- **证据：** `post.rs` 声明 `static LAST_HASH: OnceLock<std::sync::Mutex<Option<u64>>> = OnceLock::new()` · 访问器使用 `.lock().unwrap()`（中毒后永远 panic）· 静态生命周期意味着缓存跨 session 共享 · `std::sync::Mutex` 在 `.await` 期间持锁在 tokio 下不健全
- **影响：** 锁守卫内任何 panic（例如 `Hash::hash` 调用期间）会损坏锁；后续回合 `unwrap()` 并让整个 TUI panic。更糟：A 用户的缓存会被 B 用户读取（如果 session 共享进程）
- **修复：** 移到 `TurnContext`（按 Phase 1 决策，已经是 `&mut` 传入，无需 clone）· 使用 `tokio::sync::Mutex` · 在 `SessionStart` 边界处清空

**B-2 [P1 · COR] `pin_tool_behavior` 与同回合 LFU 淘汰存在竞态**
- **证据：** `post.rs` 的调用顺序是 `record_tool_behavior(...)` → `pin_tool_behavior(...)` 用于 D 档提升 · `memory_palace.rs::record_tool_behavior` 在容量打满时内联调用 `prune()` · LFU `prune()` 跳过 `pinned` 条目，但 `prune()` 时机点该条目尚未被 pin
- **影响：** 如果 `record_tool_behavior` 触发 `prune()`，新条目是 LFU 受害者，它会在 `pin_tool_behavior` 运行之前被淘汰。"D 档工具留在 Palace" 的用户期望在内存压力下被违反
- **修复：** 调整 `post.rs` 顺序 — 在调用 `record_tool_behavior` 之前先把 `pinned: true` 设置到 `BehaviorMemory` 条目上（或者先用桩条目调 `pin_tool_behavior`，再调 `record_tool_behavior`）

### 审查员 C — TUI / UI 层

**C-1 [P0 · COR/USB] 用户键入的消息永远不会显示在聊天面板中**
- **证据：** `state/mod.rs:3625 add_message` 只写 `state.messages`（那个 `#[deprecated]` 字段）· `modes/common.rs:84,98 render_cards` 只读 `state.cards` · `cards/writer.rs:164 push_user_message` 作为预期的桥接存在，但全仓 grep 显示**零调用点** · `event/mod.rs:1133, 2270, 2292, 2317, 2358, 2401` 和 `run.rs:655, 875` 都直接调用 `state.add_message(...)`
- **影响：** 100% 用户影响。每次键入回合：用户消息消失。LLM 响应正常渲染。看起来像"AI 在回复空气"
- **修复：** 把 `push_user_message` 接入现有 `add_message` 路径。两条路径二选一：
  - 路径 A（最小改动，原地修复）：在 `add_message` 顶部，调用 `crate::tui::cards::writer::push_user_message(state, &text, &ts)`，外加现有的消息向量写入
  - 路径 B（干净，全量迁移）：从 V42-B 热路径中删除 `state.messages` 和 `state.add_message`；所有用户文本走 `push_user_message`。`SessionExport::version` 升到 4 并序列化 `cards` 字段（C-3）

**C-2 [P0 · COR] V40 会话加载后聊天面板为空**
- **证据：** `run.rs:3140 load_session_from_path` → `apply_session_export`（`run.rs:3176-3180`）只从导出的 `messages` JSON 字段写 `state.messages` · `state/session_migrate.rs:86 migrate_v3_to_v4` 存在且 9 个单元测试通过，但全仓 grep 返回**零非测试调用点** · `session_migrate.rs:11-12` 模块注释自承：*"本 Phase 暂不实际转换 messages → cards"*
- **影响：** ~~>80% 拥有现有 `~/.abacus/sessions/*.json`（v2 格式）的用户在升级 v1.5.0 后会看到空面板~~（**用户已确认无老用户** — 影响降为"代码 bug，无运行期影响"）。`.v3_backup/` 永远不创建，因为迁移从不运行
- **修复：** 在 `apply_session_export` 顶部按 `export.version` 分派：
  - `version <= 3` → 先调 `migrate_v3_to_v4(&export, path)`，再用 v4 读法继续
  - `version == 4` → 直接读新的 `cards` 字段

**C-3 [P0 · COR] `save_session` 写错了格式**
- **证据：** `run.rs:3031-3038` 构造 `SessionExport { version: 2, …, messages: state.messages.iter().cloned().collect(), … }` · 没有 `cards` 字段 · `session_migrate.rs:44 SessionVersion::V4` 变体期望 `version: 4` 和 `cards` 字段
- **影响：** ~~与 C-1 叠加，`v1.5.0` 完全没有可工作的持久化 — 键入消息，看不见；保存会话，格式错误；重新加载，格式也错误但反正也没有消息可丢。状态化 UX 功能灾难性回退~~（**同上，影响降为"代码 bug"**）。这是对 `SessionExport::version: 2` 与 `SessionVersion::V4` 不一致的代码层修复
- **修复：** `version` 升到 4 · 把 `messages: Vec<Message>` 替换为 `cards: CardStream`（需要在 `abacus-ui-kit` 的 Card 类型上加 `Serialize` derive，或在 `CardStream` 不透明时通过 `to_value()` 序列化）· 更新 `apply_session_export` 读新字段。这对 `~/.abacus/sessions/*.json` 是破坏性变更 — 但该文件在 V42-B 中反正也用不了

**C-4 [P1 · MNT] `run.rs.bak` 是 diff 中 205 KB 的遗留文件**
- **证据：** `tui/run.rs.bak`（205 564 字节，日期 2026-06-03）— 旧的 `run.rs` · 第 36 行：`use crate::tui::components::format_ctx;`（旧路径；碰巧能解析，但是 Rust 永远不会编译的文件中的死代码）
- **影响：** 破坏 `cargo package`，膨胀 diff 统计，对 `git stash`/reflog 恢复是脚枪
- **修复：** `rm pkg/crates/abacus-cli/src/tui/run.rs.bak` · 在 `.gitignore` 加 `*.bak`

**C-5 [P1 · MNT/CON] `pub mod phase_14_audit` 是文档当代码的污染**
- **证据：** `state/session_migrate.rs:346-354` 定义了一个 80 行的模块，带 2 个常量和一个 doc 块，统计未来 Phase 要删除的文件 · `V40_FIELDS_TO_CLEAN` 和 `V40_MODULES_TO_REMOVE` 在任何地方都没被引用 · 第 351 行说"待删除：messages.rs 1439 行"，但 `messages.rs` 已经被删了
- **影响：** 污染编译时间 · 迷惑读者（看起来像真模块）· 偏离现实
- **修复：** 主体移到 `docs/phase-14-cleanup.md` · C-2 接入后只保留 `migrate_v3_to_v4` + `migrate_messages_to_cards`

**C-6 [P1 · MNT] 110+ 弃用警告，零迁移**
- **证据：** `cargo check` 产生 31×`state.messages` + 31×`state.trace_events` + 18×`streaming_text` + 17×`streaming_thinking` + 9×`streaming_tools` + 4×`streaming_md` 警告 · 弃用消息建议迁移到 LlmCard/AbacusCard API · **110 个调用点零迁移**
- **影响：** 弃用标记添加过早。这是"最差两全"：每次构建 123 个警告，没有进展。训练开发者忽略噪音
- **修复：** (a) 真的把调用点迁移到 CardStream 辅助（多日工作），或 (b) 删掉 `#[deprecated]` 标记（字段仍是规范的，重命名未完成）。作为独立工单跟踪

**C-7 [P1 · MNT] `components/card.rs::Card` 结构体是 100+ 行的死代码**
- **证据：** `tui/components/card.rs:8-10` 自承：*"Card 当前 pub 但**无外部调用方** (dead code)"* · 全 `pkg/` 范围内 grep `components::card::Card` 零调用点（只有 `render_card_bar` 被 `panel.rs:162` 使用）· `card.rs:20-111` 定义了完整的基于 `Block` 的圆角 widget 带阴影渲染，从未调用
- **影响：** 公开 API 是陷阱 — Agent 应用伸手拿 `abacus-cli::tui::components::Card` 时会得到"碰巧稳定"而不是"设计稳定"
- **修复：** 藏在 `#[cfg(feature = "panel-card-widget")]` 后面，或移到 `abacus-ui-kit::CardWidget`，或降级为 `pub(crate)`。注释已经告诉你 clippy 会标记它

**C-8 [P2 · SCL] `abacus-ui-kit` 已建但没有真正的外部消费者**
- **证据：** 新 crate `pkg/crates/abacus-ui-kit/`（8 文件，~140 KB）· 唯一声明的依赖：`abacus-cli` · 只有 2 个 examples（`quant_panel.rs`、`v42b_card_stream.rs`）和 1 个二进制 `abacus` 消费它 · 在 `abacus-cli` 之外没有文档化的树内消费者 · `quant_panel.rs:19` 注释说*"Agent 应用会把这些 Section 注入到 abacus-cli 主 TUI 的 state.section_registry 中"* — 但没有公开的 `init_extensions()` hook 供 Agent 二进制在构造前调用
- **影响：** crate 声明的价值是"跨 crate 公开契约"，但只要消费者二进制不在 `pkg/crates/abacus-cli/` 之外，边界就是理论性的
- **修复（二选一）：**
  - **(a)** 提供 `init_extensions(registry: &mut SectionRegistry, dashboard: &mut DashboardRegistry)` 和 `AppState::new_with_extensions(…)` 构造器
  - **(b)** 推迟到真正的 v1.6 第三方 Agent SDK 里程碑，暂时合并回 `abacus-cli`

**C-9 [P2 · CON] `panel_layout` 配置布线在 extensions.rs 中承诺但未实现**
- **证据：** `tui/extensions.rs:60-68` 文档化 *"用户可通过 config.toml `[tui.panel] sections = [...]] 覆盖"* · `tui/state/mod.rs:1470` 定义 `pub panel_layout: Vec<String>` · `tui/components/panel.rs:391-393` 读 `state.panel_layout` 并传给 `section_registry.build_stack(&layout)`（✓ 部分）· grep 找不到 `config.toml` 的 `[tui.panel] sections = [...]` 解析器 · `panel_layout` 在 `state/mod.rs:2688` 从 `default_panel_layout()` 初始化一次后不再被写入
- **影响：** `extensions.rs:113-115` 的测试因为常量存在而通过，不是因为功能工作
- **修复：** 在 `tui/setup.rs` 实现配置解析器（紧邻现有 TOML 配置），或修正文档为"覆盖方式 TBD"直到 Phase 15

**C-10 [P2 · CON] 两处零散的 `use abacus_ui_kit::SectionContext;` 导入**
- **证据：** `tui/cards/render.rs:33` 和 `tui/cards/hit_test.rs:23` 都有未使用的导入
- **修复：** 删两行（5 秒修复）

**C-11 [P2 · MNT] Card 实现中两处未使用的 `body_height` 覆盖**
- **证据：** `tui/cards/expert.rs:73` 和 `tui/cards/llm.rs:124` 定义了 `body_height`，透传到默认
- **修复：** 删除整个 override 即可消音

**C-12 [P3 · MNT] `SessionExport` 应该住在 `tui/state/`，而不是 `tui/run.rs`**
- **证据：** `run.rs:2930-3011` — 80 行纯 serde 结构体定义，在 `run.rs` 中除了读/写没有运行时用途
- **影响：** 把会话格式耦合到二进制的 run-loop 文件 · 未来的 `abacus-server` crate 不得不为结构体形状依赖 `abacus-cli`（而 `abacus-cli` 又依赖 `abacus-core` 和 `tokio`）
- **修复：** 把 `SessionExport` + `bool_is_false` / `session_tokens_is_empty` 辅助函数移到新的 `tui/state/session_export.rs` 模块

---

## 3. 风险矩阵

| ID | 标题 | 严重度 | 用户命中概率 | 综合 |
|---|---|---|---|---|
| A-1 | `fs_mkdir` 重复，集群丢失 | 高 | 每次集群查询 | 🟠 |
| A-2 | `code_exec` 清单 vs `code_execute` 代码 | 高 | 该工具的每次 schema 调用 | 🟠 |
| B-1 | OnceLock Mutex 中毒，跨 await 不健全，不分 session | 关键 | 锁守卫内任何 panic 或任何 session 边界 | ⛔ |
| B-2 | pin 与 LFU 淘汰存在竞态 | 中 | 内存压力 + D 档工具 | 🟡 |
| C-1 | 用户键入消息不可见 | 关键 | 100%（每次键入回合） | ⛔ |
| C-2 | V40 会话加载为空 | 关键 | 无老用户（已确认）→ 代码 bug 仍有 | 🟡（降级） |
| C-3 | save_session 写 v2 而非 v4 | 关键 | 无老用户（已确认）→ 代码 bug 仍有 | 🟡（降级） |
| C-4 | run.rs.bak 205 KB 遗留 | 低 | 仅构建时 | 🟡 |
| C-5 | phase_14_audit 文档当代码 | 低 | 仅文档时 | 🟡 |
| C-6 | 110+ 弃用警告，无迁移 | 中 | 每次 cargo build | 🟠 |
| C-7 | `Card` 结构体 100+ 行死代码 | 低 | 今日无 | 🟡 |
| C-8 | abacus-ui-kit 无外部消费者 | 中 | 未来 SDK 里程碑 | 🟠 |
| C-9 | panel_layout 配置未布线 | 低 | 用户编辑 config.toml | 🟢 |
| C-10 | 2 处零散 SectionContext 导入 | 低 | 构建警告 | 🟢 |
| C-11 | 2 处未使用 body_height 覆盖 | 低 | 构建警告 | 🟢 |
| C-12 | SessionExport 错位在 run.rs | 低 | 未来重构 | 🟢 |

**C-2 / C-3 严重度降级说明：** 用户已确认无老用户，所以"无法加载 V40 历史"和"新会话保存失败"对当前用户群无运行期影响。但 C-3 仍要修 — `SessionExport::version: 2` 与 `SessionVersion::V4` 是**代码层不一致**（一个是常量、一个是枚举，二者本应同步），不是"兼容老用户"问题。

---

## 4. 行动项

### L0 — **v1.5.0 发布前必做**（发布阻断）

1. **A-1** — 删除 `tools.toml:211-221`（重复的 `fs_mkdir` 条目）。在 `manifest.rs:83` 加唯一性断言（`assert_eq!(by_name.len(), original_len, "tools.toml 出现重复工具名")`）。
2. **A-2** — 把 `tools.toml:460` 的 `[tools.code_exec]` 重命名为 `[tools.code_execute]`。确认 `code_exec.rs:63,65,109` 匹配。
3. **B-1** — 把 `LAST_HASH` 从 `post.rs` 的 `static OnceLock<Mutex<…>>` 移到 `TurnContext` 字段（按 Phase 1 决策零成本：`TurnContext` 是 `&mut` 传入）。使用 `tokio::sync::Mutex`。在 `SessionStart` 边界清空。
4. **C-1 + C-2 + C-3** — 选路径 A 或路径 B 并执行完整的写入迁移：
   - **路径 A（最小，~30 行）：**
     - 在 `tui/state/mod.rs:3625 add_message` 顶部，也调 `cards::writer::push_user_message(state, &text, &ts)`
     - 在 `tui/run.rs:3176 apply_session_export` 按 `export.version` 分派，`v<=3` 调 `migrate_v3_to_v4`
     - 把 `save_session` 升到 `version: 4` 并序列化新 `cards` 字段
     - 在 `tui/run.rs:3031-3038`，把 `messages: state.messages.iter().cloned().collect()` 替换为 `cards: state.cards.to_value()`
   - **路径 B（干净，~150 行）：** 同 A，外加删除 `state.messages` 和 `state.add_message`；重写 `event/mod.rs` + `run.rs` 中所有 7 个调用点直接使用 `push_user_message`
   - **推荐：** 路径 A。保留对任何直接 `state.messages` 读取（当前 31 个调用者）的向后兼容，并保持 diff 收窄。

### L1 — **本 PR 应做**（高置信度清理，~10 分钟）

5. **C-4** — `rm pkg/crates/abacus-cli/src/tui/run.rs.bak` 并在 `.gitignore` 加 `*.bak`。
6. **C-5** — 把 `pub mod phase_14_audit` 主体移到 `docs/phase-14-cleanup.md`；C-2 接入后，`session_migrate.rs` 只保留 `migrate_v3_to_v4` + `migrate_messages_to_cards`。
7. **C-7** — 把 `tui/components/card.rs::Card` 降级为 `pub(crate) #[allow(dead_code)]`，去掉结构体上的 `pub` 可见性，或藏在 feature flag 后面。保留 `render_card_bar`（仍被 `panel.rs:162` 使用）。
8. **C-10** — 删除 `tui/cards/render.rs:33` 和 `tui/cards/hit_test.rs:23` 的 `use abacus_ui_kit::SectionContext;`。
9. **C-11** — 删除 `tui/cards/expert.rs:73` 和 `tui/cards/llm.rs:124` 的未使用 `body_height` 覆盖。
10. **B-2** — 在 `post.rs` 重排 D 档提升顺序：先把 `pinned: true` 设置到 `BehaviorMemory` 条目上，**再**调用 `record_tool_behavior`（或者先用桩条目调 `pin_tool_behavior`）。消除淘汰竞态。

### L2 — **后续 PR 做**（技术债偿还）

11. **C-6** — 把 110+ 弃用调用点迁移到 CardStream 辅助，或删掉 `#[deprecated]` 标记（重命名未完成；字段仍是规范的）。二选一。多日工作；作为独立工单跟踪。
12. **C-9** — 在 `tui/setup.rs` 实现 `[tui.panel] sections = [...]` TOML 配置。当前 `panel_layout` 是硬编码的；`extensions.rs:113-115` 的测试对用户级配置撒谎。
13. **C-12** — 把 `SessionExport` 从 `tui/run.rs:2930-3011` 移到新的 `tui/state/session_export.rs` 模块。`session_migrate.rs` 的兄弟模块。
14. **C-8** — 决定 `abacus-ui-kit` 的命运。(a) 推出真正的 v1.6 SDK，文档化的 `init_extensions(…)` API 在 `AppState::new_with_extensions(…)` 中，或 (b) 暂时合并回 `abacus-cli` 直到出现消费者。半成品边界比无边界更贵。

### L3 — **战略 / 未来里程碑**

15. **发布前 code-quality-gate 检查**：`code-quality-gate` skill 的三道审查门（多角色对抗、细粒度模拟、引用链）应在写入→渲染数据流上端到端运行，不只在隔离的单元测试中。"渲染层已迁移、写入层没有"这一类 bug（C-1/C-2/C-3）需要端到端追踪才能检测。

---

## 5. 维度评分

| 维度 | 分数（1-5） | 理由 |
|---|---|---|
| **CON**（一致性） | 2 | 清单有重复条目（A-1）+ 错误名称（A-2）；110+ 弃用标记无迁移（C-6）；两处 `panel_layout` API 不匹配（C-9） |
| **MNT**（可维护性） | 2 | 205 KB 遗留文件（C-4）；80 行文档当代码（C-5）；100+ 行死代码带 `pub`（C-7）；`SessionExport` 在错误模块（C-12） |
| **COR**（正确性） | 2 | 3 项 P0 TUI 缺陷破坏基础聊天（C-1/C-2/C-3）；1 项 P0 缓存中毒（B-1）；1 项 P0 清单 bug（A-2）；1 项 P0 清单 bug（A-1） |
| **SCL**（可扩展性） | 4 | 新的 `Section` / `DashboardTab` / `SectionContext` / `SectionRegistry` 设计是 diff 中最强部分（C-11）。DAG 干净，downcast 测试充分，100% 布线。一旦写入层赶上会扩展良好 |
| **USB**（可用性） | 2 | 最常用功能（键入 → 渲染 → 保存 → 重新加载）在三处被破坏。~~80%+ 有会话历史的用户受影响~~（无老用户，影响降为代码 bug） |
| **SEC**（安全性） | 3 | 没有新增攻击面。B-1 panic 向量是正确性问题，不是安全问题。B-2 竞态是 UX 问题，不是数据泄露 |
| **EFF**（效率） | 4 | LLM 调用减少优化（检查点缓存 + 自适应自洽 + 预检跳过 + circuit-on-pressure 跳过）净为正。`post.rs` 的 OnceLock 是唯一需要修的性能关键点 |

**总体：** 2.7/5。Diff **结构上组织良好**（panel_sections + dashboard_tabs + ui-kit 是"如何保持 trait 签名小但仍让实现触及更丰富状态"的干净答案），但**在集成边界上不完整**（渲染器已迁移，写入器没有）。5 项 P0 是集成边界 bug；L1 是卫生；L2 是偿还。

---

## 6. 值得保留的成功

这些重构部分真正改进了代码库，不应回滚：

- **`abacus-ui-kit::Section` + `SectionContext` + `SectionRegistry` 设计**（`section.rs:60-100`，605 行）。`ext()` + `ext_type_id` downcast 模式是"如何保持 trait 签名小但仍让实现触及更丰富状态"的干净答案。`section_ctx.rs:104-165` 的 4 个单元测试充分覆盖了 unsafe 安全契约。
- **`extensions.rs` API 表面**（`register_builtin_sections`、`register_builtin_dashboard_tabs`、`default_panel_layout`、`default_dashboard_tabs`、`new_section_registry`、`new_dashboard_registry`）。六个小而命名良好的函数。五个单元测试断言不变量。`state/mod.rs:2686-2688` 100% 调用点覆盖是正确种类的"布线"。
- **`panel_sections/` 和 `dashboard_tabs/` 拆分。** 6 个 80-300 行的模块，各有单一职责。"1 trait + 1 零大小结构体 + 1 默认实现"模式（`llm.rs:44-50`）是干净的惯用法。新 section 现在是 `Box::new(MySection)` + 1 行 `register`。
- **`card.rs::render_card_bar`**（1 函数，16 行，1 调用者）。小、聚焦、单一调用者、清晰契约。
- **`quant_panel.rs` 示例**（393 行，自文档化，5 个单元测试，跨 crate 上下文 mock）。为"如何文档化新的公开 trait"设立标杆。
- **`v42b_card_stream.rs` 示例**（156 行）。复用生产 `modes/common.rs` 渲染路径 — 生产环境回退也会破坏示例。良好的安全网。
- **主题迁移**（42 个导入点更新，没有悬挂的 `crate::tui::theme` 引用）。ui-kit 中的新 `theme.rs` 在 crate 根重新导出旧的子模块路径（`brand`、`mode_color`、`z_index`），所以使用公开项的外部代码继续编译。
- **LLM 调用减少**（检查点缓存 + 自适应自洽 + 预检跳过 + circuit-on-pressure 跳过）。B-1 修完后净为正；每回合会节省真实 LLM token。
- **压力监视器多源**（`SourceRegistration` + `SourceThresholds` + `ManualPressureSource` + `combined_pressure` + `should_reject` + `classify_with`）。Diff 中对敏感子系统的最干净扩展。
- **SilentRouter 从清单迁移**（`build_maps_from_manifest()` + `Domain::Session=7` + `DOMAIN_COUNT=8`）。7 个 domain → 8 个 domain，无行为变化，所有布线读自 `tools.toml`。

---

## 7. 推荐下一步

1. **暂停 v1.5.0 发布。** 3 项 P0 TUI 缺陷在每次按键时都可见。L0 #4 修完前该版本不能发布。
2. **按此次序执行 L0**（最小 diff 优先，然后集成）：A-1 → A-2 → B-1 → B-2 → C-1 → C-2 → C-3。
3. **把 L1 #5-10 一起进同 PR**（外加 10 分钟，全部卫生）。
4. **L0+L1 之后端到端冒烟测试**：键入消息，出现在面板 · 加载 v2 会话，看到所有 cards · Ctrl+S 保存，重新加载，所有 cards 重新出现 · 任何工具执行器 panic，验证 B-1 修复健全。
5. **把 L2（#11-14）调度为独立工单。** 都不阻塞发布。
6. **C-8（abacus-ui-kit）需要决策。** 我会单独问。
7. **L0 变绿后启动外部工具预处理流水线。**
