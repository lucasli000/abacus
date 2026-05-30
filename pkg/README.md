# Abacus

Production-grade LLM Agent kernel with multi-mode orchestration.

## Features

- **Clarify Mode** — Single-agent deep reasoning with progressive gate control
- **Plan Mode** — Two-phase state machine: Research → Approval → Execute
- **Team Mode** — Multi-agent parallel execution with ToolAgent delegation
- **Meeting Mode** — Multi-expert consultation with weighted routing

## Architecture

```
abacus-cli        — TUI + CLI interface (ratatui)
abacus-core       — Engine kernel (pipeline, context, tools, safety)
abacus-orchestrator — Meeting/Team orchestration
abacus-types      — Shared type definitions
abacus-server     — HTTP/SSE server mode
```

## Install

### From source

```bash
cargo install --path crates/abacus-cli
```

### Pre-built binary (macOS/Linux)

```bash
curl -fsSL https://github.com/lucasli000/abacus/releases/latest/download/install.sh | sh
```

## Usage

```bash
# Interactive TUI (default)
abacus

# Single query
abacus ask "explain this code"

# With specific model
abacus --model deepseek-v4 ask "optimize this function"

# Shell completions
abacus completions zsh > ~/.zfunc/_abacus
```

## Configuration

First run launches a setup wizard. Config stored at `~/.abacus/config.yaml`.

## Key Components

| Component | Description |
|-----------|-------------|
| ToolActionClassifier | Rule-based safety evaluator (hard_deny/soft_deny/allow) |
| ToolAgent | Automatic batch delegation for read-only tool operations |
| CompositeScorer | Mathematical compression engine (IB + H2O + ARC) |
| GreedyKnapsack | Optimal message retention under token budget |
| MCIP | Multi-level permission gate (role → confirm → capability) |
| ProgressiveGate | Complexity-aware output strategy |

## License

MIT
