<p align="center">
  <img src="assets/logo.svg" width="360" alt="Abacus Logo" />
</p>

<h1 align="center">Abacus</h1>

<p align="center">
  <strong>Production-grade LLM Agent Kernel</strong><br>
  Multi-mode orchestration · Mathematical context compression · Built-in safety
</p>

<p align="center">
  <code>v1.2.0</code> &nbsp;·&nbsp; MIT License &nbsp;·&nbsp; Rust 1.75+ &nbsp;·&nbsp; macOS / Linux
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
├──────────────────────────────────────────────────────────┬──────────────────────┤
│ ╭──────────────────────────────────────────────────────╮ │ deepseek(✓) · v4     │
│ │ ⠋ Thinking 澄清 · 分析认证模块  3.2s                │ │ ⬡ 52K/128K/1M · 3轮 │
│ │ Ask anything...                           ⏎ Enter   │ │ thinking · ¥0.08     │
│ ╰──────────────────────────────────────────────────────╯ │                      │
├──────────────────────────────────────────────────────────┴──────────────────────┤
│ ● 澄清                                            1.2K tok  Cmd+↑↓ Ctrl+B Esc │
└─────────────────────────────────────────────────────────────────────────────────┘
```

**Plan Mode** — two-phase: research → approval → execute:

```
┌─────────────────────────────────────────────────────────────────────────────────┐
│ ● ABACUS ▸ Refactor auth module · 澄清 · [📋 规划]             deepseek-v4    │
├──────────────────────────────────────────────────────────┬──────────────────────┤
│                                                          │ 📊 [======----] 58% │
│  Session:                                                │    820K↑ 12K↓ c65%  │
│  📋 计划已就绪 — 3 阶段, 12 步骤                          │ 🔧 14/14 · 调12✓10  │
│                                                          │ ╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌  │
│  选择执行策略:                                            │ Focus · 规划 2/3     │
│  [A] 自动执行 — 工具调用自动放行                           │  目标 重构auth模块   │
│  [S] 逐步确认 — 每步操作需确认                            │  ✓ 分析现有代码      │
│  [T] 团队分发 — 多专家并行执行                            │  ⟳ 生成迁移方案      │
│  [C] 取消                                                │  ○ 执行重构          │
│                                                          │  ████░░░░░░ 2/3     │
│  输入 A/S/T/C:                                           │                      │
│                                                          │                      │
├──────────────────────────────────────────────────────────┬──────────────────────┤
│ ╭──────────────────────────────────────────────────────╮ │ deepseek(✓) · v4     │
│ │ ● Ready · 澄清                                      │ │ ⬡ 75K/128K/1M · 8轮 │
│ │ 输入 A/S/T/C 选择策略...                    ⏎ Enter │ │ thinking · ¥0.32     │
│ ╰──────────────────────────────────────────────────────╯ │                      │
├──────────────────────────────────────────────────────────┴──────────────────────┤
│ ● 澄清 · 📋 规划就绪                              4.8K tok  Cmd+↑↓ Ctrl+B Esc │
└─────────────────────────────────────────────────────────────────────────────────┘
```

**Meeting Mode** — multi-expert consultation:

```
┌─────────────────────────────────────────────────────────────────────────────────┐
│ ⠙ ABACUS ▸ 性能优化方案 · 会诊                                 deepseek-v4    │
├──────────────────────────────────────────────────────────┬──────────────────────┤
│                                                          │ 📊 [=======---] 72% │
│  User: @架构师 @DBA 这个查询太慢了怎么优化                  │    1.1M↑ 8K↓ c80%   │
│                                                          │ 🔧 14/14 · 调15✓12  │
│  🔊 架构师:                                              │ ╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌  │
│  建议从两个层面入手：                                      │ Focus · 会诊 2/3位   │
│  1. 应用层：添加 Redis 缓存热点查询                        │  🔊 架构师 (后端)    │
│  2. 数据层：对 user_id 添加复合索引                        │  🔊 DBA (数据库)     │
│                                                          │  ○  前端 (前端)      │
│  🔊 DBA:                                                 │  阶段 ● 发言中       │
│  补充几点：                                               │                      │
│  - EXPLAIN 显示全表扫描，缺少 (user_id, created_at) 索引   │                      │
│  - 建议分区表（按月），历史数据走冷存储                      │                      │
│                                                          │                      │
├──────────────────────────────────────────────────────────┬──────────────────────┤
│ ╭──────────────────────────────────────────────────────╮ │ deepseek(✓) · v4     │
│ │ ⠹ Working 会诊 · DBA 发言中  5.1s                   │ │ ⬡ 92K/128K/1M · 5轮 │
│ │ Ask anything...                             ⏎ Enter │ │ thinking · ¥0.56     │
│ ╰──────────────────────────────────────────────────────╯ │                      │
├──────────────────────────────────────────────────────────┴──────────────────────┤
│ ● 会诊 · DBA 发言中                                6.2K tok  Cmd+↑↓ Ctrl+B Esc │
└─────────────────────────────────────────────────────────────────────────────────┘
```

## Prerequisites

Abacus is a pre-compiled binary with **no runtime dependencies**. You only need:

- **macOS** 12+ (Monterey) or **Linux** (glibc 2.31+, e.g. Ubuntu 20.04+)
- A terminal emulator (Terminal.app, iTerm2, Warp, Kitty, Alacritty, etc.)
- An LLM API key (DeepSeek / OpenAI / Anthropic / any OpenAI-compatible endpoint)

For building from source, you additionally need:
- Rust 1.75+ (`curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`)
- A C linker (macOS: Xcode Command Line Tools; Linux: `build-essential`)
- Protocol Buffers compiler (`brew install protobuf` on macOS; `apt install protobuf-compiler` on Linux)

## Install

### Option 1: One-line Install (Recommended)

```bash
curl -fsSL https://github.com/lucasli000/abacus/releases/latest/download/install.sh | sh
```

This auto-detects your platform (macOS/Linux, ARM64/x86_64), downloads the correct binary, and installs to `/usr/local/bin/`.

To install to a custom path:

```bash
INSTALL_DIR=~/.local/bin curl -fsSL https://github.com/lucasli000/abacus/releases/latest/download/install.sh | sh
```

### Option 2: Manual Download

Go to [Releases](https://github.com/lucasli000/abacus/releases/latest) and download for your platform:

| Platform | File | Size |
|----------|------|------|
| macOS Apple Silicon (M1/M2/M3/M4) | `abacus-aarch64-apple-darwin.tar.gz` | ~19 MB |
| macOS Intel | `abacus-x86_64-apple-darwin.tar.gz` | ~20 MB |
| Linux x86_64 | `abacus-x86_64-unknown-linux-gnu.tar.gz` | ~22 MB |
| Linux ARM64 | `abacus-aarch64-unknown-linux-gnu.tar.gz` | ~21 MB |

Then install:

```bash
# 1. Extract
tar -xzf abacus-aarch64-apple-darwin.tar.gz

