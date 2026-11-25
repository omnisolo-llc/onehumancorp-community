import test from 'node:test';
import assert from 'node:assert/strict';
import http from 'node:http';
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const SCRIPT_PATH = path.resolve(path.dirname(fileURLToPath(import.meta.url)), 'ohc-mcp-stdio.mjs');
const TEST_TOKEN = 'ohc-live-test-bearer-9f2a7d1e4c8b';

function startLoopbackMcpServer(handler) {
  const received = [];
  const server = http.createServer((req, res) => {
    const chunks = [];
    req.on('data', (chunk) => chunks.push(chunk));
    req.on('end', () => {
      const rawBody = Buffer.concat(chunks).toString('utf8');
      let body;
      try {
        body = rawBody ? JSON.parse(rawBody) : null;
      } catch {
        body = null;
      }
      const record = {
        method: req.method,
        url: req.url,
        headers: { ...req.headers },
        rawBody,
        body,
      };
      received.push(record);
      handler(record, res);
    });
  });

  return new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(0, '127.0.0.1', () => {
      const address = server.address();
      resolve({
        baseUrl: `http://127.0.0.1:${address.port}`,
        received,
        close: () => new Promise((done) => server.close(done)),
      });
    });
  });
}

function defaultMcpHandler(req, res) {
  if (req.method !== 'POST' || req.url !== '/api/v1/integrations/mcp') {
    res.writeHead(404, { 'content-type': 'application/json' });
    res.end(JSON.stringify({ error: 'Not Found' }));
    return;
  }
  const rpc = req.body;
  if (!rpc || rpc.id === undefined) {
    res.writeHead(204);
    res.end();
    return;
  }
  if (rpc.method === 'initialize') {
    res.writeHead(200, { 'content-type': 'application/json' });
    res.end(
      JSON.stringify({
        jsonrpc: '2.0',
        id: rpc.id,
        result: {
          protocolVersion: '2025-03-26',
          capabilities: { tools: {}, resources: {} },
          serverInfo: { name: 'omni-assistant', version: '1.0.0' },
        },
      }),
    );
    return;
  }
  if (rpc.method === 'tools/list') {
    res.writeHead(200, { 'content-type': 'application/json' });
    res.end(
      JSON.stringify({
        jsonrpc: '2.0',
        id: rpc.id,
        result: {
          tools: [
            { name: 'ohc_list_spaces', description: 'List Omni Assistant spaces' },
            { name: 'ohc_list_approvals', description: 'List pending approvals' },
          ],
        },
      }),
    );
    return;
  }
  if (rpc.method === 'tools/call') {
    res.writeHead(200, { 'content-type': 'application/json' });
    res.end(
      JSON.stringify({
        jsonrpc: '2.0',
        id: rpc.id,
        result: {
          content: [
            {
              type: 'text',
              text: JSON.stringify({
                tool: rpc.params?.name ?? null,
                writeHeaderPresent: Object.prototype.hasOwnProperty.call(req.headers, 'x-ohc-mcp-write'),
                writeHeaderValue: req.headers['x-ohc-mcp-write'] ?? null,
                echoLabel: rpc.params?.arguments?.label ?? null,
              }),
            },
          ],
        },
      }),
    );
    return;
  }
  res.writeHead(200, { 'content-type': 'application/json' });
  res.end(
    JSON.stringify({
      jsonrpc: '2.0',
      id: rpc.id,
      error: { code: -32601, message: `Method not found: ${rpc.method}` },
    }),
  );
}

