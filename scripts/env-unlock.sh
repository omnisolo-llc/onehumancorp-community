#!/usr/bin/env bash
# Thin wrapper: verifies authenticated integrity and decrypts .env.enc -> .env (mode 0600).
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
exec "${SCRIPT_DIR}/env-vault.sh" unlock "$@"
