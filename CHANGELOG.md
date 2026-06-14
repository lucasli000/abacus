# Changelog

## v2.5.6 (2026-06-14) — UX 质量 + 架构重构（参考 OpenCode TUI）

### 完成总结

本次会话完成 **22 个任务**，共产生 **28 次 commit**，最终状态：
- 1552/1552 测试通过，0 error，0 clippy warning
- 6 个 workspace crate 全量 clippy 清零

### 关键架构决策

- **Scrollback trait** — `Scrollable` trait 统一 4 类滚动容器（SimpleScrollOffset + ScrollableStack），面板滚动迁移至 trait 方法调用
- **CompletionEngine** — 内联补全状态机封装（suggestion + candidates + candidate_idx），Tab handler 简化为 4 条路径
- **tui-textarea SSoT** — 输入状态机完全替换为 tui-textarea widget，`state.input` 保持 SSoT（文本内容），textarea 通过 `sync_to_textarea`/`sync_from_textarea` 双向同步
- **卡片语义识别** — `MessageCard` trait 新增 `fn kind()` 方法，支持 Markdown 标题识别和交互语义
- **Tool Agent 结果可视化** — LLM 输出 `ToolAgentResult` JSON 时自动渲染 tool-badge
- **27 色主题系统** — 8 markdown + 8 syntax + 6 diff + 2 thinking + 1 blend，所有主题通过 `with_semantic_colors()` 自动派生
- **双数据源重构** — `add_message` 同步写入 `state.messages` 和 `state.cards`，`iter_rev().take(5)` 优化去重
- **死代码清理** — 删除 ~130 行不可达代码 + PanelFocus 死枚举
- **方法提取** — `clear_input()` 消除 4 处重复；`short_path()` 6→1 次分配
- **27 色主题系统** — 8 markdown + 8 syntax + 6 diff + 2 thinking + 1 blend，所有主题通过 `with_semantic_colors()` 自动派生
- **双数据源重构** — `add_message` 同步写入 `state.messages` 和 `state.cards`，`iter_rev().take(5)` 优化去重

### 后续规划（下一阶段）

1. **Scrollback trait 抽象** — 定义 `ScrollableContent` trait，统一 Scrollback/ScrollbackOverlay/CardStream/Picker 四类滚动容器
2. **tui-textarea Phase 5** — Tab 补全完全委托给 textarea
3. **TUI 质量加固** — 继续清理存量优化点

### UX / 美观

- **跨角色呼吸感** — 相邻卡片 kind 不同时插入 1 行空白（User→LLM、LLM→Tool 等），同类紧贴
- **焦点感知输入框** — Focus::Input 时 bg 从 theme.bg 升级到 theme.surface，强化"我现在在这"
- **Placeholder 视觉强化** — 加 `▎` accent bold cursor 提示，用户感知可输入区域
- **Dynamic 快捷键提示栏** — 底行显示上下文相关快捷键（`/` 命令 / Tab 补全 / Ctrl+B 等），替代原静态 `⏎ Enter`
- **TopBar 紧凑指标** — 任务头右侧新增 tokens + context% + cost 一行汇总（参考 OpenCode 截图）
- **工具调用语义符号** — Running→⠇ / Success→→ / Failed→✗，替代原装饰性 ●
- **tui-textarea 集成（Phase 1）** — 引入 ratatui 生态最成熟的多行编辑器，替换手写 Paragraph 渲染

### 稳定

- **dedup 强化** — normalize_for_dedup（trim + collapse 空白）+ suffix 匹配，消除 ToolAgent prefix 注入导致的重复
- **set_scroll 安全上限** — total==0 时 max 从 usize::MAX 改为 10_000，防首帧前 scroll 溢出
- **scroll 字段改 pub(crate)** — 强制所有修改走 set_scroll()，绕过 clamp 不再可能
- **RefCell 合并** — 4 个 RefCell<u16>（last_msg_area_x/y/width/height）→ 1 个 RefCell<Rect>，减少 75% borrow 操作