function runBridge({ args = [], env = {}, stdinChunks = [], closeStdin = true }) {
  return new Promise((resolve, reject) => {
    const child = spawn(process.execPath, [SCRIPT_PATH, ...args], {
      env: { PATH: process.env.PATH ?? '', ...env },
      stdio: ['pipe', 'pipe', 'pipe'],
    });
    const outChunks = [];
    const errChunks = [];
    child.stdout.on('data', (chunk) => outChunks.push(chunk));
    child.stderr.on('data', (chunk) => errChunks.push(chunk));
    child.once('error', reject);
    child.once('close', (code, signal) => {
      resolve({
        code,
        signal,
        stdoutBuf: Buffer.concat(outChunks),
        stdout: Buffer.concat(outChunks).toString('utf8'),
        stderr: Buffer.concat(errChunks).toString('utf8'),
      });
    });

    (async () => {
      for (const chunk of stdinChunks) {
        child.stdin.write(chunk);
        await new Promise((r) => setTimeout(r, 5));
      }
      if (closeStdin) child.stdin.end();
    })().catch(reject);
  });
}

function encodeContentLength(obj) {
  const json = JSON.stringify(obj);
  const bytes = Buffer.byteLength(json, 'utf8');
  return Buffer.from(`Content-Length: ${bytes}\r\n\r\n${json}`, 'utf8');
}

function decodeContentLengthFrames(buf) {
  const frames = [];
  let offset = 0;
  while (offset < buf.length) {
    const sep = buf.indexOf('\r\n\r\n', offset, 'ascii');
    assert.notEqual(sep, -1, 'Expected CRLF header delimiter in Content-Length output');
    const header = buf.subarray(offset, sep).toString('ascii');
    const match = header.match(/^Content-Length:\s*(\d+)$/i);
    assert.ok(match, `Unexpected header: ${header}`);
    const len = Number(match[1]);
    const bodyStart = sep + 4;
    const bodyEnd = bodyStart + len;
    assert.ok(bodyEnd <= buf.length, 'Truncated Content-Length body');
    frames.push(JSON.parse(buf.subarray(bodyStart, bodyEnd).toString('utf8')));
    offset = bodyEnd;
  }
  return frames;
}

test('CLI --help exits 0 without requiring credentials', async () => {
  const res = await runBridge({ args: ['--help'] });
  assert.equal(res.code, 0);
  assert.match(res.stdout, /Omni Assistant MCP stdio-to-HTTP bridge/);
  assert.match(res.stdout, /OHC_BASE_URL/);
  assert.match(res.stdout, /OHC_BEARER_TOKEN/);
  assert.equal(res.stderr, '');
});

test('fails closed when OHC_BASE_URL or OHC_BEARER_TOKEN is missing or invalid', async () => {
  const missingUrl = await runBridge({ env: { OHC_BEARER_TOKEN: TEST_TOKEN } });
  assert.equal(missingUrl.code, 1);
  assert.match(missingUrl.stderr, /Missing OHC_BASE_URL/);
  assert.equal(missingUrl.stderr.includes(TEST_TOKEN), false);

  const missingToken = await runBridge({ env: { OHC_BASE_URL: 'http://127.0.0.1:18080' } });
  assert.equal(missingToken.code, 1);
  assert.match(missingToken.stderr, /Missing OHC_BEARER_TOKEN/);

  const badProtocol = await runBridge({
    env: { OHC_BASE_URL: 'file:///etc/passwd', OHC_BEARER_TOKEN: TEST_TOKEN },
  });
  assert.equal(badProtocol.code, 1);
  assert.match(badProtocol.stderr, /http and https/);
  assert.equal(badProtocol.stderr.includes(TEST_TOKEN), false);

  const embeddedCreds = await runBridge({
    env: { OHC_BASE_URL: 'https://user:pass@ohc.example.com', OHC_BEARER_TOKEN: TEST_TOKEN },
  });
  assert.equal(embeddedCreds.code, 1);
  assert.match(embeddedCreds.stderr, /embedded URL credentials/);

  const badWriteFlag = await runBridge({
    env: { OHC_BASE_URL: 'http://127.0.0.1:18080', OHC_BEARER_TOKEN: TEST_TOKEN, OHC_MCP_ALLOW_WRITE: 'yes' },
  });
  assert.equal(badWriteFlag.code, 1);
  assert.match(badWriteFlag.stderr, /OHC_MCP_ALLOW_WRITE/);
});

