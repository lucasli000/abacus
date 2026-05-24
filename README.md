# Abacus

[![Version](https://img.shields.io/badge/version-1.0.0-blue.svg)](https://github.com/lucasli000/abacus/releases/tag/v1.0.0)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/Rust-1.75%2B-orange.svg)](https://www.rust-lang.org)
[![CI](https://github.com/lucasli000/abacus/actions/workflows/ci.yml/badge.svg)](https://github.com/lucasli000/abacus/actions/workflows/ci.yml)

LLM Agent Kernel — 模块化 Agent 运行时，支持 Chat / Team / Meeting 三种交互模式。

## Quick Start

```bash
# 1. 设置 API key
export ABACUS_API_KEY=sk-xxx
# 或
export DEEPSEEK_API_KEY=sk-xxx

# 2. 进入交互式 TUI
cargo run --bin abacus
# 或直接 chat
cargo run --bin abacus -- chat --model deepseek-v4-flash

# 3. 生成 Shell 补全
eval "$(cargo run --bin abacus -- completions bash)"
```

## Features

| 模块 | 功能 |
|------|------|
| **Chat** | 单 Agent 对话，实时思考 + 工具调用渲染 |
| **Team** | 多角色协同执行，任务自动分配+顺序仿真 |
| **Meeting** | 专家会议模式，并发推理+主持人分发汇总 |
| **CLI** | Rustyline 行编辑、Shell 补全、消息复制 |
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

```bash
# Build from source
cd pkg && cargo build --release

# Run TUI
./target/release/abacus

# Run HTTP server
./target/release/abacus-server
```

### Docker

```bash
docker compose up -d
curl http://localhost:8080/api/v1/health
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
