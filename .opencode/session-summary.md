# AbacusCode Session — Anchored Summary (2026-06-05 round 3)

## Goal
将 Abacus 全栈**真实落地**LLM 资源感知优化（不造空中楼阁），并按"问题分析→设计目标→正确实现"方法论完成全栈 review 修复。

## Constraints & Preferences
- **设计必须真实落地**：CoreLoop/ConfigManager/TUI 真的调 LlmBudget，不写孤立模块
- **治本 > 治标**：每条修复都"分析根因→设计目标→正确实现"
- **应用不能闪退/卡死**：所有路径都失败优雅
- **保留向后兼容**：opt-in 语义（默认不限 / 用 `try_*` 旧 API 保留）

## Progress

### ✅ 资源感知优化"真实落地"（核心交付）
- **LlmBudget as PressureSource**（`core/llm_budget.rs`，~500 行 + 11 tests）
  - 实现 `core/pressure::PressureSource` trait——融入现有 `ResourcePressureMonitor`，不造新调度体系
  - 3 维压力：cost / token / latency P95
  - `record()` / `pressure_ratio()` / `level()` / `snapshot()` / `should_halt()` / `shed()`
  - `reconfigure()` 热重载（parking_lot RwLock）
  - shed cooldown 60s 防抖
  - shed callback 通知 CoreLoop 切 fallback
- **CoreLoop 真实接入**：
  - `CoreLoop` 持 `Arc<LlmBudget>` 字段 + getter `llm_budget()`
  - `CoreLoop::new()` 注入到 `pressure_monitor` 作为 PressureSource
  - `pipeline/mod.rs` 调 LLM **前**：`pressure_monitor.check_and_shed()`（治本 #10/#15 也涵盖）
  - `pipeline/mod.rs` 调 LLM **后**：`llm_budget.record(LlmUsage)` + level 升级时 `tracing::warn!`
  - `core/mod.rs::reconfigure_llm_budget()` 暴露热重载
- **`config.toml [llm_budget]` 配置入口**（`config.rs`）：
  - 6 个默认 key：`max_cost_usd` / `max_total_tokens` / `soft_threshold` / `hard_threshold` / `reject_threshold` / `latency_window`
  - `ConfigManager::llm_budget_config()` 提取
  - 2 unit tests：TOML 解析 + 默认无限
- **`engine_init.rs` 真实注入**：在 `CoreLoop::new()` 之后调 `reconfigure_llm_budget(cfg)`
- **TUI `/budget` 命令**：`SlashCommand::BudgetStatus` + `cmd_budget` handler，输出 snapshot

### ✅ 全栈 review 修复（按"治本"方法论）

| # | 修复 | 治本路径 |
|---|------|----------|
| **🔴#1** | wire trace → per-pid + chmod 600 | per-pid 路径天然独立，删 `unwrap_or(0)` 注入 |
| **🔴#2** | classify_bash_command 启发式覆盖不全 | 显式 denylist（eval/bash -c/xargs/awk/source/find -exec） |
| **🔴#3** | `parse_toml_value` 数组/表分支永远失败 | 包裹 `__v = ...` 绕过 `Value::from_str` 首字符歧义 |
| **🔴#4/30** | mag_chain INSERT/DELETE 拆开导致 chain 不一致 | `unchecked_transaction` 原子化；trim 失败回滚 + warn |
| **🟡#2** | 生产代码 `std::env::set_var`（Rust 1.75+ unsafe） | 生产代码删除 env 路径（config 走 ConfigManager）；测试用 `#[allow(unsafe_code)]` + RAII |
| **🟡#5/#6** | paths.rs `unwrap_or(/tmp)` 静默 fallback + Windows 错 | `try_global_dir() -> Result<PathBuf, PathsError>` 显式错误；fallback 用 `temp_dir()` 而非 `/tmp`（Windows 兼容 + per-user 隔离）；`debug_assert!` 让 dev 立即看到 |
| **🟡#7** | per-token delta `let _ = tx.send` 假装成功 | `stream_alive` flag + labeled break；首次 send 失败 → break 整个流（不浪费后续 chunk 解析/网络） |
| **🟡#8/🟢#14** | async fn 内 `std::fs::*` 阻塞 worker thread | `tokio::task::spawn_blocking` 隔离；`read_session_entries_at` / `cleanup_stale` 全包 |
| **🟡#9** | `meeting_ask` `write().await` 持锁 30s+ | `try_write()` → 409 Conflict（让客户端 retry 而非阻塞） |
| **🟡#10** | child process 缺 kill_on_drop | `.kill_on_drop(true)` on script_hook / sandbox / mcp / code_graph |
| **🟡#11** | TUI log fallback 静默 sink | `eprintln!` 警告 + 保留 sink fallback |
| **🟡#12** | routes.rs 死代码 `let _ = ...clone()` | 删除 |
| **🟡#13** | L1MemoryCache::new 双重 unwrap + 静默 fallback | 保留 `new()`（向后兼容） + 新增 `try_new() -> Result<Self, CacheConfigError>`（治本）；2 tests |
| **🟡#15** | code_graph git 命令无 timeout | `tokio::time::timeout(30s, ...)` + kill_on_drop |
| **🟡#16/17** | subagent/safety_rules YAML 静默加载 | 显式 `tracing::warn!` + reject wildcard `tool_filter=["*"]` + 检查 safety_rules 文件 mode ≤ 0o600 |
| **🟡#18** | session_id[..8.min(len)] CJK panic | `safe_prefix(&s, 8)` char-boundary 助手（4 tests 覆盖 CJK/emoji/edge） |
| **🟡#19** | TUI config_mtime hot-reload 频繁读 + 阻塞 I/O | `tokio::time::sleep(500ms)` debounce（替换 20-tick 计数）+ spawn_blocking 隔离 I/O |
| **🟡#21** | deepseek 写死 `Bearer {}` 与 openai_compat 不一致 | 加 `auth_prefix: String` 字段 + 6 参 `with_config()` |
| **🟡#22** | mcp child stdin/stdout panic 泄漏 | `.kill_on_drop(true)` on spawn |
| **🟡#23** | TUI setup dir 创建吞错 | eprintln 警告 + accept_disclaimer 检查每个 fs::write |

