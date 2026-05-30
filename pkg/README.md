<p align="center">
  <img src="assets/icon-256.png" width="120" alt="Abacus Logo" />
</p>

<h1 align="center">Abacus</h1>

<p align="center">
  <strong>Production-grade LLM Agent Kernel</strong><br>
  Multi-mode orchestration · Mathematical context compression · Built-in safety
</p>

<p align="center">
  <a href="https://github.com/lucasli000/abacus/releases/latest"><img src="https://img.shields.io/github/v/release/lucasli000/abacus?style=flat-square&color=blue" alt="Release"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-MIT-blue?style=flat-square" alt="License"></a>
  <img src="https://img.shields.io/badge/rust-1.75+-orange?style=flat-square&logo=rust" alt="Rust">
  <img src="https://img.shields.io/badge/platform-macOS%20%7C%20Linux-lightgrey?style=flat-square" alt="Platform">
</p>

---

## What is Abacus?

Abacus is a terminal-native LLM agent kernel that orchestrates AI reasoning across four collaborative modes. It provides a rich TUI experience with real-time streaming, tool execution, and multi-expert consultation — all from your terminal.

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

## Features

### Four Orchestration Modes

| Mode | Description | Use Case |
|------|-------------|----------|
| **Clarify** | Single-agent deep reasoning with progressive gate | Default mode — ask questions, write code, debug |
| **Plan** | Two-phase state machine: Research → Approval → Execute | Complex multi-step tasks requiring user strategy selection |
| **Team** | Multi-agent parallel execution with ToolAgent delegation | Large-scale refactoring, parallel tool operations |
| **Meeting** | Multi-expert weighted consultation | Cross-domain problems needing diverse expertise |

### Core Engine

- **Mathematical Context Compression** — Information Bottleneck + H2O Heavy-Hitter + ARC adaptive cache + Greedy Knapsack selection. Proven 38% message reduction with 100% key decision retention.
- **ToolActionClassifier** — Rule-based safety evaluator (hard_deny → soft_deny → allow_rules). Zero LLM overhead, prevents destructive operations.
- **ToolAgent** — Automatic batch delegation for read-only tool operations. Reduces message flow noise, 4 built-in types (Explorer, Researcher, Coder, Mathematician).
- **MCIP** — Multi-level permission gate (role → confirm → capability). Supports Once/Always authorization with grant-and-rerun.
- **ProgressiveGate** — Complexity-aware output strategy. Adapts verbosity based on task difficulty profile.
- **Dynamic Timeout** — Complexity + tool count + LLM self-extension. Configurable max with runtime adjustment.

### TUI Experience

- Real-time streaming with thinking visualization
- Soft-wrap input bar with cursor tracking
- Right panel: status dashboard + timeline + focus context
- Theme support (12 built-in themes)
- Shell completions (bash/zsh/fish)
- Inline suggestions with Tab completion

## Install

### Quick Install (macOS / Linux)

```bash
curl -fsSL https://github.com/lucasli000/abacus/releases/latest/download/install.sh | sh
```

### From Source

```bash
git clone https://github.com/lucasli000/abacus.git
cd abacus/pkg
cargo install --path crates/abacus-cli
```

### Supported Platforms

| Platform | Architecture | Status |
|----------|-------------|--------|
| macOS | Apple Silicon (arm64) | ✅ Primary |
| macOS | Intel (x86_64) | ✅ |
| Linux | x86_64 | ✅ |
| Linux | aarch64 | ✅ |

## Usage

```bash
# Interactive TUI (default)
abacus

# Single query mode
abacus ask "explain this error"

# Specify model + provider
abacus --model deepseek-v4 ask "optimize this function"

# Version info (includes git hash)
abacus --version
# → abacus 1.0.0 (3b43be7d 2026-05-30)

# Shell completions
abacus completions zsh > ~/.zfunc/_abacus
```

### TUI Keyboard Shortcuts

| Key | Action |
|-----|--------|
| `Enter` | Send message |
| `Ctrl+B` | Toggle right panel |
| `Ctrl+D` | Exit |
| `Esc` | Cancel current operation |
| `Cmd+↑↓` | Scroll message history |
| `Tab` | Accept inline suggestion |

### Mode Switching

```bash
/clarify          # Single-agent mode (default)
/plan <goal>      # Two-phase plan execution
/team <task>      # Multi-agent parallel
/meeting          # Multi-expert consultation
@expert_name msg  # Auto-route to Meeting mode
```

## Architecture

```
abacus/pkg/
├── crates/
│   ├── abacus-cli/          TUI + CLI interface (ratatui + clap)
│   ├── abacus-core/         Engine kernel
│   │   ├── core/            Pipeline, context, safety, compression
│   │   ├── llm/             Multi-provider LLM abstraction
│   │   ├── tool/            Built-in tool registry (30+ tools)
│   │   └── ...
│   ├── abacus-orchestrator/ Meeting/Team orchestration
│   ├── abacus-types/        Shared type definitions
│   └── abacus-server/       HTTP/SSE server mode
├── assets/                  Logo + icons
└── scripts/                 Install script
```

### Key Design Decisions

- **No external LLM calls for safety** — ToolActionClassifier uses rule engine (zero overhead)
- **SQLite bundled** — No runtime database dependency
- **TLS via rustls** — No OpenSSL required
- **All resources embedded** — i18n, themes, syntax highlighting compiled in
- **Single binary** — ~19MB self-contained, no runtime dependencies beyond system libs

## Configuration

First run launches an interactive setup wizard:

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
│  4. Optional features                       │
│     □ Meeting mode (multi-expert)           │
│     □ Code graph (project indexing)         │
│     □ Memory palace (long-term memory)      │
│                                              │
└──────────────────────────────────────────────┘
```

Config stored at `~/.abacus/config.yaml`. Supports multiple providers simultaneously.

## Multi-Provider Support

| Provider | Models | Status |
|----------|--------|--------|
| OpenAI | GPT-4o, o1, o3 | ✅ |
| Anthropic | Claude Sonnet/Opus | ✅ |
| DeepSeek | V3, V4, R1 | ✅ |
| Google | Gemini 2.x | ✅ |
| OpenAI-compatible | Any | ✅ |

## Development

```bash
# Check
cargo check --workspace

# Test
cargo test --workspace

# Build release
make build

# Package for distribution
make package

# macOS universal binary
make universal
```

## License

MIT — see [LICENSE](LICENSE)
