#!/bin/bash
set -euo pipefail

ARCH="$(uname -m)"
case "$ARCH" in
    arm64) ARCH="aarch64" ;;
    x86_64) ARCH="x86_64" ;;
esac

case "$(uname -s)" in
    Darwin)
        echo "${ARCH}-apple-macos"
        ;;
    Linux)
        echo "${ARCH}-unknown-linux-gnu"
        ;;
    *)
        echo "${ARCH}-unknown-$(uname -s | tr '[:upper:]' '[:lower:]')"
        ;;
esac
