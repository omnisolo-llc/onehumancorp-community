#!/usr/bin/env bash
set -euo pipefail

source_root="${TEST_SRCDIR:-}/${TEST_WORKSPACE:-_main}/src/server"
if [[ ! -f "$source_root/lib.rs" ]]; then
  source_root="$(git rev-parse --show-toplevel)/src/server"
fi

server_source="$source_root/lib.rs"
http_routes_source="$source_root/server_builder/http_routes.rs"
http_subrouters_source="$source_root/server_builder/http_subrouters.rs"
capabilities_source="$source_root/server_builder/capabilities.rs"
db_source="$source_root/db.rs"
catalog_source="$source_root/api/catalog.rs"
docs_source="$source_root/api/docs.rs"
payment_source="$source_root/api/payment_ledger.rs"
pos_source="$source_root/api/pos.rs"
inventory_source="$source_root/api/pos_inventory.rs"
local_seo_source="$source_root/api/local_seo.rs"

[[ -f "$server_source" ]] || { echo "server source is unavailable: $server_source" >&2; exit 1; }
[[ -f "$db_source" ]] || { echo "database source is unavailable: $db_source" >&2; exit 1; }
[[ -f "$catalog_source" ]] || { echo "catalog source is unavailable: $catalog_source" >&2; exit 1; }
[[ -f "$docs_source" ]] || { echo "docs source is unavailable: $docs_source" >&2; exit 1; }
[[ -f "$payment_source" ]] || { echo "payment source is unavailable: $payment_source" >&2; exit 1; }
[[ -f "$pos_source" ]] || { echo "POS source is unavailable: $pos_source" >&2; exit 1; }
[[ -f "$inventory_source" ]] || { echo "inventory persistence source is unavailable: $inventory_source" >&2; exit 1; }
[[ -f "$local_seo_source" ]] || { echo "local SEO source is unavailable: $local_seo_source" >&2; exit 1; }

router_surface="$server_source"
if [[ -f "$http_routes_source" ]]; then
  router_surface="$http_routes_source"
fi

python3 - "$server_source" "$router_surface" "$capabilities_source" "$db_source" "$catalog_source" "$docs_source" "$payment_source" "$pos_source" "$local_seo_source" "$inventory_source" "$http_subrouters_source" <<'PY'
import pathlib
import re
import sys

server = pathlib.Path(sys.argv[1]).read_text().rsplit("\n#[cfg(test)]\nmod tests {", 1)[0]
router_src = pathlib.Path(sys.argv[2]).read_text()
caps_path = pathlib.Path(sys.argv[3])
caps_src = caps_path.read_text() if caps_path.exists() else server
db = pathlib.Path(sys.argv[4]).read_text()
catalog = pathlib.Path(sys.argv[5]).read_text()
docs = pathlib.Path(sys.argv[6]).read_text()
payment = pathlib.Path(sys.argv[7]).read_text()
pos = pathlib.Path(sys.argv[8]).read_text()
local_seo = pathlib.Path(sys.argv[9]).read_text()
inventory = pathlib.Path(sys.argv[10]).read_text()
subrouters_path = pathlib.Path(sys.argv[11])
subrouters_src = subrouters_path.read_text() if subrouters_path.exists() else ""

combined_routes = router_src + "\n" + subrouters_src + "\n" + server + "\n" + caps_src

if "legacy_db_compatibility_layer" not in combined_routes:
    raise SystemExit("legacy database routes do not use a shared compatibility layer")

if "Extension(std::sync::Arc::new(db.clone()))" in combined_routes:
    raise SystemExit("legacy database layer still wraps an Arc<DB> in a nested Arc")

for route in ("/api/v1/help", "/api/v1/tooltips", "/api/v1/videos", "/api/v1/capabilities"):
    if route not in combined_routes:
        raise SystemExit(f"OSS route is missing from the server router: {route}")

combined_surface = "\n".join((combined_routes, db, docs, payment, pos)).lower()
for forbidden in ("/api/v1/mesh/v2/collective", "get_mysql_pool_if_exists", "row level security", "bypassrls"):
    if forbidden in combined_surface:
        raise SystemExit(f"Cloud/legacy reference leaked into OSS surface: {forbidden}")

if "DbStore::Sqlite" not in docs or "WHERE tenant_id = ?" not in docs:
    raise SystemExit("documentation handlers do not declare parameterized SQLite tenant queries")
if "WHERE tenant_id = ?" not in payment:
    raise SystemExit("payment ledger handlers do not declare parameterized SQLite tenant queries")
if not re.search(r'"/api/v1/ui/inventory",\s*(?:axum::routing::)?get\((?:crate::)?api::pos::get_inventory_handler\)', combined_routes):
    raise SystemExit("inventory UI route is not mounted to its real read handler")
inventory_handler = pos.split("pub async fn get_inventory_handler(", 1)[-1].split("#[cfg(test)]", 1)[0]
if not re.search(r'let Some\(tenant\) = pos_tenant\(claims\.as_ref\(\)\)', inventory_handler):
    raise SystemExit("inventory UI handler does not dispatch its signed tenant to the reader")
if "DbStore::Sqlite" not in inventory or "DbStore::Postgres" not in inventory:
    if "WHERE tenant_id = ?" not in inventory and "WHERE p.tenant_id=?" not in inventory:
        raise SystemExit("inventory persistence module does not contain parameterized tenant queries")
if "/api/v1/ledger/accounts" not in combined_routes or "pub async fn get_accounts" not in payment:
    raise SystemExit("dashboard ledger account alias is not wired to the real ledger handler")
if "/api/v1/ledger/entries" not in combined_routes or "pub async fn get_entries" not in payment:
    raise SystemExit("dashboard ledger entry alias is not wired to the real ledger handler")
if "/api/v1/user/usage" not in combined_routes or "pub async fn get_user_usage" not in payment:
    raise SystemExit("dashboard usage alias is not wired to persisted usage data")
if "ohc_storage::migrate_sqlite" not in db or "ohc_storage::migrate_postgres" not in db:
    raise SystemExit("db.rs must delegate schema migrations to ohc_storage")
if "pub async fn get_discovery_report" not in local_seo:
    raise SystemExit("Local SEO discovery reports handler is missing")
if "OMNISOLO_DATABASE_URL" not in db:
    raise SystemExit("canonical OmniSolo database URL alias is missing")

for fabricated in ("Generated Offering", "AI description", '"10.00"'):
    if fabricated in catalog:
        raise SystemExit(f"catalog generation still contains fabricated fallback data: {fabricated}")

if not re.search(r"MINIMAX_API_KEY", catalog):
    raise SystemExit("catalog generation does not declare its real provider dependency")
PY
