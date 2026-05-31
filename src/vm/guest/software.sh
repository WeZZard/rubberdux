#!/bin/bash
set -euo pipefail

# ── Pinned versions ──────────────────────────────────────────────
NODE_MAJOR=22
CLAUDE_CODE_VERSION="2.1.157"
CODEX_VERSION="0.135.0"
TOLL_FREE_HARNESS_VERSION="0.1.2"
NVM_VERSION="v0.40.3"
# ─────────────────────────────────────────────────────────────────

OS="$(uname -s)"

install_nvm() {
    if [ ! -d "$HOME/.nvm" ]; then
        curl -o- "https://raw.githubusercontent.com/nvm-sh/nvm/${NVM_VERSION}/install.sh" | bash
    fi
    export NVM_DIR="$HOME/.nvm"
    [ -s "$NVM_DIR/nvm.sh" ] && \. "$NVM_DIR/nvm.sh"
}

install_node() {
    install_nvm
    nvm install "$NODE_MAJOR" --lts
    nvm alias default "$NODE_MAJOR"
}

install_agent_tools() {
    npm install -g "@anthropic-ai/claude-code@${CLAUDE_CODE_VERSION}"
    npm install -g "@openai/codex@${CODEX_VERSION}"
    npm install -g "toll-free-harness@${TOLL_FREE_HARNESS_VERSION}"
}

if [[ "$OS" == "Darwin" ]]; then
    if ! command -v brew &> /dev/null; then
        echo "Installing Homebrew..."
        NONINTERACTIVE=1 /bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"
    fi
    eval "$(/opt/homebrew/bin/brew shellenv)"

    brew install jq || true
    brew install --cask google-chrome || true

    install_node
    install_agent_tools

    if [ -d "/Applications/Google Chrome.app" ]; then
        echo "Google Chrome is pre-installed"
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" --version
    fi

elif [[ "$OS" == "Linux" ]]; then
    sudo apt-get update -qq
    sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -qq \
        curl \
        git \
        jq \
        build-essential \
        python3 \
        chromium-browser \
        chromium-chromedriver

    install_node
    install_agent_tools
fi

if command -v chromium-browser &> /dev/null; then
    echo "Chromium installed: $(chromium-browser --version)"
elif command -v google-chrome &> /dev/null; then
    echo "Chrome installed: $(google-chrome --version)"
else
    echo "WARNING: Chrome/Chromium not found"
fi

echo "Node.js: $(node --version 2>/dev/null || echo 'NOT INSTALLED')"
echo "Claude Code: $(claude --version 2>/dev/null || echo 'NOT INSTALLED')"
echo "Codex: $(codex --version 2>/dev/null || echo 'NOT INSTALLED')"
echo "toll-free-harness: $(npm ls -g toll-free-harness --depth=0 2>/dev/null | grep toll-free || echo 'NOT INSTALLED')"
