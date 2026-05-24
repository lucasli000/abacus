# Abacus

[![Version](https://img.shields.io/badge/version-1.0.0-blue.svg)](https://github.com/lucasli000/abacus/releases/tag/v1.0.0)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/Rust-1.75%2B-orange.svg)](https://www.rust-lang.org)
[![CI](https://github.com/lucasli000/abacus/actions/workflows/ci.yml/badge.svg)](https://github.com/lucasli000/abacus/actions/workflows/ci.yml)

LLM Agent Kernel — 模块化 Agent 运行时，TUI 内置 Clarify → Plan → Team → Meeting 四阶交互模式 DAG。

## Quick Start

已装好 abacus（见 [Installation](#installation)）后：

```bash
export ABACUS_API_KEY=sk-xxx        # 或 DEEPSEEK_API_KEY=sk-xxx
abacus                              # 进入 TUI（默认 Clarify 模式）
```

零起点首次安装请直接看下方 Installation 节。

## Features

### TUI 交互模式（4 阶 DAG）

| 模式 | 功能 |
|------|------|
| **Clarify** | 默认入口——澄清需求，agent 通过提问消除歧义 |
| **Plan** | 规划任务——Planner agent 将需求拆解为 TaskSpec[] |
| **Team** | 执行任务——多 agent 并行执行，消费上阶段产出 |
| **Meeting** | 专家会诊——多专家并行发言，综合得出讨论结论 |

### 系统能力

| 模块 | 功能 |
|------|------|
| **CLI** | Rustyline 行编辑、Shell 补全、消息复制、`abacus chat/team/meeting` 子命令 |
| **HTTP Server** | REST API + SSE 流式推送，Bearer token 认证 |
| **Config** | 首次配置向导、config.yaml、环境变量覆盖 |

## Architecture

```
abacus-core       — Agent kernel: CoreLoop, LLM providers, tools, skills
abacus-types      — Shared types: models, errors, sandbox, progressive
abacus-orchestrator — Team/Meeting engine: specialists, sub-agents, sessions
abacus-server     — HTTP REST + SSE server (axum)
abacus-cli        — CLI + TUI (ratatui + crossterm)
```

## Installation

### 1. 前置依赖

Rust 1.75+。未装：

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

### 2. 从源码编译

```bash
git clone https://github.com/lucasli000/abacus.git
cd abacus/pkg
cargo build --release
# 产物：./target/release/{abacus, abacus-server}
```

### 3. 入 PATH（可选）

```bash
cargo install --path crates/abacus-cli      # abacus（CLI + TUI）
cargo install --path crates/abacus-server   # abacus-server（HTTP）
# 或直接复制：cp ./target/release/abacus ~/.local/bin/
```

### 4. 配置 API Key

```bash
export ABACUS_API_KEY=sk-xxx                # 或
export DEEPSEEK_API_KEY=sk-xxx
```

### 5. 验证安装

```bash
abacus --version                            # 1.0.0
abacus chat -m "ping"                       # 端到端冒烟
```

### Shell 补全（可选）

```bash
eval "$(abacus completions bash)"           # bash
abacus completions zsh > ~/.zsh/completions/_abacus
abacus completions fish > ~/.config/fish/completions/abacus.fish
```

### Docker（HTTP server 场景）

```bash
git clone https://github.com/lucasli000/abacus.git
cd abacus
docker compose up -d
curl http://localhost:8080/api/v1/health    # 验证
```

### 卸载

```bash
cargo uninstall abacus-cli abacus-server
rm -rf ~/.abacus/                           # 清理配置 + sessions + sqlite
```

## Configuration

| 方式 | 示例 |
|------|------|
| 环境变量 | `ABACUS_API_KEY=sk-xxx` `ABACUS_SERVER_TOKEN=secret` |
| config.yaml | `~/.abacus/config.yaml`（首次启动自动生成） |
| CLI | `abacus config list-keys` / `abacus config set key val` |

完整配置模板见 [`config.example.toml`](config.example.toml)。

**必配项**：`ABACUS_API_KEY` 或 `DEEPSEEK_API_KEY`（二选一）

## CoreLoop API

```rust
let (core, session) = create_engine("deepseek-v4-flash", None, "high").await?;
let result = core.process_turn("Write a Rust quick sort", &session).await?;
println!("{}", result.response);
```

## Development

```bash
# Full check
cargo check --workspace
cargo test --workspace
cargo clippy --workspace -- -D warnings

# Run single crate
cargo run -p abacus-cli -- chat -m "hello"
```

## License

MIT
