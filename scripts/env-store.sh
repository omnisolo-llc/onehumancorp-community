#!/usr/bin/env bash
# Thin wrapper: stores or updates a KEY=VALUE secret in gitignored .env (mode 0600).
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
exec "${SCRIPT_DIR}/env-vault.sh" store "$@"