test('newline-delimited JSON-RPC initialize, notification, tools/list, tools/call and read-only default', async () => {
  const srv = await startLoopbackMcpServer(defaultMcpHandler);
  try {
    const res = await runBridge({
      env: { OHC_BASE_URL: srv.baseUrl, OHC_BEARER_TOKEN: TEST_TOKEN },
      stdinChunks: [
        `${JSON.stringify({ jsonrpc: '2.0', id: 0, method: 'initialize', params: { protocolVersion: '2025-03-26' } })}\n`,
        `${JSON.stringify({ jsonrpc: '2.0', method: 'notifications/initialized' })}\n`,
        `${JSON.stringify({ jsonrpc: '2.0', id: 'list-1', method: 'tools/list' })}\n`,
        `${JSON.stringify({ jsonrpc: '2.0', id: 3, method: 'tools/call', params: { name: 'ohc_list_spaces', arguments: {} } })}\n`,
      ],
    });

    assert.equal(res.code, 0, `stderr: ${res.stderr}`);
    assert.equal(res.stderr, '');

    const lines = res.stdout.trim().split('\n').map((line) => JSON.parse(line));
    assert.equal(lines.length, 3, 'Notification must not produce a stdout response');
    assert.equal(lines[0].id, 0);
    assert.equal(lines[0].result.protocolVersion, '2025-03-26');
    assert.equal(lines[0].result.serverInfo.name, 'omni-assistant');

    assert.equal(lines[1].id, 'list-1');
    assert.equal(lines[1].result.tools.length, 2);

    assert.equal(lines[2].id, 3);
    const toolPayload = JSON.parse(lines[2].result.content[0].text);
    assert.equal(toolPayload.tool, 'ohc_list_spaces');
    assert.equal(toolPayload.writeHeaderPresent, false);
    assert.equal(toolPayload.writeHeaderValue, null);

    assert.equal(srv.received.length, 4);
    for (const req of srv.received) {
      assert.equal(req.url, '/api/v1/integrations/mcp');
      assert.equal(req.headers.authorization, `Bearer ${TEST_TOKEN}`);
      assert.equal(Object.prototype.hasOwnProperty.call(req.headers, 'x-ohc-mcp-write'), false);
    }
    assert.equal(res.stdout.includes(TEST_TOKEN), false);
    assert.equal(res.stderr.includes(TEST_TOKEN), false);
  } finally {
    await srv.close();
  }
});

test('Content-Length framed round-trip with multi-byte UTF-8 and split chunks', async () => {
  const srv = await startLoopbackMcpServer(defaultMcpHandler);
  try {
    const frame1 = encodeContentLength({
      jsonrpc: '2.0',
      id: 11,
      method: 'initialize',
      params: { protocolVersion: '2025-03-26' },
    });
    const frame2 = encodeContentLength({
      jsonrpc: '2.0',
      id: 'utf8-call',
      method: 'tools/call',
      params: { name: 'ohc_list_spaces', arguments: { label: 'Omni ↔ Assistant €99' } },
    });
    const combined = Buffer.concat([frame1, frame2]);

    const res = await runBridge({
      args: ['--base-url', srv.baseUrl],
      env: { OHC_BEARER_TOKEN: TEST_TOKEN },
      stdinChunks: [
        combined.subarray(0, 7),
        combined.subarray(7, 25),
        combined.subarray(25, frame1.length + 12),
        combined.subarray(frame1.length + 12),
      ],
    });

    assert.equal(res.code, 0, `stderr: ${res.stderr}`);
    assert.equal(res.stderr, '');
    const replies = decodeContentLengthFrames(res.stdoutBuf);
    assert.equal(replies.length, 2);
    assert.equal(replies[0].id, 11);
    assert.equal(replies[0].result.serverInfo.name, 'omni-assistant');
    assert.equal(replies[1].id, 'utf8-call');
    const payload = JSON.parse(replies[1].result.content[0].text);
    assert.equal(payload.echoLabel, 'Omni ↔ Assistant €99');
  } finally {
    await srv.close();
  }
});

