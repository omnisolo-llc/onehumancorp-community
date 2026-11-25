"use client";

import React, { useId, useState } from "react";
import {
  formatMicroUsd,
  formatUnixTimestamp,
  type ApproveTaskRequest,
  type TaskApprovalPayload,
} from "@/lib/taskCenterContract";
import { ProvenanceBadge } from "./ProvenanceBadge";

export interface ApprovalCardProps {
  taskId: string;
  taskTitle: string;
  approval: TaskApprovalPayload;
  busy?: boolean;
  conflictMessage?: string | null;
  onDecision: (
    taskId: string,
    decision: ApproveTaskRequest,
  ) => Promise<void> | void;
}

function formatActionKind(kind: string): string {
  return kind.replace(/_/g, " ");
}

export function ApprovalCard({
  taskId,
  taskTitle,
  approval,
  busy = false,
  conflictMessage = null,
  onDecision,
}: ApprovalCardProps) {
  const [reason, setReason] = useState("");
  const reasonId = useId();
  const isPending = approval.status === "pending";
  const exceedsStandingLimit =
    approval.estimated_cost_micros > approval.standing_limit_micros;
  const formattedPayload = JSON.stringify(approval.payload_preview, null, 2);

  async function handleDecide(approved: boolean) {
    if (!isPending || busy) return;
    const trimmedReason = reason.trim();
    await onDecision(taskId, {
      approved,
      payload_sha256: approval.payload_sha256,
      ...(trimmedReason ? { reason: trimmedReason } : {}),
    });
  }

  return (
    <section
      aria-label={`Approval required for ${taskTitle}`}
      data-testid="task-approval-card"
      className="app-card p-4 space-y-3 border-2 border-amber-500/70 dark:border-amber-500/60 bg-amber-50/70 dark:bg-amber-950/30"
    >
      <header className="flex flex-wrap items-start justify-between gap-2">
        <div className="min-w-0">
          <p className="text-[11px] font-bold uppercase tracking-wide text-amber-900 dark:text-amber-200">
            Owner Approval Gate · {approval.department}
          </p>
          <h3 className="text-sm font-bold text-slate-900 dark:text-slate-100 capitalize">
            {formatActionKind(approval.action_kind)}
          </h3>
        </div>
        <div className="flex items-center gap-1.5">
          <ProvenanceBadge
            provenance={
              approval.status === "pending"
                ? "awaiting_approval"
                : approval.status === "approved"
                  ? "verified"
                  : "cancelled"
            }
            compact
          />
          <span className="app-badge">{approval.risk}</span>
        </div>
      </header>

      <p className="text-sm text-slate-800 dark:text-slate-100">
        {approval.summary}
      </p>

      <dl className="grid grid-cols-2 gap-2 rounded-lg border border-slate-200/80 dark:border-slate-700/80 bg-white/80 dark:bg-slate-900/70 p-3 text-xs">
        <div>
          <dt className="text-slate-600 dark:text-slate-400 font-medium">
            Estimated cost
          </dt>
          <dd
            data-testid="approval-estimated-cost"
            className="mt-0.5 font-bold text-slate-900 dark:text-slate-100 tabular-nums"
          >
            {formatMicroUsd(approval.estimated_cost_micros)}{" "}
            <span className="font-normal text-slate-600 dark:text-slate-400">
              ({approval.estimated_cost_micros.toLocaleString("en-US")} µUSD)
            </span>
          </dd>
        </div>
        <div>
          <dt className="text-slate-600 dark:text-slate-400 font-medium">
            Standing limit ({approval.department})
          </dt>
          <dd
            data-testid="approval-standing-limit"
            className="mt-0.5 font-bold text-slate-900 dark:text-slate-100 tabular-nums"
          >
            {formatMicroUsd(approval.standing_limit_micros)}{" "}
            <span className="font-normal text-slate-600 dark:text-slate-400">
              ({approval.standing_limit_micros.toLocaleString("en-US")} µUSD)
            </span>
          </dd>
        </div>
      </dl>

      {exceedsStandingLimit && (
        <p
          role="note"
          className="rounded-lg border border-amber-400/80 bg-amber-100/80 dark:bg-amber-950/60 px-3 py-2 text-xs font-medium text-amber-950 dark:text-amber-100"
        >
          Estimated cost exceeds the department standing auto-approval limit.
          Explicit owner confirmation is required before dispatch.
        </p>
      )}

      <div className="space-y-1.5">
        <div className="flex items-center justify-between gap-2 text-xs">
          <span className="font-semibold text-slate-800 dark:text-slate-200">
            Exact payload preview
          </span>
          <code
            title={approval.payload_sha256}
            data-testid="approval-payload-sha256"
            className="text-[11px] font-mono text-slate-600 dark:text-slate-400 tabular-nums"
          >
            SHA-256: {approval.payload_sha256.slice(0, 12)}…
          </code>
        </div>
        <pre
          data-testid="approval-payload-preview"
          className="max-h-48 overflow-auto rounded-lg border border-slate-200 dark:border-slate-700 bg-slate-950 text-slate-100 p-3 text-xs font-mono whitespace-pre-wrap break-words"
        >
          {formattedPayload}
        </pre>
      </div>

      {conflictMessage && (
        <p
          role="alert"
          className="rounded-lg border border-rose-400 bg-rose-50 dark:bg-rose-950/60 p-2.5 text-xs font-medium text-rose-900 dark:text-rose-100"
        >
          {conflictMessage}
        </p>
      )}

      {isPending ? (
        <div className="space-y-3 pt-1">
          <div>
            <label
              htmlFor={reasonId}
              className="block text-xs font-medium text-slate-700 dark:text-slate-300 mb-1"
            >
              Decision note (optional)
            </label>
            <input
              id={reasonId}
              type="text"
              value={reason}
              onChange={(event) => setReason(event.target.value)}
              disabled={busy}
              maxLength={500}
              placeholder="Record why you approved or rejected this action..."
              className="glass-control w-full px-3 min-h-[44px] text-sm text-slate-900 dark:text-slate-100 focus-visible:outline-2 focus-visible:outline-[#2784ff] focus-visible:outline-offset-2"
            />
          </div>

          <div className="flex flex-wrap gap-2">
            <button
              type="button"
              disabled={busy}
              onClick={() => void handleDecide(true)}
              className="app-button primary flex-1 min-h-[44px] px-4 focus-visible:outline-2 focus-visible:outline-[#2784ff] focus-visible:outline-offset-2 disabled:opacity-50"
            >
              {busy ? "Submitting decision..." : "Approve & Execute"}
            </button>
            <button
              type="button"
              disabled={busy}
              onClick={() => void handleDecide(false)}
              className="app-button min-h-[44px] px-4 text-rose-800 border-rose-300 hover:bg-rose-50 dark:text-rose-200 dark:border-rose-700 dark:bg-slate-900 focus-visible:outline-2 focus-visible:outline-[#2784ff] focus-visible:outline-offset-2 disabled:opacity-50"
            >
              Reject
            </button>
          </div>
        </div>
      ) : (
        <p className="text-xs text-slate-700 dark:text-slate-300 tabular-nums">
          Decision: <strong className="capitalize">{approval.status}</strong>
          {approval.decided_by ? ` by ${approval.decided_by}` : ""}
          {approval.decided_at_unix
            ? ` · ${formatUnixTimestamp(approval.decided_at_unix)}`
            : ""}
        </p>
      )}
    </section>
  );
}
