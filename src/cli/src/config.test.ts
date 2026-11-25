// SPDX-License-Identifier: Apache-2.0
import { describe, it, expect } from 'vitest';
import {
  CliConfigError,
  DEFAULT_API_BASE_URL,
  DEFAULT_DEPARTMENT,
  DEFAULT_POLL_INTERVAL_MS,
  DEFAULT_REQUEST_TIMEOUT_MS,
  buildAuthHeaders,
  normalizeBaseUrl,
  resolveCliConfig,
} from './config.js';

describe('CLI typed configuration', () => {
  it('resolves defaults with configured API token', () => {
    const config = resolveCliConfig({
      OMNISOLO_API_TOKEN: 'ohc-live-session-token',
    });

    expect(config).toEqual({
      baseUrl: DEFAULT_API_BASE_URL,
      authToken: 'ohc-live-session-token',
      department: DEFAULT_DEPARTMENT,
      pollIntervalMs: DEFAULT_POLL_INTERVAL_MS,
      timeoutMs: DEFAULT_REQUEST_TIMEOUT_MS,
    });
    expect(buildAuthHeaders(config, { 'Content-Type': 'application/json' })).toEqual({
      Authorization: 'Bearer ohc-live-session-token',
      Accept: 'application/json',
      'Content-Type': 'application/json',
    });
  });

  it('prefers OMNISOLO_API_URL over legacy OMNISOLO_AGENT_URL and normalizes trailing slashes', () => {
    const config = resolveCliConfig({
      OMNISOLO_API_URL: 'https://workspace.example.com/ohc///',
      OMNISOLO_AGENT_URL: 'http://127.0.0.1:9999',
      OMNISOLO_AUTH_TOKEN: 'bearer-tok-2',
      OMNISOLO_TASK_DEPARTMENT: 'Finance',
      OMNISOLO_CLI_POLL_INTERVAL_MS: '25',
      OMNISOLO_CLI_TIMEOUT_MS: '5000',
    });

    expect(config.baseUrl).toBe('https://workspace.example.com/ohc');
    expect(config.authToken).toBe('bearer-tok-2');
    expect(config.department).toBe('finance');
    expect(config.pollIntervalMs).toBe(25);
    expect(config.timeoutMs).toBe(5000);
  });

  it('fails closed when authentication token is absent', () => {
    expect(() => resolveCliConfig({ OMNISOLO_API_URL: 'http://127.0.0.1:18789' })).toThrowError(
      CliConfigError,
    );
    expect(() => resolveCliConfig({ OMNISOLO_API_TOKEN: '   ' })).toThrowError(
      /Authentication required/,
    );
  });

  it('rejects insecure non-loopback HTTP, embedded credentials, and malformed URLs', () => {
    for (const invalid of [
      'http://api.example.com',
      'https://user:secret@api.example.com',
      'https://api.example.com?token=abc',
      'https://api.example.com#frag',
      'ftp://127.0.0.1:18789',
      'not-a-url',
    ]) {
      expect(() => normalizeBaseUrl(invalid)).toThrowError(CliConfigError);
    }

    expect(normalizeBaseUrl('http://localhost:18789/')).toBe('http://localhost:18789');
    expect(normalizeBaseUrl('http://[::1]:18789/')).toBe('http://[::1]:18789');
  });

  it('rejects invalid department and polling intervals', () => {
    expect(() =>
      resolveCliConfig({
        OMNISOLO_API_TOKEN: 'tok',
        OMNISOLO_TASK_DEPARTMENT: 'bad department!',
      }),
    ).toThrowError(CliConfigError);

    expect(() =>
      resolveCliConfig({
        OMNISOLO_API_TOKEN: 'tok',
        OMNISOLO_CLI_POLL_INTERVAL_MS: '1',
      }),
    ).toThrowError(CliConfigError);

    expect(() =>
      resolveCliConfig({
        OMNISOLO_API_TOKEN: 'tok',
        OMNISOLO_CLI_TIMEOUT_MS: '50',
      }),
    ).toThrowError(CliConfigError);
  });
});
