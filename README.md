<p align="center">
  <img src="assets/logo.svg" width="360" alt="Abacus Logo" />
</p>

<h1 align="center">Abacus</h1>

<p align="center">
  <strong>Production-grade LLM Agent Kernel</strong><br>
  Multi-mode orchestration · Mathematical context compression · Built-in safety
</p>

<p align="center">
  <code>v1.0.0</code> &nbsp;·&nbsp; MIT License &nbsp;·&nbsp; Rust 1.75+ &nbsp;·&nbsp; macOS / Linux
</p>

---

## What is Abacus?

Abacus is a terminal-native LLM agent kernel that orchestrates AI reasoning across four collaborative modes. It provides a rich TUI experience with real-time streaming, tool execution, and multi-expert consultation — all from your terminal.

**Clarify Mode** — single-agent deep reasoning:

```
┌─────────────────────────────────────────────────────────────────────────────────┐
│ ⠋ ABACUS ▸ Refactor auth module · 澄清                         deepseek-v4    │
├──────────────────────────────────────────────────────────┬──────────────────────┤
│                                                          │ 📊 [====------] 38% │
│  User: 帮我把 session 认证改成 JWT                        │    415K↑ 5K↓ c78%   │
│                                                          │ 🔧 12/14 · 调8✓7    │
│  Session:                                                │ 🧠 3域 12行为        │
│  ## 方案确认                                              │ ╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌  │
│  我决定使用 JWT token 替代 session cookie。                │ 现场                 │
│  原因：无状态、易扩展、前后端解耦。                          │ ▸ 12:03 分析代码     │
│                                                          │   ⚙ fs_read auth.rs │
│  ```rust                                                 │ ▸ 12:04 方案生成     │
│  impl AuthService {                                      │   ✓ 2 工具完成       │
│      pub fn verify_token(&self, token: &str)             │ ╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌  │
│          -> Result<Claims, AuthError>                     │ Focus · 澄清 · 3轮  │
│  }                                                       │   JWT 认证重构       │
│  ```                                                     │   → 方案已确认       │
│                                                          │                      │
├──────────────────────────────────────────────────┬───────┴──────────────────────┤
│ ╭──────────────────────────────────────────────╮ │ ╭────────────────────────╮   │
│ │ ⠋ Thinking 澄清 · 分析认证模块  3.2s        │ │ │ deepseek(✓) · v4       │   │
│ │ Ask anything...                     ⏎ Enter │ │ │ ⬡ 52K/128K/1M · 3轮   │   │
│ ╰──────────────────────────────────────────────╯ │ │ thinking · ¥0.08       │   │
│                                                  │ ╰────────────────────────╯   │
├──────────────────────────────────────────────────┴──────────────────────────────┤
│ ● 澄清                                            1.2K tok  Cmd+↑↓ Ctrl+B Esc │
└─────────────────────────────────────────────────────────────────────────────────┘
```

**Plan Mode** — two-phase: research → approval → execute:

```
┌─────────────────────────────────────────────────────────────────────────────────┐
│ ● ABACUS ▸ Refactor auth module · 澄清 · [📋 规划]             deepseek-v4    │
├──────────────────────────────────────────────────────────┬──────────────────────┤
│                                                          │ 📊 [======----] 58% │
│  Session:                                                │ ╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌  │
│  📋 计划已就绪 — 3 阶段, 12 步骤                          │ Focus · 规划 2/3     │
│                                                          │  目标 重构auth模块   │
│  选择执行策略:                                            │  ✓ 分析现有代码      │
│  [A] 自动执行 — 工具调用自动放行                           │  ⟳ 生成迁移方案      │
│  [S] 逐步确认 — 每步操作需确认                            │  ○ 执行重构          │
│  [T] 团队分发 — 多专家并行执行                            │  ████░░░░░░ 2/3     │
│  [C] 取消                                                │                      │
│                                                          │                      │
│  输入 A/S/T/C:                                           │                      │
│                                                          │                      │
├──────────────────────────────────────────────────┬───────┴──────────────────────┤
│ ╭──────────────────────────────────────────────╮ │ ╭────────────────────────╮   │
│ │ ● Ready · 澄清                              │ │ │ deepseek(✓) · v4       │   │
│ │ 输入 A/S/T/C 选择策略...            ⏎ Enter │ │ │ ⬡ 75K/128K/1M · 8轮   │   │
│ ╰──────────────────────────────────────────────╯ │ │ thinking · ¥0.32       │   │
│                                                  │ ╰────────────────────────╯   │
├──────────────────────────────────────────────────┴──────────────────────────────┤
│ ● 澄清 · 📋 规划就绪                              4.8K tok  Cmd+↑↓ Ctrl+B Esc │
└─────────────────────────────────────────────────────────────────────────────────┘
```

