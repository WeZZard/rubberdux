#!/usr/bin/env bash
# Thin wrapper around the canonical launcher. All real logic (build, VM
# provisioning, session archiving, launch through the user's shell env,
# health-check, rollback) lives in `cargo xtask bootstrap`.
set -euo pipefail
cd "$(dirname "$0")/../../../.." && exec cargo xtask bootstrap