test('explicit --allow-write and OHC_MCP_ALLOW_WRITE=true send x-ohc-mcp-write: true', async () => {
  const srv = await startLoopbackMcpServer(defaultMcpHandler);
  try {
    const reqLine = `${JSON.stringify({
      jsonrpc: '2.0',
      id: 1,
      method: 'tools/call',
      params: { name: 'ohc_propose_task', arguments: {} },
    })}\n`;

    const viaFlag = await runBridge({
      args: ['--allow-write'],
      env: { OHC_BASE_URL: srv.baseUrl, OHC_BEARER_TOKEN: TEST_TOKEN },
      stdinChunks: [reqLine],
    });
    assert.equal(viaFlag.code, 0);
    const flagReply = JSON.parse(viaFlag.stdout.trim());
    const flagPayload = JSON.parse(flagReply.result.content[0].text);
    assert.equal(flagPayload.writeHeaderPresent, true);
    assert.equal(flagPayload.writeHeaderValue, 'true');

    const viaEnv = await runBridge({
      env: { OHC_BASE_URL: srv.baseUrl, OHC_BEARER_TOKEN: TEST_TOKEN, OHC_MCP_ALLOW_WRITE: 'true' },
      stdinChunks: [reqLine],
    });
    assert.equal(viaEnv.code, 0);
    const envReply = JSON.parse(viaEnv.stdout.trim());
    const envPayload = JSON.parse(envReply.result.content[0].text);
    assert.equal(envPayload.writeHeaderPresent, true);
    assert.equal(envPayload.writeHeaderValue, 'true');
  } finally {
    await srv.close();
  }
});

test('HTTP 401/403/500 propagate clean JSON-RPC errors without leaking bearer token', async () => {
  let step = 0;
  const statuses = [401, 403, 500];
  const srv = await startLoopbackMcpServer((req, res) => {
    const status = statuses[step++] ?? 500;
    const authHeader = req.headers.authorization ?? '';
    res.writeHead(status, { 'content-type': 'application/json' });
    res.end(
      JSON.stringify({
        error: `Rejected request for ${authHeader} with raw token ${TEST_TOKEN}`,
      }),
    );
  });

  try {
    const res = await runBridge({
      env: { OHC_BASE_URL: srv.baseUrl, OHC_BEARER_TOKEN: TEST_TOKEN },
      stdinChunks: [
        `${JSON.stringify({ jsonrpc: '2.0', id: 'e401', method: 'tools/list' })}\n`,
        `${JSON.stringify({ jsonrpc: '2.0', id: 'e403', method: 'tools/list' })}\n`,
        `${JSON.stringify({ jsonrpc: '2.0', id: 'e500', method: 'tools/list' })}\n`,
      ],
    });

    assert.equal(res.code, 0);
    assert.equal(res.stdout.includes(TEST_TOKEN), false, 'Token must never appear in stdout');
    assert.equal(res.stderr.includes(TEST_TOKEN), false, 'Token must never appear in stderr');

    const replies = res.stdout.trim().split('\n').map((line) => JSON.parse(line));
    assert.equal(replies.length, 3);
    assert.equal(replies[0].id, 'e401');
    assert.equal(replies[0].error.code, -32000);
    assert.equal(replies[0].error.data.httpStatus, 401);
    assert.match(replies[0].error.message, /\[REDACTED\]/);

    assert.equal(replies[1].id, 'e403');
    assert.equal(replies[1].error.code, -32000);
    assert.equal(replies[1].error.data.httpStatus, 403);

    assert.equal(replies[2].id, 'e500');
    assert.equal(replies[2].error.code, -32603);
    assert.equal(replies[2].error.data.httpStatus, 500);
  } finally {
    await srv.close();
  }
});