# 2. Move to PATH
sudo mv abacus /usr/local/bin/

# 3. (macOS only) Remove quarantine flag
xattr -d com.apple.quarantine /usr/local/bin/abacus 2>/dev/null || true

# 4. Verify
abacus
```

### Option 3: Build from Source

```bash
# Install Rust (if not already)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source ~/.cargo/env

# Clone and build
git clone https://github.com/lucasli000/abacus.git
cd abacus/pkg
cargo build --release --package abacus-cli

# Install
sudo cp target/release/abacus /usr/local/bin/
```

### First Run

```bash
# Start Abacus — it launches a full-screen TUI
abacus
```

On first run, a setup wizard will guide you through:
1. Choosing an LLM provider (DeepSeek / OpenAI / Anthropic / custom)
2. Entering your API key
3. Selecting a default model

Config is saved to `~/.abacus/config.yaml`. You can skip the wizard in CI:

```bash
export ABACUS_API_KEY=sk-xxx
```

### Verify Installation

If Abacus launches and shows the TUI interface, installation is successful. The program uses the full terminal screen — press `Ctrl+D` or type `/quit` to exit.

### Uninstall

```bash
rm /usr/local/bin/abacus
rm -rf ~/.abacus  # Remove config + data (optional)
```

### Troubleshooting

| Problem | Solution |
|---------|----------|
| `zsh: killed abacus` | macOS code signing issue. Run: `codesign --sign - --force /usr/local/bin/abacus` |
| `permission denied` | Run: `chmod +x /usr/local/bin/abacus` |
| `command not found` | Ensure `/usr/local/bin` is in your `$PATH` |
| `"abacus" cannot be opened` (macOS) | Run: `xattr -d com.apple.quarantine /usr/local/bin/abacus` |
| `Error in the HTTP2 framing layer` | Network issue (common in China). Use mirror: see below |
| Blank screen / no output | Ensure your terminal supports alternate screen (most modern terminals do) |

**China mainland users** — if downloads fail due to network issues:

```bash
# Option A: Use mirror proxy
curl -fsSL https://gh-proxy.com/https://github.com/lucasli000/abacus/releases/download/v1.2.0/abacus-aarch64-apple-darwin.tar.gz | tar -xz
sudo mv abacus /usr/local/bin/

