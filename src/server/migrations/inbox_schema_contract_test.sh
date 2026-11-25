#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
migration_dir="${1:-${repo_root}/crates/ohc-storage/migrations/postgres}"

normalized_sql="$({ cat "${migration_dir}"/*.sql; } | tr '\n' ' ' | tr -s '[:space:]' ' ')"

if ! grep -Eiq '(CREATE TABLE IF NOT EXISTS inbox_messages[^;]*sender_id[[:space:]]+TEXT|ALTER TABLE inbox_messages ADD COLUMN IF NOT EXISTS sender_id[[:space:]]+TEXT)' <<<"${normalized_sql}"; then
  echo "inbox_messages.sender_id is queried by the desktop inbox API but is absent from the OSS migrations" >&2
  exit 1
fi
