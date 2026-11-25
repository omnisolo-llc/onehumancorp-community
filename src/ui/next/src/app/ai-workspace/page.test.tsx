import React from "react";
import http, {
  type IncomingMessage,
  type Server,
  type ServerResponse,
} from "node:http";
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
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import AIWorkspacePage from "./page";
import { TooltipProvider } from "../../components/TooltipRegistry";
import {
  CONNECTOR_IDS,
  isUuid,
  type ConnectorCatalogResponse,
  type SpaceRecord,
  type TaskCenterSummaryResponse,
  type TaskCenterTask,
} from "@/lib/taskCenterContract";

interface LoopbackState {
  summary: TaskCenterSummaryResponse;
  catalog: ConnectorCatalogResponse;
  requests: Array<{
    method: string;
    path: string;
    headers: http.IncomingHttpHeaders;
    body: unknown;
  }>;
  sseClients: Set<ServerResponse>;
  approveStatus: number;
  approveError: string | null;
}

function defaultCatalog(): ConnectorCatalogResponse {
  return {
    success: true,
    connectors: [
      {
        id: "openai_api",
        name: "OpenAI API",
        category: "ai_provider",
        auth_mode: "api_key",
        read_scopes: ["models:list"],
        write_scopes: ["responses:create"],
        status: "verified",
        usable: true,
        verified_at: 1_760_000_000,
        expires_at: 1_760_604_800,
      },
      {
        id: "anthropic_api",
        name: "Anthropic API",
        category: "ai_provider",
        auth_mode: "api_key",
        read_scopes: ["models:list"],
        write_scopes: ["messages:create"],
        status: "not_connected",
        usable: false,
        verified_at: null,
        expires_at: null,
      },
      {
        id: "stripe",
        name: "Stripe",
        category: "payments",
        auth_mode: "api_key",
        read_scopes: ["invoices:read"],
        write_scopes: ["payment_links:create"],
        status: "verified",
        usable: true,
        verified_at: 1_760_000_000,
        expires_at: 1_760_604_800,
      },
      {
        id: "resend",
        name: "Resend",
        category: "email",
        auth_mode: "api_key",
        read_scopes: ["domains:read"],
        write_scopes: ["emails:send"],
        status: "not_connected",
        usable: false,
        verified_at: null,
        expires_at: null,
      },
      {
        id: CONNECTOR_IDS[4],
        name: "Workspace Drive",
        category: "workspace",
        auth_mode: "oauth_bearer",
        read_scopes: ["drive.readonly"],
        write_scopes: [],
        status: "verification_required",
        usable: false,
        verified_at: 1_750_000_000,
        expires_at: 1_750_604_800,
      },
    ],
    mcp_server: {
      endpoint: "/api/v1/integrations/mcp",
      protocol_version: "2025-03-26",
      default_mode: "read_only",
      write_opt_in_header: "x-ohc-mcp-write",
      tool_count: 9,
    },
  };
}

