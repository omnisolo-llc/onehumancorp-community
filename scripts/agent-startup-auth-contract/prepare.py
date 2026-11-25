"""Compile actual server builder authentication setup against non-test auth code."""
from pathlib import Path
import hashlib
import json
HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
builder_path = ROOT / 'src/server/server_builder/builder.rs'
source = builder_path.read_text()
marker = '        let grpc_tls_config = grpc_tls_config_from_env(standalone)?;'
redis_marker = '\n        // Validate Redis startup before spawning workers.'
database_marker = '\n        let db = self.resolve_database().await?;'
for boundary in [marker, database_marker]:
    assert source.count(boundary) == 1, 'Startup extraction boundaries must be unique'
assert source.count(redis_marker) <= 1, 'Optional Redis startup boundary must be unique'
start = source.index(marker) + len(marker)
database = source.index(database_marker)
if redis_marker in source:
    end = source.index(redis_marker)
    assert start < end < database, 'Authentication must precede Redis and database startup'
    # Keep the entire authentication segment, including added validations. Fail
    # closed if an auth reference moves into the separately compiled Redis region.
    redis_setup = source[end:database]
    for auth_reference in ['builtin_agent_auth', 'omnisolo_builtin_agent::auth']:
        assert auth_reference not in redis_setup, 'Authentication moved outside the compiled auth segment'
else:
    assert start < database, 'Authentication must precede database startup'
    end = database
post_db = source[database:]
for auth_reference in ['let builtin_agent_auth', 'omnisolo_builtin_agent::auth::auth_mode_from_env']:
    assert auth_reference not in post_db, 'Authentication moved after database initialization'
setup = source[start:end]
assert 'let builtin_agent_auth = if standalone' in setup
assert 'omnisolo_builtin_agent::auth::auth_mode_from_env()' in setup
assert '.await' not in setup and 'redis::' not in setup and 'crate::queue' not in setup
(HERE/'generated.rs').write_text('\n'.join([
    'extern crate self as omnisolo_builtin_agent;',
    '#[path='+json.dumps(str(ROOT/'src/agents/builtin/auth.rs'))+'] pub mod auth;',
    'pub fn actual_startup(standalone: bool) -> Result<Option<auth::AuthMode>, std::io::Error> {',
    setup, 'Ok(builtin_agent_auth)', '}',
])+'\n')
inputs = [builder_path, ROOT/'src/agents/builtin/auth.rs', ROOT/'Cargo.lock',
          ROOT/'scripts/agent-startup-auth.test.mjs', ROOT/'.github/workflows/ci.yml']
inputs += [HERE/name for name in ['Cargo.toml','prepare.py','probe.rs','test.rs','run.sh','verify_lock.py','README.md']]
(HERE/'source-manifest.json').write_text(json.dumps({str(p.relative_to(ROOT)):hashlib.sha256(p.read_bytes()).hexdigest() for p in inputs},indent=2)+'\n')
