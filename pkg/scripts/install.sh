#!/usr/bin/env bash
# Abacus CLI installer — macOS (arm64/x86_64) + Linux (x86_64/aarch64)
#
# Usage:
#   curl -fsSL https://github.com/lucasli000/abacus/releases/latest/download/install.sh | sh
#
# 引用关系：GitHub release 附件，用户通过 curl 下载执行
# 生命周期：一次性执行后自销毁

set -euo pipefail

REPO="lucasli000/abacus"
BINARY_NAME="abacus"
INSTALL_DIR="${INSTALL_DIR:-/usr/local/bin}"

# 检测平台
detect_platform() {
    local os arch
    os="$(uname -s)"
    arch="$(uname -m)"

    case "$os" in
        Darwin) os="apple-darwin" ;;
        Linux)  os="unknown-linux-gnu" ;;
        *)      echo "Unsupported OS: $os" >&2; exit 1 ;;
    esac

    case "$arch" in
        x86_64|amd64)  arch="x86_64" ;;
        arm64|aarch64) arch="aarch64" ;;
        *)             echo "Unsupported architecture: $arch" >&2; exit 1 ;;
    esac

    echo "${arch}-${os}"
}

# 获取最新版本
get_latest_version() {
    curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
        | grep '"tag_name"' \
        | sed -E 's/.*"v?([^"]+)".*/\1/'
}

main() {
    local platform version url tmp

    platform="$(detect_platform)"
    echo "Platform: ${platform}"

    version="$(get_latest_version)"
    if [ -z "$version" ]; then
        echo "Failed to determine latest version" >&2
        exit 1
    fi
    echo "Version: v${version}"

    url="https://github.com/${REPO}/releases/download/v${version}/${BINARY_NAME}-${platform}.tar.gz"
    echo "Downloading: ${url}"

    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' EXIT

    curl -fsSL "$url" | tar -xz -C "$tmp"

    # 安装
    if [ -w "$INSTALL_DIR" ]; then
        mv "$tmp/${BINARY_NAME}" "${INSTALL_DIR}/${BINARY_NAME}"
    else
        echo "Installing to ${INSTALL_DIR} (requires sudo)..."
        sudo mv "$tmp/${BINARY_NAME}" "${INSTALL_DIR}/${BINARY_NAME}"
    fi

    chmod +x "${INSTALL_DIR}/${BINARY_NAME}"
    echo ""
    echo "   ╭━━━━━━━━━━━━━━━━━━━━━━━━━━╮"
    echo "   │  A B A C U S              │"
    echo "   │  LLM Agent Kernel         │"
    echo "   ╰━━━━━━━━━━━━━━━━━━━━━━━━━━╯"
    echo ""
    echo "  ✓ v${version} installed to ${INSTALL_DIR}/${BINARY_NAME}"
    echo "  Run 'abacus' to start."
}

main "$@"
