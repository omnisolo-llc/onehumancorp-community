#!/usr/bin/env node
// Zero-dependency Node 22+ stdio-to-HTTP MCP JSON-RPC 2.0 bridge for Omni Assistant.
// Bridges newline-delimited or Content-Length stdio frames to POST /api/v1/integrations/mcp.

const MCP_PATH = '/api/v1/integrations/mcp';
const MCP_PROTOCOL = '2025-03-26';
const [MAX_PAYLOAD, MAX_HEADER, MAX_RESPONSE] = [256 * 1024, 8 * 1024, 1024 * 1024];
const [DEFAULT_TIMEOUT, MIN_TIMEOUT, MAX_TIMEOUT] = [30_000, 100, 120_000];

const HELP_TEXT =
  'Omni Assistant MCP stdio-to-HTTP bridge (Node 22+)\n' +
  'Usage: ohc-mcp-stdio.mjs [--base-url <url>] [--allow-write] [--timeout-ms <ms>] [--self-check] [--help]\n' +
  'Env: OHC_BASE_URL (http/https), OHC_BEARER_TOKEN (required), OHC_MCP_ALLOW_WRITE (false), OHC_MCP_TIMEOUT_MS (30000)\n';

function redactSecrets(text, token) {
  let out = String(text ?? '');
  if (token) {
    out = out.split(token).join('[REDACTED]');
    const enc = encodeURIComponent(token);
    if (enc && enc !== token) out = out.split(enc).join('[REDACTED]');
  }
  return out.replace(/Bearer\s+\S+/gi, 'Bearer [REDACTED]');
}

function normalizeEndpoint(rawUrl) {
  if (!rawUrl || !rawUrl.trim()) throw new Error('Missing OHC_BASE_URL (set OHC_BASE_URL or pass --base-url).');
  let u;
  try { u = new URL(rawUrl.trim()); } catch { throw new Error('Invalid OHC_BASE_URL: must be a valid http or https URL.'); }
  if (u.protocol !== 'http:' && u.protocol !== 'https:') throw new Error('Invalid OHC_BASE_URL: only http and https protocols are supported.');
  if (u.username || u.password) throw new Error('Invalid OHC_BASE_URL: embedded URL credentials are not allowed.');
  if (u.search || u.hash) throw new Error('Invalid OHC_BASE_URL: query strings and fragments are not allowed.');
  const clean = u.pathname.replace(/\/+$/, '');
  u.pathname = clean.endsWith(MCP_PATH) ? clean : `${clean}${MCP_PATH}`;
  return u.toString();
}

function parseConfig(argv, env) {
  let baseUrlArg, timeoutArg, allowWriteFlag = false, selfCheck = false;
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === '--help' || arg === '-h') return { help: true };
    if (arg === '--self-check') selfCheck = true;
    else if (arg === '--allow-write') allowWriteFlag = true;
    else if (arg === '--base-url' || arg.startsWith('--base-url=')) {
      baseUrlArg = arg === '--base-url' ? argv[++i] : arg.slice(11);
      if (!baseUrlArg) throw new Error('Missing value for --base-url.');
    } else if (arg === '--timeout-ms' || arg.startsWith('--timeout-ms=')) {
      timeoutArg = arg === '--timeout-ms' ? argv[++i] : arg.slice(13);
      if (!timeoutArg) throw new Error('Missing value for --timeout-ms.');
    } else throw new Error(`Unknown argument: ${arg}`);
  }

  const endpoint = normalizeEndpoint(baseUrlArg ?? env.OHC_BASE_URL);
  const bearerToken = env.OHC_BEARER_TOKEN?.trim() ?? '';
  if (!bearerToken) throw new Error('Missing OHC_BEARER_TOKEN environment variable.');
  if (bearerToken.includes('\r') || bearerToken.includes('\n') || bearerToken.includes('\0')) {
    throw new Error('Invalid OHC_BEARER_TOKEN: control characters are not allowed.');
  }

  const rawWrite = env.OHC_MCP_ALLOW_WRITE?.trim().toLowerCase() ?? '';
  if (rawWrite && rawWrite !== 'true' && rawWrite !== 'false') {
    throw new Error('Invalid OHC_MCP_ALLOW_WRITE: expected "true" or "false".');
  }

  const rawTimeout = timeoutArg ?? env.OHC_MCP_TIMEOUT_MS;
  let timeoutMs = DEFAULT_TIMEOUT;
  if (rawTimeout !== undefined && String(rawTimeout).trim() !== '') {
    const t = String(rawTimeout).trim();
    if (!/^\d+$/.test(t)) throw new Error('Invalid OHC_MCP_TIMEOUT_MS: must be an integer in milliseconds.');
    timeoutMs = Number(t);
    if (!Number.isSafeInteger(timeoutMs) || timeoutMs < MIN_TIMEOUT || timeoutMs > MAX_TIMEOUT) {
      throw new Error(`Invalid OHC_MCP_TIMEOUT_MS: must be between ${MIN_TIMEOUT} and ${MAX_TIMEOUT}.`);
    }
  }
  return { help: false, selfCheck, endpoint, bearerToken, allowWrite: allowWriteFlag || rawWrite === 'true', timeoutMs };
}

