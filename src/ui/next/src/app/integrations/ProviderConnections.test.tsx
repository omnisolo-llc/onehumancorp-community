import React from "react";
import http, { type IncomingMessage, type Server } from "node:http";
import type { AddressInfo } from "node:net";
import { fetch as undiciFetch } from "undici";
import {
  cleanup,
  fireEvent,
  render,
  screen,
  waitFor,
  within,
} from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import ProviderConnections from "./ProviderConnections";

interface IntegrationEntry {
  id: string;
  status: string;
  usable: boolean;
}

interface ProviderServerState {
  integrations: IntegrationEntry[];
  connectHandler?: (
    providerId: string,
    body: Record<string, unknown>,
  ) => { status: number; body: unknown };
  verifyHandler?: (providerId: string) => { status: number; body: unknown };
  deleteHandler?: (providerId: string) => { status: number; body: unknown };
  requests: Array<{ method: string; path: string; body: unknown }>;
}

async function readJson(req: IncomingMessage): Promise<unknown> {
  const chunks: Buffer[] = [];
  for await (const chunk of req) {
    chunks.push(Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk));
  }
  const text = Buffer.concat(chunks).toString("utf8");
  return text.trim() ? JSON.parse(text) : null;
}

async function startProviderLoopback(state: ProviderServerState): Promise<{
  origin: string;
  close: () => Promise<void>;
}> {
  const server: Server = http.createServer(async (req, res) => {
    const pathname = new URL(req.url || "/", "http://127.0.0.1").pathname;
    const method = req.method || "GET";
    const body =
      method === "POST" || method === "DELETE" ? await readJson(req) : null;
    state.requests.push({ method, path: pathname, body });

    const reply = (status: number, data: unknown) => {
      res.writeHead(status, { "Content-Type": "application/json" });
      res.end(JSON.stringify(data));
    };

    if (method === "GET" && pathname === "/api/v1/integrations") {
      reply(200, { success: true, integrations: state.integrations });
      return;
    }

    const connectMatch = pathname.match(/^\/api\/v1\/integrations\/([^/]+)\/connect$/);
    if (method === "POST" && connectMatch) {
      const id = connectMatch[1];
      if (state.connectHandler) {
        const result = state.connectHandler(
          id,
          (body as Record<string, unknown>) ?? {},
        );
        reply(result.status, result.body);
        return;
      }
      state.integrations = [
        ...state.integrations.filter((item) => item.id !== id),
        { id, status: "verified", usable: true },
      ];
      reply(200, { success: true, status: "verified", usable: true });
      return;
    }

    const verifyMatch = pathname.match(/^\/api\/v1\/integrations\/([^/]+)\/verify$/);
    if (method === "POST" && verifyMatch) {
      const id = verifyMatch[1];
      if (state.verifyHandler) {
        const result = state.verifyHandler(id);
        reply(result.status, result.body);
        return;
      }
      state.integrations = [
        ...state.integrations.filter((item) => item.id !== id),
        { id, status: "verified", usable: true },
      ];
      reply(200, { success: true, status: "verified", usable: true });
      return;
    }

    const deleteMatch = pathname.match(/^\/api\/v1\/integrations\/([^/]+)$/);
    if (method === "DELETE" && deleteMatch) {
      const id = deleteMatch[1];
      if (state.deleteHandler) {
        const result = state.deleteHandler(id);
        reply(result.status, result.body);
        return;
      }
      state.integrations = [
        ...state.integrations.filter((item) => item.id !== id),
        { id, status: "revoked", usable: false },
      ];
      reply(200, { success: true, status: "revoked", usable: false });
      return;
    }

    reply(404, { error: "not found" });
  });

  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const origin = `http://127.0.0.1:${(server.address() as AddressInfo).port}`;

  return {
    origin,
    close: () => new Promise<void>((resolve) => server.close(() => resolve())),
  };
}