### 高性能

- **cached_msg_rows 消除重复遍历** — 渲染循环中收集高度，末尾直接赋值，消除每帧 ~100 次冗余 card_total_height 调用
- **dedup O(1)** — CardStream 新增 iter_rev()，push_session_message dedup 从 O(n) 全量遍历 → 逆序 take(5)

### 可维护

- **MessageCard trait 加 is_empty()** — 消除 render_cards 中 3 种类型硬编码 downcast，新增 Card 类型无需改 render.rs
- **Space 折叠迁移到 cards** — handle_chat_scroll_key 从读 state.messages (V40) 改为读 state.cards (V42-B)，消除双数据源不一致
- **鼠标 hit-test 迁移到 cards** — screen_pos_to_msg_char → screen_pos_to_card_char，extract_selection_text → extract_card_selection_text，scroll_to_message 改用 cached_msg_rows
- **MessageCard trait 加 text_content()** — 各 Card 类型覆写返回对应文本，支持鼠标选择复制等场景
- **/new 命令同步清空 cards** — 修复只清 messages 不清 cards 导致旧会话卡片残留的 bug
- **学习笔记** — docs/opencode-tui-design-study.md，OpenCode TUI 架构分析 + 可借鉴设计点

### 待做（下次 session）

- **tui-textarea Phase 2** — 键盘处理迁移到 textarea.input()，替换 handle_input_key 中的手写 cursor 状态机
- **Scrollback trait 抽象** — 架构级重构，需 C3/C4 完成后才有干净基础

### 测试

- 新增 12 个 dedup 测试 + 9 个 metrics 测试 + 3 个 abacus card 测试
- 全量 300/300 测试通过

## v2.0.0 (2026-06-10) — 配置系统重构 + 知识库 + 全平台 CI

### Architecture

- **V42-B Streaming 迁移** — 流式输出引擎重写，支持增量渲染 + 工具调用交织
- **配置系统全面重构** — TOML 多层配置（`config.toml` + `provider.toml` + `security.toml`），环境变量 `ABACUS_*` 覆盖，`config set/get/validate` 命令
- **ARK Provider 接入** — 新增 `openai-compatible` provider 类型，支持任意 OpenAI 兼容 API
- **LLM 资源感知预算** — `LlmBudget` 实时追踪 cost/token/latency，`ResourcePressureMonitor` 自适应降级
- **知识库系统** — `KnowledgeStore` + FTS5 全文检索 + `VllmEmbedder` 向量嵌入，`kb_search`/`kb_ingest` 工具
- **记忆宫殿** — `DualPalaceMemory`（BehaviorPalace + KnowledgePalace），SM2 间隔重复算法
- **ScriptHook 管道钩子** — Rhai/Shell/Python 三种运行时，`TurnStart`/`TurnEnd` 事件注入
- **Panel 布局重写** — Dashboard Hooks 改造，Section 优先级布局，可配置面板可见性
- **渐进输出协议** — `ProgressiveGate` 门控输出，Checklist 驱动多阶段确认
- **工作流检查器** — `WorkflowChecker` 代码质量/安全/完整性多维检查
- **代码图谱** — `CodeGraph` 跨文件依赖分析，Rust/Python/Go/TypeScript 语言支持
- **LSP 集成** — 语言服务器协议客户端，实时诊断 + 补全
- **WASM 插件系统** — `PluginLoader` 沙箱执行，签名验证

### CI / Build

- **跨平台 Release** — macOS (aarch64 + x86_64) + Linux (aarch64 + x86_64) 四目标构建
- **Windows 构建支持** — `cfg(unix)` 条件编译拆分，protobuf 路径动态查找
- **GitHub Actions** — 自动 release 发布 + artifact 上传 + SHA256 checksum

### UI / UX

