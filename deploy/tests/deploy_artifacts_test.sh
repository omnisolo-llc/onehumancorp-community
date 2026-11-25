#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

compose_file="${root}/deploy/docker-compose.yml"
toolchain_file="${root}/rust-toolchain.toml"
standalone_file="${root}/deploy/scripts/omnisolo-standalone.sh"
server_dockerfile="${root}/deploy/docker/server/Dockerfile"
web_dockerfile="${root}/deploy/docker/web/Dockerfile"
web_package_file="${root}/src/ui/next/package.json"
dockerignore_file="${root}/.dockerignore"

# Verify required standalone OSS deployment files are present and non-empty.
for file in \
  "$compose_file" \
  "$toolchain_file" \
  "$dockerignore_file" \
  "$server_dockerfile" \
  "$web_dockerfile" \
  "$web_package_file" \
  "$standalone_file"; do
  test -s "$file"
done

# Native target-platform builds preserve SQLCipher's OpenSSL headers/runtime.
grep -q 'libssl-dev' "$server_dockerfile"
grep -q '^FROM rust:' "$server_dockerfile"
! grep -q 'FROM --platform=\$BUILDPLATFORM' "$server_dockerfile"
grep -q 'cargo chef cook --release --locked' "$server_dockerfile"
grep -q 'cargo build --locked --release' "$server_dockerfile"
grep -q 'libssl3' "$server_dockerfile"
grep -q 'npm ci' "$web_dockerfile"
grep -q '^FROM node:' "$web_dockerfile"
! grep -q 'FROM --platform=\$BUILDPLATFORM' "$web_dockerfile"
grep -q 'npm run build:web' "$web_dockerfile"
grep -q 'COPY --from=builder --chown=node:node /src/target/native-web' "$web_dockerfile"
grep -q '^USER node' "$web_dockerfile"
grep -Fq 'CMD ["node", "src/ui/next/server.js"]' "$web_dockerfile"
python3 - "$web_package_file" <<'PY'
import json
import pathlib
import sys

package = json.loads(pathlib.Path(sys.argv[1]).read_text())
if "next" not in package.get("dependencies", {}):
    raise SystemExit("Next.js must be a runtime dependency for the production web image")
if "next" in package.get("devDependencies", {}):
    raise SystemExit("Next.js must not remain a dev-only dependency in the production web image")
PY
grep -qx "target" "$dockerignore_file"
grep -qx "node_modules" "$dockerignore_file"

# All required native executables and migrations must be included, with an
# unprivileged runtime and independently selectable server/agent/worker images.
for binary in server omnisolo-builtin-agent omnisolo-harness-worker; do
  grep -Fq "/src/target/release/$binary /usr/local/bin/$binary" "$server_dockerfile"
done
for role in server agent worker; do
  grep -q "^FROM runtime AS $role$" "$server_dockerfile"
done
grep -q '^USER omnisolo$' "$server_dockerfile"
grep -q '/src/src/server/migrations /src/server/migrations' "$server_dockerfile"
grep -q '/src/src/server/db/migrations /src/server/db/migrations' "$server_dockerfile"

python3 - "$server_dockerfile" <<'PY'
import pathlib, sys
text = pathlib.Path(sys.argv[1]).read_text()
builder = text.split('FROM chef AS builder', 1)[1]
assert builder.index('cargo chef cook') < builder.index('COPY . .') < builder.index('cargo build'), 'Dependency-layer order is broken'
PY

# Verify standalone docker-compose wires server + web + optional postgres/ollama
# without multi-tenant or cloud-only infrastructure.
grep -q "^  server:" "$compose_file"
grep -q "^  web:" "$compose_file"
grep -q "^  postgres:" "$compose_file"
grep -q "^  ollama:" "$compose_file"
grep -q "omnisolo/server:latest" "$compose_file"
grep -q "omnisolo/web:latest" "$compose_file"
! grep -q "omnisolo/ui" "$compose_file"
! grep -q "^  ui:" "$compose_file"

# Verify standalone startup script uses the readiness endpoint exposed by src/server/lib.rs.
grep -q "/readyz" "$standalone_file"
! grep -q "/health " "$standalone_file"

echo "standalone deployment artifact checks passed"
