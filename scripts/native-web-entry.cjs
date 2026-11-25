'use strict';
const fs = require('node:fs');
const path = require('node:path');
const crypto = require('node:crypto');

const SESSION_KEY_ID_RE = /^[A-Za-z0-9._-]{1,32}$/;
const SESSION_SECRET_RE = /^[A-Za-z0-9_-]{43}$/;

async function main() {
  const port = Number(process.env.OMNISOLO_NATIVE_WEB_PORT);
  const launch = process.env.OMNISOLO_NATIVE_LAUNCH_ID;
  if (!Number.isSafeInteger(port) || port < 1024 || port > 65535 || !/^[a-zA-Z0-9-]{16,128}$/.test(launch || '')) {
    throw new Error('Invalid native runtime launch contract');
  }
  const configuredKeyId = process.env.OMNISOLO_WEB_SESSION_KEY_ID;
  const configuredSecret = process.env.OMNISOLO_WEB_SESSION_SECRET;
  if (configuredKeyId !== undefined && !SESSION_KEY_ID_RE.test(configuredKeyId)) {
    throw new Error('Invalid OMNISOLO_WEB_SESSION_KEY_ID');
  }
  if (configuredSecret !== undefined && !SESSION_SECRET_RE.test(configuredSecret)) {
    throw new Error('Invalid OMNISOLO_WEB_SESSION_SECRET');
  }
  // Local browser sessions are independent of model/provider credentials.
  process.env.NODE_ENV = 'production';
  process.env.NEXT_TELEMETRY_DISABLED = '1';
  process.env.OMNISOLO_WEB_CANONICAL_ORIGIN = `http://127.0.0.1:${port}`;
  process.env.OMNISOLO_WEB_LOCAL_DEV = 'true';
  process.env.BACKEND_URL ||= 'http://127.0.0.1:18789';
  process.env.OMNISOLO_WEB_SESSION_KEY_ID = configuredKeyId || 'native-session-v1';
  process.env.OMNISOLO_WEB_SESSION_SECRET = configuredSecret || crypto.randomBytes(32).toString('base64url');
  const config = JSON.parse(fs.readFileSync(path.join(__dirname, '.next/required-server-files.json'), 'utf8')).config;
  process.env.__NEXT_PRIVATE_STANDALONE_CONFIG = JSON.stringify(config);
  process.chdir(__dirname);
  require('next');
  const { startServer } = require('next/dist/server/lib/start-server');
  await startServer({ dir: __dirname, isDev: false, config, hostname: '127.0.0.1',
    port, allowRetry: false, keepAliveTimeout: 5000 });
  // Readiness is emitted by this owned process only after its listener starts.
  console.log(`OMNISOLO_NATIVE_READY ${launch} ${port}`);
}
main().catch((error) => { console.error(`Native web startup failed: ${error.message}`); process.exitCode = 1; });