- **8+3 个 UI/UX 修复** — 边框渲染、数据面板、横线分隔、CJK 折行
- **输入框 soft-wrap** — `char_width` 精确计算，修复粘贴带格式文本布局错乱
- **Streaming 内容落档** — 用户中途发消息时自动保存未完成 streaming，不再丢失 trace
- **模型切换修复** — provider/model 切换去同步问题，认证失败处理
- **超时可配置** — 默认 300→600s，`/timeout` 命令 + `EngineHandle` 配置字段
- **Short-Mode 阈值可配** — `tool_prune_after_turns` 控制工具裁剪时机

### Code Quality

- **1510 tests 全部通过**（vs v1.0.0 的 695）
- **0 clippy errors**，84 warnings 均为设计决策级 `allow`
- **0 `unimplemented!()` / `todo!()`** 生产代码
- **3 处 `unsafe`** 均有 `#[allow(unsafe_code)]` 标注（`libc::kill` / `mlock` / `munlock`）
- 依赖版本统一升级至 2.0.0，workspace 级别版本管理

### Breaking Changes

- `llm.*` 配置键废弃，迁移至 `provider.toml`
- `default_model` 改为 `auto`（自动选择第一个 provider 的第一个 model）
- Provider 注册前置检查：缺 base_url 和 key 时不注册

---

## v1.0.0 (2026-05-24) — 首个稳定版发布

### TUI 交互模式（V33 4 阶 DAG）
- **Clarify**（默认入口）→ **Plan** → **Team** → **Meeting** 四模式按需流转

### Architecture

- **协议同构感知层（J1+J2）** — `tool::cluster::ClusterRegistry` 把工具间横向关系显式化；
  `build_tool_definitions_for` 自动给每个工具 description 末尾追加 cluster + siblings + differentiator；
  新增 `tool_compass` 自省工具让 LLM 在不确定时主动咨询「该用哪个工具」
- **adaptive_d_tier_hide 默认开（K1~K5）** — 5 层兜底防误杀：
  K1 `ToolOutcome::EnvFailure` 不入 success_rate 分母（修网络/auth 失败拖累评分）；
  K2 扩展工具（MCP/Plugin/Skill）30 次冷启动期；
  K3 hide 层 floor + 极端兜底（每 provider 至少保留 N、整 cluster 至少保留 1、整体回退）；
  K4 `palace_demoted` 每 50 turn 试探放行（修单调降级死锁）；
  K5 default=true + audit 透明化
- **机制接入完整闭环（L1~L6）** — pipeline `record_outcome` 真识别失败类型；
  `sync_from_palace_at(turn)` 让 K4 试探放行机制启动；
  audit_report 输出 `env_failure_dominated` 提示运维；
  history.jsonl 加 `search_history` 读取 API；
  tool_compass 推荐时过滤 hide 状态
- **跨 session 持久化（A~I 9 段）** — 进程注册表 + jsonl event log + GlobalHistoryHook +
  rotation + replay + SessionResumeReport；4 个端到端集成测试锁全链路
- **DualPalaceMemory** — BehaviorPalace + KnowledgePalace + ContextTiers FIFO 三层记忆机制

### Code Quality

- **0 warning / 0 error** 全仓编译
- **695 lib tests + 4 cross-session e2e** 全部通过

### Security

- 所有源码本地路径 / 个人项目名 / 第三方品牌引用清洗完成
- `subtle::ConstantTimeEq` 用于 HMAC / auth token / admin token 比较
- 审计 / 决策 / 会话 / 限流 key / Context 声明栈全部上限管理

---

## v0.2.0 (2026-05) — 后端深度审查 + 技术债清扫

### Architecture