describe("verified provider connections", () => {
  const originalFetch = globalThis.fetch;
  let activeClose: (() => Promise<void>) | null = null;

  afterEach(async () => {
    cleanup();
    globalThis.fetch = originalFetch;
    if (activeClose) {
      await activeClose();
      activeClose = null;
    }
  });

  async function mountWithServer(state: ProviderServerState) {
    const loopback = await startProviderLoopback(state);
    activeClose = loopback.close;
    globalThis.fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
      const rawUrl =
        typeof input === "string"
          ? input
          : input instanceof URL
            ? input.toString()
            : input.url;
      const target = rawUrl.startsWith("http")
        ? rawUrl
        : `${loopback.origin}${rawUrl}`;
      return undiciFetch(
        target,
        init as Parameters<typeof undiciFetch>[1],
      ) as unknown as Response;
    }) as typeof fetch;

    render(<ProviderConnections />);
  }

  it("renders all 5 vault providers and does not turn an unverified status into a connected provider", async () => {
    const state: ProviderServerState = {
      integrations: [
        { id: "openai_api", status: "pending", usable: true },
        { id: "google_workspace", status: "verification_required", usable: false },
      ],
      requests: [],
    };
    await mountWithServer(state);

    await waitFor(() =>
      expect(
        state.requests.some((r) => r.path === "/api/v1/integrations"),
      ).toBe(true),
    );

    expect(
      within(screen.getByRole("article", { name: "OpenAI API" })).getByText(
        "Not connected",
      ),
    ).toBeVisible();
    expect(
      screen.getByRole("article", { name: "Anthropic API" }),
    ).toBeVisible();
    expect(
      screen.getByRole("article", { name: "Stripe payments" }),
    ).toBeVisible();
    expect(
      screen.getByRole("article", { name: "Resend email" }),
    ).toBeVisible();
    expect(
      within(
        screen.getByRole("article", { name: "Google Workspace" }),
      ).getByText("Verification required"),
    ).toBeVisible();
    expect(screen.getByText("0/5 verified")).toBeVisible();
    expect(screen.queryByText("Verified connection")).toBeNull();
  });

  it("sends only the entered key and requires verified usable confirmation", async () => {
    const state: ProviderServerState = {
      integrations: [],
      requests: [],
    };
    const storageWrites: Array<[string, string]> = [];
    const originalSetItem = Storage.prototype.setItem;
    Storage.prototype.setItem = function recordSetItem(key: string, value: string) {
      storageWrites.push([key, value]);
      return originalSetItem.call(this, key, value);
    };
    try {
      await mountWithServer(state);

      fireEvent.click(screen.getByRole("button", { name: "Configure OpenAI API" }));
      const input = screen.getByLabelText("OpenAI API API key");
      expect(input).toHaveAttribute("type", "password");
      expect(
        screen.getByRole("button", { name: "Verify and save key" }),
      ).toBeDisabled();

      fireEvent.change(input, {
        target: { value: "  sk-test-only-not-a-real-key  " },
      });
      fireEvent.click(screen.getByRole("button", { name: "Verify and save key" }));

      await waitFor(() => {
        const post = state.requests.find(
          (r) =>
            r.method === "POST" &&
            r.path === "/api/v1/integrations/openai_api/connect",
        );
        expect(post?.body).toEqual({ api_token: "sk-test-only-not-a-real-key" });
      });

      expect(await screen.findByText("Verified connection")).toBeVisible();
      expect(screen.getByText("1/5 verified")).toBeVisible();
      expect(screen.queryByLabelText("OpenAI API API key")).toBeNull();
      expect(storageWrites).toHaveLength(0);
    } finally {
      Storage.prototype.setItem = originalSetItem;
    }
  });

  it("verifies Google Workspace OAuth bearer tokens and Resend API keys", async () => {
    const state: ProviderServerState = {
      integrations: [],
      requests: [],
    };
    await mountWithServer(state);

    fireEvent.click(
      screen.getByRole("button", { name: "Configure Google Workspace" }),
    );
    const gwInput = screen.getByLabelText("Google Workspace OAuth bearer token");
    fireEvent.change(gwInput, {
      target: { value: "oauth-bearer-token-sample" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Verify and save key" }));

    await waitFor(() => {
      const post = state.requests.find(
        (r) =>
          r.method === "POST" &&
          r.path === "/api/v1/integrations/google_workspace/connect",
      );
      expect(post?.body).toEqual({ api_token: "oauth-bearer-token-sample" });
    });
    expect(
      await within(
        screen.getByRole("article", { name: "Google Workspace" }),
      ).findByText("Verified connection"),
    ).toBeVisible();

    fireEvent.click(
      screen.getByRole("button", { name: "Configure Resend email" }),
    );
    fireEvent.change(screen.getByLabelText("Resend email API key"), {
      target: { value: "re_example_key_value" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Verify and save key" }));

    await waitFor(() => {
      const post = state.requests.find(
        (r) =>
          r.method === "POST" &&
          r.path === "/api/v1/integrations/resend/connect",
      );
      expect(post?.body).toEqual({ api_token: "re_example_key_value" });
    });
    expect(await screen.findByText("2/5 verified")).toBeVisible();
  });

  it("clears the key on failed verification without reflecting an upstream secret", async () => {
    const state: ProviderServerState = {
      integrations: [],
      requests: [],
      connectHandler: () => ({
        status: 200,
        body: {
          success: true,
          status: "pending",
          usable: false,
          error: "sk-secret-echo",
        },
      }),
    };
    await mountWithServer(state);

    fireEvent.click(
      screen.getByRole("button", { name: "Configure Stripe payments" }),
    );
    const input = screen.getByLabelText("Stripe payments API key");
    fireEvent.change(input, { target: { value: "sk-secret-echo" } });
    fireEvent.click(screen.getByRole("button", { name: "Verify and save key" }));

    expect(
      await screen.findByText(/The key could not be verified/),
    ).toBeVisible();
    expect(input).toHaveValue("");
    expect(screen.queryByText("sk-secret-echo")).toBeNull();
    expect(screen.queryByText("Verified connection")).toBeNull();
  });

  it("requires an explicit disconnect confirmation and does not hide a revocation failure", async () => {
    const state: ProviderServerState = {
      integrations: [{ id: "stripe", status: "verified", usable: true }],
      requests: [],
      deleteHandler: () => ({ status: 503, body: { success: false } }),
    };
    await mountWithServer(state);

    fireEvent.click(
      await screen.findByRole("button", { name: "Disconnect Stripe payments" }),
    );
    expect(
      state.requests.some(
        (r) =>
          r.method === "DELETE" && r.path === "/api/v1/integrations/stripe",
      ),
    ).toBe(false);

    fireEvent.click(screen.getByRole("button", { name: "Confirm disconnect" }));
    expect(await screen.findByText(/could not be revoked/)).toBeVisible();
    expect(screen.getByText("Verified connection")).toBeVisible();
  });

  it("rechecks expired verification through the owner-authenticated endpoint", async () => {
    const state: ProviderServerState = {
      integrations: [
        { id: "stripe", status: "reauthentication_required", usable: false },
      ],
      requests: [],
    };
    await mountWithServer(state);

    fireEvent.click(
      await screen.findByRole("button", { name: "Recheck Stripe payments" }),
    );

    await waitFor(() =>
      expect(
        state.requests.some(
          (r) =>
            r.method === "POST" &&
            r.path === "/api/v1/integrations/stripe/verify",
        ),
      ).toBe(true),
    );
    expect(await screen.findByText("Verified connection")).toBeVisible();
  });
});
