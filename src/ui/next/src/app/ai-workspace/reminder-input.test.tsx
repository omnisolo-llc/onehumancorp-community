import React from "react";
import http, { type Server } from "node:http";
import type { AddressInfo } from "node:net";
import { fetch as undiciFetch } from "undici";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import AIWorkspacePage from "./page";
import { TooltipProvider } from "../../components/TooltipRegistry";
import {
  formatMicroUsd,
  parseTaskCenterSummaryResponse,
  validateCreateTaskPayload,
  type TaskCenterSummaryResponse,
} from "@/lib/taskCenterContract";

async function startGuardrailServer(options: {
  llmConfigured: boolean;
  postStatus?: number;
}) {
  const requests: Array<{ method: string; path: string }> = [];
  const summary: TaskCenterSummaryResponse = {
    spaces: [],
    tasks: [],
    standing_policies: [],
    runtime: {
      llm_configured: options.llmConfigured,
      provider: options.llmConfigured ? "openai_api" : null,
      model: options.llmConfigured ? "gpt-4o-mini" : null,
      payer_mode: options.llmConfigured ? "byok_api" : "unconfigured",
      reason: options.llmConfigured
        ? null
        : "Provider API key has not been verified.",
    },
    budget: {
      configured: true,
      limit_micros: 1_000_000,
      spent_micros: 0,
      reserved_micros: 0,
      currency: "USD",
      unit: "micro_usd",
      customer_direct_inference_is_not_rebilled: true,
    },
  };

  const server: Server = http.createServer((req, res) => {
    const pathname = new URL(req.url || "/", "http://127.0.0.1").pathname;
    const method = req.method || "GET";
    requests.push({ method, path: pathname });

    if (pathname === "/api/v1/task-center/summary") {
      res.writeHead(200, { "Content-Type": "application/json" });
      res.end(JSON.stringify(summary));
      return;
    }

    if (pathname === "/api/v1/integrations/catalog") {
      res.writeHead(200, { "Content-Type": "application/json" });
      res.end(
        JSON.stringify({
          success: true,
          connectors: [],
          mcp_server: {
            endpoint: "/api/v1/integrations/mcp",
            protocol_version: "2025-03-26",
            default_mode: "read_only",
            write_opt_in_header: "x-ohc-mcp-write",
            tool_count: 0,
          },
        }),
      );
      return;
    }

    if (method === "POST" && pathname === "/api/v1/task-center/tasks") {
      res.writeHead(options.postStatus ?? 503, {
        "Content-Type": "application/json",
      });
      res.end(
        JSON.stringify({
          error: "AI runtime is unconfigured; request failed closed.",
          provenance: "unconfigured",
        }),
      );
      return;
    }

    res.writeHead(404, { "Content-Type": "application/json" });
    res.end(JSON.stringify({ error: "not found" }));
  });

  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const origin = `http://127.0.0.1:${(server.address() as AddressInfo).port}`;

  return {
    origin,
    requests,
    close: () => new Promise<void>((resolve) => server.close(() => resolve())),
  };
}

describe("Task Center input guardrails and contract validation", () => {
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

  function bindLoopbackFetch(origin: string) {
    globalThis.fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
      const rawUrl =
        typeof input === "string"
          ? input
          : input instanceof URL
            ? input.toString()
            : input.url;
      const target = rawUrl.startsWith("http") ? rawUrl : `${origin}${rawUrl}`;
      return undiciFetch(
        target,
        init as Parameters<typeof undiciFetch>[1],
      ) as unknown as Response;
    }) as typeof fetch;
  }

  it.each(["", "   ", "\n\t"])(
    "keeps Run Task disabled for empty or whitespace prompt %j and sends zero POST requests",
    async (value) => {
      const server = await startGuardrailServer({ llmConfigured: true });
      activeClose = server.close;
      bindLoopbackFetch(server.origin);

      render(
        <TooltipProvider>
          <AIWorkspacePage />
        </TooltipProvider>,
      );

      await waitFor(() =>
        expect(
          server.requests.some((r) => r.path === "/api/v1/task-center/summary"),
        ).toBe(true),
      );

      fireEvent.change(screen.getByLabelText("Task prompt"), {
        target: { value },
      });
      const submit = screen.getByRole("button", { name: "Run Task" });
      expect(submit).toBeDisabled();
      expect(
        server.requests.filter((r) => r.method === "POST"),
      ).toHaveLength(0);
    },
  );

  it("fails closed on 503 unconfigured backend response without fabricating any completed task", async () => {
    const server = await startGuardrailServer({
      llmConfigured: true,
      postStatus: 503,
    });
    activeClose = server.close;
    bindLoopbackFetch(server.origin);

    render(
      <TooltipProvider>
        <AIWorkspacePage />
      </TooltipProvider>,
    );

    await waitFor(() =>
      expect(
        server.requests.some((r) => r.path === "/api/v1/task-center/summary"),
      ).toBe(true),
    );

    fireEvent.change(screen.getByLabelText("Task prompt"), {
      target: { value: "Review unpaid Q4 invoices" },
    });
    const submit = screen.getByRole("button", { name: "Run Task" });
    await waitFor(() => expect(submit).toBeEnabled());
    fireEvent.click(submit);

    expect(await screen.findByTestId("task-center-error")).toHaveTextContent(
      "AI runtime is unconfigured; request failed closed.",
    );
    expect(screen.queryByTestId(/task-card-/)).toBeNull();
  });

  it("validates Task Center payloads and micro-USD integer formatting strictly", () => {
    expect(formatMicroUsd(0)).toBe("$0.00");
    expect(formatMicroUsd(180_000)).toBe("$0.18");
    expect(formatMicroUsd(1_250_500)).toBe("$1.2505");

    expect(validateCreateTaskPayload({ prompt: "   " })).toBeNull();
    expect(
      validateCreateTaskPayload({
        prompt: "Valid prompt",
        estimated_cost_micros: -1,
      }),
    ).toBeNull();
    expect(
      validateCreateTaskPayload({
        prompt: "Valid prompt",
        estimated_cost_micros: 12.5,
      }),
    ).toBeNull();
    expect(
      validateCreateTaskPayload({
        prompt: "  Draft follow-up  ",
        department: "finance",
        estimated_cost_micros: 50_000,
      }),
    ).toEqual({
      prompt: "Draft follow-up",
      department: "finance",
      estimated_cost_micros: 50_000,
    });

    expect(parseTaskCenterSummaryResponse({ spaces: [] })).toBeNull();
  });
});
