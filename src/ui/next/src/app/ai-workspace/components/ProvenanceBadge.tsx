"use client";

import React from "react";
import type {
  AttentionRing,
  ProvenanceState,
  SpaceSourceHealth,
  TaskCenterTaskStatus,
} from "@/lib/taskCenterContract";

const PROVENANCE_CONFIG: Record<
  ProvenanceState,
  { label: string; className: string }
> = {
  draft: {
    label: "Draft",
    className:
      "bg-slate-100 text-slate-800 border-slate-300 dark:bg-slate-800 dark:text-slate-100 dark:border-slate-600",
  },
  awaiting_approval: {
    label: "Awaiting approval",
    className:
      "bg-amber-100 text-amber-950 border-amber-400 dark:bg-amber-950/70 dark:text-amber-100 dark:border-amber-600",
  },
  verified: {
    label: "Verified",
    className:
      "bg-emerald-100 text-emerald-950 border-emerald-400 dark:bg-emerald-950/70 dark:text-emerald-100 dark:border-emerald-600",
  },
  failed: {
    label: "Failed",
    className:
      "bg-rose-100 text-rose-950 border-rose-400 dark:bg-rose-950/70 dark:text-rose-100 dark:border-rose-600",
  },
  cancelled: {
    label: "Cancelled",
    className:
      "bg-slate-200 text-slate-800 border-slate-400 dark:bg-slate-800 dark:text-slate-200 dark:border-slate-600",
  },
  outcome_unknown: {
    label: "Outcome unknown",
    className:
      "bg-orange-100 text-orange-950 border-orange-500 dark:bg-orange-950/80 dark:text-orange-100 dark:border-orange-500",
  },
  unconfigured: {
    label: "Unconfigured",
    className:
      "bg-zinc-200 text-zinc-900 border-zinc-400 dark:bg-zinc-800 dark:text-zinc-100 dark:border-zinc-600",
  },
};

const STATUS_CONFIG: Record<
  TaskCenterTaskStatus,
  { label: string; className: string }
> = {
  draft: {
    label: "Draft",
    className:
      "bg-slate-100 text-slate-800 dark:bg-slate-800 dark:text-slate-200",
  },
  queued: {
    label: "Queued",
    className:
      "bg-sky-100 text-sky-900 dark:bg-sky-950/70 dark:text-sky-100",
  },
  running: {
    label: "Running",
    className:
      "bg-blue-100 text-blue-950 dark:bg-blue-950/70 dark:text-blue-100",
  },
  awaiting_approval: {
    label: "Awaiting approval",
    className:
      "bg-amber-100 text-amber-950 dark:bg-amber-950/70 dark:text-amber-100",
  },
  waiting_for_input: {
    label: "Waiting for input",
    className:
      "bg-indigo-100 text-indigo-950 dark:bg-indigo-950/70 dark:text-indigo-100",
  },
  completed: {
    label: "Completed",
    className:
      "bg-emerald-100 text-emerald-950 dark:bg-emerald-950/70 dark:text-emerald-100",
  },
  failed: {
    label: "Failed",
    className:
      "bg-rose-100 text-rose-950 dark:bg-rose-950/70 dark:text-rose-100",
  },
  cancelled: {
    label: "Cancelled",
    className:
      "bg-slate-200 text-slate-800 dark:bg-slate-800 dark:text-slate-200",
  },
  outcome_unknown: {
    label: "Outcome unknown",
    className:
      "bg-orange-100 text-orange-950 dark:bg-orange-950/80 dark:text-orange-100",
  },
};

const ATTENTION_CONFIG: Record<
  Exclude<AttentionRing, "none">,
  { label: string; dotClass: string; badgeClass: string }
