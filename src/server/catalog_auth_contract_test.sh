#!/usr/bin/env bash
set -euo pipefail

server_source="${OMNISOLO_SERVER_LIB_SOURCE:-${TEST_SRCDIR:-}/${TEST_WORKSPACE:-_main}/src/server/server_builder/http_routes.rs}"
if [[ ! -f "$server_source" ]]; then
  repo_root="$(git rev-parse --show-toplevel)/src/server"
  if [[ -f "${repo_root}/server_builder/http_routes.rs" ]]; then
    server_source="${repo_root}/server_builder/http_routes.rs"
  else
    server_source="${repo_root}/lib.rs"
  fi
fi

python3 - "$server_source" <<'PY'
import pathlib
import re
import sys

source = pathlib.Path(sys.argv[1]).read_text()
pattern = r'"/api/v1/catalog",\s*(?:crate::)?api::catalog::router\([\s\S]*?strict_bearer_auth_middleware'
if not re.search(pattern, source):
    raise SystemExit("catalog router is not protected by strict bearer authentication")
PY
