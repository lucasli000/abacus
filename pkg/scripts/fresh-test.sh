#!/usr/bin/env bash
# fresh-test.sh — 跨 session 协作的 build cache 防御工具
#
# # 设计动机
# Cargo incremental build 在多 session 并行修改时会"伪通过":
#   - session A 给 struct 加字段 → struct 文件变, 但 caller 文件 mtime 不变
#   - cargo build 只重编 struct 文件 (incremental cache 复用 caller object)
#   - 真实运行时, caller 用旧 vtable 访问新 struct → 错误掩盖到 runtime
#
# # 触发场景 (来自 V29.14 + V30 实战)
# 1. session 末尾 cargo test 报 5 fail, 但单独 cargo test <name> 通过 → 缓存幽灵
# 2. cargo clean 后再编, 揭示 3 处 missing field — incremental 完全没看到
#
# # 推荐用法
# - 任何 session 接手前先跑 `./scripts/fresh-test.sh` 拿到 ground truth 状态
# - 风险高的改动 (跨 crate struct 字段 / enum variant) 跑一遍
# - CI / pre-PR 必跑
#
# # 引用关系
# - 场景图谱 abacus-runtime-map.md § "Stale Binary Defense" 引用此脚本
# - V29.14 Risk 1+2 防御措施

set -euo pipefail

cd "$(dirname "$0")/.."

echo "==> Step 1/3: cargo clean (清 incremental cache, 让所有 crate 重编)"
cargo clean -p abacus-types
cargo clean -p abacus-core
cargo clean -p abacus-orchestrator
cargo clean -p abacus-server
cargo clean -p abacus-cli

echo ""
echo "==> Step 2/3: cargo build --bin abacus (全量重编, 暴露所有 struct/enum 漂移)"
cargo build --bin abacus -p abacus-cli

echo ""
echo "==> Step 3/3: cargo test (全测套, ground truth 状态)"
echo "    abacus-core:"
cargo test -p abacus-core --lib 2>&1 | tail -3
echo "    abacus-cli:"
cargo test -p abacus-cli --lib 2>&1 | tail -3
echo "    abacus-types:"
cargo test -p abacus-types --lib 2>&1 | tail -3
echo "    abacus-orchestrator:"
cargo test -p abacus-orchestrator --lib 2>&1 | tail -3

echo ""
echo "==> fresh-test 完成. 上面是 ground truth 状态, 不受 cache 干扰"