- **取消语义贯通**：`tokio_util::CancellationToken` 三层透传（server → pipeline → provider），`tokio::select!` 与 in-flight reqwest 竞速取消，timeout 后不再泄漏未完成请求
- **Meeting L4 接通**：5 个 HTTP handler 落地（create / list / detail / ask / delete），`MeetingStore` 镜像 `TeamManager` 形态，与 L3 实现解耦
- **Pipeline 模块拆分**：1511 行单文件 → `pipeline/{mod.rs, post.rs}` 跨文件 inherent impl，post.rs 承载 Phase 5/6（post_process + detect_inertia ≈ 315 行）
- **Provider 共享 HTTP 客户端**：`shared_http_client()` 静态 `OnceLock<reqwest::Client>`（pool_idle_timeout=90s, pool_max_idle_per_host=32），三个 provider 复用单一连接池
- **首次启动模型自动发现**：`CoreLoop::discover_and_cache()` 后台 task；CLI 新增 `abacus models discover`；`/api/v1/models` 实时拉取（5s 超时） + cache + static 三级 fallback；ConfigManager 自动补全 `[available_models]`
- **BoundedFifo 抽象**：`abacus-types::collections::BoundedFifo<T>` 统一 5 处有界 FIFO 场景（`RateLimiter.clients` / `ServerSessionManager.snapshots` / `SecretsManager.audit_log` / `McipGateway.decision_log` / `ContextManager.{pending, retained_content}`），替代 Vec.remove(0) O(n) 滚动

### Performance / Resource Safety

- **C1 RateLimiter GC**：clients HashMap 加 `last_seen` + 4096 阈值触发 5×window 闲置项清理，防 token 旋转期 OOM
- **C2 RateLimiter 哈希常时比较**：`subtle::ConstantTimeEq` 替代字面 ==，规避基于响应时间的 key 探测
- **C5 auth_middleware 抗短路**：去除早期 return 短路，所有 token 比对走完整 ct_eq 路径
- **C6 SecretString::generate 不再 panic**：CSPRNG 失败返回 `Result<Self, getrandom::Error>`，调用方决定 fail-fast 或 fallback
- **H3 FallbackProvider 抖动**：`SystemTime::subsec_millis` → CSPRNG 真随机，并发同周期请求避免 thundering herd
- **H4 FallbackProvider 借用优化**：`execute_with_retry` 改为 `&LlmRequest`，省掉成功路径外层 deep clone（messages/tools 全 Vec 复制）
- **H6 Provider 注册前置检查**：OpenAI provider 同时缺 base_url 和 key 时不注册，缺 key 时 warn log
- **H9 error_response 状态码保留**：`Response::builder` + `*resp.status_mut()` 双层兜底，避免 `.status_code()` 返回 200 误导监控
- **M7 CORS 生产硬化**：`ABACUS_ENV=production` 且无 `CORS_ORIGINS` → deny-all（之前默默允许所有源）
- **P1 LazyLock 编译期 Regex**：context.rs 9 个 Regex 改为 `LazyLock`，进程级单次编译
- **P3-D Prometheus seqlock**：`record_request` 用 epoch 协议（奇数=写入中，偶数=稳态），render 期间检测并发写入计入 `dirty_scrape_total`，暴露 `metrics_write_epoch` 给 Grafana 一致性告警

### Bug Fixes

- **TUI 主题对比度**：Nord muted/bg 1.69 → 2.8（提亮 nord3 至 Snow Storm 0 变体），通过 WCAG 2.5+ 次要文本断言
- **`Theme: !Clone`**：补 `#[derive(Clone)]` 让 contrast 测试可批量验证
- **TUI Markdown highlight_code 调用签名**：补 `&self.theme` 参数；`render_table` 用 `std::mem::take` 拆借用
- **`normalized_buf` unused assignment**：去除冗余 String::new() 兜底，编译期确认 else 分支不读取该变量
- **TUI panel_visible 默认 true**：测试断言对齐实际 UX 设计

### Code Quality

- **0 warning / 0 error 全仓**：清理 7 处遗留警告（unused imports / unused mut / value never read）
- **433 tests 全部通过**（vs v0.1.0 的 350，新增 BoundedFifo / model_cache / pipeline post 等 83 项）
- 三层 cancel API 加入 trait 默认方法 (`complete_cancellable`, `discover_models`)，向后兼容现有 provider 实现

### Security

- 已扩展 v0.1.0 防护到所有遗漏：HMAC 比较、auth token 比较、admin token 比较全部走 `subtle::ConstantTimeEq`
- 审计 log / 决策 log / 会话快照 / 限流 key / Context 声明栈全部上限管理