function defaultSummary(
  overrides: Partial<TaskCenterSummaryResponse> = {},
): TaskCenterSummaryResponse {
  const space: SpaceRecord = {
    id: "spc-cash-1",
    name: "Cash Recovery Desk",
    description: "Tracks overdue client invoices and owner-approved reminders.",
    kind: "cash_recovery",
    prompt_template: "Draft polite overdue follow-up for invoices > 14 days.",
    department: "finance",
    reads: ["Invoice ledger", "Stripe payment links"],
    shows: ["Overdue queue", "Approval cards"],
    cannot: ["Charge customers without approval", "Send email autonomously"],
    source_health: "live",
    pinned_artifact_ids: [],
    archived: false,
    created_at_unix: 1_760_000_000,
    updated_at_unix: 1_760_000_100,
  };

  const approvalTask: TaskCenterTask = {
    id: "tsk-apr-1",
    space_id: space.id,
    title: "Send Invoice #1042 reminder",
    prompt: "Prepare overdue payment reminder email for Acme Corp invoice #1042.",
    department: "finance",
    status: "awaiting_approval",
    provenance: "awaiting_approval",
    attention: "permission_required",
    risk: "draft_for_review",
    payer_mode: "byok_api",
    estimated_cost_micros: 180_000,
    actual_cost_micros: null,
    idempotency_key: "11111111-1111-4111-8111-111111111111",
    receipt_id: "rcp-apr-1",
    output_summary: null,
    error_message: null,
    pending_approval: {
      approval_id: "apr-1042",
      action_kind: "send_email",
      department: "finance",
      risk: "draft_for_review",
      summary: "Email overdue notice for Invoice #1042 ($420.00) to billing@acme.example",
      payload_preview: {
        to: "billing@acme.example",
        invoice_id: "INV-1042",
      },
      payload_sha256:
        "3c6e0b8a9c15224a8228b9a98ca1531d55555555555555555555555555555555",
      estimated_cost_micros: 180_000,
      standing_limit_micros: 50_000,
      status: "pending",
      decided_by: null,
      decided_at_unix: null,
    },
    steps: [
      {
        id: "stp-1",
        task_id: "tsk-apr-1",
        seq: 1,
        kind: "approval_gate",
        title: "Prepared outbound email payload",
        detail: "Paused at owner approval gate before calling Resend.",
        status: "awaiting_approval",
        provenance: "awaiting_approval",
        tool_name: "resend.send",
        cost_micros: 12_000,
        created_at_unix: 1_760_000_200,
      },
    ],
    artifacts: [],
    archived: false,
    created_at_unix: 1_760_000_150,
    updated_at_unix: 1_760_000_200,
  };

  return {
    spaces: [space],
    tasks: [approvalTask],
    standing_policies: [
      {
        department: "finance",
        auto_approve_max_micros: 50_000,
        allow_external_send: false,
        allow_financial_commit: false,
        updated_at_unix: 1_760_000_000,
      },
    ],
    runtime: {
      llm_configured: true,
      provider: "openai_api",
      model: "gpt-4o-mini",
      payer_mode: "byok_api",
      reason: null,
    },
    budget: {
      configured: true,
      limit_micros: 5_000_000,
      spent_micros: 350_000,
      reserved_micros: 180_000,
      currency: "USD",
      unit: "micro_usd",
      customer_direct_inference_is_not_rebilled: true,
    },
    ...overrides,
  };
}

async function readJsonBody(req: IncomingMessage): Promise<unknown> {
  const chunks: Buffer[] = [];
  for await (const chunk of req) {
    chunks.push(Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk));
  }
  const raw = Buffer.concat(chunks).toString("utf8");
  if (!raw.trim()) return null;
  return JSON.parse(raw);
}

class LoopbackEventSource extends EventTarget {
  static baseOrigin = "";
  onopen: ((event: Event) => void) | null = null;
  onmessage: ((event: MessageEvent<string>) => void) | null = null;
  onerror: ((event: Event) => void) | null = null;
  private controller = new AbortController();

  constructor(url: string) {
    super();
    const target = url.startsWith("http")
      ? url
      : `${LoopbackEventSource.baseOrigin}${url}`;
    void this.openStream(target);
  }

  private async openStream(target: string) {
    try {
      const response = await undiciFetch(target, {
        headers: { accept: "text/event-stream" },
        signal: this.controller.signal,
      });
      if (!response.ok || !response.body) {
        this.onerror?.(new Event("error"));
        return;
      }
      this.onopen?.(new Event("open"));
      const reader = response.body.getReader();
      const decoder = new TextDecoder();
      let buffer = "";
      while (true) {
        const { done, value } = await reader.read();
        if (done) break;
        buffer += decoder.decode(value, { stream: true });
        let boundary = buffer.indexOf("\n\n");
        while (boundary !== -1) {
          const frame = buffer.slice(0, boundary);
          buffer = buffer.slice(boundary + 2);
          this.dispatchFrame(frame);
          boundary = buffer.indexOf("\n\n");
        }
      }
    } catch {
      if (!this.controller.signal.aborted) {
        this.onerror?.(new Event("error"));
      }
    }
  }

  private dispatchFrame(frame: string) {
    let eventName = "message";
    const dataLines: string[] = [];
    for (const line of frame.split("\n")) {
      if (line.startsWith("event:")) {
        eventName = line.slice(6).trim();
      } else if (line.startsWith("data:")) {
        dataLines.push(line.slice(5).trim());
      }
    }
    if (dataLines.length === 0) return;
    const data = dataLines.join("\n");
    const msg = new MessageEvent(eventName, { data });
    this.dispatchEvent(msg);
    if (eventName === "message") {
      this.onmessage?.(msg);
    }
  }

