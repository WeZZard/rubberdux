#!/bin/bash
set -euo pipefail

# Detect OS and set share path
OS="$(uname -s)"
if [[ "$OS" == "Darwin" ]]; then
    SHARE="/Volumes/My Shared Files/__provision"
elif [[ "$OS" == "Linux" ]]; then
    SHARE="/mnt/shared/__provision"
else
    echo "Unsupported OS: $OS"
    exit 1
fi

# Configure SSH authorized_keys for passwordless access
mkdir -p ~/.ssh
chmod 700 ~/.ssh
if [[ -f "$SHARE/authorized_keys" ]]; then
    cp "$SHARE/authorized_keys" ~/.ssh/authorized_keys
    chmod 600 ~/.ssh/authorized_keys
    echo "Installed SSH authorized_keys"
fi

# Run software provisioning script if present
if [[ -f "$SHARE/software.sh" ]]; then
    echo "Running software provisioning..."
    bash "$SHARE/software.sh"
fi

# Install bridge script for external agent communication
if [[ -d "$SHARE/bridge-claude-code" ]]; then
    sudo mkdir -p /opt/rubberdux/bridge-claude-code
    sudo cp -r "$SHARE/bridge-claude-code/"* /opt/rubberdux/bridge-claude-code/
    if command -v npm &> /dev/null; then
        cd /opt/rubberdux/bridge-claude-code && sudo npm install --production 2>/dev/null || true
    fi
    echo "Installed bridge-claude-code to /opt/rubberdux"
fi

echo "Guest provisioning complete"