test('rejects oversized payloads (> 256 KiB) in both newline and Content-Length modes and recovers', async () => {
  const srv = await startLoopbackMcpServer(defaultMcpHandler);
  try {
    const hugeBlob = 'x'.repeat(270 * 1024);
    const hugeLine = `${JSON.stringify({ jsonrpc: '2.0', id: 99, method: 'tools/call', params: { blob: hugeBlob } })}\n`;
    const followUpLine = `${JSON.stringify({ jsonrpc: '2.0', id: 100, method: 'tools/list' })}\n`;

    const nlRes = await runBridge({
      env: { OHC_BASE_URL: srv.baseUrl, OHC_BEARER_TOKEN: TEST_TOKEN },
      stdinChunks: [hugeLine, followUpLine],
    });
    assert.equal(nlRes.code, 0);
    const nlReplies = nlRes.stdout.trim().split('\n').map((line) => JSON.parse(line));
    assert.equal(nlReplies.length, 2);
    assert.equal(nlReplies[0].id, null);
    assert.equal(nlReplies[0].error.code, -32600);
    assert.match(nlReplies[0].error.message, /262144/);
    assert.equal(nlReplies[1].id, 100);
    assert.equal(nlReplies[1].result.tools.length, 2);
    assert.equal(srv.received.length, 1, 'Oversized newline message must not be forwarded upstream');

    const hugeClFrame = encodeContentLength({ jsonrpc: '2.0', id: 101, method: 'tools/call', params: { blob: hugeBlob } });
    const okClFrame = encodeContentLength({ jsonrpc: '2.0', id: 102, method: 'tools/list' });

    const clRes = await runBridge({
      env: { OHC_BASE_URL: srv.baseUrl, OHC_BEARER_TOKEN: TEST_TOKEN },
      stdinChunks: [hugeClFrame.subarray(0, 64 * 1024), hugeClFrame.subarray(64 * 1024), okClFrame],
    });
    assert.equal(clRes.code, 0);
    const clReplies = decodeContentLengthFrames(clRes.stdoutBuf);
    assert.equal(clReplies.length, 2);
    assert.equal(clReplies[0].id, null);
    assert.equal(clReplies[0].error.code, -32600);
    assert.match(clReplies[0].error.message, /262144/);
    assert.equal(clReplies[1].id, 102);
    assert.equal(clReplies[1].result.tools.length, 2);
    assert.equal(srv.received.length, 2, 'Oversized Content-Length message must not be forwarded upstream');
  } finally {
    await srv.close();
  }
});

test('--self-check succeeds against live MCP endpoint and fails closed on HTTP 401', async () => {
  const srv = await startLoopbackMcpServer(defaultMcpHandler);
  try {
    const ok = await runBridge({
      args: ['--self-check'],
      env: { OHC_BASE_URL: srv.baseUrl, OHC_BEARER_TOKEN: TEST_TOKEN },
    });
    assert.equal(ok.code, 0, `stderr: ${ok.stderr}`);
    const summary = JSON.parse(ok.stdout.trim());
    assert.equal(summary.ok, true);
    assert.equal(summary.mode, 'read-only');
    assert.equal(summary.protocolVersion, '2025-03-26');
    assert.equal(summary.serverInfo.name, 'omni-assistant');
    assert.equal(ok.stdout.includes(TEST_TOKEN), false);
  } finally {
    await srv.close();
  }

  const authFailSrv = await startLoopbackMcpServer((req, res) => {
    const presented = req.headers.authorization ?? '';
    res.writeHead(401, { 'content-type': 'application/json' });
    res.end(JSON.stringify({ error: `Unauthorized ${presented} (${TEST_TOKEN})` }));
  });
  try {
    const failed = await runBridge({
      args: ['--self-check'],
      env: { OHC_BASE_URL: authFailSrv.baseUrl, OHC_BEARER_TOKEN: TEST_TOKEN },
    });
    assert.equal(failed.code, 1);
    assert.match(failed.stderr, /HTTP 401/);
    assert.equal(failed.stderr.includes(TEST_TOKEN), false);
  } finally {
    await authFailSrv.close();
  }
});

