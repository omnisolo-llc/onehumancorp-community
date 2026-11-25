// SPDX-License-Identifier: Apache-2.0
/* @vitest-environment jsdom */
import http from 'node:http';
import type { AddressInfo } from 'node:net';
import { renderHook, act } from '@testing-library/react';
import { describe, it, expect, beforeEach, afterEach } from 'vitest';
import { useOrchestrator } from './useOrchestrator.js';

interface RecordedRequest {
  method: string;
  url: string;
  headers: http.IncomingHttpHeaders;
  body: unknown;
}

const UUID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;

async function startLoopbackServer(
  handler: (req: RecordedRequest, res: http.ServerResponse) => void,
): Promise<{ url: string; requests: RecordedRequest[]; close: () => Promise<void> }> {
  const requests: RecordedRequest[] = [];
  const server = http.createServer((req, res) => {
    const chunks: Buffer[] = [];
    req.on('data', (chunk: Buffer) => chunks.push(chunk));
    req.on('end', () => {
      const rawBody = Buffer.concat(chunks).toString('utf8');
      let parsedBody: unknown = null;
      if (rawBody.length > 0) {
        try {
          parsedBody = JSON.parse(rawBody);
        } catch {
          parsedBody = rawBody;
        }
      }
      const recorded: RecordedRequest = {
        method: req.method ?? 'GET',
        url: req.url ?? '/',
        headers: req.headers,
        body: parsedBody,
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

describe('useOrchestrator (real loopback Task Center API)', () => {
  const envKeys = [
    'OMNISOLO_API_URL',
    'OMNISOLO_BACKEND_URL',
    'OMNISOLO_AGENT_URL',
    'OMNISOLO_API_TOKEN',
    'OMNISOLO_AUTH_TOKEN',
    'OMNISOLO_BEARER_TOKEN',
    'OMNISOLO_TASK_DEPARTMENT',
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
      if (value === undefined) {
        delete process.env[key];
      } else {
        process.env[key] = value;
      }
    }
  });

  it('initializes in idle state', () => {
    const { result, unmount } = renderHook(() => useOrchestrator());

    expect(result.current.status).toBe('Idle');
    expect(result.current.tools).toEqual([]);
    expect(result.current.error).toBeNull();
    expect(result.current.output).toBeNull();

    unmount();
  });

  it('submits an authenticated Task Center task and polls until completion', async () => {
    let polls = 0;
    const server = await startLoopbackServer((req, res) => {
      res.setHeader('Content-Type', 'application/json');
      if (req.method === 'POST' && req.url === '/api/v1/task-center/tasks') {
        res.statusCode = 202;
        res.end(
          JSON.stringify({
            id: 'tsk-cli-101',
            title: 'Summarize invoices',
            prompt: 'Summarize open invoices',
            department: 'operations',
            status: 'queued',
            output_summary: null,
            error_message: null,
            pending_approval: null,
            steps: [
              {
                id: 'stp-1',
                seq: 1,
                kind: 'plan',
                title: 'Admitted under standing authority',
                detail: 'Queued',
                status: 'queued',
                tool_name: null,
              },
            ],
            artifacts: [],
          }),
        );
        return;
      }
      if (req.method === 'GET' && req.url === '/api/v1/task-center/tasks/tsk-cli-101') {
        polls += 1;
        res.statusCode = 200;
        res.end(
          JSON.stringify({
            id: 'tsk-cli-101',
            title: 'Summarize invoices',
            prompt: 'Summarize open invoices',
            department: 'operations',
            status: polls === 1 ? 'running' : 'completed',
            output_summary: polls === 1 ? null : '3 open invoices totaling $1,450.00.',
            error_message: null,
            pending_approval: null,
            steps: [
              {
                id: 'stp-1',
                seq: 1,
                kind: 'tool_call',
                title: 'Query invoice ledger',
                detail: 'Fetched open invoices',
                status: 'completed',
                tool_name: 'invoice_ledger',
              },
            ],
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
    process.env.OMNISOLO_API_TOKEN = 'owner-bearer-secret';

    const { result, unmount } = renderHook(() => useOrchestrator());

    await act(async () => {
      await result.current.runAgent('Summarize open invoices');
    });

    expect(server.requests.length).toBe(3);
    const createReq = server.requests[0];
    expect(createReq.method).toBe('POST');
    expect(createReq.url).toBe('/api/v1/task-center/tasks');
    expect(createReq.headers.authorization).toBe('Bearer owner-bearer-secret');
    expect(String(createReq.headers['idempotency-key'])).toMatch(UUID_RE);
    expect(createReq.body).toEqual({
      prompt: 'Summarize open invoices',
      department: 'operations',
      requested_action: 'run_task',
    });

    expect(server.requests[1].headers.authorization).toBe('Bearer owner-bearer-secret');
    expect(result.current.status).toBe('Complete');
    expect(result.current.output).toBe('3 open invoices totaling $1,450.00.');
    expect(result.current.tools).toEqual([{ name: 'invoice_ledger', status: 'success' }]);
    expect(result.current.error).toBeNull();

    unmount();
  });

  it('surfaces awaiting_approval tasks without treating them as errors', async () => {
    const server = await startLoopbackServer((_req, res) => {
      res.setHeader('Content-Type', 'application/json');
      res.statusCode = 201;
      res.end(
        JSON.stringify({
          id: 'tsk-apr-9',
          title: 'Send client reminder',
          prompt: 'Email overdue clients',
          department: 'operations',
          status: 'awaiting_approval',
          output_summary: null,
          error_message: null,
          pending_approval: {
            approval_id: 'apr-1',
            action_kind: 'run_task',
            department: 'operations',
            summary: 'Approval required for external send',
            payload_sha256: 'a'.repeat(64),
            status: 'pending',
          },
          steps: [],
          artifacts: [],
        }),
      );
    });
    activeClose = server.close;

    process.env.OMNISOLO_API_URL = server.url;
    process.env.OMNISOLO_API_TOKEN = 'owner-token';

    const { result, unmount } = renderHook(() => useOrchestrator());

    await act(async () => {
      await result.current.runAgent('Email overdue clients');
    });

    expect(result.current.status).toBe('Awaiting Approval');
    expect(result.current.output).toContain('Approval required (tsk-apr-9)');
    expect(result.current.error).toBeNull();

    unmount();
  });

  it('falls back to artifact content or default text when output_summary is empty', async () => {
    const server = await startLoopbackServer((_req, res) => {
      res.setHeader('Content-Type', 'application/json');
      res.statusCode = 200;
      res.end(
        JSON.stringify({
          id: 'tsk-art-1',
          title: 'Draft report',
          prompt: 'Draft report',
          department: 'operations',
          status: 'completed',
          output_summary: null,
          error_message: null,
          pending_approval: null,
          steps: [],
          artifacts: [
            { id: 'art-1', title: 'Report', format: 'markdown', content: 'Artifact markdown body' },
          ],
        }),
      );
    });
    activeClose = server.close;

    process.env.OMNISOLO_API_URL = server.url;
    process.env.OMNISOLO_API_TOKEN = 'owner-token';

    const { result, unmount } = renderHook(() => useOrchestrator());

    await act(async () => {
      await result.current.runAgent('Draft report');
    });

    expect(result.current.status).toBe('Complete');
    expect(result.current.output).toBe('Artifact markdown body');

    unmount();
  });

  it('fails closed before sending a request when no bearer token is configured', async () => {
    const server = await startLoopbackServer((_req, res) => {
      res.statusCode = 200;
      res.end('{}');
    });
    activeClose = server.close;

    process.env.OMNISOLO_API_URL = server.url;

    const { result, unmount } = renderHook(() => useOrchestrator());

    await act(async () => {
      await result.current.runAgent('Hello');
    });

    expect(server.requests).toHaveLength(0);
    expect(result.current.status).toBe('Error');
    expect(result.current.error).toContain('OMNISOLO_API_TOKEN');

    unmount();
  });

  it('surfaces backend HTTP error messages and terminal failed task errors', async () => {
    const server = await startLoopbackServer((req, res) => {
      res.setHeader('Content-Type', 'application/json');
      const body = req.body as { prompt?: string };
      if (body.prompt === 'unconfigured') {
        res.statusCode = 503;
        res.end(
          JSON.stringify({
            error: 'No LLM inference provider is configured for this runtime; no task was dispatched',
            code: 'llm_unconfigured',
          }),
        );
        return;
      }
      res.statusCode = 200;
      res.end(
        JSON.stringify({
          id: 'tsk-fail-1',
          title: 'Fail',
          prompt: 'fail',
          department: 'operations',
          status: 'failed',
          output_summary: null,
          error_message: 'Provider returned rate limit error',
          pending_approval: null,
          steps: [],
          artifacts: [],
        }),
      );
    });
    activeClose = server.close;

    process.env.OMNISOLO_API_URL = server.url;
    process.env.OMNISOLO_API_TOKEN = 'owner-token';

    const { result, unmount } = renderHook(() => useOrchestrator());

    await act(async () => {
      await result.current.runAgent('unconfigured');
    });
    expect(result.current.status).toBe('Error');
    expect(result.current.error).toContain('No LLM inference provider is configured');

    await act(async () => {
      await result.current.runAgent('fail');
    });
    expect(result.current.status).toBe('Error');
    expect(result.current.error).toBe('Provider returned rate limit error');

    unmount();
  });

  it('handles unreachable loopback endpoint cleanly', async () => {
    const server = await startLoopbackServer((_req, res) => res.end());
    const closedUrl = server.url;
    await server.close();

    process.env.OMNISOLO_API_URL = closedUrl;
    process.env.OMNISOLO_API_TOKEN = 'owner-token';

    const { result, unmount } = renderHook(() => useOrchestrator());

    await act(async () => {
      await result.current.runAgent('Hello');
    });

    expect(result.current.status).toBe('Error');
    expect(result.current.output).toBeNull();
    expect(result.current.error).toBeTruthy();

    unmount();
  });
});
