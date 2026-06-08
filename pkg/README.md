<p align="center">
  <img src="assets/icon-256.png" width="120" alt="AbacusCode Logo" />
</p>

<h1 align="center">AbacusCode</h1>

<p align="center">
  <strong>Production-grade LLM Agent Kernel for Developers</strong><br>
  Multi-mode orchestration · Dual-palace memory · CardStream rendering · 17 providers
</p>

<p align="center">
  <a href="https://github.com/lucasli000/abacus/releases/latest"><img src="https://img.shields.io/github/v/release/lucasli000/abacus?style=flat-square&color=blue" alt="Release"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-MIT-blue?style=flat-square" alt="License"></a>
  <img src="https://img.shields.io/badge/rust-1.75+-orange?style=flat-square&logo=rust" alt="Rust">
  <img src="https://img.shields.io/badge/platform-macOS%20%7C%20Linux%20%7C%20Windows-lightgrey?style=flat-square" alt="Platform">
  <img src="https://img.shields.io/badge/tests-1490%20pass-brightgreen?style=flat-square" alt="Tests">
</p>

---

## What is AbacusCode?

AbacusCode is a terminal-native LLM agent kernel that orchestrates AI reasoning across four collaborative modes. It provides a rich TUI experience with real-time streaming, tool execution, multi-expert consultation, and a dual-palace memory system — all from your terminal.

```
┌──────────────────────────────────────────────────────────────┐
│  ⠋ ABACUS ▸ Refactor auth module · 澄清                     │
├──────────────────────────────────────────────────────────────┤
│                                                              │
│  User: 帮我把 session 认证改成 JWT                            │
│                                                              │
│  Session:                                                    │
│  ## 方案确认                                                  │
│  我决定使用 JWT token 替代 session cookie。                    │
│  原因：无状态、易扩展、前后端解耦。                              │
│                                                              │
│  ```rust                                                     │
│  impl AuthService {                                          │
│      pub fn verify_token(&self, token: &str) -> Result<...>  │
│  }                                                           │
│  ```                                                         │
│                                                              │
├──────────────────────────────────────────────────────────────┤
│  ╭─────────────────────────────────────────────────────╮     │
│  │ ● Ready · 澄清           输入 A/S/T/C...  ⏎ Enter │     │
│  ╰─────────────────────────────────────────────────────╯     │
└──────────────────────────────────────────────────────────────┘
```

---

## Features

### Four Orchestration Modes

| Mode | Description | Use Case |
|------|-------------|----------|
| **Clarify** | Single-agent deep reasoning with progressive gate | Default — ask questions, write code, debug |
| **Plan** | Two-phase state machine: Research → Approval → Execute | Complex multi-step tasks requiring strategy selection |
| **Team** | Multi-agent parallel execution with ToolAgent delegation | Large-scale refactoring, parallel tool operations |
| **Meeting** | Multi-expert weighted consultation | Cross-domain problems needing diverse expertise |

### Core Engine

- **CardStream Rendering** — V42-B card-based streaming with LlmCard, AbacusCard, UserCard, ExpertCard
- **Dual-Palace Memory** — BehaviorPalace (interaction patterns) + KnowledgePalace (domain knowledge) with SM-2 spaced repetition
- **Knowledge Store** — FTS5 full-text search + semantic search with chunking
- **Skill Engine** — Multi-strategy matching (keyword, regex, domain, semantic) with palace-enhanced evaluation
- **Tool Registry** — 30+ built-in tools with lazy loading, visibility tiers (S/A/B/C/D), panic isolation
- **Progressive Gate** — Complexity-aware output strategy adapting verbosity based on task difficulty
- **Dynamic Timeout** — Complexity + tool count + LLM self-extension with runtime adjustment
- **Middleware Chain** — Priority-ordered MagChain (CircuitBreaker → RateLimiter → EpistemicGuard → PiiRedactor → AuditLogger)

### TUI Experience

- Real-time streaming with thinking visualization
- CardStream-based rendering (LlmCard, AbacusCard, UserCard, ExpertCard)
- Right panel: status dashboard + timeline + focus context + memory palace
- 12 built-in themes (brand, light, apple, google, monokai, dracula, nord, gruvbox, catppuccin, tokyo-night, solarized-dark, one-dark)
- Shell completions (bash/zsh/fish)
- Inline suggestions with Tab completion
- Session management with auto-save and restore

---

## Install

### Quick Install (Recommended)

One command, auto-detects your platform:

```bash
curl -fsSL https://github.com/lucasli000/abacus/releases/latest/download/install.sh | sh
```

Default install path: `/usr/local/bin/abacus`. Custom path:

```bash
INSTALL_DIR=~/.local/bin curl -fsSL https://github.com/lucasli000/abacus/releases/latest/download/install.sh | sh
```

### Manual Download

