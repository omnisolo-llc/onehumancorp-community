#!/usr/bin/env bash
# Thin wrapper: encrypts .env -> .env.enc, verifies round-trip, and removes plaintext .env.
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
exec "${SCRIPT_DIR}/env-vault.sh" lock "$@"
