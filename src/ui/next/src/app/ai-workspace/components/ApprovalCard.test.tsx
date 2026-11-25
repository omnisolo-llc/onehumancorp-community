import React from "react";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { ApprovalCard } from "./ApprovalCard";
import type {
  ApproveTaskRequest,
  TaskApprovalPayload,
} from "@/lib/taskCenterContract";

function createApprovalPayload(
  overrides: Partial<TaskApprovalPayload> = {},
): TaskApprovalPayload {
  return {
    approval_id: "apr-001",
    action_kind: "send_email",
    department: "finance",
    risk: "draft_for_review",
    summary: "Send overdue invoice reminder #1042 to billing@acme.example",
    payload_preview: {
      to: "billing@acme.example",
      subject: "Invoice #1042 past due",
      amount_usd: "420.00",
    },
    payload_sha256:
      "8f434346648f6b96df89dda901c5176b10a6d83961dd3c1ac88b59b2dc327aa4",
    estimated_cost_micros: 250_000,
    standing_limit_micros: 100_000,
    status: "pending",
    decided_by: null,
    decided_at_unix: null,
    ...overrides,
  };
}

describe("ApprovalCard", () => {
  it("renders exact payload preview, SHA-256 digest, and micro-USD cost vs standing limit", () => {
    const approval = createApprovalPayload();
    render(
      <ApprovalCard
        taskId="tsk-101"
        taskTitle="Recover Invoice #1042"
        approval={approval}
        onDecision={() => undefined}
      />,
    );

    expect(screen.getByText(/Owner Approval Gate/i)).toBeVisible();
    expect(screen.getByText(approval.summary)).toBeVisible();
    expect(screen.getByTestId("approval-estimated-cost")).toHaveTextContent(
      "$0.25 (250,000 µUSD)",
    );
    expect(screen.getByTestId("approval-standing-limit")).toHaveTextContent(
      "$0.10 (100,000 µUSD)",
    );
    expect(screen.getByTestId("approval-payload-sha256")).toHaveTextContent(
      "SHA-256: 8f434346648f…",
    );
    expect(screen.getByTestId("approval-payload-preview")).toHaveTextContent(
      '"to": "billing@acme.example"',
    );
    expect(
      screen.getByText(
        /Estimated cost exceeds the department standing auto-approval limit/i,
      ),
    ).toBeVisible();
  });

  it("binds the exact payload_sha256 and trimmed reason on approve and reject", async () => {
    const decisions: Array<{ taskId: string; decision: ApproveTaskRequest }> = [];
    const approval = createApprovalPayload({
      estimated_cost_micros: 50_000,
      standing_limit_micros: 100_000,
    });

    const { rerender } = render(
      <ApprovalCard
        taskId="tsk-202"
        taskTitle="Send Client Update"
        approval={approval}
        onDecision={(taskId, decision) => {
          decisions.push({ taskId, decision });
        }}
      />,
    );

    expect(
      screen.queryByText(/exceeds the department standing auto-approval limit/i),
    ).toBeNull();

    fireEvent.change(screen.getByLabelText("Decision note (optional)"), {
      target: { value: "  Verified invoice amount matches ledger  " },
    });
    fireEvent.click(screen.getByRole("button", { name: "Approve & Execute" }));

    await waitFor(() => expect(decisions).toHaveLength(1));
    expect(decisions[0]).toEqual({
      taskId: "tsk-202",
      decision: {
        approved: true,
        payload_sha256: approval.payload_sha256,
        reason: "Verified invoice amount matches ledger",
      },
    });

    rerender(
      <ApprovalCard
        taskId="tsk-202"
        taskTitle="Send Client Update"
        approval={approval}
        conflictMessage="Payload hash mismatch (409 Conflict)."
        onDecision={(taskId, decision) => {
          decisions.push({ taskId, decision });
        }}
      />,
    );

    expect(screen.getByRole("alert")).toHaveTextContent(
      "Payload hash mismatch (409 Conflict).",
    );

    fireEvent.click(screen.getByRole("button", { name: "Reject" }));
    await waitFor(() => expect(decisions).toHaveLength(2));
    expect(decisions[1]?.decision.approved).toBe(false);
    expect(decisions[1]?.decision.payload_sha256).toBe(approval.payload_sha256);
  });

  it("renders read-only audit stamp without action buttons once decided", () => {
    const decided = createApprovalPayload({
      status: "approved",
      decided_by: "owner@business.example",
      decided_at_unix: 1_760_000_000,
    });
    render(
      <ApprovalCard
        taskId="tsk-303"
        taskTitle="Completed Approval"
        approval={decided}
        onDecision={() => undefined}
      />,
    );

    expect(screen.queryByRole("button", { name: "Approve & Execute" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Reject" })).toBeNull();
    expect(screen.getByText(/owner@business\.example/)).toBeVisible();
  });
});