function writeFrame(framing, payload) {
  const json = JSON.stringify(payload);
  process.stdout.write(framing === 'content-length' ? `Content-Length: ${Buffer.byteLength(json, 'utf8')}\r\n\r\n${json}` : `${json}\n`);
}

const rpcError = (id, code, message, data) => ({
  jsonrpc: '2.0', id, error: data === undefined ? { code, message } : { code, message, data },
});

async function readBoundedText(response, maxBytes) {
  const declared = Number(response.headers.get('content-length'));
  if (Number.isFinite(declared) && declared > maxBytes) {
    await response.body?.cancel();
    throw new Error(`Upstream response exceeds ${maxBytes} byte limit`);
  }
  if (!response.body) return '';
  const chunks = [];
  let total = 0;
  for await (const chunk of response.body) {
    total += chunk.byteLength;
    if (total > maxBytes) throw new Error(`Upstream response exceeds ${maxBytes} byte limit`);
    chunks.push(Buffer.from(chunk));
  }
  return Buffer.concat(chunks, total).toString('utf8');
}

function tryParseJson(text) {
  if (!text.trim()) return null;
  try { return JSON.parse(text); } catch { return null; }
}

async function forwardJsonRpc(config, message, hasId) {
  const headers = {
    'content-type': 'application/json', accept: 'application/json',
    authorization: `Bearer ${config.bearerToken}`, 'user-agent': 'ohc-mcp-stdio/1.0',
    ...(config.allowWrite ? { 'x-ohc-mcp-write': 'true' } : {}),
  };
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), config.timeoutMs);
  try {
    const response = await fetch(config.endpoint, {
      method: 'POST', headers, body: JSON.stringify(message), signal: controller.signal, redirect: 'error',
    });
    if (!hasId) { await response.body?.cancel(); return null; }

    const bodyText = await readBoundedText(response, MAX_RESPONSE);
    let parsed = tryParseJson(bodyText);

    if (response.ok) {
      if (parsed && typeof parsed === 'object' && !Array.isArray(parsed) && parsed.jsonrpc === '2.0' && ('result' in parsed || 'error' in parsed)) {
        if (typeof parsed.error?.message === 'string') {
          parsed = { ...parsed, error: { ...parsed.error, message: redactSecrets(parsed.error.message, config.bearerToken) } };
        }
        return { ...parsed, id: message.id };
      }
      return rpcError(message.id, -32603, 'Upstream MCP server returned an invalid JSON-RPC response');
    }

    const fallbackCode = response.status >= 400 && response.status < 500 ? -32000 : -32603;
    if (parsed?.jsonrpc === '2.0' && typeof parsed.error?.message === 'string') {
      const code = Number.isInteger(parsed.error.code) ? parsed.error.code : fallbackCode;
      return rpcError(message.id, code, redactSecrets(parsed.error.message, config.bearerToken), { httpStatus: response.status });
    }
    const rawDetail = (typeof parsed?.error === 'string' && parsed.error) ||
      (typeof parsed?.error?.message === 'string' && parsed.error.message) ||
      (typeof parsed?.message === 'string' && parsed.message) || bodyText.trim();
    const safeDetail = redactSecrets(rawDetail, config.bearerToken).slice(0, 200);
    return rpcError(
      message.id, fallbackCode,
      safeDetail ? `HTTP ${response.status}: ${safeDetail}` : `HTTP ${response.status} from Omni Assistant MCP endpoint`,
      { httpStatus: response.status },
    );
  } catch (err) {
    if (!hasId) return null;
    if (err?.name === 'AbortError') return rpcError(message.id, -32603, `Upstream MCP request timed out after ${config.timeoutMs}ms`, { reason: 'timeout' });
    const detail = redactSecrets(err?.message || 'network error', config.bearerToken).slice(0, 200);
    return rpcError(message.id, -32603, `Upstream MCP request failed: ${detail}`, { reason: 'network_error' });
  } finally {
    clearTimeout(timer);
  }
}

