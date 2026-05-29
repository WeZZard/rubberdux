#!/bin/bash
set -euo pipefail

OS="$(uname -s)"

if [[ "$OS" == "Darwin" ]]; then
    # macOS packages via Homebrew
    if ! command -v brew &> /dev/null; then
        echo "Installing Homebrew..."
        NONINTERACTIVE=1 /bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"
    fi
    eval "$(/opt/homebrew/bin/brew shellenv)"
    
    # Pre-installed apps for macOS:
    # - curl (built-in)
    # - git (built-in)  
    # - jq (built-in or install)
    # - Google Chrome (for web_fetch JS rendering)
    brew install jq || true
    brew install --cask google-chrome || true

    # Node.js for external agent bridges
    brew install node || true

    # External coding agent CLIs
    npm install -g @anthropic-ai/claude-code || true
    npm install -g @openai/codex || true
    npm install -g toll-free-harness || true

    # Verify Chrome installation
    if [ -d "/Applications/Google Chrome.app" ]; then
        echo "Google Chrome is pre-installed"
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" --version
    fi
    
elif [[ "$OS" == "Linux" ]]; then
    # Ubuntu packages - minimal for fast provisioning
    sudo apt-get update -qq
    sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -qq \
        curl \
        git \
        jq \
        build-essential \
        python3 \
        chromium-browser \
        chromium-chromedriver

    # Node.js via nvm (more reliable than apt on Ubuntu)
    if ! command -v node &> /dev/null; then
        curl -o- https://raw.githubusercontent.com/nvm-sh/nvm/v0.40.3/install.sh | bash
        export NVM_DIR="$HOME/.nvm"
        [ -s "$NVM_DIR/nvm.sh" ] && \. "$NVM_DIR/nvm.sh"
        nvm install --lts
    fi

    # External coding agent CLIs
    npm install -g @anthropic-ai/claude-code || true
    npm install -g @openai/codex || true
    npm install -g toll-free-harness || true
fi

# Verify Chrome is installed
if command -v chromium-browser &> /dev/null; then
    echo "Chromium installed: $(chromium-browser --version)"
elif command -v google-chrome &> /dev/null; then
    echo "Chrome installed: $(google-chrome --version)"
else
    echo "WARNING: Chrome/Chromium not found"
fi

# Verify external agent toolchain
echo "Node.js: $(node --version 2>/dev/null || echo 'NOT INSTALLED')"
echo "Claude Code: $(claude --version 2>/dev/null || echo 'NOT INSTALLED')"
echo "Codex: $(codex --version 2>/dev/null || echo 'NOT INSTALLED')"
