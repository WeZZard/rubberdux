#!/bin/bash
set -euo pipefail

OS="$(uname -s)"

if [[ "$OS" == "Darwin" ]]; then
    # macOS packages via Homebrew
    :
elif [[ "$OS" == "Linux" ]]; then
    # Ubuntu packages - minimal for fast provisioning
    sudo apt-get update -qq
    sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -qq \
        curl \
        git \
        jq
fi