function createFrameParser(onFrame, onError) {
  let buf = Buffer.alloc(0), discardRemaining = 0, discardUntilNl = false;

  function drain() {
    while (buf.length > 0) {
      let lead = 0;
      while (lead < buf.length && (buf[lead] === 0x0d || buf[lead] === 0x0a)) lead += 1;
      if (lead > 0) buf = buf.subarray(lead);
      if (buf.length === 0) break;

      const head = buf.subarray(0, Math.min(buf.length, 15)).toString('ascii').toLowerCase();
      const isLsp = head.startsWith('content-length:') || head.startsWith('content-type:');
      if (!isLsp && buf.length < 15 && buf.indexOf(0x0a) === -1 && ('content-length:'.startsWith(head) || 'content-type:'.startsWith(head))) break;

      if (isLsp) {
        const crlf = buf.indexOf('\r\n\r\n'), lf = buf.indexOf('\n\n');
        const useCrlf = crlf !== -1 && (lf === -1 || crlf < lf);
        const headerEnd = useCrlf ? crlf : lf;
        if (headerEnd === -1) {
          if (buf.length > MAX_HEADER) { buf = Buffer.alloc(0); onError('content-length', -32600, `Header block exceeds ${MAX_HEADER} byte limit`); }
          break;
        }
        const bodyStart = headerEnd + (useCrlf ? 4 : 2);
        const match = buf.subarray(0, headerEnd).toString('ascii').match(/(?:^|\r?\n)content-length\s*:\s*(\d+)\s*(?:\r?\n|$)/i);
        if (!match) { buf = buf.subarray(bodyStart); onError('content-length', -32700, 'Invalid or missing Content-Length header'); continue; }
        const len = Number(match[1]);
        if (!Number.isSafeInteger(len) || len > MAX_PAYLOAD) {
          const avail = buf.length - bodyStart;
          if (avail >= len) buf = buf.subarray(bodyStart + len);
          else { discardRemaining = len - avail; buf = Buffer.alloc(0); }
          onError('content-length', -32600, `Payload exceeds ${MAX_PAYLOAD} byte limit`);
          continue;
        }
        if (buf.length < bodyStart + len) break;
        onFrame('content-length', buf.subarray(bodyStart, bodyStart + len).toString('utf8'));
        buf = buf.subarray(bodyStart + len);
      } else {
        const nl = buf.indexOf(0x0a);
        if (nl === -1) {
          if (buf.length > MAX_PAYLOAD) { discardUntilNl = true; buf = Buffer.alloc(0); onError('newline', -32600, `Payload exceeds ${MAX_PAYLOAD} byte limit`); }
          break;
        }
        const lineBuf = buf.subarray(0, nl > 0 && buf[nl - 1] === 0x0d ? nl - 1 : nl);
        buf = buf.subarray(nl + 1);
        if (lineBuf.length === 0) continue;
        if (lineBuf.length > MAX_PAYLOAD) onError('newline', -32600, `Payload exceeds ${MAX_PAYLOAD} byte limit`);
        else onFrame('newline', lineBuf.toString('utf8'));
      }
    }
  }

  return {
    push(chunk) {
      let data = chunk;
      if (discardRemaining > 0) {
        if (data.length <= discardRemaining) { discardRemaining -= data.length; return; }
        data = data.subarray(discardRemaining);
        discardRemaining = 0;
      }
      if (discardUntilNl) {
        const nl = data.indexOf(0x0a);
        if (nl === -1) return;
        data = data.subarray(nl + 1);
        discardUntilNl = false;
      }
      if (data.length === 0) return;
      buf = buf.length === 0 ? data : Buffer.concat([buf, data]);
      drain();
    },
    flush() {
      if (discardRemaining > 0 || discardUntilNl || buf.length === 0) return;
      const tail = buf.toString('utf8').trim();
      buf = Buffer.alloc(0);
      if (tail) onFrame('newline', tail);
    },
  };
}

