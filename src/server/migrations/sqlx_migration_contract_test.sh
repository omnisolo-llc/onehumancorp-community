#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
sqlite_dir="${1:-${repo_root}/crates/ohc-storage/migrations/sqlite}"
postgres_dir="${2:-${repo_root}/crates/ohc-storage/migrations/postgres}"

python3 - "${sqlite_dir}" "${postgres_dir}" <<'PY'
from pathlib import Path
import re
import sys

def check_dir(migrations: Path):
    if not migrations.is_dir():
        raise SystemExit(f"SQLx migration directory is missing: {migrations}")
    versions = {}
    for migration in sorted(migrations.glob("*.sql")):
        match = re.fullmatch(r"([0-9]+)_(.+?)(?:\.(up|down))?\.sql", migration.name)
        if not match or int(match[1]) > 9223372036854775807:
            raise SystemExit(f"Invalid SQLx migration identity: {migration.name}")
        version, description, direction = int(match[1]), match[2], match[3] or "simple"
        entries = versions.setdefault(version, {})
        if direction in entries or (entries and (direction == "simple" or "simple" in entries)):
            raise SystemExit(f"SQLx migration versions must be unique; duplicate numeric version {version}")
        entries[direction] = (description, migration.name)
    if not versions:
        raise SystemExit(f"SQLx migration directory contains no migrations: {migrations}")
    return {v: d for v, entries in versions.items() for d, _ in entries.values()}

sq = check_dir(Path(sys.argv[1]))
pg = check_dir(Path(sys.argv[2]))
if sq != pg:
    raise SystemExit("SQLite and Postgres migration version sequences do not match")
PY

for migration_dir in "${sqlite_dir}" "${postgres_dir}"; do
  if matches="$(grep -RIn --include='*.sql' '^-- +goose Down' "${migration_dir}" || true)" && \
     [[ -n "${matches}" ]]; then
    echo "SQLx executes Goose down sections as forward SQL; remove these rollback sections:" >&2
    printf '%s\n' "${matches}" >&2
    exit 1
  fi

  merged="$(cat "${migration_dir}"/*.sql)"
  for required_table in \
    'CREATE TABLE IF NOT EXISTS help_articles' \
    'CREATE TABLE IF NOT EXISTS video_tutorials' \
    'CREATE TABLE IF NOT EXISTS tooltips' \
    'CREATE TABLE IF NOT EXISTS walkthrough_steps' \
    'CREATE TABLE IF NOT EXISTS job_locations' \
    'base_currency' \
    'enabled_currencies'; do
    if ! grep -Fq "${required_table}" <<<"${merged}"; then
      echo "Unified OSS migrations (${migration_dir}) are missing required marker: ${required_table}" >&2
      exit 1
    fi
  done

  if grep -REiq 'ROW LEVEL SECURITY|CREATE[[:space:]]+POLICY|BYPASSRLS' "${migration_dir}"; then
    echo "OSS migrations (${migration_dir}) must not contain multi-tenant RLS or bypass-role DDL" >&2
    exit 1
  fi
done