# Option B: Use local SOCKS proxy
curl -x socks5://127.0.0.1:7890 -fsSL https://github.com/lucasli000/abacus/releases/latest/download/install.sh | sh
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

Auto-detects 9 providers from URL.

## Configuration

Abacus uses layered configuration files in `~/.abacus/`:

| File | Purpose |
|------|---------|
| `config.yaml` | LLM providers + core behavior (main config) |
| `models.yaml` | Model capabilities override (context_window, thinking, etc.) |
| `security.yaml` | MCIP permissions, safety rules |
| `policy.toml` | LLM behavioral constraints (entropy guard, thresholds) |
| `conf.d/*.yaml` | Custom overrides (loaded last, highest priority) |

### Minimal config (recommended)

```yaml
# ~/.abacus/config.yaml
providers:
  - id: primary
    type: openai-compatible    # openai-compatible / anthropic / deepseek
    api_key: "your-api-key"
    base_url: "https://api.deepseek.com"
    models:
      - name: deepseek-chat
        context_window: 128000

core:
  default_model: "deepseek-chat"
  stream: true
```

Everything else uses sensible defaults. Override only what you need.

### Per-model settings

Context window and thinking mode follow the model, not global config:

```yaml
providers:
  - id: primary
    models:
      - name: deepseek-chat
        context_window: 128000
        thinking: medium         # off / low / medium / high / max
        max_tokens: 64000
      - name: deepseek-reasoner
        context_window: 64000
        thinking: high
        max_tokens: 32000
```

### Advanced config keys

<details>
<summary>Click to expand full reference</summary>

```yaml
core:
  silent_router_enabled: true        # Smart tool routing
  task_kind_routing: true            # Filter tools by task type
  tool_frequency_pruning_turns: 20   # Hide unused tools after N turns (0=off)
  scene_tool_loading: true           # Scene-based tool loading
  adaptive_d_tier_hide: true         # Auto-hide low-effectiveness tools
  event_sink_enabled: true           # Cross-session event log
  max_escalations: 10                # Model upgrade budget per session
  dedup:
    enabled: true                    # Tool result deduplication cache
    ttl_secs: 60
    capacity_kb: 2048

deduction:
  observer_contamination: true
  cross_session: true
  prompt_impact: true
  context_degradation: true

epistemic:
  threshold: 3                       # Violations before forced declaration

palace:
  enabled: true                      # Memory palace
  sync_interval_turns: 5             # Write frequency (0=off)

lsp:
  enabled: true
code_graph:
  enabled: true
```

</details>

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
