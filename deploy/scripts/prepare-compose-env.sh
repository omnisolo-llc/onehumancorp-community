#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Create private, local-only credentials for standalone Docker Compose.
set -euo pipefail
umask 077

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd -P)"
STATE_DIR="${OMNISOLO_COMPOSE_STATE_DIR:-${REPO_ROOT}/.omnisolo-compose}"
ENV_FILE="${STATE_DIR}/compose.env"

for tool in openssl sed; do
  command -v "$tool" >/dev/null 2>&1 || {
    echo "error: required tool '$tool' not found" >&2
    exit 1
  }
done

if [[ -f "${ENV_FILE}" ]]; then
  echo "Compose credentials already exist at ${ENV_FILE}."
  exit 0
fi
if [[ $# -gt 0 ]]; then
  echo "usage: $0" >&2
  exit 2
fi

mkdir -p "${STATE_DIR}"

SQLITE_KEY="$(openssl rand -hex 32)"
POSTGRES_PASSWORD="$(openssl rand -hex 24)"
JWT_SECRET="$(openssl rand -hex 48)"
WEB_SESSION_SECRET="$(openssl rand -hex 48)"
SETUP_TOKEN="$(openssl rand -hex 32)"

printf '%s' "${POSTGRES_PASSWORD}" > "${STATE_DIR}/postgres-password"
printf '%s' "${JWT_SECRET}" > "${STATE_DIR}/jwt-secret"
printf '%s' "${SETUP_TOKEN}" > "${STATE_DIR}/setup-token"

escape_env_value() {
  printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g'
}

cat > "${ENV_FILE}" <<EOF
OMNISOLO_SQLITE_KEY="$(escape_env_value "${SQLITE_KEY}")"
OMNISOLO_WEB_SESSION_SECRET="$(escape_env_value "${WEB_SESSION_SECRET}")"
JWT_SECRET_FILE="$(escape_env_value "${STATE_DIR}/jwt-secret")"
OMNISOLO_SETUP_TOKEN_FILE="$(escape_env_value "${STATE_DIR}/setup-token")"
OMNISOLO_POSTGRES_PASSWORD_FILE="$(escape_env_value "${STATE_DIR}/postgres-password")"
OMNISOLO_WEB_CANONICAL_ORIGIN="${OMNISOLO_WEB_CANONICAL_ORIGIN:-http://127.0.0.1:3000}"
OMNISOLO_WEB_LOCAL_DEV="${OMNISOLO_WEB_LOCAL_DEV:-true}"
EOF
chmod 700 "${STATE_DIR}"
find "${STATE_DIR}" -type f -exec chmod 600 {} +

echo "Created private standalone Compose configuration: ${ENV_FILE}"
echo "Start with: docker compose --env-file ${ENV_FILE} -f deploy/docker-compose.yml up -d --build"
echo "Optional self-hosted Postgres: add --profile postgres (and set DATABASE_URL)"
echo "Optional local Ollama: add --profile ollama"
