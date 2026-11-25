// SPDX-License-Identifier: Apache-2.0

export const DEFAULT_API_BASE_URL = 'http://127.0.0.1:18789';
export const DEFAULT_DEPARTMENT = 'operations';
export const DEFAULT_POLL_INTERVAL_MS = 500;
export const DEFAULT_REQUEST_TIMEOUT_MS = 120_000;

const LOOPBACK_HOSTS = new Set(['localhost', '127.0.0.1', '[::1]', '::1']);
const DEPARTMENT_RE = /^[a-z0-9_-]{1,64}$/;

export interface CliConfig {
  readonly baseUrl: string;
  readonly authToken: string;
  readonly department: string;
  readonly pollIntervalMs: number;
  readonly timeoutMs: number;
}

export class CliConfigError extends Error {
  constructor(message: string) {
    super(message);
    this.name = 'CliConfigError';
  }
}

export function normalizeBaseUrl(raw: string): string {
  let parsed: URL;
  try {
    parsed = new URL(raw.trim());
  } catch {
    throw new CliConfigError('OMNISOLO_API_URL must be a valid absolute URL');
  }

  const isLoopback = LOOPBACK_HOSTS.has(parsed.hostname.toLowerCase());
  const validScheme = parsed.protocol === 'https:' || (parsed.protocol === 'http:' && isLoopback);
  if (!validScheme) {
    throw new CliConfigError('OMNISOLO_API_URL must use https:// or a local loopback http:// origin');
  }
  if (parsed.username || parsed.password) {
    throw new CliConfigError('OMNISOLO_API_URL must not contain embedded credentials');
  }
  if (parsed.search || parsed.hash) {
    throw new CliConfigError('OMNISOLO_API_URL must not contain query parameters or fragments');
  }

  const normalizedPath = parsed.pathname.replace(/\/+$/, '');
  return `${parsed.origin}${normalizedPath}`;
}

function parseBoundedInt(
  raw: string | undefined,
  fallback: number,
  min: number,
  max: number,
  envName: string,
): number {
  if (raw === undefined || raw.trim() === '') {
    return fallback;
  }
  const value = Number(raw.trim());
  if (!Number.isSafeInteger(value) || value < min || value > max) {
    throw new CliConfigError(`${envName} must be an integer between ${min} and ${max}`);
  }
  return value;
}

export function resolveCliConfig(env: NodeJS.ProcessEnv = process.env): CliConfig {
  const rawBaseUrl =
    env.OMNISOLO_API_URL?.trim() ||
    env.OMNISOLO_BACKEND_URL?.trim() ||
    env.OMNISOLO_AGENT_URL?.trim() ||
    DEFAULT_API_BASE_URL;
  const baseUrl = normalizeBaseUrl(rawBaseUrl);

  const authToken =
    env.OMNISOLO_API_TOKEN?.trim() ||
    env.OMNISOLO_AUTH_TOKEN?.trim() ||
    env.OMNISOLO_BEARER_TOKEN?.trim() ||
    '';
  if (!authToken) {
    throw new CliConfigError(
      'Authentication required: configure OMNISOLO_API_TOKEN (or OMNISOLO_AUTH_TOKEN) before calling the OneHumanCorp API.',
    );
  }

  const rawDepartment = env.OMNISOLO_TASK_DEPARTMENT?.trim().toLowerCase() || DEFAULT_DEPARTMENT;
  if (!DEPARTMENT_RE.test(rawDepartment)) {
    throw new CliConfigError(
      'OMNISOLO_TASK_DEPARTMENT must match [a-z0-9_-]{1,64}',
    );
  }

  const pollIntervalMs = parseBoundedInt(
    env.OMNISOLO_CLI_POLL_INTERVAL_MS,
    DEFAULT_POLL_INTERVAL_MS,
    10,
    30_000,
    'OMNISOLO_CLI_POLL_INTERVAL_MS',
  );
  const timeoutMs = parseBoundedInt(
    env.OMNISOLO_CLI_TIMEOUT_MS,
    DEFAULT_REQUEST_TIMEOUT_MS,
    100,
    600_000,
    'OMNISOLO_CLI_TIMEOUT_MS',
  );

  return {
    baseUrl,
    authToken,
    department: rawDepartment,
    pollIntervalMs,
    timeoutMs,
  };
}

export function buildAuthHeaders(
  config: CliConfig,
  extra: Record<string, string> = {},
): Record<string, string> {
  return {
    Authorization: `Bearer ${config.authToken}`,
    Accept: 'application/json',
    ...extra,
  };
}
