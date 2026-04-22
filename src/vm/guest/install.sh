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

# Install rubberdux binary (copied by host before boot)
if [[ -f "$SHARE/rubberdux" ]]; then
    if [[ "$OS" == "Darwin" ]]; then
        sudo cp "$SHARE/rubberdux" /usr/local/bin/rubberdux
        sudo chmod +x /usr/local/bin/rubberdux
    else
        sudo cp "$SHARE/rubberdux" /usr/local/bin/rubberdux
        sudo chmod +x /usr/local/bin/rubberdux
    fi
    echo "Installed rubberdux to /usr/local/bin"
else
    echo "Warning: rubberdux binary not found in provision share"
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

echo "Guest provisioning complete"
