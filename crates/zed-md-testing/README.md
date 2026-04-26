# Zed Extension for Markdown Test Cases

Zed extension that provides LSP support for `*.testcase.md` files used by the md-testing framework.

## Features

- **Inline diagnostics**: Shows test results directly in `.testcase.md` files
  - Pass/fail indicators on `## Storyline`, `## User Message`, and `## Assistant Message` lines
  - Lint errors for malformed test cases
- **Syntax highlighting**: Custom captures for test case structure
- **File watching**: Automatically updates diagnostics when new test results are written

## Prerequisites

- [Rust via rustup](https://rustup.rs/) (required for Zed extension development)
- Zed editor

## Quick Start (One Command)

```bash
cd crates/zed-md-testing
make dev
```

This will:
1. Check that rustup is properly installed
2. Build the Zed extension WASM
3. Build the LSP server (`md-testing-lsp`)
4. Launch Zed with the correct Rust toolchain in PATH

Then in Zed:
1. Open the command palette (`Cmd+Shift+P`)
2. Run: `extensions: install dev extension`
3. Select: `crates/zed-md-testing/`

## Manual Setup

If the automatic setup doesn't work, ensure rustup's cargo is in your PATH before Homebrew's:

```bash
# Check which cargo is first
which -a cargo

# If you see /opt/homebrew/bin/cargo before ~/.cargo/bin/cargo,
# add this to your ~/.zshrc or ~/.bashrc:
export PATH="$HOME/.cargo/bin:$PATH"

# Then reload your shell and try again
```

## Usage

### Enable Inline Diagnostics (Required)

To see assertion pass/fail indicators inline, enable inline diagnostics in Zed:

```json
// ~/.config/zed/settings.json
{
  "diagnostics": {
    "inline": {
      "enabled": true
    }
  }
}
```

### Run Tests

```bash
cargo test --test telegram_channel_agent
```

Results are written to `tests/results/<run_id>/<case>/results.json`. The LSP automatically picks them up.

## Architecture

```
Zed Editor
    |
    v
Zed Extension (WASM) -- loads --> md-testing-lsp (binary)
                                      |
                                      v
                              watches tests/results/
                                      |
                                      v
                              publishes LSP diagnostics
```

## Development

```bash
# Build just the extension WASM
make build

# Build just the LSP server
make lsp

# Clean all build artifacts
make clean
```

## How It Works

1. When you open a `.testcase.md` file, the LSP server starts
2. The LSP reads the latest test results from `tests/results/<run_id>/<case>/results.json`
3. It publishes diagnostics at the line numbers of each assertion
4. Zed renders these as inline hints and underlines

## Troubleshooting

### "Failed to compile Rust extension"

This means Zed found Homebrew's Rust instead of rustup's. Solutions:

1. **Run `make dev` instead** - it handles the PATH for you
2. **Or fix your PATH permanently:**
   ```bash
   echo 'export PATH="$HOME/.cargo/bin:$PATH"' >> ~/.zshrc
   source ~/.zshrc
   ```
3. **Or uninstall Homebrew Rust:**
   ```bash
   brew uninstall rust
   ```

### No diagnostics showing

1. Run tests first: `cargo test --test telegram_channel_agent`
2. Check that `tests/results/` exists with result files
3. Open Zed log: `zed: open log`

## License

MIT