test('drains in-flight request cleanly on immediate stdin EOF and handles upstream timeout', async () => {
  const delayedSrv = await startLoopbackMcpServer((req, res) => {
    const delayMs = req.body?.params?.delayMs ?? 60;
    setTimeout(() => {
      if (!res.writableEnded) defaultMcpHandler(req, res);
    }, delayMs);
  });

  try {
    const eofRes = await runBridge({
      env: { OHC_BASE_URL: delayedSrv.baseUrl, OHC_BEARER_TOKEN: TEST_TOKEN },
      stdinChunks: [`${JSON.stringify({ jsonrpc: '2.0', id: 'eof-1', method: 'tools/list', params: { delayMs: 80 } })}\n`],
      closeStdin: true,
    });
    assert.equal(eofRes.code, 0);
    const eofReply = JSON.parse(eofRes.stdout.trim());
    assert.equal(eofReply.id, 'eof-1');
    assert.equal(eofReply.result.tools.length, 2);

    const timeoutRes = await runBridge({
      args: ['--timeout-ms', '150'],
      env: { OHC_BASE_URL: delayedSrv.baseUrl, OHC_BEARER_TOKEN: TEST_TOKEN },
      stdinChunks: [`${JSON.stringify({ jsonrpc: '2.0', id: 'slow-1', method: 'tools/list', params: { delayMs: 600 } })}\n`],
    });
    assert.equal(timeoutRes.code, 0);
    const timeoutReply = JSON.parse(timeoutRes.stdout.trim());
    assert.equal(timeoutReply.id, 'slow-1');
    assert.equal(timeoutReply.error.code, -32603);
    assert.equal(timeoutReply.error.data.reason, 'timeout');
  } finally {
    await delayedSrv.close();
  }
});

test('handles malformed JSON, direct executable invocation, endpoint normalization, and unreachable upstream', async () => {
  const srv = await startLoopbackMcpServer(defaultMcpHandler);
  const closedPortUrl = srv.baseUrl;
  try {
    const directRes = await new Promise((resolve, reject) => {
      const child = spawn(SCRIPT_PATH, ['--base-url', `${srv.baseUrl}/api/v1/integrations/mcp/`], {
        env: { PATH: process.env.PATH ?? '', OHC_BEARER_TOKEN: TEST_TOKEN },
        stdio: ['pipe', 'pipe', 'pipe'],
      });
      const out = [];
      const err = [];
      child.stdout.on('data', (c) => out.push(c));
      child.stderr.on('data', (c) => err.push(c));
      child.once('error', reject);
      child.once('close', (code) => resolve({ code, stdout: Buffer.concat(out).toString('utf8'), stderr: Buffer.concat(err).toString('utf8') }));
      child.stdin.write('not-valid-json\n');
      child.stdin.write(`${JSON.stringify({ jsonrpc: '2.0', id: 'after-bad', method: 'tools/list' })}\n`);
      child.stdin.end();
    });

    assert.equal(directRes.code, 0, `stderr: ${directRes.stderr}`);
    const replies = directRes.stdout.trim().split('\n').map((l) => JSON.parse(l));
    assert.equal(replies.length, 2);
    assert.equal(replies[0].id, null);
    assert.equal(replies[0].error.code, -32700);
    assert.equal(replies[1].id, 'after-bad');
    assert.equal(replies[1].result.tools.length, 2);
    assert.equal(srv.received.length, 1);
    assert.equal(srv.received[0].url, '/api/v1/integrations/mcp');
  } finally {
    await srv.close();
  }

  const netErr = await runBridge({
    env: { OHC_BASE_URL: closedPortUrl, OHC_BEARER_TOKEN: TEST_TOKEN },
    stdinChunks: [`${JSON.stringify({ jsonrpc: '2.0', id: 'net-1', method: 'tools/list' })}\n`],
  });
  assert.equal(netErr.code, 0);
  const netReply = JSON.parse(netErr.stdout.trim());
  assert.equal(netReply.id, 'net-1');
  assert.equal(netReply.error.code, -32603);
  assert.equal(netReply.error.data.reason, 'network_error');
  assert.equal(netErr.stdout.includes(TEST_TOKEN), false);
  assert.equal(netErr.stderr.includes(TEST_TOKEN), false);
});