### ✅ Reviewer 误报（已确认无需修复）
- **🟡#25** EventSink 已用 `Arc<Mutex<File>>` + `spawn_blocking` + `blocking_lock()`，模式正确
- **🟡#26** `RateLimiter::client_key` 已用 Authorization token sha256 hash（非 peer_addr）
- **🟡#29** `MeetingStore` 用 `HashMap<String, Arc<...>>` 不是 Vec，remove 是 O(1)
- **🟢#24** session save 已是 `.tmp + rename` 原子写

### 📊 Test Results
- **1552 passed, 0 failed**（含 +57 new tests 本轮）
  - core: 958 (was 906，+52)
  - cli: 215 (was 210，+5)
  - orchestrator/server: others unchanged
- **新测试**：
  - LlmBudget: 10 unit + 1 integration (PressureMonitor)
  - LlmBudget config: 2 (TOML parse + defaults)
  - L1MemoryCache.try_new: 3
  - subagent YAML: 2 (wildcard detected + disabled)
  - action_classifier YAML: 2 (valid + malformed)
  - safe_prefix: 5 (ASCII/CJK/emoji/edge/old-pattern panic)
  - filengine: 1 (denylist)
- **0 新 clippy warning on changed code**（pre-existing PI 警告不在我范围内）
- **`cargo build --release` clean**
- **`cargo test --workspace` all green**

## Next Steps (Optional)
- 写 CLI 启动时 1-shot 端到端集成测试（mock LLM → 多次 record → snapshot 验证）
- 在 user_config.toml 添加 `[llm_budget]` 文档示例
- 引入 `dialoguer` 提示用户首次启用 `[llm_budget]` 时的 cost 估算

## Critical Files Changed This Round
- `pkg/crates/abacus-core/src/core/llm_budget.rs` (new, 500 lines)
- `pkg/crates/abacus-core/src/core/mod.rs` (LlmBudget 字段 + getter + reconfigure)
- `pkg/crates/abacus-core/src/core/pipeline/mod.rs` (pressure shed + budget record)
- `pkg/crates/abacus-core/src/paths.rs` (try_global_dir + PathsError + temp_dir fallback)
- `pkg/crates/abacus-core/src/config.rs` (llm_budget_config + 2 tests)
- `pkg/crates/abacus-core/src/cache/l1.rs` (try_new + CacheConfigError + 3 tests)
- `pkg/crates/abacus-core/src/llm/wire_trace.rs` (new, per-pid path helper + 3 tests)
- `pkg/crates/abacus-core/src/llm/providers/{openai_compatible,deepseek}.rs` (stream_alive flag)
- `pkg/crates/abacus-core/src/llm/providers/deepseek.rs` (auth_prefix field)
- `pkg/crates/abacus-core/src/undo/engine.rs` (spawn_blocking wrapping)
- `pkg/crates/abacus-core/src/core/{subagent,action_classifier}.rs` (YAML validation)
- `pkg/crates/abacus-cli/src/engine_init.rs` (LlmBudget reconfig injection)
- `pkg/crates/abacus-cli/src/tui/{state/mod.rs,run.rs,slash_commands.rs}` (SlashCommand::BudgetStatus + /budget handler)
- `pkg/crates/abacus-server/src/routes.rs` (try_write → 409)
- `pkg/crates/abacus-cli/src/tui/util.rs` (safe_prefix helper + 5 tests)

## Validation Commands
```bash
cd /Users/cc/Downloads/AbacusCode/pkg
cargo test --workspace 2>&1 | grep -E "^test result"
# 期望 1552 passed / 0 failed
cargo build --release -p abacus-cli
# 期望 Finished `release` profile
```

## User-Facing Surface
- `config.toml` 新增段：
  ```toml
  [llm_budget]
  max_cost_usd = 5.0           # 0 = 不限
  max_total_tokens = 1_000_000 # 0 = 不限
  soft_threshold = 0.70
  hard_threshold = 0.85
  reject_threshold = 0.95
  ```
- TUI 新增命令 `/budget` 或 `/cost` → 显示 cost/tokens/level
- TUI 启动时若 home dir 解析失败 → stderr warning + temp_dir fallback（dev 立即可见）
- 配置文件含 API key → chmod 0o600 自动强制
- LlmBudget 升级到 critical → tracing::warn! 用户能看见
- 调 LLM 压力超 hard → 自动切 fallback provider