**Meeting Mode** — multi-expert consultation:

```
┌─────────────────────────────────────────────────────────────────────────────────┐
│ ⠙ ABACUS ▸ 性能优化方案 · 会诊                                 deepseek-v4    │
├──────────────────────────────────────────────────────────┬──────────────────────┤
│                                                          │ 📊 [=======---] 72% │
│  User: @架构师 @DBA 这个查询太慢了怎么优化                  │ ╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌  │
│                                                          │ Focus · 会诊 2/3位   │
│  🔊 架构师:                                              │  🔊 架构师 (后端)    │
│  建议从两个层面入手：                                      │  🔊 DBA (数据库)     │
│  1. 应用层：添加 Redis 缓存热点查询                        │  ○  前端 (前端)      │
│  2. 数据层：对 user_id 添加复合索引                        │  阶段 ● 发言中       │
│                                                          │                      │
│  🔊 DBA:                                                 │                      │
│  补充几点：                                               │                      │
│  - EXPLAIN 显示全表扫描，缺少 (user_id, created_at) 索引   │                      │
│  - 建议分区表（按月），历史数据走冷存储                      │                      │
│                                                          │                      │
├──────────────────────────────────────────────────┬───────┴──────────────────────┤
│ ╭──────────────────────────────────────────────╮ │ ╭────────────────────────╮   │
│ │ ⠹ Working 会诊 · DBA 发言中  5.1s           │ │ │ deepseek(✓) · v4       │   │
│ │ Ask anything...                     ⏎ Enter │ │ │ ⬡ 92K/128K/1M · 5轮   │   │
│ ╰──────────────────────────────────────────────╯ │ │ thinking · ¥0.56       │   │
│                                                  │ ╰────────────────────────╯   │
├──────────────────────────────────────────────────┴──────────────────────────────┤
│ ● 会诊 · DBA 发言中                                6.2K tok  Cmd+↑↓ Ctrl+B Esc │
└─────────────────────────────────────────────────────────────────────────────────┘
```

## Install

### Quick Install (macOS / Linux)

```bash
curl -fsSL https://github.com/lucasli000/abacus/releases/latest/download/install.sh | sh
```

Custom install path:

```bash
INSTALL_DIR=~/.local/bin curl -fsSL https://github.com/lucasli000/abacus/releases/latest/download/install.sh | sh
```

### Manual Download