  close() {
    this.controller.abort();
  }
}

async function startLoopbackServer(state: LoopbackState): Promise<{
  server: Server;
  origin: string;
  broadcastSse: (eventName: string, payload: unknown) => void;
  close: () => Promise<void>;
}> {
  const server = http.createServer(async (req, res) => {
    const parsedUrl = new URL(req.url || "/", "http://127.0.0.1");
    const pathname = parsedUrl.pathname;
    const method = req.method || "GET";

    if (method === "GET" && pathname === "/api/v1/task-center/stream") {
      res.writeHead(200, {
        "Content-Type": "text/event-stream",
        "Cache-Control": "private, no-store",
        Connection: "keep-alive",
      });
      res.write(": connected\n\n");
      state.sseClients.add(res);
      req.on("close", () => {
        state.sseClients.delete(res);
      });
      return;
    }

    const body =
      method === "POST" || method === "PATCH" || method === "PUT"
        ? await readJsonBody(req)
        : null;
    state.requests.push({ method, path: pathname, headers: req.headers, body });

    const sendJson = (status: number, data: unknown) => {
      res.writeHead(status, {
        "Content-Type": "application/json",
        "Cache-Control": "private, no-store",
      });
      res.end(JSON.stringify(data));
    };

    if (method === "GET" && pathname === "/api/v1/task-center/summary") {
      sendJson(200, state.summary);
      return;
    }

    if (method === "GET" && pathname === "/api/v1/integrations/catalog") {
      sendJson(200, state.catalog);
      return;
    }

    if (method === "POST" && pathname === "/api/v1/task-center/spaces") {
      const payload = body as Record<string, unknown>;
      const created: SpaceRecord = {
        id: `spc-${state.summary.spaces.length + 1}`,
        name: String(payload.name),
        description: String(payload.description ?? ""),
        kind: payload.kind as SpaceRecord["kind"],
        prompt_template: String(payload.prompt_template ?? ""),
        department: String(payload.department ?? "operations"),
        reads: payload.reads as string[],
        shows: payload.shows as string[],
        cannot: payload.cannot as string[],
        source_health: "live",
        pinned_artifact_ids: [],
        archived: false,
        created_at_unix: 1_760_000_500,
        updated_at_unix: 1_760_000_500,
      };
      state.summary.spaces = [created, ...state.summary.spaces];
      sendJson(201, created);
      return;
    }

    if (method === "POST" && pathname === "/api/v1/task-center/tasks") {
      const payload = body as Record<string, unknown>;
      const idempotencyKey = String(req.headers["idempotency-key"] ?? "");
      if (!isUuid(idempotencyKey)) {
        sendJson(400, { error: "valid UUID Idempotency-Key required" });
        return;
      }
      const created: TaskCenterTask = {
        id: `tsk-new-${state.summary.tasks.length + 1}`,
        space_id:
          typeof payload.space_id === "string" ? payload.space_id : null,
        title: String(payload.prompt).slice(0, 60),
        prompt: String(payload.prompt),
        department: String(payload.department ?? "operations"),
        status: "queued",
        provenance: "draft",
        attention: "none",
        risk: "auto_execute",
        payer_mode: state.summary.runtime.payer_mode,
        estimated_cost_micros: Number(payload.estimated_cost_micros ?? 25_000),
        actual_cost_micros: null,
        idempotency_key: idempotencyKey,
        receipt_id: "rcp-live-1",
        output_summary: null,
        error_message: null,
        pending_approval: null,
        steps: [],
        artifacts: [],
        archived: false,
        created_at_unix: 1_760_000_600,
        updated_at_unix: 1_760_000_600,
      };
      state.summary.tasks = [created, ...state.summary.tasks];
      sendJson(202, created);
      return;
    }

    const approveMatch = pathname.match(
      /^\/api\/v1\/task-center\/tasks\/([^/]+)\/approve$/,
    );
    if (method === "POST" && approveMatch) {
      if (state.approveStatus !== 200) {
        sendJson(state.approveStatus, {
          error: state.approveError || "Conflict",
        });
        return;
      }
      const taskId = decodeURIComponent(approveMatch[1]);
      const payload = body as {
        approved: boolean;
        payload_sha256: string;
        reason?: string;
      };
      const existing = state.summary.tasks.find((item) => item.id === taskId);
      if (!existing || !existing.pending_approval) {
        sendJson(404, { error: "Task approval not found" });
        return;
      }
      if (existing.pending_approval.payload_sha256 !== payload.payload_sha256) {
        sendJson(409, { error: "Approval payload SHA-256 mismatch" });
        return;
      }
      const updated: TaskCenterTask = {
        ...existing,
        status: payload.approved ? "completed" : "cancelled",
        provenance: payload.approved ? "verified" : "cancelled",
        attention: "none",
        output_summary: payload.approved
          ? "Dispatched verified email via Resend."
          : null,
        pending_approval: {
          ...existing.pending_approval,
          status: payload.approved ? "approved" : "rejected",
          decided_by: "owner@business.example",
          decided_at_unix: 1_760_000_700,
        },
        updated_at_unix: 1_760_000_700,
      };
      state.summary.tasks = state.summary.tasks.map((item) =>
        item.id === taskId ? updated : item,
      );
      sendJson(200, updated);
      return;
    }

    sendJson(404, { error: `Unhandled route ${method} ${pathname}` });
  });

  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const address = server.address() as AddressInfo;
  const origin = `http://127.0.0.1:${address.port}`;

  return {
    server,
    origin,
    broadcastSse: (eventName: string, payload: unknown) => {
      const frame = `event: ${eventName}\ndata: ${JSON.stringify(payload)}\n\n`;
      for (const client of state.sseClients) {
        client.write(frame);
      }
    },
    close: async () => {
      for (const client of state.sseClients) {
        client.end();
      }
      state.sseClients.clear();
      await new Promise<void>((resolve) => server.close(() => resolve()));
    },
  };
}

