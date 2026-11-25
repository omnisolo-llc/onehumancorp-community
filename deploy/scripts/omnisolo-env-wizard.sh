#!/bin/bash
# SPDX-License-Identifier: Apache-2.0
# OneHumanCorp Community Edition — Interactive Standalone Environment Wizard

set -euo pipefail

RESET="\033[0m"
BOLD="\033[1m"
DIM="\033[2m"
BLUE="\033[38;5;39m"
CYAN="\033[38;5;87m"
GREEN="\033[38;5;120m"

echo -e "${BOLD}${BLUE}======================================================${RESET}"
echo -e "${BOLD}${CYAN}   OneHumanCorp: Standalone Environment Wizard (.env) ${RESET}"
echo -e "${BOLD}${BLUE}======================================================${RESET}"
echo ""

ENV_FILE=".env"

if [ -f "$ENV_FILE" ]; then
    cp "$ENV_FILE" "${ENV_FILE}.bak"
    chmod 0600 "${ENV_FILE}.bak"
    echo -e "${DIM}[Backup created at ${ENV_FILE}.bak]${RESET}"
else
    touch "$ENV_FILE"
    chmod 0600 "$ENV_FILE"
fi

update_env() {
    local key=$1
    local value=$2
    if grep -q "^${key}=" "$ENV_FILE"; then
        sed -i "s|^${key}=.*|${key}=${value}|" "$ENV_FILE"
    else
        echo "${key}=${value}" >> "$ENV_FILE"
    fi
}

echo -e "${BOLD}Let's configure your single-tenant standalone environment.${RESET}"
echo ""

# 1. Logging Level
read -r -p "Log Level [info, debug, warn, error] (default: info): " log_level
log_level=${log_level:-info}
update_env "LOG_LEVEL" "$log_level"

# 2. HTTP Port
read -r -p "HTTP Server Port (default: 8080): " port
port=${port:-8080}
update_env "PORT" "$port"

# 3. Standalone Runtime Mode
echo -e "\n${BOLD}Standalone Runtime Settings:${RESET}"
update_env "OMNISOLO_STANDALONE_MODE" "true"
update_env "OMNISOLO_SOURCE_MODE" "standalone"
read -r -p "Run headless API-only mode? (true/false) (default: false): " headless
headless=${headless:-false}
update_env "OMNISOLO_HEADLESS" "$headless"

# 4. BYOK / Local LLM Providers
echo -e "\n${BOLD}LLM Provider Configuration (BYOK or Local Ollama):${RESET}"
read -r -p "Default LLM Provider [openai, anthropic, minimax, ollama, openai-compatible] (leave blank to skip): " llm_provider
if [ -n "$llm_provider" ]; then
    update_env "OMNISOLO_LLM_PROVIDER" "$llm_provider"
fi

read -r -p "OpenAI API Key (leave blank to skip): " openai_key
if [ -n "$openai_key" ]; then
    update_env "OPENAI_API_KEY" "$openai_key"
fi

read -r -p "Anthropic API Key (leave blank to skip): " anthropic_key
if [ -n "$anthropic_key" ]; then
    update_env "ANTHROPIC_API_KEY" "$anthropic_key"
fi

read -r -p "Local Ollama endpoint (e.g. http://127.0.0.1:11434, leave blank to skip): " ollama_url
if [ -n "$ollama_url" ]; then
    update_env "OMNISOLO_LOCAL_LLM_ENDPOINT" "$ollama_url"
fi

# 5. Optional Single-Tenant PostgreSQL (SQLCipher SQLite is the default)
echo -e "\n${BOLD}Database Configuration (SQLCipher SQLite is default):${RESET}"
read -r -p "Optional single-tenant PostgreSQL DATABASE_URL (leave blank for SQLCipher SQLite): " db_url
if [ -n "$db_url" ]; then
    update_env "DATABASE_URL" "$db_url"
fi

# 6. Advanced Features
echo -e "\n${BOLD}Advanced Features:${RESET}"
read -r -p "Enable MCP (Model Context Protocol) features? (true/false) (default: false): " mcp_enabled
mcp_enabled=${mcp_enabled:-false}
update_env "MCP_ENABLED" "$mcp_enabled"

chmod 0600 "$ENV_FILE"

echo -e "\n${GREEN}Setup Complete! Your standalone .env file is configured.${RESET}"
echo -e "${DIM}Re-run anytime via bash deploy/scripts/omnisolo-env-wizard.sh.${RESET}"
echo ""
