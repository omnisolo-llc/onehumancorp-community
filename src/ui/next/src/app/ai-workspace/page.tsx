"use client";

import React, { useEffect, useMemo, useState } from "react";
import { AppShell } from "../components/AppShell";
import {
  formatMicroUsd,
  formatPayerMode,
  type ApproveTaskRequest,
  type CreateSpaceRequest,
  type CreateTaskRequest,
} from "@/lib/taskCenterContract";
import { useTaskCenter } from "./useTaskCenter";
import { SpacesRail } from "./components/SpacesRail";
import {
  TaskStreamPanel,
  type TaskViewMode,
} from "./components/TaskStreamPanel";
import {
  InspectorDrawer,
  type InspectorTab,
} from "./components/InspectorDrawer";
import { ProvenanceBadge } from "./components/ProvenanceBadge";

const DEFAULT_FOCUS_SECONDS = 25 * 60;

function formatClock(totalSeconds: number): string {
  const minutes = Math.floor(totalSeconds / 60);
  const seconds = totalSeconds % 60;
  return `${String(minutes).padStart(2, "0")}:${String(seconds).padStart(2, "0")}`;
}

export default function AIWorkspacePage() {
  const taskCenter = useTaskCenter();

  const [selectedSpaceId, setSelectedSpaceId] = useState<string | null>(null);
  const [selectedTaskId, setSelectedTaskId] = useState<string | null>(null);
  const [attentionOnly, setAttentionOnly] = useState(false);
  const [viewMode, setViewMode] = useState<TaskViewMode>("overview");
  const [inspectorTab, setInspectorTab] = useState<InspectorTab>("now");

  // Task composer state
  const [composerPrompt, setComposerPrompt] = useState("");
  const [composerDepartment, setComposerDepartment] = useState("operations");
  const [composerAction, setComposerAction] = useState("analyze");
  const [composerEstimatedCostMicros, setComposerEstimatedCostMicros] =
    useState(0);

  // Client-only Pomodoro stopwatch utility (never creates or alters server tasks)
  const [focusSeconds, setFocusSeconds] = useState(DEFAULT_FOCUS_SECONDS);
  const [focusRunning, setFocusRunning] = useState(false);

  useEffect(() => {
    if (!focusRunning) return;
    const interval = window.setInterval(() => {
      setFocusSeconds((prev) => {
        if (prev <= 1) {
          setFocusRunning(false);
          return 0;
        }
        return prev - 1;
      });
    }, 1000);
    return () => window.clearInterval(interval);
  }, [focusRunning]);

  const spaces = useMemo(
    () => taskCenter.summary?.spaces ?? [],
    [taskCenter.summary],
  );
  const tasks = useMemo(
    () => taskCenter.summary?.tasks ?? [],
    [taskCenter.summary],
  );
  const standingPolicies = useMemo(
    () => taskCenter.summary?.standing_policies ?? [],
    [taskCenter.summary],
  );
  const runtime = taskCenter.summary?.runtime ?? null;
  const budget = taskCenter.summary?.budget ?? null;

  const selectedTask = useMemo(() => {
    if (selectedTaskId) {
      const found = tasks.find((item) => item.id === selectedTaskId);
      if (found) return found;
    }
    const awaiting = tasks.find(
      (item) => !item.archived && item.pending_approval?.status === "pending",
    );
    return awaiting ?? tasks.find((item) => !item.archived) ?? null;
  }, [selectedTaskId, tasks]);

  const verifiedConnectorCount = useMemo(
    () =>
      taskCenter.catalog?.connectors.filter((item) => item.usable).length ?? 0,
    [taskCenter.catalog],
  );

  const totalConnectorCount = taskCenter.catalog?.connectors.length ?? 0;

  const streamTone =
    taskCenter.streamState === "live"
      ? "good"
      : taskCenter.streamState === "reconnecting"
        ? "warn"
        : "neutral";

  const runtimeTone = !runtime
    ? "neutral"
    : runtime.llm_configured
      ? "good"
      : "warn";

  const statusItems = [
    {
      label: "Runtime",
      value: !runtime
        ? taskCenter.loading
          ? "Checking…"
          : "Unavailable"
        : runtime.llm_configured
          ? `${runtime.provider ?? "Configured"}`
          : "Unconfigured",
      tone: runtimeTone as "good" | "warn" | "neutral",
    },
    {
      label: "Stream",
      value:
        taskCenter.streamState === "live"
          ? "Live SSE"
          : taskCenter.streamState === "reconnecting"
            ? "Reconnecting"
            : taskCenter.streamState === "connecting"
              ? "Connecting"
              : "Offline",
      tone: streamTone as "good" | "warn" | "neutral",
    },
    {
      label: "Connectors",
      value: taskCenter.catalog
        ? `${verifiedConnectorCount}/${totalConnectorCount} verified`
        : "—",
      tone: (verifiedConnectorCount > 0 ? "good" : "neutral") as
        | "good"
        | "neutral",
    },
  ];

  async function handleSubmitTask(input: CreateTaskRequest): Promise<boolean> {
    const created = await taskCenter.submitTask(input);
    if (created) {
      setSelectedTaskId(created.id);
      if (created.pending_approval?.status === "pending") {
        setInspectorTab("now");
      }
      return true;
    }
    return false;
  }

  async function handleCreateSpace(
    input: CreateSpaceRequest,
  ): Promise<boolean> {
    const created = await taskCenter.createSpace(input);
    if (created) {
      setSelectedSpaceId(created.id);
      return true;
    }
    return false;
  }

  async function handleDecideApproval(
    taskId: string,
    decision: ApproveTaskRequest,
  ): Promise<void> {
    await taskCenter.decideApproval(taskId, decision);
  }

  function handleSelectTask(taskId: string) {
    setSelectedTaskId(taskId);
    taskCenter.clearApprovalConflict();
    const target = tasks.find((item) => item.id === taskId);
    if (target?.pending_approval?.status === "pending") {
      setInspectorTab("now");
    }
  }

  function handleApplyTemplate(
    template: string,
    spaceId: string,
    department: string,
  ) {
    setSelectedSpaceId(spaceId);
    setComposerPrompt(template);
    setComposerDepartment(department);
  }

  const focusModified = focusRunning || focusSeconds !== DEFAULT_FOCUS_SECONDS;

  return (
    <AppShell
      title="Omni Assistant"
      subtitle="AI Task Center for governed Spaces, live execution receipts, owner approvals, and verified tools."
      statusItems={statusItems}
      actions={[{ label: "Integrations", href: "/integrations" }]}
    >
      <div className="flex flex-col gap-4 pb-8 min-w-0">
        {/* Top Budget, Payer & Focus Utility Strip */}
        <header
          aria-label="Task Center status bar"
          className="app-card p-3.5 flex flex-wrap items-center justify-between gap-3 border border-slate-200/80 dark:border-slate-700/80"
        >
          <div className="flex flex-wrap items-center gap-3 text-xs tabular-nums">
            <div className="flex items-center gap-2">
              <span className="font-semibold text-slate-600 dark:text-slate-400">
                Payer:
              </span>
              <span className="font-bold text-slate-900 dark:text-slate-100">
                {runtime ? formatPayerMode(runtime.payer_mode) : "Unavailable"}
              </span>
            </div>

            <span aria-hidden="true" className="text-slate-300 dark:text-slate-600">
              |
            </span>

            <div className="flex items-center gap-2">
              <span className="font-semibold text-slate-600 dark:text-slate-400">
                Budget:
              </span>
              <span
                data-testid="budget-summary-strip"
                className="font-bold text-slate-900 dark:text-slate-100"
              >
                {budget
                  ? `${formatMicroUsd(budget.spent_micros)} spent · ${formatMicroUsd(budget.reserved_micros)} reserved / ${formatMicroUsd(budget.limit_micros)} limit`
                  : "Unavailable"}
              </span>
            </div>

            {runtime && !runtime.llm_configured && (
              <ProvenanceBadge provenance="unconfigured" compact />
            )}
          </div>

          <div className="flex flex-wrap items-center gap-2">
            {/* Local-only focus timer utility (zero server mutations) */}
            <div
              aria-label="Local focus timer"
              className="flex items-center gap-1.5 rounded-lg border border-slate-200/80 dark:border-slate-700/80 bg-white/70 dark:bg-slate-900/60 px-2.5 py-1"
            >
              <span className="text-xs font-mono font-bold text-slate-800 dark:text-slate-100 tabular-nums px-1">
                {formatClock(focusSeconds)}
              </span>
              <button
                type="button"
                onClick={() => setFocusRunning((prev) => !prev)}
                className="glass-control min-h-[36px] px-2.5 text-xs font-semibold"
              >
                {focusRunning ? "Pause Focus" : "Start Focus"}
              </button>
              <button
                type="button"
                disabled={!focusModified}
                onClick={() => {
                  setFocusRunning(false);
                  setFocusSeconds(DEFAULT_FOCUS_SECONDS);
                }}
                className="glass-control min-h-[36px] px-2.5 text-xs font-medium disabled:opacity-45"
              >
                Reset
              </button>
            </div>

            <button
              type="button"
              disabled={taskCenter.refreshing}
              onClick={() => void taskCenter.reload()}
              className="app-button min-h-[44px] px-3 text-xs disabled:opacity-50"
            >
              {taskCenter.refreshing ? "Refreshing…" : "Refresh"}
            </button>
          </div>
        </header>

        {taskCenter.error && (
          <div
            role="alert"
            data-testid="task-center-error"
            className="app-card p-3.5 border border-rose-400 bg-rose-50/90 dark:bg-rose-950/50 text-xs font-medium text-rose-900 dark:text-rose-100"
          >
            {taskCenter.error}
          </div>
        )}

        {taskCenter.notice && (
          <div
            role="status"
            aria-live="polite"
            data-testid="task-center-notice"
            className="app-card p-3 border border-blue-300 dark:border-blue-700 bg-blue-50/80 dark:bg-blue-950/40 text-xs font-medium text-blue-950 dark:text-blue-100"
          >
            {taskCenter.notice}
          </div>
        )}

        {/* 3-Pane Governed AI Task Center */}
        <div
          data-testid="omni-assistant-task-center-grid"
          className="grid grid-cols-1 xl:grid-cols-[272px_minmax(0,1fr)_368px] gap-4 items-start min-w-0"
        >
          <SpacesRail
            spaces={spaces}
            tasks={tasks}
            selectedSpaceId={selectedSpaceId}
            attentionOnly={attentionOnly}
            creatingSpace={taskCenter.creatingSpace}
            onSelectSpace={(spaceId) => {
              setSelectedSpaceId(spaceId);
              if (spaceId === null) {
                setAttentionOnly(false);
              }
            }}
            onToggleAttentionOnly={() => setAttentionOnly((prev) => !prev)}
            onCreateSpace={handleCreateSpace}
            onArchiveSpace={async (id, archived) => {
              await taskCenter.archiveSpace(id, archived);
            }}
            onApplyTemplate={handleApplyTemplate}
          />

          <TaskStreamPanel
            tasks={tasks}
            spaces={spaces}
            selectedSpaceId={selectedSpaceId}
            selectedTaskId={selectedTask?.id ?? null}
            attentionOnly={attentionOnly}
            runtime={runtime}
            viewMode={viewMode}
            submittingTask={taskCenter.submittingTask}
            composerPrompt={composerPrompt}
            composerDepartment={composerDepartment}
            composerAction={composerAction}
            composerEstimatedCostMicros={composerEstimatedCostMicros}
            onComposerPromptChange={setComposerPrompt}
            onComposerDepartmentChange={setComposerDepartment}
            onComposerActionChange={setComposerAction}
            onComposerEstimatedCostChange={setComposerEstimatedCostMicros}
            onViewModeChange={setViewMode}
            onSelectTask={handleSelectTask}
            onSubmitTask={handleSubmitTask}
          />

          <InspectorDrawer
            task={selectedTask}
            spaces={spaces}
            runtime={runtime}
            budget={budget}
            standingPolicies={standingPolicies}
            catalog={taskCenter.catalog}
            activeTab={inspectorTab}
            busyTask={
              selectedTask ? Boolean(taskCenter.busyTaskIds[selectedTask.id]) : false
            }
            approvalConflict={taskCenter.approvalConflict}
            onTabChange={setInspectorTab}
            onDecideApproval={handleDecideApproval}
            onCancelTask={async (id) => {
              await taskCenter.cancelTask(id);
            }}
            onSendFollowUp={taskCenter.sendFollowUp}
            onTogglePinArtifact={async (taskId, artifactId, spaceId, pinned) => {
              await taskCenter.togglePinArtifact(
                taskId,
                artifactId,
                spaceId,
                pinned,
              );
            }}
            onUpdatePolicy={async (dept, policy) => {
              await taskCenter.updateStandingPolicy(dept, policy);
            }}
          />
        </div>
      </div>
    </AppShell>
  );
}