### Known Limitations

- M1 pipeline 拆分仅完成 Phase 5/6 → post.rs；setup/execute/finalize 三阶段保留在 mod.rs 中。剩余拆分纯粹是组织优化，无功能/性能影响，遵循「最低风险增量」原则推迟到下一次大重构（避免 1500 行同改批量引入 lifetime/import 风险）

---

## v0.1.0 (2026-05)

### Architecture

- Provider fallback 链：Anthropic → OpenAI → DeepSeek 自动降级
- Meeting 并发多专家：Semaphore 限流 + tokio::spawn 并行执行
- Meeting 主持人机制：首个 expert 自动 host，分发任务 + 汇总结论
- 命令注册表：`slash_commands.rs` 替换 110 行 giant match，/help 自动生成
- 反馈分层：Toast（瞬态）/ Panel（持久）/ StatusBar（实时）

### Code Quality

- **0 warning / 0 error** 全仓编译
- **350 tests** 全部通过
- 消除死代码：`run_tui_demo`、`disclaimer` 模块、重复 `MeetingManager` struct
- 消除 `unwrap()` panic 点：`auth_middleware` / `rate_limit_mw` → `error_response()` 安全 fallback
- CI 纳入 `abacus-server` 全量编译
- token 计量改用引擎返回的 `session_tokens.total_tokens`
- analyzer 假阳性修复：中文单字 "分析"/"实现" 不再误触发

### Security

- RateLimiter key 改为 SHA256 哈希（原存完整 Bearer token）
- 工具参数解析失败日志不再输出原始内容
- `audit_log` / `decision_log` 加 Vec 上限防 OOM
- ConfigManager 对 `api_key`/`server_token` 输出 `[REDACTED]`

### UX

- **Tab 补全可达**：Input 焦点时 Tab 不再切面板
- **引擎 init 失败直接报错**：不再静默降级 demo 模式
- **模式不自动切换**：单词触发的 analyzer 改为 toast 建议
- **消息折行 + OSC 52 复制**：非 TUI 模式支持
- **Toast 可读**：1s → 2s 最低时长
- **渲染缓存**：消息不变时零开销复用 `cached_lines`
- 消息渲染增强：gutter 缩进、代码块识别、角色 emoji、转场箭头
- 处理进度：`🤔 Thinking... [1/4] 12.3s` step 步数 + 阶段描述
- 模型品牌色：DeepSeek 冷灰蓝 / OpenAI 冷灰绿 / Anthropic 暖灰橙 等

### Product Completeness

- Shell completions（`abacus completions <bash|zsh|fish>`）
- Rustyline 行编辑 + 历史持久化（`~/.abacus/history.txt`）
- `config list-keys` 14 个配置项 + 中文通俗说明
- `skill install/remove`、`mcp add/remove` 命令
- 会话持久化：退出自动保存 `~/.abacus/sessions/latest.json`
- 会话自动恢复：重启 TUI 自动加载上次消息
- 设置面板可达：Ctrl+O + /settings 双入口
- Docker entrypoint 修正：`abacus-server` 二进制正确打包

### Bug Fixes

- **docker-compose**：端口 3000/8080 不一致 → 统一 8080
- **Dockerfile**：缺少 `abacus-server` 二进制 copy
- **session_id 复用**：chat_handler 读请求中的 session_id
- **SSE keep-alive**：30s 间隔防代理超时
- **错误格式统一**：chat_handler 返回 `ErrorResponse` 而非 `ChatResponse`
- **logging.rs 死代码**：main.rs 改为调用 `logging::init()`
- **skill/MCP 命令 enum**：恢复 Enable/Disable/Disconnect 丢失变体
- **`tui_main` binary 路径**：`src/tui/main.rs` → `src/bin/tui_main.rs`
- **`block` 渲染 move 后 borrow**：交换 `.inner()` 和 `render_widget` 顺序
- **new_with_gate_config 缺 max_sessions**：参数传入