async function runSelfCheck(config) {
  const probe = {
    jsonrpc: '2.0', id: 'self-check', method: 'initialize',
    params: { protocolVersion: MCP_PROTOCOL, capabilities: {}, clientInfo: { name: 'ohc-mcp-stdio', version: '1.0.0' } },
  };
  const reply = await forwardJsonRpc(config, probe, true);
  if (reply && !reply.error && reply.result !== undefined) {
    process.stdout.write(`${JSON.stringify({
      ok: true, endpoint: config.endpoint, mode: config.allowWrite ? 'write-enabled' : 'read-only',
      protocolVersion: reply.result?.protocolVersion ?? null, serverInfo: reply.result?.serverInfo ?? null,
    })}\n`);
    return 0;
  }
  process.stderr.write(`ohc-mcp-stdio self-check failed: ${redactSecrets(reply?.error?.message || 'MCP self-check failed', config.bearerToken)}\n`);
  return 1;
}

async function main() {
  let config;
  try { config = parseConfig(process.argv.slice(2), process.env); } catch (err) {
    process.stderr.write(`ohc-mcp-stdio error: ${redactSecrets(err?.message || 'Configuration error', process.env.OHC_BEARER_TOKEN)}\n`);
    process.exit(1);
  }
  if (config.help) { process.stdout.write(HELP_TEXT); process.exit(0); }
  if (config.selfCheck) process.exit(await runSelfCheck(config));

  process.stdout.on('error', (err) => { if (err?.code === 'EPIPE') process.exit(0); });
  let queue = Promise.resolve();
  const enqueue = (task) => {
    queue = queue.then(task).catch((err) => {
      process.stderr.write(`ohc-mcp-stdio internal error: ${redactSecrets(err?.message, config.bearerToken)}\n`);
    });
  };

  const parser = createFrameParser(
    (framing, rawText) => enqueue(async () => {
      const parsed = tryParseJson(rawText);
      if (parsed === null && rawText.trim() !== 'null') { writeFrame(framing, rpcError(null, -32700, 'Parse error: invalid JSON')); return; }
      if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) { writeFrame(framing, rpcError(null, -32600, 'Invalid Request: expected JSON-RPC object')); return; }
      const hasId = Object.hasOwn(parsed, 'id') && parsed.id !== undefined;
      const reply = await forwardJsonRpc(config, parsed, hasId);
      if (hasId && reply) writeFrame(framing, reply);
    }),
    (framing, code, message) => enqueue(() => writeFrame(framing, rpcError(null, code, message))),
  );

  process.stdin.on('data', (chunk) => parser.push(Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk)));
  process.stdin.on('end', () => { parser.flush(); queue.finally(() => process.exit(0)); });
  process.on('SIGINT', () => process.exit(0));
  process.on('SIGTERM', () => process.exit(0));
}

main();
