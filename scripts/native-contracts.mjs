// Native replacement for the former sh_test/py_test build targets. Keep these
// checks in CI: Cargo and Vitest alone cannot discover shell/security contracts.
import { spawnSync } from 'node:child_process';
import { existsSync, readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
// Node build/discovery tests are owned by test:scripts (part of make test).
// Keep this runner focused on checks Cargo and Node test discovery cannot find.
// Single-tenant OSS contracts always run; proprietary Cloud contracts run when
// the private cloud/ contract manifest is present in the monorepo checkout.
const ossChecks = [
  ['python3', 'scripts/agent_feed_query_regression.py'],
  ['python3', 'scripts/catalog-edit/source_contract_test.py'],
  ['python3', '.github/scripts/check_checkout_paths_test.py'],
  ['bash', '.github/scripts/check_repo_hygiene_test.sh'],
  ['bash', '.github/scripts/check_repo_hygiene.sh'],
  ['bash', 'deploy/tests/deploy_artifacts_test.sh'],
  ['bash', 'deploy/tests/operational_api_contract_test.sh', 'deploy/scripts/omnisolo-agent-wizard.sh'],
  ['bash', 'scripts/api_versioning_docs_test.sh'],
  ['bash', 'src/server/production_feature_contract_test.sh'],
  ['bash', 'src/server/catalog_auth_contract_test.sh'],
  ['bash', 'src/server/pii_leakage_check.sh', 'src/server'],
  ['bash', 'src/server/migrations/sqlx_migration_contract_test.sh'],
  ['bash', 'src/server/migrations/inbox_schema_contract_test.sh'],
  ['bash', 'src/server/persistence/backend_neutrality_test.sh'],
];
const cloudManifest = path.join(root, 'cloud', 'tests', 'contracts', 'cloud-contracts.json');
const cloudChecks = existsSync(cloudManifest)
  ? JSON.parse(readFileSync(cloudManifest, 'utf8'))
  : [];
const checks = [...ossChecks, ...cloudChecks];
let failures = 0;
for (const [executable, ...args] of checks) {
  console.log(`\nContract: ${args.join(' ')}`);
  const result = spawnSync(executable, args, { cwd: root, shell: false, stdio: 'inherit', timeout: 180_000 });
  if (result.error || result.signal || result.status !== 0) {
    failures++;
    console.error(`FAILED (${result.error?.message || result.signal || result.status})`);
  }
}
console.log(`\nNative contracts: ${checks.length - failures}/${checks.length} passed; ${failures} failed.`);
process.exitCode = failures ? 1 : 0;
