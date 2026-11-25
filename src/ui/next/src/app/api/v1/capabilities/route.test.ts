// SPDX-License-Identifier: Apache-2.0
import { createServer, type IncomingMessage, type Server, type ServerResponse } from "node:http";
import { afterAll, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { COMMUNITY_DEFAULT_CAPABILITIES } from "@/lib/capabilities";

let backendServer: Server;
let backendOrigin = "";
let responder: (req: IncomingMessage, res: ServerResponse) => void = (_req, res) => {
  res.writeHead(404, { "content-type": "application/json" });
  res.end(JSON.stringify({ error: "not found" }));
};

const savedEnv = {
  OMNISOLO_WEB_LOCAL_DEV: process.env.OMNISOLO_WEB_LOCAL_DEV,
  OMNISOLO_WEB_CANONICAL_ORIGIN: process.env.OMNISOLO_WEB_CANONICAL_ORIGIN,
  BACKEND_URL: process.env.BACKEND_URL,
  OMNISOLO_WEB_SESSION_KEY_ID: process.env.OMNISOLO_WEB_SESSION_KEY_ID,
  OMNISOLO_WEB_SESSION_SECRET: process.env.OMNISOLO_WEB_SESSION_SECRET,
};

beforeAll(async () => {
  backendServer = createServer((req, res) => responder(req, res));
  await new Promise<void>((resolve) => backendServer.listen(0, "127.0.0.1", () => resolve()));
  const address = backendServer.address();
  if (!address || typeof address === "string") {
    throw new Error("Failed to bind loopback backend server");
  }
  backendOrigin = `http://127.0.0.1:${address.port}`;
  process.env.OMNISOLO_WEB_LOCAL_DEV = "true";
  process.env.OMNISOLO_WEB_CANONICAL_ORIGIN = "http://127.0.0.1:3000";
  process.env.BACKEND_URL = backendOrigin;
  process.env.OMNISOLO_WEB_SESSION_KEY_ID = "test-key";
  process.env.OMNISOLO_WEB_SESSION_SECRET = Buffer.alloc(32, 7).toString("base64url");
});

afterAll(async () => {
  await new Promise<void>((resolve) => backendServer.close(() => resolve()));
  for (const [key, value] of Object.entries(savedEnv)) {
    if (value === undefined) delete process.env[key];
    else process.env[key] = value;
  }
});

beforeEach(() => {
  // Restore real Node fetch to exercise the local loopback HTTP server.
  vi.unstubAllGlobals();
  responder = (_req, res) => {
    res.writeHead(404, { "content-type": "application/json" });
    res.end(JSON.stringify({ error: "not found" }));
  };
});

describe("GET /api/v1/capabilities", () => {
  it("returns fail-closed community defaults when backend endpoint is absent (404)", async () => {
    const { GET } = await import("./route");
    const response = await GET();
    expect(response.status).toBe(200);
    expect(response.headers.get("cache-control")).toBe("private, no-store");
    await expect(response.json()).resolves.toEqual(COMMUNITY_DEFAULT_CAPABILITIES);
  });

  it("forwards validated cloud capabilities when advertised by the backend", async () => {
    responder = (_req, res) => {
      res.writeHead(200, { "content-type": "application/json" });
      res.end(
        JSON.stringify({
          edition: "cloud",
          features: {
            cloud_sync: true,
            oidc: true,
            saas_billing: true,
            multitenant: true,
          },
        }),
      );
    };
    const { GET } = await import("./route");
    const response = await GET();
    expect(response.status).toBe(200);
    await expect(response.json()).resolves.toEqual({
      edition: "cloud",
      features: {
        cloud_sync: true,
        oidc: true,
        saas_billing: true,
        multitenant: true,
      },
    });
  });

  it("strips cloud flags when backend advertises community edition", async () => {
    responder = (_req, res) => {
      res.writeHead(200, { "content-type": "application/json" });
      res.end(
        JSON.stringify({
          edition: "community",
          features: {
            cloud_sync: true,
            oidc: true,
            saas_billing: true,
            multitenant: true,
          },
        }),
      );
    };
    const { GET } = await import("./route");
    const response = await GET();
    expect(response.status).toBe(200);
    await expect(response.json()).resolves.toEqual(COMMUNITY_DEFAULT_CAPABILITIES);
  });
});
