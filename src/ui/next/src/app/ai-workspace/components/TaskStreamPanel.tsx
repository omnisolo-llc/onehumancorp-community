"use client";

import React, { FormEvent, useId, useMemo, useState } from "react";
import Link from "next/link";
import {
  attentionPriority,
  formatMicroUsd,
  formatPayerMode,
  formatUnixTimestamp,
  type CreateTaskRequest,
  type SpaceRecord,
  type TaskArtifactRecord,
  type TaskCenterRuntimeInfo,
  type TaskCenterTask,
  type TaskCenterTaskStatus,
} from "@/lib/taskCenterContract";
import {
  AttentionRingBadge,
  ProvenanceBadge,
  TaskStatusBadge,
} from "./ProvenanceBadge";

export type TaskViewMode = "overview" | "tasks" | "board" | "timeline";
type StatusFilter =
  | "all"
  | "active"
  | "awaiting_approval"
  | "completed"
  | "outcome_unknown"
  | "failed"
  | "cancelled";
type SortOrder = "urgency" | "recent";

export interface TaskStreamPanelProps {
  tasks: TaskCenterTask[];
  spaces: SpaceRecord[];
  selectedSpaceId: string | null;
  selectedTaskId: string | null;
  attentionOnly: boolean;
  runtime: TaskCenterRuntimeInfo | null;
  viewMode: TaskViewMode;
  submittingTask: boolean;
  composerPrompt: string;
  composerDepartment: string;
  composerAction: string;
  composerEstimatedCostMicros: number;
  onComposerPromptChange: (value: string) => void;
  onComposerDepartmentChange: (value: string) => void;
  onComposerActionChange: (value: string) => void;
  onComposerEstimatedCostChange: (micros: number) => void;
  onViewModeChange: (mode: TaskViewMode) => void;
  onSelectTask: (taskId: string) => void;
  onSubmitTask: (input: CreateTaskRequest) => Promise<boolean>;
}

const VIEW_MODES: readonly TaskViewMode[] = [
  "overview",
  "tasks",
  "board",
  "timeline",
];

function isActiveStatus(status: TaskCenterTaskStatus): boolean {
  return (
    status === "draft" ||
    status === "queued" ||
    status === "running" ||
    status === "awaiting_approval" ||
    status === "waiting_for_input"
  );
}

