#!/bin/bash
set -euo pipefail

OS="$(uname -s)"

if [[ "$OS" == "Darwin" ]]; then
    # macOS packages via Homebrew
    if ! command -v brew &> /dev/null; then
        echo "Installing Homebrew..."
        /bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"
    fi
    
    # Pre-installed apps for macOS:
    # - curl (built-in)
    # - git (built-in)  
    # - jq (built-in or install)
    # - Google Chrome (for web_fetch JS rendering)
    brew install jq || true
    brew install --cask google-chrome || true
    
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
        chromium-browser \
        chromium-chromedriver
fi

# Verify Chrome is installed
if command -v chromium-browser &> /dev/null; then
    echo "Chromium installed: $(chromium-browser --version)"
elif command -v google-chrome &> /dev/null; then
    echo "Chrome installed: $(google-chrome --version)"
else
    echo "WARNING: Chrome/Chromium not found"
fi
