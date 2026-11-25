// SPDX-License-Identifier: Apache-2.0
import http from 'node:http';
import type { AddressInfo } from 'node:net';
import React from 'react';
import { render } from 'ink-testing-library';
import { describe, it, expect, beforeEach, afterEach } from 'vitest';
import { App } from './App.js';

const delay = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

async function startAppLoopbackServer(
  handler: (req: http.IncomingMessage, body: string, res: http.ServerResponse) => void,
): Promise<{ url: string; close: () => Promise<void> }> {
  const server = http.createServer((req, res) => {
    const chunks: Buffer[] = [];
    req.on('data', (chunk: Buffer) => chunks.push(chunk));
    req.on('end', () => {
      handler(req, Buffer.concat(chunks).toString('utf8'), res);
    });
  });

  await new Promise<void>((resolve) => {
    server.listen(0, '127.0.0.1', () => resolve());
  });
  const address = server.address() as AddressInfo;
  return {
    url: `http://127.0.0.1:${address.port}`,
    close: () => new Promise<void>((resolve, reject) => server.close((err) => (err ? reject(err) : resolve()))),
  };
}

describe('App (real loopback HTTP integration)', () => {
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

  it('renders the header and main sections', async () => {
    const { lastFrame, unmount } = render(<App />);
    await delay(10);
    const frame = lastFrame() ?? '';
    expect(frame).toContain('OmniSolo');
    expect(frame).toContain('Select an action');
    expect(frame).toContain('Ask Agent >');
    unmount();
  });

  it('submits a prompt to /api/v1/task-center/tasks and renders the real response', async () => {
    let seenPrompt = '';
    let seenAuth = '';
    const server = await startAppLoopbackServer((req, rawBody, res) => {
      res.setHeader('Content-Type', 'application/json');
      if (req.method === 'POST' && req.url === '/api/v1/task-center/tasks') {
        seenAuth = String(req.headers.authorization ?? '');
        const parsed = JSON.parse(rawBody) as { prompt: string };
        seenPrompt = parsed.prompt;
        res.statusCode = 200;
        res.end(
          JSON.stringify({
            id: 'tsk-app-1',
            title: parsed.prompt,
            prompt: parsed.prompt,
            department: 'operations',
            status: 'completed',
            output_summary: 'Verified loopback task reply',
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
    process.env.OMNISOLO_API_TOKEN = 'cli-owner-token';

    const { stdin, lastFrame, unmount } = render(<App />);
    await delay(10);
    stdin.write('h');
    await delay(10);
    stdin.write('i');
    await delay(10);
    stdin.write('\r');

    await delay(120);

    const frame = lastFrame() ?? '';
    expect(seenPrompt).toBe('hi');
    expect(seenAuth).toBe('Bearer cli-owner-token');
    expect(frame).toContain('User:');
    expect(frame).toContain('hi');
    expect(frame).toContain('Agent:');
    expect(frame).toContain('Verified loopback task reply');
    unmount();
  });

  it('renders error state when backend rejects task submission', async () => {
    const server = await startAppLoopbackServer((_req, _rawBody, res) => {
      res.setHeader('Content-Type', 'application/json');
      res.statusCode = 503;
      res.end(JSON.stringify({ error: 'LLM provider unconfigured' }));
    });
    activeClose = server.close;

    process.env.OMNISOLO_API_URL = server.url;
    process.env.OMNISOLO_API_TOKEN = 'cli-owner-token';

    const { stdin, lastFrame, unmount } = render(<App />);
    await delay(10);
    stdin.write('go');
    await delay(10);
    stdin.write('\r');

    await delay(120);

    const frame = lastFrame() ?? '';
    expect(frame).toContain('ERROR');
    expect(frame).toContain('LLM provider unconfigured');
    expect(frame).not.toContain('Ask Agent >');
    unmount();
  });

  it('navigates to Browse Agent Marketplace and fetches live agents', async () => {
    const server = await startAppLoopbackServer((req, _rawBody, res) => {
      res.setHeader('Content-Type', 'application/json');
      if (req.url === '/api/v1/agents/marketplace') {
        res.statusCode = 200;
        res.end(
          JSON.stringify([
            {
              id: 'mkt-1',
              name: 'Scheduler Pro',
              description: 'Manages appointments',
              author: 'OHC',
              downloads: 12,
            },
          ]),
        );
        return;
      }
      res.statusCode = 404;
      res.end('{}');
    });
    activeClose = server.close;

    process.env.OMNISOLO_API_URL = server.url;
    process.env.OMNISOLO_API_TOKEN = 'cli-owner-token';

    const { stdin, lastFrame, unmount } = render(<App />);
    await delay(10);
    for (let i = 0; i < 9; i++) {
      stdin.write('\x1B[B');
      await delay(10);
    }
    stdin.write('\r');
    await delay(120);

    const frame = lastFrame() ?? '';
    expect(frame).toContain('Agent Marketplace');
    expect(frame).toContain('Scheduler Pro');
    unmount();
  });

  it('navigates to Visual Workflow Builder', async () => {
    const { stdin, lastFrame, unmount } = render(<App />);
    await delay(10);
    for (let i = 0; i < 10; i++) {
      stdin.write('\x1B[B');
      await delay(10);
    }
    stdin.write('\r');
    await delay(50);

    const frame = lastFrame() ?? '';
    expect(frame).toContain('Visual Workflow Builder');
    unmount();
  });
});
