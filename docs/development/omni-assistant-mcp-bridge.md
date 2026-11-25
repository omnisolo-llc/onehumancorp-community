# Omni Assistant stdio-to-HTTP MCP Bridge

`scripts/ohc-mcp-stdio.mjs` is a zero-dependency Node.js 22+ bridge that connects stdio MCP clients—such as LeapTerm's `leap-agent` (`ProviderConfig`) or any standard stdio MCP host—to OneHumanCorp's Omni Assistant inbound MCP JSON-RPC 2.0 (`2025-03-26`) endpoint (`POST /api/v1/integrations/mcp`).

This enables immediate Phase 1 integration with zero changes required in LeapTerm or other stdio MCP clients.

## Security & Authority Model

- **Read-only by default:** The bridge omits the `x-ohc-mcp-write` header unless write-proposal opt-in is explicitly enabled via `--allow-write` or `OHC_MCP_ALLOW_WRITE=true`.
- **Server-side enforcement remains authoritative:** Enabling `--allow-write` only allows forwarding the opt-in header. OneHumanCorp's server still enforces tenant isolation, owner/admin Bearer JWT checks, and `awaiting_approval` draft-only creation for write-proposal tools.
- **Fail-closed startup:** Missing or malformed `OHC_BASE_URL` (must be `http:` or `https:` with no embedded URL credentials, query strings, or fragments) or missing `OHC_BEARER_TOKEN` causes the bridge to exit non-zero immediately.
- **Secret redaction:** `OHC_BEARER_TOKEN` is accepted strictly via environment variable (never CLI arguments) and is redacted from stderr logs and JSON-RPC error messages even if an upstream error body echoes authorization headers.
- **Bounded framing & redirects disabled:** Incoming messages are bounded at `256 KiB` (`262144` bytes), upstream responses are bounded at `1 MiB`, HTTP timeouts default to `30000 ms` (configurable within `100..120000 ms`), and HTTP redirects are rejected (`redirect: 'error'`).

## Configuration

| Setting | CLI Flag | Required | Default | Description |
| --- | --- | --- | --- | --- |
| `OHC_BASE_URL` | `--base-url <url>` | Yes | — | OHC server origin or full `/api/v1/integrations/mcp` URL (`http://` or `https://`). |
| `OHC_BEARER_TOKEN` | *(env only)* | Yes | — | Bearer JWT/token sent in `Authorization: Bearer <token>`. |
| `OHC_MCP_ALLOW_WRITE` | `--allow-write` | No | `false` | Set to `true` (or pass `--allow-write`) to send `x-ohc-mcp-write: true`. |
| `OHC_MCP_TIMEOUT_MS` | `--timeout-ms <ms>` | No | `30000` | Upstream HTTP request timeout (`100` to `120000` ms). |
| — | `--self-check` | No | — | Validates config and sends a live MCP `initialize` handshake, exiting `0` on success. |
| — | `--help`, `-h` | No | — | Prints CLI usage and exits `0`. |

## Protocol & Framing Behavior

- **Auto-detected framing:** Accepts both newline-delimited JSON-RPC 2.0 frames and LSP-style `Content-Length: <bytes>\r\n\r\n<json>` frames, automatically matching the input framing on stdout.
- **Notifications vs. requests:** JSON-RPC notifications (messages without an `id` field) are forwarded upstream without emitting a stdout reply; requests preserve their exact `id` type (`number`, `string`, `0`, or `null`).
- **Error propagation:** HTTP `4xx` responses map to JSON-RPC `-32000`, HTTP `5xx` / timeout / network failures map to `-32603`, invalid JSON maps to `-32700`, and oversized payloads (`> 256 KiB`) map to `-32600` without crashing the bridge.
- **Clean EOF shutdown:** When stdin closes, any in-flight queued requests finish flushing to stdout before exiting `0`.

## LeapTerm `leap-agent` `ProviderConfig` Setup

Configure an owner-approved stdio provider entry in your LeapTerm `leap-agent` configuration pointing to `scripts/ohc-mcp-stdio.mjs`. Keep read-only mode enabled unless you explicitly want Omni Assistant write-proposal tools (`awaiting_approval` drafts).

Example JSON `ProviderConfig` entry (read-only default):

```json
{
  "id": "omni-assistant",
  "executable": "node",
  "argv": [
    "/path/to/onehumancorp/scripts/ohc-mcp-stdio.mjs"
  ],
  "environment": {
    "OHC_BASE_URL": "https://ohc.example.com",
    "OHC_BEARER_TOKEN": "${OHC_BEARER_TOKEN}"
  },
  "projects": ["default"],
  "tools": {
    "mcp_tool": "ohc_list_spaces"
  },
  "trusted_host_access": false
}
```

To opt in to write-proposal tools (requires an owner/admin Bearer token on the OHC server):

- Add `"--allow-write"` to `argv` (or set `"OHC_MCP_ALLOW_WRITE": "true"` in `environment`).
- Never commit raw `OHC_BEARER_TOKEN` values to configuration files; inject the token from your host secret store or process environment.

## Verification

1. **Syntax check:**

   ```bash
   node --check scripts/ohc-mcp-stdio.mjs
   ```

2. **Real child-process & loopback HTTP test suite:**

   ```bash
   node --test scripts/test_ohc_mcp_stdio.mjs
   ```

3. **Live connectivity self-check against a running OHC instance:**

   ```bash
   OHC_BASE_URL="https://ohc.example.com" \
   OHC_BEARER_TOKEN="$OHC_BEARER_TOKEN" \
   node scripts/ohc-mcp-stdio.mjs --self-check
   ```

## Capability & Verification Scope

- **Test-verified locally:** Framing (newline and `Content-Length`), read-only vs. `--allow-write` header gating, fail-closed validation, secret redaction on 401/403/500 responses, `> 256 KiB` rejection, timeout handling, `--self-check`, and EOF draining are verified against a local `node:http` loopback server using the real `scripts/ohc-mcp-stdio.mjs` process.
- **Live deployment verification:** Connecting to a production or staging OneHumanCorp server requires a valid deployment URL and tenant Bearer token via `--self-check`; local loopback tests do not certify live remote server credentials.