export function TaskStreamPanel({
  tasks,
  spaces,
  selectedSpaceId,
  selectedTaskId,
  attentionOnly,
  runtime,
  viewMode,
  submittingTask,
  composerPrompt,
  composerDepartment,
  composerAction,
  composerEstimatedCostMicros,
  onComposerPromptChange,
  onComposerDepartmentChange,
  onComposerActionChange,
  onComposerEstimatedCostChange,
  onViewModeChange,
  onSelectTask,
  onSubmitTask,
}: TaskStreamPanelProps) {
  const [searchQuery, setSearchQuery] = useState("");
  const [statusFilter, setStatusFilter] = useState<StatusFilter>("all");
  const [sortOrder, setSortOrder] = useState<SortOrder>("urgency");

  const promptId = useId();
  const deptId = useId();
  const actionId = useId();
  const costId = useId();
  const filterId = useId();

  const selectedSpace = useMemo(
    () => spaces.find((space) => space.id === selectedSpaceId) ?? null,
    [spaces, selectedSpaceId],
  );

  const scopedTasks = useMemo(() => {
    const query = searchQuery.trim().toLowerCase();
    return tasks
      .filter((task) => {
        if (task.archived) return false;
        if (selectedSpaceId && task.space_id !== selectedSpaceId) return false;
        if (attentionOnly && task.attention === "none") return false;
        if (statusFilter === "active" && !isActiveStatus(task.status)) {
          return false;
        }
        if (
          statusFilter !== "all" &&
          statusFilter !== "active" &&
          task.status !== statusFilter
        ) {
          return false;
        }
        if (query) {
          const haystack = [
            task.title,
            task.prompt,
            task.department,
            task.output_summary ?? "",
            task.error_message ?? "",
          ]
            .join(" ")
            .toLowerCase();
          if (!haystack.includes(query)) return false;
        }
        return true;
      })
      .sort((a, b) => {
        if (sortOrder === "urgency") {
          const prioDiff =
            attentionPriority(b.attention) - attentionPriority(a.attention);
          if (prioDiff !== 0) return prioDiff;
        }
        return b.updated_at_unix - a.updated_at_unix;
      });
  }, [tasks, selectedSpaceId, attentionOnly, statusFilter, searchQuery, sortOrder]);

  const pinnedSpaceArtifacts = useMemo(() => {
    if (!selectedSpace) return [] as Array<{ artifact: TaskArtifactRecord; taskTitle: string; taskId: string }>;
    const pinnedIds = new Set(selectedSpace.pinned_artifact_ids);
    const items: Array<{ artifact: TaskArtifactRecord; taskTitle: string; taskId: string }> = [];
    for (const task of tasks) {
      for (const artifact of task.artifacts) {
        if (
          artifact.pinned &&
          (artifact.space_id === selectedSpace.id || pinnedIds.has(artifact.id))
        ) {
          items.push({ artifact, taskTitle: task.title, taskId: task.id });
        }
      }
    }
    return items;
  }, [selectedSpace, tasks]);

  const timelineEntries = useMemo(() => {
    const entries = scopedTasks.flatMap((task) =>
      task.steps.map((step) => ({
        task,
        step,
      })),
    );
    return entries.sort(
      (a, b) =>
        b.step.created_at_unix - a.step.created_at_unix ||
        b.step.seq - a.step.seq,
    );
  }, [scopedTasks]);

  const counts = useMemo(() => {
    const active = tasks.filter((t) => !t.archived);
    return {
      pendingApprovals: active.filter((t) => t.status === "awaiting_approval")
        .length,
      running: active.filter(
        (t) => t.status === "queued" || t.status === "running",
      ).length,
      unknown: active.filter(
        (t) =>
          t.status === "outcome_unknown" || t.provenance === "outcome_unknown",
      ).length,
      completed: active.filter((t) => t.status === "completed").length,
    };
  }, [tasks]);

  const llmConfigured = runtime?.llm_configured === true;
  const trimmedPrompt = composerPrompt.trim();

  async function handleSubmit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (!trimmedPrompt || submittingTask || !llmConfigured) return;
    const payload: CreateTaskRequest = {
      prompt: trimmedPrompt,
      ...(selectedSpaceId ? { space_id: selectedSpaceId } : {}),
      department: composerDepartment,
      ...(composerAction !== "analyze"
        ? { requested_action: composerAction }
        : {}),
      ...(composerEstimatedCostMicros > 0
        ? { estimated_cost_micros: composerEstimatedCostMicros }
        : {}),
    };
    const ok = await onSubmitTask(payload);
    if (ok) {
      onComposerPromptChange("");
    }
  }

  function renderTaskCard(task: TaskCenterTask) {
    const selected = task.id === selectedTaskId;
    const isUnknown =
      task.status === "outcome_unknown" ||
      task.provenance === "outcome_unknown";
    const latestStep =
      task.steps.length > 0
        ? [...task.steps].sort((a, b) => b.seq - a.seq)[0]
        : null;

    return (
      <article
        key={task.id}
        data-testid={`task-card-${task.id}`}
        className={`app-card p-4 transition-colors motion-reduce:transition-none border ${
          selected
            ? "border-[#2784ff] ring-2 ring-[#2784ff]/30"
            : "border-slate-200/80 dark:border-slate-700/80 hover:border-slate-300 dark:hover:border-slate-600"
        }`}
      >
        <button
          type="button"
          onClick={() => onSelectTask(task.id)}
          aria-pressed={selected}
          className="w-full text-left space-y-2 focus-visible:outline-2 focus-visible:outline-[#2784ff] focus-visible:outline-offset-2 rounded"
        >
          <div className="flex flex-wrap items-start justify-between gap-2">
            <h3 className="text-sm font-bold text-slate-900 dark:text-slate-100">
              {task.title}
            </h3>
            <div className="flex flex-wrap items-center gap-1.5">
              <AttentionRingBadge attention={task.attention} />
              <TaskStatusBadge status={task.status} />
              <ProvenanceBadge provenance={task.provenance} compact />
            </div>
          </div>

          <p className="text-xs text-slate-700 dark:text-slate-300 line-clamp-2">
            {task.output_summary || task.error_message || task.prompt}
          </p>

          {isUnknown && (
            <p
              role="alert"
              className="rounded-md border border-orange-400 bg-orange-50 dark:bg-orange-950/60 px-2.5 py-1.5 text-xs font-medium text-orange-950 dark:text-orange-100"
            >
              Outcome unknown — reconcile provider receipt before retrying.
            </p>
          )}

          <div className="flex flex-wrap items-center justify-between gap-2 pt-1 text-[11px] text-slate-600 dark:text-slate-400 tabular-nums">
            <div className="flex flex-wrap items-center gap-2">
              <span className="capitalize font-semibold">{task.department}</span>
              <span>· {formatPayerMode(task.payer_mode)}</span>
              <span>
                ·{" "}
                {task.actual_cost_micros !== null
                  ? `Actual ${formatMicroUsd(task.actual_cost_micros)}`
                  : `Est. ${formatMicroUsd(task.estimated_cost_micros)}`}
              </span>
              {latestStep && <span>· Step #{latestStep.seq}: {latestStep.title}</span>}
            </div>
            <time>{formatUnixTimestamp(task.updated_at_unix)}</time>
          </div>
        </button>
      </article>
    );
  }

  return (
    <section
      aria-label="Live task center stream"
      className="flex flex-col gap-4 min-w-0"
    >
      {runtime && !runtime.llm_configured && (
        <div
          id="task-runtime-banner"
          role="alert"
          data-testid="unconfigured-runtime-banner"
          className="app-card p-4 border-2 border-amber-500/70 bg-amber-50/85 dark:bg-amber-950/40 space-y-2"
        >
          <div className="flex flex-wrap items-center justify-between gap-2">
            <div className="flex items-center gap-2">
              <ProvenanceBadge provenance="unconfigured" />
              <h2 className="text-sm font-bold text-amber-950 dark:text-amber-100">
                AI runtime is unconfigured
              </h2>
            </div>
            <Link
              href="/integrations"
              className="app-button min-h-[44px] px-3 text-xs"
            >
              Configure provider key
            </Link>
          </div>
          <p className="text-xs text-amber-950 dark:text-amber-100">
            {runtime.reason ||
              "No verified AI model provider is configured for this business. Task execution fails closed and never fabricates local replies; existing persisted tasks remain readable."}
          </p>
        </div>
      )}

      <form
        onSubmit={handleSubmit}
        aria-label="Task composer"
        className="app-panel p-4 space-y-3"
      >
        <div className="flex flex-wrap items-center justify-between gap-2">
          <div>
            <h2 className="text-sm font-bold text-slate-900 dark:text-slate-100">
              New Governed Task
            </h2>
            <p className="text-xs text-slate-600 dark:text-slate-400">
              {selectedSpace
                ? `Scoped to Space: ${selectedSpace.name}`
                : "Dispatches via idempotent receipt with standing-authority checks"}
            </p>
          </div>
          {runtime && (
            <span className="app-badge tabular-nums">
              {runtime.llm_configured
                ? `${runtime.provider ?? "Provider"} · ${runtime.model ?? "Model"}`
                : "Runtime unconfigured"}
            </span>
          )}
        </div>

        <div>
          <label
            htmlFor={promptId}
            className="block text-xs font-semibold text-slate-800 dark:text-slate-200 mb-1"
          >
            Task prompt
          </label>
          <textarea
            id={promptId}
            aria-label="Task prompt"
            rows={3}
            maxLength={16000}
            value={composerPrompt}
            onChange={(event) => onComposerPromptChange(event.target.value)}
            placeholder="Describe what you want Omni Assistant to plan, draft, or prepare for approval..."
            className="glass-control w-full p-3 text-sm text-slate-900 dark:text-slate-100 focus-visible:outline-2 focus-visible:outline-[#2784ff] focus-visible:outline-offset-2"
          />
        </div>

        <div className="grid grid-cols-1 sm:grid-cols-3 gap-2.5">
          <div>
            <label
              htmlFor={deptId}
              className="block text-xs font-medium text-slate-700 dark:text-slate-300 mb-1"
            >
              Department
            </label>
            <select
              id={deptId}
              value={composerDepartment}
              onChange={(event) => onComposerDepartmentChange(event.target.value)}
              className="glass-control w-full px-2.5 min-h-[44px] text-xs"
            >
              <option value="operations">Operations</option>
              <option value="finance">Finance</option>
              <option value="sales">Sales</option>
              <option value="marketing">Marketing</option>
            </select>
          </div>

          <div>
            <label
              htmlFor={actionId}
              className="block text-xs font-medium text-slate-700 dark:text-slate-300 mb-1"
            >
              Action class
            </label>
            <select
              id={actionId}
              value={composerAction}
              onChange={(event) => onComposerActionChange(event.target.value)}
              className="glass-control w-full px-2.5 min-h-[44px] text-xs"
            >
              <option value="analyze">Analysis / Read-only</option>
              <option value="send_email">Send Email (approval gated)</option>
              <option value="create_invoice_request">
                Create Invoice Request (approval gated)
              </option>
              <option value="publish">Publish Externally (approval gated)</option>
            </select>
          </div>

          <div>
            <label
              htmlFor={costId}
              className="block text-xs font-medium text-slate-700 dark:text-slate-300 mb-1"
            >
              Estimated ceiling (µUSD)
            </label>
            <input
              id={costId}
              type="number"
              min={0}
              step={1000}
              value={composerEstimatedCostMicros}
              onChange={(event) => {
                const parsed = Number.parseInt(event.target.value || "0", 10);
                onComposerEstimatedCostChange(
                  Number.isSafeInteger(parsed) && parsed >= 0 ? parsed : 0,
                );
              }}
              className="glass-control w-full px-2.5 min-h-[44px] text-xs tabular-nums"
            />
          </div>
        </div>

        <div className="flex flex-wrap items-center justify-between gap-2 pt-1">
          <p className="text-xs text-slate-600 dark:text-slate-400 tabular-nums">
            {llmConfigured
              ? composerEstimatedCostMicros > 0
                ? `Declared ceiling: ${formatMicroUsd(composerEstimatedCostMicros)}`
                : "Protected by client UUID Idempotency-Key & server budget reservation."
              : "Execution is disabled while runtime is unconfigured."}
          </p>
          <button
            type="submit"
            disabled={!trimmedPrompt || submittingTask || !llmConfigured}
            aria-describedby={
              !llmConfigured ? "task-runtime-banner" : undefined
            }
            className="app-button primary min-h-[44px] px-4 text-xs focus-visible:outline-2 focus-visible:outline-[#2784ff] focus-visible:outline-offset-2 disabled:opacity-50"
          >
            {submittingTask ? "Submitting..." : "Run Task"}
          </button>
        </div>
      </form>

      <div className="app-panel p-3.5 space-y-3">
        <div className="flex flex-wrap items-center justify-between gap-2">
          <nav
            aria-label="Task Center view modes"
            className="flex flex-wrap gap-1.5"
          >
            {VIEW_MODES.map((mode) => (
              <button
                key={mode}
                type="button"
                aria-pressed={viewMode === mode}
                onClick={() => onViewModeChange(mode)}
                className={`min-h-[44px] rounded-lg px-3.5 text-xs font-semibold capitalize transition-colors motion-reduce:transition-none focus-visible:outline-2 focus-visible:outline-[#2784ff] focus-visible:outline-offset-2 ${
                  viewMode === mode
                    ? "bg-[#2784ff] text-white"
                    : "bg-slate-100/80 dark:bg-slate-800/80 text-slate-800 dark:text-slate-200 hover:bg-slate-200/80 dark:hover:bg-slate-700"
                }`}
              >
                {mode}
              </button>
            ))}
          </nav>

          <div className="flex flex-wrap items-center gap-2">
            <label htmlFor={filterId} className="sr-only">
              Filter tasks
            </label>
            <input
              id={filterId}
              type="search"
              value={searchQuery}
              onChange={(event) => setSearchQuery(event.target.value)}
              placeholder="Filter tasks..."
              className="glass-control px-3 min-h-[44px] text-xs w-44"
            />
            <select
              aria-label="Status filter"
              value={statusFilter}
              onChange={(event) =>
                setStatusFilter(event.target.value as StatusFilter)
              }
              className="glass-control px-2.5 min-h-[44px] text-xs"
            >
              <option value="all">All statuses</option>
              <option value="active">Active</option>
              <option value="awaiting_approval">Awaiting approval</option>
              <option value="completed">Completed</option>
              <option value="outcome_unknown">Outcome unknown</option>
              <option value="failed">Failed</option>
              <option value="cancelled">Cancelled</option>
            </select>
            <select
              aria-label="Sort tasks"
              value={sortOrder}
              onChange={(event) =>
                setSortOrder(event.target.value as SortOrder)
              }
              className="glass-control px-2.5 min-h-[44px] text-xs"
            >
              <option value="urgency">Sort: Urgency</option>
              <option value="recent">Sort: Most recent</option>
            </select>
          </div>
        </div>

        {viewMode === "overview" && (
          <div className="space-y-4">
            <div className="grid grid-cols-2 sm:grid-cols-4 gap-2.5">
              <div className="app-card p-3 border border-slate-200/70 dark:border-slate-700/70">
                <p className="app-metric-label">Awaiting Approval</p>
                <p className="mt-1 text-xl font-bold text-amber-800 dark:text-amber-300 tabular-nums">
                  {counts.pendingApprovals}
                </p>
              </div>
              <div className="app-card p-3 border border-slate-200/70 dark:border-slate-700/70">
                <p className="app-metric-label">Queued / Running</p>
                <p className="mt-1 text-xl font-bold text-[#2784ff] tabular-nums">
                  {counts.running}
                </p>
              </div>
              <div className="app-card p-3 border border-slate-200/70 dark:border-slate-700/70">
                <p className="app-metric-label">Outcome Unknown</p>
                <p className="mt-1 text-xl font-bold text-orange-800 dark:text-orange-300 tabular-nums">
                  {counts.unknown}
                </p>
              </div>
              <div className="app-card p-3 border border-slate-200/70 dark:border-slate-700/70">
                <p className="app-metric-label">Completed</p>
                <p className="mt-1 text-xl font-bold text-emerald-800 dark:text-emerald-300 tabular-nums">
                  {counts.completed}
                </p>
              </div>
            </div>

            {selectedSpace && pinnedSpaceArtifacts.length > 0 && (
              <section
                aria-label={`Pinned artifacts in ${selectedSpace.name}`}
                className="space-y-2"
              >
                <h3 className="text-xs font-bold uppercase tracking-wide text-slate-600 dark:text-slate-400">
                  Pinned to {selectedSpace.name} ({pinnedSpaceArtifacts.length})
                </h3>
                <div className="grid grid-cols-1 md:grid-cols-2 gap-2.5">
                  {pinnedSpaceArtifacts.map(({ artifact, taskTitle, taskId }) => (
                    <button
                      key={artifact.id}
                      type="button"
                      onClick={() => onSelectTask(taskId)}
                      className="app-card p-3 text-left border border-slate-200/80 dark:border-slate-700/80 space-y-1 hover:border-[#2784ff]"
                    >
                      <div className="flex items-center justify-between gap-2">
                        <span className="text-xs font-bold text-slate-900 dark:text-slate-100 truncate">
                          {artifact.title}
                        </span>
                        <ProvenanceBadge provenance={artifact.provenance} compact />
                      </div>
                      <p className="text-[11px] text-slate-600 dark:text-slate-400 truncate">
                        From: {taskTitle}
                      </p>
                      <p className="text-xs text-slate-700 dark:text-slate-300 line-clamp-2 whitespace-pre-wrap">
                        {artifact.content}
                      </p>
                    </button>
                  ))}
                </div>
              </section>
            )}

            <div className="space-y-2.5">
              {scopedTasks.length === 0 ? (
                <p className="app-empty">
                  No persisted tasks match this filter yet.
                </p>
              ) : (
                scopedTasks.map(renderTaskCard)
              )}
            </div>
          </div>
        )}

        {viewMode === "tasks" && (
          <div className="space-y-2.5">
            {scopedTasks.length === 0 ? (
              <p className="app-empty">
                No persisted tasks match this filter yet.
              </p>
            ) : (
              scopedTasks.map(renderTaskCard)
            )}
          </div>
        )}

        {viewMode === "board" && (
          <div className="grid grid-cols-1 md:grid-cols-2 gap-3">
            {(
              [
                {
                  title: "Queued & Running",
                  items: scopedTasks.filter(
                    (t) =>
                      t.status === "draft" ||
                      t.status === "queued" ||
                      t.status === "running" ||
                      t.status === "waiting_for_input",
                  ),
                },
                {
                  title: "Awaiting Approval",
                  items: scopedTasks.filter(
                    (t) => t.status === "awaiting_approval",
                  ),
                },
                {
                  title: "Completed",
                  items: scopedTasks.filter((t) => t.status === "completed"),
                },
                {
                  title: "Failed, Cancelled & Unknown",
                  items: scopedTasks.filter(
                    (t) =>
                      t.status === "failed" ||
                      t.status === "cancelled" ||
                      t.status === "outcome_unknown",
                  ),
                },
              ] as const
            ).map((column) => (
              <div
                key={column.title}
                className="rounded-xl border border-slate-200/80 dark:border-slate-700/80 bg-slate-50/70 dark:bg-slate-900/40 p-3 space-y-2.5"
              >
                <div className="flex items-center justify-between gap-2">
                  <h3 className="text-xs font-bold uppercase tracking-wide text-slate-700 dark:text-slate-300">
                    {column.title}
                  </h3>
                  <span className="app-badge tabular-nums">
                    {column.items.length}
                  </span>
                </div>
                {column.items.length === 0 ? (
                  <p className="text-xs text-slate-500 dark:text-slate-400 py-4 text-center">
                    Empty
                  </p>
                ) : (
                  <div className="space-y-2">
                    {column.items.map(renderTaskCard)}
                  </div>
                )}
              </div>
            ))}
          </div>
        )}

        {viewMode === "timeline" && (
          <div className="space-y-2">
            {timelineEntries.length === 0 ? (
              <p className="app-empty">
                No execution step events recorded for the selected scope.
              </p>
            ) : (
              <ol className="space-y-2">
                {timelineEntries.map(({ task, step }) => (
                  <li
                    key={`${task.id}-${step.id}`}
                    className="app-card p-3 border border-slate-200/80 dark:border-slate-700/80 space-y-1"
                  >
                    <div className="flex flex-wrap items-center justify-between gap-2">
                      <button
                        type="button"
                        onClick={() => onSelectTask(task.id)}
                        className="text-xs font-bold text-slate-900 dark:text-slate-100 hover:underline text-left"
                      >
                        {task.title} · Step #{step.seq}: {step.title}
                      </button>
                      <ProvenanceBadge provenance={step.provenance} compact />
                    </div>
                    {step.detail && (
                      <p className="text-xs text-slate-700 dark:text-slate-300">
                        {step.detail}
                      </p>
                    )}
                    <div className="flex flex-wrap items-center gap-2 text-[11px] text-slate-600 dark:text-slate-400 tabular-nums">
                      <span>{step.kind}</span>
                      {step.tool_name && <span>tool: {step.tool_name}</span>}
                      <span>{formatMicroUsd(step.cost_micros)}</span>
                      <time>{formatUnixTimestamp(step.created_at_unix)}</time>
                    </div>
                  </li>
                ))}
              </ol>
            )}
          </div>
        )}
      </div>
    </section>
  );
}
