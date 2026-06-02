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

### Quick Install (Recommended)

One command, auto-detects your platform:

```bash
curl -fsSL "https://github.com/lucasli000/abacus/releases/download/v1.3/install.sh" | bash
```

Default install path: `/usr/local/bin/abacus`. Custom path:

```bash
INSTALL_DIR=~/.local/bin curl -fsSL "https://github.com/lucasli000/abacus/releases/download/v1.3/install.sh" | bash
```

### Manual Download

Download from [Releases](https://github.com/lucasli000/abacus/releases/latest) and install:

| Platform | File |
|----------|------|
| macOS Apple Silicon | `abacus-aarch64-apple-darwin.tar.gz` |
| Linux x86_64 | `abacus-x86_64-unknown-linux-gnu.tar.gz` |
| Linux ARM64 | `abacus-aarch64-unknown-linux-gnu.tar.gz` |

```bash
# macOS Apple Silicon
curl -fsSL -o abacus.tar.gz "https://github.com/lucasli000/abacus/releases/download/v1.3/abacus-aarch64-apple-darwin.tar.gz"
tar xzf abacus.tar.gz
sudo mv abacus /usr/local/bin/
sudo xattr -cr /usr/local/bin/abacus   # Remove quarantine (macOS)
rm abacus.tar.gz

# Verify
abacus --version
```

### Using gh CLI

If you have GitHub CLI installed (works even when curl to GitHub fails):

```bash
gh release download v1.3 -R lucasli000/abacus -p "abacus-aarch64-apple-darwin.tar.gz"
tar xzf abacus-aarch64-apple-darwin.tar.gz
sudo mv abacus /usr/local/bin/
sudo xattr -cr /usr/local/bin/abacus
```

### From Source

Requires Rust 1.75+ and protoc:

```bash
git clone https://github.com/lucasli000/abacus.git
cd abacus/pkg
cargo install --path crates/abacus-cli
```

### Upgrade

Same install command — only the binary is replaced, your `~/.abacus/` config and data are preserved.

### Uninstall

```bash
sudo rm -f /usr/local/bin/abacus        # Remove binary
rm -rf ~/.abacus                         # Remove config + data (optional)
```

### Supported Platforms

| Platform | Architecture | Binary Size | Status |
|----------|-------------|-------------|--------|
| macOS | Apple Silicon (arm64) | ~9MB | ✅ Primary |
| Linux | x86_64 (glibc) | ~11MB | ✅ |
| Linux | aarch64 (glibc) | ~10MB | ✅ |

> Binary is fully self-contained — no runtime dependencies. SQLite bundled, TLS via rustls, all resources embedded.

### Troubleshooting Install

| Problem | Solution |
|---------|----------|
| `curl: (56) 403` | GitHub CDN blocked — use `gh release download` or set proxy `export https_proxy=http://127.0.0.1:7890` |
| `no such file or directory` | New Mac missing /usr/local/bin — `sudo mkdir -p /usr/local/bin` |
| `killed` / macOS blocks binary | Run `sudo xattr -cr /usr/local/bin/abacus` then retry |
| `codesign internal error` | Skip codesign — just use `xattr -cr` instead |

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

### First Run — Setup Wizard

First launch auto-detects missing config and triggers an interactive wizard:

1. Select provider type (Anthropic / OpenAI / DeepSeek / OpenRouter / Custom)
2. Paste API key
3. Confirm base URL (Enter for default)

This generates `~/.abacus/providers.json`. You can re-run the wizard anytime with `/config`.

### providers.json (LLM Providers)

```json
[
  {
    "id": "deepseek",
    "type": "deepseek",
    "api_key": "sk-...",
    "base_url": "https://api.deepseek.com",
    "models": ["deepseek-v4-pro", "deepseek-v4-flash"]
  },
  {
    "id": "openrouter",
    "type": "openai-compatible",
    "api_key": "env:OPENROUTER_API_KEY",
    "base_url": "https://openrouter.ai/api/v1/",
    "models": ["anthropic/claude-sonnet-4", "meta-llama/llama-3.3-70b-instruct:free"]
  }
]
```

- `api_key` supports `"env:VAR_NAME"` to read from environment variables
- `type`: `openai-compatible` (covers most providers), `deepseek`, `anthropic`
- Multiple providers can be configured simultaneously — switch with `/model provider:model`

### config.yaml (Core Behavior)

```yaml
core:
  default_model: "deepseek/deepseek-v4-pro"   # provider_id/model_name
  temperature: 0.3
  max_tokens: 16384
  thinking: low          # off / low / medium / high
  stream: true
  context_window: 1000000
```

### File Layout

```
~/.abacus/
├── providers.json       # LLM provider config (API keys, models)
├── config.yaml          # Core behavior settings
├── safety_rules.yaml    # Custom safety rules (optional)
└── data/                # Sessions, knowledge DBs, etc.
```

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