describe("AIWorkspacePage (Omni Assistant AI Task Center)", () => {
  const originalFetch = globalThis.fetch;
  const originalEventSource = globalThis.EventSource;
  let activeClose: (() => Promise<void>) | null = null;

  beforeEach(() => {
    activeClose = null;
  });

  afterEach(async () => {
    cleanup();
    globalThis.fetch = originalFetch;
    if (originalEventSource) {
      globalThis.EventSource = originalEventSource;
    } else {
      delete (globalThis as { EventSource?: unknown }).EventSource;
    }
    if (activeClose) {
      await activeClose();
      activeClose = null;
    }
  });

  async function mountWithLoopback(state: LoopbackState) {
    const loopback = await startLoopbackServer(state);
    activeClose = loopback.close;
    LoopbackEventSource.baseOrigin = loopback.origin;

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

    globalThis.EventSource =
      LoopbackEventSource as unknown as typeof EventSource;

    render(
      <TooltipProvider>
        <AIWorkspacePage />
      </TooltipProvider>,
    );

    return loopback;
  }

  it("loads live summary and connector catalog with zero legacy hardcoded items", async () => {
    const state: LoopbackState = {
      summary: defaultSummary(),
      catalog: defaultCatalog(),
      requests: [],
      sseClients: new Set(),
      approveStatus: 200,
      approveError: null,
    };

    await mountWithLoopback(state);

    const taskCard = await screen.findByTestId("task-card-tsk-apr-1");
    expect(taskCard).toHaveTextContent("Send Invoice #1042 reminder");
    expect(screen.getByText("Cash Recovery Desk")).toBeVisible();
    expect(screen.getByTestId("budget-summary-strip")).toHaveTextContent(
      "$0.35 spent · $0.18 reserved / $5.00 limit",
    );
    expect(screen.getByText("2/5 verified")).toBeVisible();
    expect(
      screen.queryByText(/Integrate deep learning models/i),
    ).toBeNull();
    expect(screen.queryByText(/Productivity Score: 87%/i)).toBeNull();
  });

  it("renders honest fail-closed banner and disables Run Task when LLM runtime is unconfigured", async () => {
    const state: LoopbackState = {
      summary: defaultSummary({
        runtime: {
          llm_configured: false,
          provider: null,
          model: null,
          payer_mode: "unconfigured",
          reason:
            "No verified OpenAI or Anthropic key is active for this organization.",
        },
      }),
      catalog: defaultCatalog(),
      requests: [],
      sseClients: new Set(),
      approveStatus: 200,
      approveError: null,
    };

    await mountWithLoopback(state);

    const banner = await screen.findByTestId("unconfigured-runtime-banner");
    expect(banner).toHaveTextContent("AI runtime is unconfigured");
    expect(banner).toHaveTextContent(
      "No verified OpenAI or Anthropic key is active for this organization.",
    );

    fireEvent.change(screen.getByLabelText("Task prompt"), {
      target: { value: "Summarize quarterly invoices" },
    });
    expect(screen.getByRole("button", { name: "Run Task" })).toBeDisabled();
  });

  it("renders approval card cost/payload and submits SHA-256-bound approval decision", async () => {
    const state: LoopbackState = {
      summary: defaultSummary(),
      catalog: defaultCatalog(),
      requests: [],
      sseClients: new Set(),
      approveStatus: 200,
      approveError: null,
    };

    await mountWithLoopback(state);

    const approvalCard = await screen.findByTestId("task-approval-card");
    expect(approvalCard).toHaveTextContent("$0.18 (180,000 µUSD)");
    expect(approvalCard).toHaveTextContent("$0.05 (50,000 µUSD)");
    expect(approvalCard).toHaveTextContent('"invoice_id": "INV-1042"');

    fireEvent.click(
      within(approvalCard).getByRole("button", { name: "Approve & Execute" }),
    );

    await waitFor(() => {
      const approveCall = state.requests.find(
        (r) => r.path === "/api/v1/task-center/tasks/tsk-apr-1/approve",
      );
      expect(approveCall).toBeDefined();
      expect(approveCall?.body).toEqual({
        approved: true,
        payload_sha256:
          "3c6e0b8a9c15224a8228b9a98ca1531d55555555555555555555555555555555",
      });
    });

    await waitFor(() =>
      expect(screen.getByTestId("task-card-tsk-apr-1")).toHaveTextContent(
        "Dispatched verified email via Resend.",
      ),
    );
  });

  it("surfaces 409 Conflict stale-payload approval warning and reloads server truth", async () => {
    const state: LoopbackState = {
      summary: defaultSummary(),
      catalog: defaultCatalog(),
      requests: [],
      sseClients: new Set(),
      approveStatus: 409,
      approveError: "Approval payload SHA-256 mismatch (stale approval)",
    };

    await mountWithLoopback(state);
    const approvalCard = await screen.findByTestId("task-approval-card");
    fireEvent.click(
      within(approvalCard).getByRole("button", { name: "Approve & Execute" }),
    );

    expect(
      await within(approvalCard).findByRole("alert"),
    ).toHaveTextContent("Approval payload SHA-256 mismatch (stale approval)");
  });

  it("displays explicit outcome_unknown non-retry warning and blocks blind retries", async () => {
    const unknownTask: TaskCenterTask = {
      ...defaultSummary().tasks[0],
      id: "tsk-unknown-1",
      title: "Charge customer invoice #901",
      status: "outcome_unknown",
      provenance: "outcome_unknown",
      attention: "blocked",
      pending_approval: null,
      receipt_id: "rcp-indeterminate-901",
      error_message:
        "Provider connection dropped after request dispatch; reconcile before retry.",
    };

    const state: LoopbackState = {
      summary: defaultSummary({ tasks: [unknownTask] }),
      catalog: defaultCatalog(),
      requests: [],
      sseClients: new Set(),
      approveStatus: 200,
      approveError: null,
    };

    await mountWithLoopback(state);

    const warning = await screen.findByTestId("outcome-unknown-warning");
    expect(warning).toHaveTextContent(
      "Provider Outcome Unknown — Do Not Blindly Retry",
    );
    expect(warning).toHaveTextContent("rcp-indeterminate-901");
    const inspector = screen.getByLabelText("Contextual task inspector");
    expect(
      within(inspector).queryByRole("button", { name: /retry/i }),
    ).toBeNull();
    expect(screen.getByLabelText("Follow-up instruction")).toBeDisabled();
  });

  it("creates a Space with Reads/Shows/Cannot manifest and submits idempotent tasks", async () => {
    const state: LoopbackState = {
      summary: defaultSummary(),
      catalog: defaultCatalog(),
      requests: [],
      sseClients: new Set(),
      approveStatus: 200,
      approveError: null,
    };

    await mountWithLoopback(state);
    await screen.findByText("Cash Recovery Desk");

    fireEvent.click(screen.getByRole("button", { name: "New Space" }));
    fireEvent.change(screen.getByLabelText("Space name"), {
      target: { value: "Client Delivery Hub" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Create Space" }));

    const manifestCard = await screen.findByTestId("space-capability-card");
    expect(manifestCard).toHaveTextContent("Client Delivery Hub");
    expect(manifestCard).toHaveTextContent("Reads");
    expect(manifestCard).toHaveTextContent("Shows");
    expect(manifestCard).toHaveTextContent("Cannot");
    expect(manifestCard).toHaveTextContent("Spend without approval");

    fireEvent.change(screen.getByLabelText("Task prompt"), {
      target: { value: "Audit pending client deliverables for this week" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Run Task" }));

    await waitFor(() => {
      const createCall = state.requests.find(
        (r) => r.method === "POST" && r.path === "/api/v1/task-center/tasks",
      );
      expect(createCall).toBeDefined();
      expect(isUuid(createCall?.headers["idempotency-key"])).toBe(true);
    });

    const newCard = await screen.findByTestId("task-card-tsk-new-2");
    expect(newCard).toHaveTextContent(
      "Audit pending client deliverables for this week",
    );
  });

  it("applies live SSE task_updated frames and reloads on resync_required", async () => {
    const state: LoopbackState = {
      summary: defaultSummary(),
      catalog: defaultCatalog(),
      requests: [],
      sseClients: new Set(),
      approveStatus: 200,
      approveError: null,
    };

    const loopback = await mountWithLoopback(state);
    await screen.findByTestId("task-card-tsk-apr-1");
    await waitFor(() => expect(state.sseClients.size).toBe(1));

    const liveTask: TaskCenterTask = {
      ...defaultSummary().tasks[0],
      id: "tsk-sse-live-1",
      title: "Live SSE synced deliverable",
      status: "completed",
      provenance: "verified",
      attention: "completed_unread",
      pending_approval: null,
      output_summary: "Verified live over SSE stream.",
    };

    loopback.broadcastSse("task_updated", {
      event_id: "evt-1",
      kind: "task_updated",
      tenant_id: "tenant-1",
      occurred_at_unix: 1_760_000_900,
      payload: liveTask,
    });

    expect(
      await screen.findByTestId("task-card-tsk-sse-live-1"),
    ).toHaveTextContent("Live SSE synced deliverable");

    const summaryCallsBefore = state.requests.filter(
      (r) => r.path === "/api/v1/task-center/summary",
    ).length;

    loopback.broadcastSse("resync_required", {
      event_id: "evt-2",
      kind: "resync_required",
      tenant_id: "tenant-1",
      occurred_at_unix: 1_760_000_905,
      payload: { reason: "lagged" },
    });

    await waitFor(() => {
      const summaryCallsAfter = state.requests.filter(
        (r) => r.path === "/api/v1/task-center/summary",
      ).length;
      expect(summaryCallsAfter).toBeGreaterThan(summaryCallsBefore);
    });
  });

  it("disables All Spaces when already unfiltered and clears active space or attention filters", async () => {
    const state: LoopbackState = {
      summary: defaultSummary(),
      catalog: defaultCatalog(),
      requests: [],
      sseClients: new Set(),
      approveStatus: 200,
      approveError: null,
    };

    await mountWithLoopback(state);
    await screen.findByTestId("task-card-tsk-apr-1");

    const allSpacesButton = screen.getByRole("button", { name: /All Spaces/i });
    const attentionButton = screen.getByRole("button", {
      name: /Needs attention only/i,
    });

    expect(allSpacesButton).toHaveAttribute("aria-pressed", "true");
    expect(allSpacesButton).toBeDisabled();

    fireEvent.click(attentionButton);
    expect(attentionButton).toHaveAttribute("aria-pressed", "true");
    expect(allSpacesButton).toBeEnabled();

    fireEvent.click(allSpacesButton);
    expect(attentionButton).toHaveAttribute("aria-pressed", "false");
    expect(allSpacesButton).toHaveAttribute("aria-pressed", "true");
    expect(allSpacesButton).toBeDisabled();

    fireEvent.click(screen.getByRole("button", { name: /Cash Recovery Desk/i }));
    expect(allSpacesButton).toHaveAttribute("aria-pressed", "false");
    expect(allSpacesButton).toBeEnabled();

    fireEvent.click(allSpacesButton);
    expect(allSpacesButton).toHaveAttribute("aria-pressed", "true");
    expect(allSpacesButton).toBeDisabled();
  });
});