Download from [Releases](https://github.com/lucasli000/abacus/releases/latest):

| Platform | File |
|----------|------|
| macOS Apple Silicon | `abacus-aarch64-apple-darwin.tar.gz` |
| macOS Intel | `abacus-x86_64-apple-darwin.tar.gz` |
| Linux x86_64 | `abacus-x86_64-unknown-linux-gnu.tar.gz` |
| Linux ARM64 | `abacus-aarch64-unknown-linux-gnu.tar.gz` |

```bash
tar -xzf abacus-aarch64-apple-darwin.tar.gz
sudo mv abacus /usr/local/bin/
abacus --version
```

### From Source

```bash
git clone https://github.com/lucasli000/abacus.git
cd abacus/pkg
cargo install --path crates/abacus-cli
```

### Verify

```bash
$ abacus --version
abacus 1.0.0 (3b43be7d 2026-05-30)
```

### Shell Completions

```bash
eval "$(abacus completions zsh)"    # Zsh
eval "$(abacus completions bash)"   # Bash
abacus completions fish | source    # Fish
```

### Uninstall

```bash
rm /usr/local/bin/abacus
rm -rf ~/.abacus  # Config + data (optional)
```

## Features

### Four Orchestration Modes

| Mode | Description | Use Case |
|------|-------------|----------|
| **Clarify** | Single-agent deep reasoning with progressive gate | Default — ask questions, write code, debug |
| **Plan** | Two-phase: Research → Approval → Execute | Complex multi-step tasks, user selects strategy (A/S/T) |
| **Team** | Multi-agent parallel with ToolAgent delegation | Large refactoring, parallel tool operations |
| **Meeting** | Multi-expert weighted consultation | Cross-domain problems needing diverse expertise |

### Core Engine

- **Mathematical Context Compression** — Information Bottleneck + H2O Heavy-Hitter + ARC + Greedy Knapsack. 38% message reduction, 100% key decision retention.
- **ToolActionClassifier** — Rule-based safety (hard_deny/soft_deny/allow). Zero LLM overhead.
- **ToolAgent** — Batch delegation for read-only ops. 4 built-in types: Explorer, Researcher, Coder, Mathematician.
- **MCIP** — Multi-level permission gate (role → confirm → capability).
- **ProgressiveGate** — Complexity-aware output strategy.
- **Dynamic Timeout** — Complexity + tool count + LLM self-extension.

### TUI

- Real-time streaming with thinking visualization
- Soft-wrap input with cursor tracking
- Right panel: dashboard + timeline + focus
- 12 built-in themes
- Inline suggestions (Tab)

## Quick Start

```bash
# Interactive TUI (default)
abacus

# Single query
abacus ask "explain this error"

# Specific model
abacus --model deepseek-v4 ask "optimize this"

# Mode switching inside TUI
/clarify          # Single-agent (default)
/plan <goal>      # Two-phase planning
/team <task>      # Multi-agent parallel
/meeting          # Multi-expert consultation
@expert msg       # Auto-route to Meeting
```

### First Run — Setup Wizard

```
┌─ Abacus Setup ──────────────────────────────┐
│                                              │
│  1. Choose LLM provider                     │
│     → OpenAI / Anthropic / DeepSeek / ...   │
│                                              │
│  2. Enter API key                           │
│                                              │
│  3. Select default model                    │
│                                              │
└──────────────────────────────────────────────┘
```

Auto-detects 9 providers from URL. Config: `~/.abacus/config.yaml`

Skip wizard (CI): `export ABACUS_API_KEY=sk-xxx`

## Architecture

```
abacus/pkg/
├── crates/
│   ├── abacus-cli/          TUI + CLI (ratatui + clap)
│   ├── abacus-core/         Engine kernel (pipeline, context, tools, safety)
│   ├── abacus-orchestrator/ Meeting/Team orchestration
│   ├── abacus-types/        Shared types
│   └── abacus-server/       HTTP/SSE server (axum)
├── assets/                  Logo + icons
└── scripts/                 Install script
```

### Multi-Provider Support

| Provider | Models | Status |
|----------|--------|--------|
| OpenAI | GPT-4o, o1, o3 | ✅ |
| Anthropic | Claude Sonnet/Opus | ✅ |
| DeepSeek | V3, V4, R1 | ✅ |
| Google | Gemini 2.x | ✅ |
| Moonshot / 智谱 / 通义 / SiliconFlow / Groq | Various | ✅ |
| Any OpenAI-compatible | Custom | ✅ |

## Development

```bash
cargo check --workspace        # Type check
cargo test --workspace         # Run tests
make build                     # Release build
make package                   # Create .tar.gz
make universal                 # macOS universal binary
```

## License

MIT — see [LICENSE](LICENSE)
