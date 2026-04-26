.PHONY: all build lsp install dev clean check

# Detect the correct cargo (rustup, not homebrew)
RUSTUP_CARGO := $(shell test -f $(HOME)/.cargo/bin/cargo && echo "$(HOME)/.cargo/bin/cargo" || echo "cargo")
RUSTUP_RUSTC := $(shell test -f $(HOME)/.cargo/bin/rustc && echo "$(HOME)/.cargo/bin/rustc" || echo "rustc")

# Directories
EXTENSION_DIR := $(shell pwd)
WORKSPACE_ROOT := $(shell cd ../.. && pwd)
LSP_CRATE := $(WORKSPACE_ROOT)/crates/md-testing-lsp

all: build lsp

# Check prerequisites
check:
	@echo "Checking prerequisites..."
	@echo "  Rustup cargo: $(RUSTUP_CARGO)"
	@echo "  Rustup rustc: $(RUSTUP_RUSTC)"
	@$(RUSTUP_RUSTC) --version
	@echo "  WASM target:"
	@$(RUSTUP_RUSTC) --print target-list | grep wasm32-wasip1 || (echo "  Installing wasm32-wasip1 target..." && rustup target add wasm32-wasip1)

# Build the WASM extension
build: check
	@echo "Building Zed extension..."
	PATH="$(HOME)/.cargo/bin:$(PATH)" $(RUSTUP_CARGO) build --release --target wasm32-wasip1
	cp $(EXTENSION_DIR)/target/wasm32-wasip1/release/zed_md_testing.wasm $(EXTENSION_DIR)/extension.wasm
	@echo "Extension built: $(EXTENSION_DIR)/extension.wasm"

# Build the LSP server
lsp:
	@echo "Building LSP server..."
	cd $(WORKSPACE_ROOT) && cargo build --release -p md-testing-lsp
	@echo "LSP server built: $(WORKSPACE_ROOT)/target/release/md-testing-lsp"

# Install everything and launch Zed in dev mode
dev: build lsp
	@echo ""
	@echo "================================"
	@echo "Launching Zed with dev extension"
	@echo "================================"
	@echo ""
	@echo "IMPORTANT: Make sure 'md-testing-lsp' is not running before installing."
	@echo ""
	@PATH="$(HOME)/.cargo/bin:$(PATH)" which cargo
	@echo ""
	@echo "Opening Zed..."
	@PATH="$(HOME)/.cargo/bin:$(PATH)" zed $(WORKSPACE_ROOT)

# Clean build artifacts
clean:
	rm -rf $(EXTENSION_DIR)/target
	rm -f $(EXTENSION_DIR)/extension.wasm
	cd $(WORKSPACE_ROOT) && cargo clean -p md-testing-lsp

# Show help
help:
	@echo "Available targets:"
	@echo "  make check  - Check prerequisites"
	@echo "  make build  - Build the Zed extension WASM"
	@echo "  make lsp    - Build the LSP server"
	@echo "  make dev    - Build everything and launch Zed"
	@echo "  make clean  - Clean build artifacts"
	@echo "  make all    - Build extension + LSP"
