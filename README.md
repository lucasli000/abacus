<p align="center">
  <img src="assets/icon-256.png" width="120" alt="Abacus Logo" />
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
│  │ ● Ready · 澄清                            ⏎ Enter │     │
│  ╰─────────────────────────────────────────────────────╯     │
└──────────────────────────────────────────────────────────────┘
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
