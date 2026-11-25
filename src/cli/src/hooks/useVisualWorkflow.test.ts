// SPDX-License-Identifier: Apache-2.0
/* @vitest-environment jsdom */
import http from 'node:http';
import type { AddressInfo } from 'node:net';
import { describe, it, expect, beforeEach, afterEach } from 'vitest';
import { renderHook, act } from '@testing-library/react';
import { useVisualWorkflow } from './useVisualWorkflow.js';

interface RecordedRequest {
  method: string;
  url: string;
  headers: http.IncomingHttpHeaders;
  body: unknown;
}

async function startWorkflowServer(
  handler: (req: RecordedRequest, res: http.ServerResponse) => void,
): Promise<{ url: string; requests: RecordedRequest[]; close: () => Promise<void> }> {
  const requests: RecordedRequest[] = [];
  const server = http.createServer((req, res) => {
    const chunks: Buffer[] = [];
    req.on('data', (chunk: Buffer) => chunks.push(chunk));
    req.on('end', () => {
      const raw = Buffer.concat(chunks).toString('utf8');
      const recorded: RecordedRequest = {
        method: req.method ?? 'GET',
        url: req.url ?? '/',
        headers: req.headers,
        body: raw ? JSON.parse(raw) : null,
      };
      requests.push(recorded);
      handler(recorded, res);
    });
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

describe('useVisualWorkflow (real loopback Task Center API)', () => {
  const envKeys = [
    'OMNISOLO_API_URL',
    'OMNISOLO_API_TOKEN',
    'OMNISOLO_CLI_POLL_INTERVAL_MS',
    'OMNISOLO_CLI_TIMEOUT_MS',
  ] as const;
  const savedEnv: Partial<Record<(typeof envKeys)[number], string | undefined>> = {};
  let activeClose: (() => Promise<void>) | null = null;

  beforeEach(() => {
    for (const key of envKeys) {
      savedEnv[key] = process.env[key];
      delete process.env[key];
    }
    process.env.OMNISOLO_CLI_POLL_INTERVAL_MS = '15';
    process.env.OMNISOLO_CLI_TIMEOUT_MS = '3000';
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

  it('initializes with idle status', () => {
    const { result, unmount } = renderHook(() => useVisualWorkflow());
    expect(result.current.status).toBe('idle');
    expect(result.current.result).toBeNull();
    expect(result.current.error).toBeNull();
    unmount();
  });

  it('submits workflow graph to /api/v1/task-center/tasks with bearer auth and completes', async () => {
    const server = await startWorkflowServer((req, res) => {
      res.setHeader('Content-Type', 'application/json');
      if (req.method === 'POST' && req.url === '/api/v1/task-center/tasks') {
        res.statusCode = 200;
        res.end(
          JSON.stringify({
            id: 'tsk-wf-1',
            title: 'Workflow execution',
            prompt: 'workflow',
            department: 'operations',
            status: 'completed',
            output_summary: 'workflow success',
            error_message: null,
            pending_approval: null,
            steps: [],
            artifacts: [],
          }),
        );
        return;
      }
      res.statusCode = 404;
      res.end(JSON.stringify({ error: 'Not found' }));
    });
    activeClose = server.close;

    process.env.OMNISOLO_API_URL = server.url;
    process.env.OMNISOLO_API_TOKEN = 'owner-token';

    const { result, unmount } = renderHook(() => useVisualWorkflow());

    const graph = {
      nodes: [
        { id: 'node_1', node_type: { type: 'Input' as const, name: 'in' } },
        { id: 'node_2', node_type: { type: 'Output' as const } },
      ],
      edges: [{ source: 'node_1', target: 'node_2' }],
    };

    await act(async () => {
      await result.current.runWorkflow(graph, { in: 'hello' });
    });

    expect(result.current.status).toBe('complete');
    expect(result.current.result).toBe('workflow success');
    expect(result.current.error).toBeNull();

    expect(server.requests).toHaveLength(1);
    expect(server.requests[0].url).toBe('/api/v1/task-center/tasks');
    expect(server.requests[0].headers.authorization).toBe('Bearer owner-token');
    const body = server.requests[0].body as { prompt: string; requested_action: string };
    expect(body.requested_action).toBe('run_workflow');
    expect(JSON.parse(body.prompt)).toEqual({
      workflow_graph: graph,
      inputs: { in: 'hello' },
    });

    unmount();
  });

  it('handles backend HTTP errors cleanly', async () => {
    const server = await startWorkflowServer((_req, res) => {
      res.setHeader('Content-Type', 'application/json');
      res.statusCode = 500;
      res.end(JSON.stringify({ error: 'Workflow execution failed' }));
    });
    activeClose = server.close;

    process.env.OMNISOLO_API_URL = server.url;
    process.env.OMNISOLO_API_TOKEN = 'owner-token';

    const { result, unmount } = renderHook(() => useVisualWorkflow());

    await act(async () => {
      await result.current.runWorkflow({ nodes: [], edges: [] }, {});
    });

    expect(result.current.status).toBe('error');
    expect(result.current.error).toBe('Workflow execution failed');
    unmount();
  });

  it('fails closed when authentication token is missing', async () => {
    const server = await startWorkflowServer((_req, res) => {
      res.statusCode = 200;
      res.end('{}');
    });
    activeClose = server.close;

    process.env.OMNISOLO_API_URL = server.url;

    const { result, unmount } = renderHook(() => useVisualWorkflow());

    await act(async () => {
      await result.current.runWorkflow({ nodes: [], edges: [] }, {});
    });

    expect(server.requests).toHaveLength(0);
    expect(result.current.status).toBe('error');
    expect(result.current.error).toContain('OMNISOLO_API_TOKEN');
    unmount();
  });
});
