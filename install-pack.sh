#!/bin/bash
# Abacus Install Pack Builder
# 生成一个脱敏的独立安装包（含编译好的二进制 + 源码 + 配置模板）
# 用法: ./install-pack.sh [output_dir]

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
OUTPUT_DIR="${1:-/tmp/abacus-install-pack}"
PACK_NAME="abacus-v1.0.0-darwin-arm64"

echo "📦 Building Abacus install pack..."
echo "   Source: $SCRIPT_DIR"
echo "   Output: $OUTPUT_DIR/$PACK_NAME"

rm -rf "$OUTPUT_DIR/$PACK_NAME"
mkdir -p "$OUTPUT_DIR/$PACK_NAME"/{bin,src,config,data,examples,scripts}

# ─── 1. 编译好的二进制 ───────────────────────────────────────────────
echo "  [1/6] Copying binaries..."
cp "$SCRIPT_DIR/pkg/target/release/abacus" "$OUTPUT_DIR/$PACK_NAME/bin/"
cp "$SCRIPT_DIR/pkg/target/release/abacus-server" "$OUTPUT_DIR/$PACK_NAME/bin/"

# ─── 2. 源码（排除 target/ 和用户数据）────────────────────────────────
echo "  [2/6] Packaging source code..."
tar -cf - \
    --exclude='pkg/target' \
    --exclude='.git' \
    --exclude='*.db' \
    --exclude='*.jsonl' \
    --exclude='install-pack.sh' \
    -C "$SCRIPT_DIR" . | tar -xf - -C "$OUTPUT_DIR/$PACK_NAME/src/"

# ─── 3. 配置模板（脱敏）─────────────────────────────────────────────
echo "  [3/6] Creating config template..."
cp "$SCRIPT_DIR/config.example.toml" "$OUTPUT_DIR/$PACK_NAME/config/"

cat > "$OUTPUT_DIR/$PACK_NAME/config/env.example" << 'EOF'
# Abacus 环境变量配置（任选一个 provider）
# 复制为 .env 后填入你的 API key

# DeepSeek（默认推荐）
ABACUS_API_KEY=sk-your-deepseek-key-here

# 或 OpenAI
# OPENAI_API_KEY=sk-your-openai-key-here

# 或 Anthropic
# ANTHROPIC_API_KEY=sk-ant-your-key-here

# HTTP Server token（可选，启用 abacus-server 时需要）
# ABACUS_SERVER_TOKEN=your-bearer-token

# 自定义数据目录（可选，默认 ~/.abacus）
# ABACUS_HOME=/path/to/custom/abacus/home
EOF

# ─── 4. 内置知识库 ──────────────────────────────────────────────────
echo "  [4/6] Copying knowledge base..."
if [ -f "$SCRIPT_DIR/data/knowledge.db" ]; then
    cp "$SCRIPT_DIR/data/knowledge.db" "$OUTPUT_DIR/$PACK_NAME/data/"
fi

# ─── 5. 示例配置 ────────────────────────────────────────────────────
echo "  [5/6] Copying examples..."
cp -r "$SCRIPT_DIR/examples/"* "$OUTPUT_DIR/$PACK_NAME/examples/" 2>/dev/null || true

# ─── 6. 安装脚本 ────────────────────────────────────────────────────
echo "  [6/6] Creating install script..."
cat > "$OUTPUT_DIR/$PACK_NAME/install.sh" << 'INSTALL_EOF'
#!/bin/bash
# Abacus Installer — 从 0 到可用
set -euo pipefail

echo "🚀 Installing Abacus v1.0.0..."
echo ""

INSTALL_DIR="$HOME/.local/bin"
DATA_DIR="$HOME/.abacus"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

# 创建目录
mkdir -p "$INSTALL_DIR"
mkdir -p "$DATA_DIR"

# 安装二进制
echo "  [1/4] Installing binaries to $INSTALL_DIR..."
cp "$SCRIPT_DIR/bin/abacus" "$INSTALL_DIR/abacus"
cp "$SCRIPT_DIR/bin/abacus-server" "$INSTALL_DIR/abacus-server"
chmod +x "$INSTALL_DIR/abacus" "$INSTALL_DIR/abacus-server"

