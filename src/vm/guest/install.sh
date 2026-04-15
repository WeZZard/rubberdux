#!/bin/bash
set -euo pipefail

SHARE="/Volumes/My Shared Files/__provision"

# Install rubberdux binary (copied by host before boot)
if [[ -f "$SHARE/rubberdux" ]]; then
    sudo cp "$SHARE/rubberdux" /usr/local/bin/rubberdux
    sudo chmod +x /usr/local/bin/rubberdux
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

echo "Guest provisioning complete"
