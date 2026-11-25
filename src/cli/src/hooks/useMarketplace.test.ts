// SPDX-License-Identifier: Apache-2.0
/* @vitest-environment jsdom */
import http from 'node:http';
import type { AddressInfo } from 'node:net';
import { expect, test, describe, beforeEach, afterEach } from 'vitest';
import { renderHook, waitFor, act } from '@testing-library/react';
import { useMarketplace } from './useMarketplace.js';

interface RecordedRequest {
  method: string;
  url: string;
  headers: http.IncomingHttpHeaders;
}

async function startMarketplaceServer(
  handler: (req: RecordedRequest, res: http.ServerResponse) => void,
): Promise<{ url: string; requests: RecordedRequest[]; close: () => Promise<void> }> {
  const requests: RecordedRequest[] = [];
  const server = http.createServer((req, res) => {
    const recorded: RecordedRequest = {
      method: req.method ?? 'GET',
      url: req.url ?? '/',
      headers: req.headers,
    };
    requests.push(recorded);
    handler(recorded, res);
  });
  await new Promise<void>((resolve) => {
    server.listen(0, '127.0.0.1', () => resolve());
  });
  const address = server.address() as AddressInfo;
  return {
    url: `http://127.0.0.1:${address.port}`,
    requests,
    close: () => new Promise<void>((resolve, reject) => server.close((err) => (err ? reject(err) : resolve()))),
  };
}

describe('useMarketplace (real loopback /api/v1/agents/marketplace)', () => {
  const envKeys = ['OMNISOLO_API_URL', 'OMNISOLO_API_TOKEN', 'OMNISOLO_AUTH_TOKEN'] as const;
  const savedEnv: Partial<Record<(typeof envKeys)[number], string | undefined>> = {};
  let activeClose: (() => Promise<void>) | null = null;

  beforeEach(() => {
    for (const key of envKeys) {
      savedEnv[key] = process.env[key];
      delete process.env[key];
    }
  });

  afterEach(async () => {
    if (activeClose) {
      await activeClose();
      activeClose = null;
    }
    for (const key of envKeys) {
      const value = savedEnv[key];
      if (value === undefined) delete process.env[key];
      else process.env[key] = value;
    }
  });

  test('fetches authenticated marketplace agents and supports query filtering', async () => {
    const server = await startMarketplaceServer((req, res) => {
      res.setHeader('Content-Type', 'application/json');
      res.statusCode = 200;
      const parsed = new URL(req.url, 'http://127.0.0.1');
      const query = parsed.searchParams.get('q') ?? '';
      res.end(
        JSON.stringify(
          query === 'billing'
            ? [
                {
                  id: 'ag-2',
                  name: 'Invoice Follow-Up',
                  description: 'Tracks overdue invoices',
                  author: 'OHC',
                  downloads: 42,
                },
              ]
            : [
                {
                  id: 'ag-1',
                  name: 'Intake Assistant',
                  description: 'Triages inbound requests',
                  author: 'OHC',
                  downloads: 100,
                },
              ],
        ),
      );
    });
    activeClose = server.close;

    process.env.OMNISOLO_API_URL = server.url;
    process.env.OMNISOLO_API_TOKEN = 'owner-token';

    const { result, unmount } = renderHook(() => useMarketplace());

    await waitFor(() => {
      expect(result.current.loading).toBe(false);
    });

    expect(server.requests[0].method).toBe('GET');
    expect(server.requests[0].url).toBe('/api/v1/agents/marketplace');
    expect(server.requests[0].headers.authorization).toBe('Bearer owner-token');
    expect(result.current.agents).toHaveLength(1);
    expect(result.current.agents[0].name).toBe('Intake Assistant');
    expect(result.current.error).toBeNull();

    await act(async () => {
      await result.current.fetchAgents('billing');
    });

    expect(server.requests[1].url).toBe('/api/v1/agents/marketplace?q=billing');
    expect(result.current.agents[0].name).toBe('Invoice Follow-Up');
    unmount();
  });

  test('surfaces HTTP 503 marketplace error message', async () => {
    const server = await startMarketplaceServer((_req, res) => {
      res.setHeader('Content-Type', 'application/json');
      res.statusCode = 503;
      res.end(JSON.stringify({ error: 'Marketplace registry is unavailable' }));
    });
    activeClose = server.close;

    process.env.OMNISOLO_API_URL = server.url;
    process.env.OMNISOLO_API_TOKEN = 'owner-token';

    const { result, unmount } = renderHook(() => useMarketplace());

    await waitFor(() => {
      expect(result.current.loading).toBe(false);
    });

    expect(result.current.error).toBe('Marketplace registry is unavailable');
    expect(result.current.agents).toHaveLength(0);
    unmount();
  });

  test('fails closed when authentication token is missing', async () => {
    const server = await startMarketplaceServer((_req, res) => {
      res.statusCode = 200;
      res.end('[]');
    });
    activeClose = server.close;

    process.env.OMNISOLO_API_URL = server.url;

    const { result, unmount } = renderHook(() => useMarketplace());

    await waitFor(() => {
      expect(result.current.loading).toBe(false);
    });

    expect(server.requests).toHaveLength(0);
    expect(result.current.error).toContain('OMNISOLO_API_TOKEN');
    unmount();
  });
});
