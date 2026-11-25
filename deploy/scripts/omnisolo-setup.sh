#!/bin/bash
# SPDX-License-Identifier: Apache-2.0
# OneHumanCorp Community Edition — Standalone Developer Setup Script

set -eo pipefail

RESET="\033[0m"
BOLD="\033[1m"
DIM="\033[2m"
BLUE="\033[38;5;39m"
CYAN="\033[38;5;87m"
GREEN="\033[38;5;120m"
PURPLE="\033[38;5;141m"

echo -e "${BOLD}${BLUE}===============================================${RESET}"
echo -e "${BOLD}${CYAN}   OneHumanCorp Standalone Developer Setup     ${RESET}"
echo -e "${BOLD}${BLUE}===============================================${RESET}"

# Check requirements
if ! command -v cargo >/dev/null 2>&1; then echo -e "${PURPLE}Cargo is required but not installed. Aborting.${RESET}"; exit 1; fi
if ! command -v docker >/dev/null 2>&1; then echo -e "${PURPLE}Docker is required but not installed. Aborting.${RESET}"; exit 1; fi
if ! command -v xvfb-run >/dev/null 2>&1; then echo -e "${PURPLE}xvfb-run is recommended for headless UI tests.${RESET}"; fi

echo -e "${DIM}[1/4] Checking environment configuration...${RESET}"
if [ ! -f .env ]; then
  echo "Creating default standalone .env file..."
  cat << 'ENV' > .env
# OneHumanCorp Community Edition — Standalone Local Config
LOG_LEVEL=info
LOG_FORMAT=json
PORT=8080
OMNISOLO_STANDALONE_MODE=true
OMNISOLO_HEADLESS=false
OMNISOLO_SOURCE_MODE=standalone
OMNISOLO_RUNTIME_DIR=.omnisolo/runtime
OMNISOLO_MEMORY_DIR=.omnisolo/runtime/memory
OMNISOLO_STATUS_DIR=.omnisolo/runtime/status
ENV
  chmod 0600 .env
fi

echo -e "${DIM}[2/4] Verifying Standalone Backend...${RESET}"
export OMNISOLO_STANDALONE_MODE=true
export OMNISOLO_HEADLESS=false
export OMNISOLO_SOURCE_MODE=standalone
cargo test --locked -p omnisolo --lib api::

echo -e "${DIM}[3/4] Verifying .env setup...${RESET}"
bash deploy/scripts/omnisolo-verify-setup.sh || { echo -e "${PURPLE}Verification failed.${RESET}"; exit 1; }

echo -e "${DIM}[4/4] Generating Local Memory Log...${RESET}"
RUNTIME_DIR="${OMNISOLO_RUNTIME_DIR:-.omnisolo/runtime}"
MEMORY_DIR="${OMNISOLO_MEMORY_DIR:-${RUNTIME_DIR}/memory}"
STATUS_DIR="${OMNISOLO_STATUS_DIR:-${RUNTIME_DIR}/status}"
mkdir -p "${MEMORY_DIR}" "${STATUS_DIR}"
TIMESTAMP=$(date +%s)

cat << MEM > "${MEMORY_DIR}/setup-${TIMESTAMP}.yml"
type: memory
metadata:
  role: Developer Setup
  timestamp: ${TIMESTAMP}
observations:
  - Developer executed omnisolo-setup.sh
actions_taken:
  - Verified local standalone environment
  - Verified .env configuration
resolution: Standalone developer environment successfully initialized.
MEM

cat << STAT > "${STATUS_DIR}/${TIMESTAMP}.yml"
type: status
metadata:
  role: Developer Setup
  timestamp: ${TIMESTAMP}
health: ok
observations:
  - omnisolo-setup.sh completed successfully.
STAT

echo -e "${BOLD}${BLUE}===============================================${RESET}"
echo -e "${BOLD}${GREEN}   Setup Complete!                             ${RESET}"
echo -e "${DIM}   Use 'source deploy/scripts/omnisolo-mode.sh standalone' to configure your shell.${RESET}"
echo -e "${BOLD}${BLUE}===============================================${RESET}"
