# ════════════════════════════════════════════════════════════════
# Abacus Build & Package Makefile
# ════════════════════════════════════════════════════════════════
#
# Usage:
#   make build          — Release build for current platform
#   make package        — Build + create tarball
#   make package-all    — Cross-compile for all supported targets
#   make install        — Install to /usr/local/bin
#   make clean          — Remove build artifacts

VERSION := $(shell grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)"/\1/')
GIT_HASH := $(shell git rev-parse --short=8 HEAD)
BINARY := abacus
TARGET_DIR := target/release
DIST_DIR := dist

# Platform detection
UNAME_S := $(shell uname -s)
UNAME_M := $(shell uname -m)

ifeq ($(UNAME_S),Darwin)
    ifeq ($(UNAME_M),arm64)
        TARGET := aarch64-apple-darwin
    else
        TARGET := x86_64-apple-darwin
    endif
else ifeq ($(UNAME_S),Linux)
    ifeq ($(UNAME_M),aarch64)
        TARGET := aarch64-unknown-linux-gnu
    else
        TARGET := x86_64-unknown-linux-gnu
    endif
endif

.PHONY: build package package-all install clean check test

# ── Build ──────────────────────────────────────────────────────

build:
	@echo "Building $(BINARY) v$(VERSION) ($(TARGET))..."
	cargo build --release --package abacus-cli
	@echo "Binary: $(TARGET_DIR)/$(BINARY) ($$(du -h $(TARGET_DIR)/$(BINARY) | cut -f1))"

# ── Package ────────────────────────────────────────────────────

package: build
	@mkdir -p $(DIST_DIR)
	@echo "Packaging $(BINARY)-$(TARGET).tar.gz..."
	tar -czf $(DIST_DIR)/$(BINARY)-$(TARGET).tar.gz \
		-C $(TARGET_DIR) $(BINARY)
	@echo "Package: $(DIST_DIR)/$(BINARY)-$(TARGET).tar.gz"
	@shasum -a 256 $(DIST_DIR)/$(BINARY)-$(TARGET).tar.gz

# Cross-compile all targets (requires cross or target toolchains)
package-all:
	@mkdir -p $(DIST_DIR)
	@echo "=== Building for aarch64-apple-darwin ==="
	cargo build --release --package abacus-cli --target aarch64-apple-darwin
	tar -czf $(DIST_DIR)/$(BINARY)-aarch64-apple-darwin.tar.gz \
		-C target/aarch64-apple-darwin/release $(BINARY)
	@echo "=== Building for x86_64-apple-darwin ==="
	cargo build --release --package abacus-cli --target x86_64-apple-darwin
	tar -czf $(DIST_DIR)/$(BINARY)-x86_64-apple-darwin.tar.gz \
		-C target/x86_64-apple-darwin/release $(BINARY)
	@echo "=== All packages in $(DIST_DIR)/ ==="
	@ls -la $(DIST_DIR)/

# Universal binary (macOS only)
universal: package-all
	@echo "Creating universal binary..."
	lipo -create \
		target/aarch64-apple-darwin/release/$(BINARY) \
		target/x86_64-apple-darwin/release/$(BINARY) \
		-output $(DIST_DIR)/$(BINARY)-universal
	tar -czf $(DIST_DIR)/$(BINARY)-universal-apple-darwin.tar.gz \
		-C $(DIST_DIR) $(BINARY)-universal
	@echo "Universal: $(DIST_DIR)/$(BINARY)-universal-apple-darwin.tar.gz"

# ── Install ────────────────────────────────────────────────────

install: build
	@echo "Installing to /usr/local/bin/$(BINARY)..."
	@cp $(TARGET_DIR)/$(BINARY) /usr/local/bin/$(BINARY)
	@chmod +x /usr/local/bin/$(BINARY)
	@echo "✓ Installed. Run 'abacus --version' to verify."

# ── Test & Check ───────────────────────────────────────────────

check:
	cargo check --workspace

test:
	cargo test --workspace

# ── Clean ──────────────────────────────────────────────────────

clean:
	cargo clean
	rm -rf $(DIST_DIR)