> = {
  permission_required: {
    label: "Approval required",
    dotClass: "bg-amber-600",
    badgeClass:
      "bg-amber-50 text-amber-950 border-amber-400 dark:bg-amber-950/60 dark:text-amber-100 dark:border-amber-600",
  },
  needs_input: {
    label: "Needs input",
    dotClass: "bg-indigo-600",
    badgeClass:
      "bg-indigo-50 text-indigo-950 border-indigo-300 dark:bg-indigo-950/60 dark:text-indigo-100 dark:border-indigo-600",
  },
  blocked: {
    label: "Blocked",
    dotClass: "bg-rose-600",
    badgeClass:
      "bg-rose-50 text-rose-950 border-rose-400 dark:bg-rose-950/60 dark:text-rose-100 dark:border-rose-600",
  },
  completed_unread: {
    label: "Completed",
    dotClass: "bg-emerald-600",
    badgeClass:
      "bg-emerald-50 text-emerald-950 border-emerald-400 dark:bg-emerald-950/60 dark:text-emerald-100 dark:border-emerald-600",
  },
};

const HEALTH_CONFIG: Record<
  SpaceSourceHealth,
  { label: string; dotClass: string; badgeClass: string }
> = {
  live: {
    label: "Live",
    dotClass: "bg-emerald-600",
    badgeClass:
      "bg-emerald-100 text-emerald-950 dark:bg-emerald-950/70 dark:text-emerald-100",
  },
  offline: {
    label: "Offline",
    dotClass: "bg-slate-500",
    badgeClass:
      "bg-slate-200 text-slate-800 dark:bg-slate-800 dark:text-slate-200",
  },
  revoked: {
    label: "Revoked",
    dotClass: "bg-rose-600",
    badgeClass:
      "bg-rose-100 text-rose-950 dark:bg-rose-950/70 dark:text-rose-100",
  },
  unconfigured: {
    label: "Unconfigured",
    dotClass: "bg-amber-600",
    badgeClass:
      "bg-amber-100 text-amber-950 dark:bg-amber-950/70 dark:text-amber-100",
  },
};

export function ProvenanceBadge({
  provenance,
  compact = false,
}: {
  provenance: ProvenanceState;
  compact?: boolean;
}) {
  const meta = PROVENANCE_CONFIG[provenance];
  return (
    <span
      data-provenance={provenance}
      aria-label={`Provenance: ${meta.label}`}
      className={`inline-flex items-center gap-1 rounded-full border font-semibold whitespace-nowrap shrink-0 tabular-nums ${
        compact ? "px-2 py-0.5 text-[11px]" : "px-2.5 py-0.5 text-xs"
      } ${meta.className}`}
    >
      {meta.label}
    </span>
  );
}

export function TaskStatusBadge({ status }: { status: TaskCenterTaskStatus }) {
  const meta = STATUS_CONFIG[status];
  return (
    <span
      data-task-status={status}
      className={`inline-flex items-center rounded-full px-2.5 py-0.5 text-xs font-semibold whitespace-nowrap shrink-0 ${meta.className}`}
    >
      {meta.label}
    </span>
  );
}

export function AttentionRingBadge({
  attention,
}: {
  attention: AttentionRing;
}) {
  if (attention === "none") return null;
  const meta = ATTENTION_CONFIG[attention];
  return (
    <span
      data-attention={attention}
      className={`inline-flex items-center gap-1.5 rounded-full border px-2 py-0.5 text-[11px] font-semibold whitespace-nowrap shrink-0 ${meta.badgeClass}`}
    >
      <span
        aria-hidden="true"
        className={`h-2 w-2 rounded-full shrink-0 ${meta.dotClass}`}
      />
      {meta.label}
    </span>
  );
}

export function SpaceHealthBadge({ health }: { health: SpaceSourceHealth }) {
  const meta = HEALTH_CONFIG[health];
  return (
    <span
      data-space-health={health}
      className={`inline-flex items-center gap-1.5 rounded-full px-2 py-0.5 text-[11px] font-semibold whitespace-nowrap shrink-0 ${meta.badgeClass}`}
    >
      <span
        aria-hidden="true"
        className={`h-1.5 w-1.5 rounded-full shrink-0 ${meta.dotClass}`}
      />
      {meta.label}
    </span>
  );
}
