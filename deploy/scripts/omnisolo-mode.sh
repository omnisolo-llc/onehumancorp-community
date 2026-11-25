#!/bin/bash
# SPDX-License-Identifier: Apache-2.0
# OneHumanCorp Community Edition — Standalone Development Mode Switcher

MODE=${1:-}

if [ -z "$MODE" ]; then
  echo "Usage: source ./deploy/scripts/omnisolo-mode.sh [standalone|headless]"
  return 1 2>/dev/null || exit 1
fi

echo "Switching OneHumanCorp Standalone Environment to: $MODE"

case "$MODE" in
  standalone)
    export OMNISOLO_HEADLESS=false
    export OMNISOLO_SOURCE_MODE=standalone
    export OMNISOLO_STANDALONE_MODE=true
    export TOKIO_WORKER_THREADS=2
    export RAYON_NUM_THREADS=2
    mkdir -p .omnisolo/memory/auto/ .omnisolo/memory/team/
    export LOG_FORMAT="json"
    export LOG_LEVEL="info"
    if [ "${OMNISOLO_TELEMETRY_ENABLED:-false}" != "true" ]; then
      export OMNISOLO_TELEMETRY_ENABLED=false
    fi
    echo "Configured for Standalone Desktop / Local Mode (encrypted SQLCipher SQLite)."
    ;;
  headless)
    export OMNISOLO_HEADLESS=true
    export OMNISOLO_SOURCE_MODE=standalone
    export OMNISOLO_STANDALONE_MODE=true
    export LOG_FORMAT="json"
    export LOG_LEVEL="info"
    if [ "${OMNISOLO_TELEMETRY_ENABLED:-false}" != "true" ]; then
      export OMNISOLO_TELEMETRY_ENABLED=false
    fi
    echo "Configured for Standalone Headless API Mode."
    ;;
  *)
    echo "Unknown mode: $MODE"
    echo "Valid modes: standalone, headless"
    return 1 2>/dev/null || exit 1
    ;;
esac

echo "Environment variables set. Run make test-backend, npm run dev:web, or npm run desktop:dev."