Download the binary for your platform from [Releases](https://github.com/lucasli000/abacus/releases/latest):

| Platform | Architecture | File |
|----------|-------------|------|
| macOS | Apple Silicon (arm64) | `abacus-aarch64-apple-darwin.tar.gz` |
| Linux | x86_64 | `abacus-x86_64-linux-gnu.tar.gz` |
| Windows | x86_64 | `abacus-x86_64-windows-msvc.zip` |

```bash
# macOS Apple Silicon
tar -xzf abacus-aarch64-apple-darwin.tar.gz
sudo mv abacus /usr/local/bin/
chmod +x /usr/local/bin/abacus

# Linux x86_64
tar -xzf abacus-x86_64-linux-gnu.tar.gz
sudo mv abacus /usr/local/bin/

# Windows (PowerShell)
Expand-Archive abacus-x86_64-windows-msvc.zip
# Add to PATH

# Verify
abacus --version
```

### From Source

Requires Rust 1.75+:

```bash
git clone https://github.com/lucasli000/abacus.git
cd abacus/pkg
cargo install --path crates/abacus-cli
```

### Supported Platforms

| Platform | Architecture | Binary Size | Status |
|----------|-------------|-------------|--------|
| macOS | Apple Silicon (arm64) | ~19MB | ✅ Primary |
| Linux | x86_64 (glibc) | ~22MB | ✅ |
| Windows | x86_64 | ~20MB | ✅ |

> Binary is fully self-contained — no runtime dependencies. SQLite bundled, TLS via rustls, all resources embedded.

---

## Quick Start

### 1. First Launch

```bash
abacus
```

First run launches an interactive setup wizard:

```
┌─ 首次配置 ──────────────────────────────────────────────────┐
│                                                              │
│  ▄     ▄        ▄                                          │
│  █  █  ▄  █  █  █  █  ▄  █                                │
│  █  ▀  █  █  █  █  ▀  █  █                                │
│        ▀              ▀                                    │
│       A  B  A  C  U  S                                      │
│                                                              │
│  使用须知                                                    │
│  ┌──────────────────────────────────────────────────────┐   │
│  │ 1. 数据安全 — AI 操作可能具有破坏性，请备份重要数据    │   │
│  │ 2. 人工审查 — AI 生成的代码可能存在缺陷              │   │
│  │ 3. 合规使用 — 严禁用于恶意攻击或非法用途             │   │
│  └──────────────────────────────────────────────────────┘   │
│                                                              │
│    API URL （DeepSeek API）                                  │
│  ┌──────────────────────────────────────────────────────┐   │
│  │ https://api.deepseek.com                             │   │
│  └──────────────────────────────────────────────────────┘   │
│                                                              │
│    API Key                                                   │
│  ┌──────────────────────────────────────────────────────┐   │
│  │ ••••••••••••••••••••                                  │   │
│  └──────────────────────────────────────────────────────┘   │
│                                                              │
│    默认模型 (Tab 循环选择)                                    │
│  ┌──────────────────────────────────────────────────────┐   │
│  │ deepseek-v4-pro                                      │   │
│  └──────────────────────────────────────────────────────┘   │
│                                                              │
│  Tab 切换字段 · Enter 确认 · Esc 退出 · Ctrl+H 显示/隐藏 Key│
└──────────────────────────────────────────────────────────────┘
```

### 2. Configure Provider

```bash
# Interactive setup
abacus

# Or manually edit config
abacus config edit
```

### 3. Start Coding

```bash
# Interactive TUI (default)
abacus

# Single query mode
abacus ask "explain this error"

# Specify model
abacus --model deepseek-v4-pro ask "optimize this function"

# Version info
abacus --version
```

---

## Configuration

### Config Files

| File | Purpose | Location |
|------|---------|----------|
| `config.toml` | Core settings (model, temperature, limits) | `~/.abacus/config.toml` |
| `provider.toml` | LLM provider configuration | `~/.abacus/provider.toml` |
| `models.toml` | Model capability overrides | `~/.abacus/models.toml` |
| `security.toml` | Safety/MCIP/sandbox settings | `~/.abacus/security.toml` |
| `abacusbr.md` | Behavior rules (coding standards) | `~/.abacus/abacusbr.md` |
| `mcp_servers.toml` | MCP server configuration | `~/.abacus/mcp_servers.toml` |
| `conf.d/` | Extension configs (*.toml) | `~/.abacus/conf.d/` |

### Config Priority (highest to lowest)

1. CLI arguments (`--key value`)
2. Environment variables (`ABACUS_*`)
3. TOML files (`~/.abacus/*.toml`)
4. Built-in defaults

### Environment Variables

| Variable | Purpose | Example |
|----------|---------|---------|
| `ABACUS_HOME` | Config root directory | `/custom/path` |
| `DEEPSEEK_API_KEY` | DeepSeek API key | `sk-...` |
| `ANTHROPIC_API_KEY` | Anthropic API key | `sk-ant-...` |
| `ABACUS_OPENAI_API_KEY` | OpenAI-compatible key | `sk-...` |
| `ABACUS_LANG` | UI language | `zh` / `en` |
| `ABACUS_THEME` | TUI theme | `dark` / `light` |

### TUI Commands

| Command | Description |
|---------|-------------|
| `/model [name]` | Switch model or open picker |
| `/model thinking [level]` | Set thinking depth (off/low/medium/high/max) |
| `/set <key> <value>` | Adjust runtime parameters |
| `/preset [name]` | Apply scene preset (quick/code/creative/lean/marathon/debug) |
| `/theme [name]` | Switch theme |
| `/lang [zh\|en]` | Switch language |
| `/streaming` | Toggle streaming output |
| `/budget` | Show resource budget |
| `/context` | Show context state |
| `/doctor` | System health check |

---

## Multi-Provider Support

17 providers supported with automatic detection:

| Provider | Default Model | Base URL |
|----------|--------------|----------|
| DeepSeek | deepseek-v4-pro | `https://api.deepseek.com` |
| OpenAI | gpt-4o | `https://api.openai.com/v1` |
| Anthropic | claude-sonnet-4-20250514 | `https://api.anthropic.com` |
| Dashscope | qwen3.7-max | `https://dashscope.aliyuncs.com/compatible-mode/v1` |
| Moonshot | kimi-k2.6 | `https://api.moonshot.cn/v1` |
| Zhipu | glm-5.1 | `https://open.bigmodel.cn/api/paas/v4` |
| ZhipuCoding | glm-5.1 | `https://open.bigmodel.cn/api/coding/paas/v4` |
| SiliconFlow | deepseek-v4-pro | `https://api.siliconflow.cn/v1` |
| Groq | llama-3.3-70b-versatile | `https://api.groq.com/openai/v1` |
| Volcengine | deepseek-v4-pro | `https://ark.cn-beijing.volces.com/api/v3` |
| VolcengineCoding | deepseek-v4-pro | `https://ark.cn-beijing.volces.com/api/coding/v3` |
| Tencent | hunyuan-turbo | `https://api.hunyuan.cloud.tencent.com/v1` |
| MiniMax | MiniMax-M3 | `https://api.minimax.chat/v1` |
| Yi | yi-lightning | `https://api.lingyiwanwu.com/v1` |
| Baichuan | Baichuan4 | `https://api.baichuan-ai.com/v1` |
| Ollama | llama3.3 | `http://localhost:11434/v1` |
| OpenCodeZen | opencode/deepseek-v4-flash | `https://opencode.ai/zen/v1` |

### Model Shortcuts

```
/pro          → deepseek-v4-pro
/flash        → deepseek-v4-flash
/qwen         → qwen3.7-max
/glm          → glm-5.1
/kimi         → kimi-k2.6
/claude       → claude-sonnet-4-20250514
/opus         → claude-opus-4-20250514
```

---

## Architecture

```
abacus/pkg/
├── crates/
│   ├── abacus-types/        Pure data types (L0)
│   ├── abacus-core/         Engine kernel (L1+L2)
│   │   ├── core/            Pipeline, context, safety, compression
│   │   ├── llm/             Multi-provider LLM abstraction
│   │   ├── tool/            Built-in tool registry (30+ tools)
│   │   ├── skill/           Skill engine with multi-strategy matching
│   │   ├── memory_palace/   Dual-palace memory system
│   │   └── knowledge_store/ FTS5 + semantic search
│   ├── abacus-orchestrator/ Team/Meeting orchestration (L3)
│   ├── abacus-ui-kit/       Shared UI primitives (L3.5)
│   ├── abacus-cli/          TUI + CLI interface (L4)
│   └── abacus-server/       HTTP/SSE server mode (L4)
├── config.example.toml      Config reference
├── provider.example.toml    Provider config reference
└── scripts/                 Install script
```

### Key Design Decisions

- **Layered architecture** — Strict L0→L4 dependency (types → core → orchestrator → UI)
- **Single binary** — ~19MB self-contained, no runtime dependencies
- **SQLite bundled** — No external database required
- **TLS via rustls** — No OpenSSL dependency
- **All resources embedded** — i18n, themes, syntax highlighting compiled in
- **SSOT pattern** — Every data concept has one owner
- **Graceful degradation** — Never crash; fall back to simpler behavior

---

## Development

```bash
# Check
cargo check --workspace

# Test
cargo test --workspace

# Build release
cargo build --release

# Run
cargo run --bin abacus
```

### Project Structure

```
crates/
├── abacus-types/     # Pure data types (KernelError, ToolId, SkillDef, etc.)
├── abacus-core/      # Core engine (CoreLoop, Pipeline, ToolRegistry, etc.)
├── abacus-orchestrator/ # Multi-agent orchestration (Team, Meeting, Plan)
├── abacus-ui-kit/    # Shared UI primitives (CardStream, Theme)
├── abacus-cli/       # TUI (ratatui) + CLI (clap)
└── abacus-server/    # HTTP server (axum) + SSE streaming
```

### Testing

```bash
# All tests
cargo test --workspace

# Specific crate
cargo test -p abacus-core

# With output
cargo test --workspace -- --nocapture
```

---

## License

MIT — see [LICENSE](LICENSE)

---

## Acknowledgments

- [ratatui](https://github.com/ratatui-org/ratatui) — TUI framework
- [tokio](https://github.com/tokio-rs/tokio) — Async runtime
- [SQLite](https://www.sqlite.org/) — Embedded database
- [rustls](https://github.com/rustls/rustls) — TLS implementation