# 安装知识库
if [ -f "$SCRIPT_DIR/data/knowledge.db" ]; then
    echo "  [2/4] Installing knowledge base..."
    cp "$SCRIPT_DIR/data/knowledge.db" "$DATA_DIR/knowledge.db"
else
    echo "  [2/4] No knowledge base found, skipping..."
fi

# 配置环境变量提示
echo "  [3/4] Checking PATH..."
if [[ ":$PATH:" != *":$INSTALL_DIR:"* ]]; then
    echo ""
    echo "  ⚠️  $INSTALL_DIR is not in your PATH."
    echo "  Add to your shell profile:"
    echo ""
    echo "    export PATH=\"\$HOME/.local/bin:\$PATH\""
    echo ""
fi

# API Key 配置
echo "  [4/4] Checking API key..."
if [ -z "${ABACUS_API_KEY:-}" ] && [ -z "${DEEPSEEK_API_KEY:-}" ] && [ -z "${OPENAI_API_KEY:-}" ]; then
    echo ""
    echo "  ⚠️  No API key detected. Set one of:"
    echo ""
    echo "    export ABACUS_API_KEY=sk-your-key    # DeepSeek"
    echo "    export OPENAI_API_KEY=sk-xxx          # OpenAI"
    echo "    export ANTHROPIC_API_KEY=sk-ant-xxx   # Anthropic"
    echo ""
    echo "  Or run 'abacus' to enter the interactive setup wizard."
fi

echo ""
echo "✅ Abacus installed successfully!"
echo ""
echo "Quick start:"
echo "  abacus                    # TUI 交互模式（首次自动进入配置向导）"
echo "  abacus chat -m \"hello\"    # 单次对话"
echo "  abacus --help             # 查看所有命令"
echo ""
echo "From source (if you want to rebuild):"
echo "  cd $SCRIPT_DIR/src/pkg"
echo "  cargo build --release"
echo ""
INSTALL_EOF
chmod +x "$OUTPUT_DIR/$PACK_NAME/install.sh"

# ─── 7. README ───────────────────────────────────────────────────────
cat > "$OUTPUT_DIR/$PACK_NAME/README.md" << 'README_EOF'
# Abacus v1.0.0 Install Pack

LLM Agent Kernel — 模块化 Agent 运行时

## 快速安装

```bash
./install.sh
```

安装后：
```bash
export ABACUS_API_KEY=sk-your-deepseek-key
abacus
```

## 包内容

```
bin/              预编译二进制（macOS arm64）
src/              完整源码（可自行 cargo build）
config/           配置模板
data/             内置知识库
examples/         Meeting 模式示例
install.sh        一键安装脚本
```

## 从源码编译

```bash
cd src/pkg
cargo build --release
# 产物: ./target/release/abacus
```

## 系统要求

- macOS 12+ (arm64) 或从源码编译适配其他平台
- Rust 1.75+ (仅源码编译需要)
- 有效的 LLM API Key (DeepSeek/OpenAI/Anthropic)

## 运行时数据

所有用户数据存储在 `~/.abacus/`：
- config.yaml — 配置文件（首次启动自动生成）
- knowledge.db — 知识库
- palace.db — 记忆宫殿
- sessions/ — 会话历史
- projects/ — 项目级数据

可通过 `ABACUS_HOME` 环境变量自定义路径。
README_EOF

# ─── 8. 打包压缩 ─────────────────────────────────────────────────────
echo ""
echo "  Compressing..."
cd "$OUTPUT_DIR"
tar -czf "${PACK_NAME}.tar.gz" "$PACK_NAME/"

FINAL_SIZE=$(du -h "$OUTPUT_DIR/${PACK_NAME}.tar.gz" | cut -f1)
echo ""
echo "✅ Pack ready: $OUTPUT_DIR/${PACK_NAME}.tar.gz ($FINAL_SIZE)"
echo ""
echo "To install on a fresh machine:"
echo "  tar xzf ${PACK_NAME}.tar.gz"
echo "  cd $PACK_NAME"
echo "  ./install.sh"
