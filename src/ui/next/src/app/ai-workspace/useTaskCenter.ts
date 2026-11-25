"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import {
  TASK_CENTER_SSE_EVENTS,
  generateClientUuid,
  parseConnectorCatalogResponse,
  parseSpaceRecord,
  parseStandingPolicyRecord,
  parseTaskApprovalPayload,
  parseTaskArtifactRecord,
  parseTaskCenterStreamEvent,
  parseTaskCenterSummaryResponse,
  parseTaskCenterTaskEnvelope,
  parseTaskStepRecord,
  type ApproveTaskRequest,
  type ConnectorCatalogResponse,
  type CreateSpaceRequest,
  type CreateTaskRequest,
  type SpaceRecord,
  type StandingPolicyRecord,
  type TaskArtifactRecord,
  type TaskCenterSummaryResponse,
  type TaskCenterTask,
  type TaskStepRecord,
} from "@/lib/taskCenterContract";

export type StreamConnectionState = "connecting" | "live" | "reconnecting" | "offline";

export interface UseTaskCenterResult {
  summary: TaskCenterSummaryResponse | null;
  catalog: ConnectorCatalogResponse | null;
  loading: boolean;
  refreshing: boolean;
  error: string | null;
  notice: string | null;
  approvalConflict: string | null;
  streamState: StreamConnectionState;
  submittingTask: boolean;
  creatingSpace: boolean;
  busyTaskIds: Record<string, boolean>;
  reload: () => Promise<void>;
  submitTask: (input: CreateTaskRequest) => Promise<TaskCenterTask | null>;
  createSpace: (input: CreateSpaceRequest) => Promise<SpaceRecord | null>;
  archiveSpace: (spaceId: string, archived: boolean) => Promise<boolean>;
  decideApproval: (taskId: string, decision: ApproveTaskRequest) => Promise<boolean>;
  cancelTask: (taskId: string) => Promise<boolean>;
  sendFollowUp: (taskId: string, prompt: string) => Promise<boolean>;
  togglePinArtifact: (
    taskId: string,
    artifactId: string,
    spaceId: string,
    pinned: boolean,
  ) => Promise<boolean>;
  updateStandingPolicy: (
    department: string,
    policy: {
      auto_approve_max_micros: number;
      allow_external_send: boolean;
      allow_financial_commit: boolean;
    },
  ) => Promise<boolean>;
  clearApprovalConflict: () => void;
}

const MAX_BACKOFF_MS = 15_000;
const BASE_BACKOFF_MS = 1_000;

function extractErrorMessage(payload: unknown, fallback: string): string {
  if (payload && typeof payload === "object" && !Array.isArray(payload)) {
    const rec = payload as Record<string, unknown>;
    for (const key of ["error", "message", "reason"] as const) {
      if (typeof rec[key] === "string" && rec[key].trim()) return rec[key];
    }
  }
  return fallback;
}

function upsertById<T extends { id: string }>(items: T[], incoming: T): T[] {
  const index = items.findIndex((item) => item.id === incoming.id);
  if (index === -1) return [incoming, ...items];
  const next = [...items];
  next[index] = incoming;
  return next;
}

function upsertStep(steps: TaskStepRecord[], incoming: TaskStepRecord): TaskStepRecord[] {
  return [...steps.filter((s) => s.id !== incoming.id), incoming].sort((a, b) => a.seq - b.seq);
}

function upsertArtifact(items: TaskArtifactRecord[], incoming: TaskArtifactRecord): TaskArtifactRecord[] {
  return [...items.filter((a) => a.id !== incoming.id), incoming];
}

function parseSpaceEnvelope(v: unknown): SpaceRecord | null {
  return parseSpaceRecord(v) ?? (v && typeof v === "object" && "space" in v ? parseSpaceRecord((v as Record<string, unknown>).space) : null);
}

function parsePolicyEnvelope(v: unknown): StandingPolicyRecord | null {
  return parseStandingPolicyRecord(v) ?? (v && typeof v === "object" && "policy" in v ? parseStandingPolicyRecord((v as Record<string, unknown>).policy) : null);
}

async function fetchJson(
  url: string,
  init: RequestInit,
): Promise<{ ok: boolean; status: number; body: unknown }> {
  const response = await fetch(url, init);
  const body: unknown = await response.json().catch(() => null);
  return { ok: response.ok, status: response.status, body };
}

export function useTaskCenter(): UseTaskCenterResult {
  const [summary, setSummary] = useState<TaskCenterSummaryResponse | null>(null);
  const [catalog, setCatalog] = useState<ConnectorCatalogResponse | null>(null);
  const [loading, setLoading] = useState(true);
  const [refreshing, setRefreshing] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [approvalConflict, setApprovalConflict] = useState<string | null>(null);
  const [streamState, setStreamState] = useState<StreamConnectionState>("connecting");
  const [submittingTask, setSubmittingTask] = useState(false);
  const [creatingSpace, setCreatingSpace] = useState(false);
  const [busyTaskIds, setBusyTaskIds] = useState<Record<string, boolean>>({});

  const mountedRef = useRef(true);
  const generationRef = useRef(0);
  const tenantIdRef = useRef<string | null>(null);
  const pendingDraftRef = useRef<{ fingerprint: string; key: string } | null>(null);

  const setTaskBusy = useCallback((taskId: string, busy: boolean) => {
    setBusyTaskIds((prev) => {
      if (busy) return { ...prev, [taskId]: true };
      const next = { ...prev };
      delete next[taskId];
      return next;
    });
  }, []);

  const loadAll = useCallback(async (isInitial = false) => {
    const token = ++generationRef.current;
    if (isInitial) setLoading(true);
    else setRefreshing(true);

    try {
      const [summaryRes, catalogRes] = await Promise.all([
        fetch("/api/v1/task-center/summary", { cache: "no-store" }),
        fetch("/api/v1/integrations/catalog", { cache: "no-store" }).catch(() => null),
      ]);
      if (!mountedRef.current || generationRef.current !== token) return;

      const summaryBody: unknown = await summaryRes.json().catch(() => null);
      if (!mountedRef.current || generationRef.current !== token) return;

      if (!summaryRes.ok) {
        setSummary(null);
        setError(
          extractErrorMessage(
            summaryBody,
            summaryRes.status === 401
              ? "Authentication required to access the AI Task Center."
              : "AI Task Center summary is unavailable. No tasks were simulated.",
          ),
        );
      } else {
        const parsed = parseTaskCenterSummaryResponse(summaryBody);
        setSummary(parsed);
        setError(parsed ? null : "Received an unexpected Task Center response format from the server.");
      }

      if (catalogRes?.ok) {
        const catalogBody: unknown = await catalogRes.json().catch(() => null);
        if (mountedRef.current && generationRef.current === token) {
          setCatalog(parseConnectorCatalogResponse(catalogBody));
        }
      } else {
        setCatalog(null);
      }
    } catch {
      if (!mountedRef.current || generationRef.current !== token) return;
      setSummary(null);
      setError("Unable to reach the AI Task Center backend. No local execution state has been fabricated.");
    } finally {
      if (mountedRef.current && generationRef.current === token) {
        setLoading(false);
        setRefreshing(false);
      }
    }
  }, []);

  const reload = useCallback(async () => {
    await loadAll(false);
  }, [loadAll]);

  useEffect(() => {
    mountedRef.current = true;
    void loadAll(true);
    return () => {
      mountedRef.current = false;
      generationRef.current += 1;
    };
  }, [loadAll]);

  useEffect(() => {
    if (typeof EventSource === "undefined") {
      setStreamState("offline");
      return;
    }

    let source: EventSource | null = null;
    let reconnectTimer: ReturnType<typeof setTimeout> | null = null;
    let attempt = 0;
    let disposed = false;

    const applyStreamEvent = (rawData: string) => {
      if (disposed || !mountedRef.current) return;
      let parsedJson: unknown;
      try {
        parsedJson = JSON.parse(rawData);
      } catch {
        return;
      }
      const event = parseTaskCenterStreamEvent(parsedJson);
      if (!event) return;

      if (tenantIdRef.current !== null && event.tenant_id && tenantIdRef.current !== event.tenant_id) {
        tenantIdRef.current = event.tenant_id;
        setSummary(null);
        void loadAll(false);
        return;
      }
      if (event.tenant_id) tenantIdRef.current = event.tenant_id;

      if (event.kind === "resync_required") {
        setNotice("Live stream requested a resync; refreshing server state.");
        void loadAll(false);
        return;
      }

      if (event.kind === "task_updated") {
        const task = parseTaskCenterTaskEnvelope(event.payload);
        if (!task) { void loadAll(false); return; }
        setSummary((prev) => (prev ? { ...prev, tasks: upsertById(prev.tasks, task) } : prev));
        return;
      }

      if (event.kind === "space_updated") {
        const space = parseSpaceEnvelope(event.payload);
        if (!space) { void loadAll(false); return; }
        setSummary((prev) => (prev ? { ...prev, spaces: upsertById(prev.spaces, space) } : prev));
        return;
      }

      if (event.kind === "step_appended") {
        const step = parseTaskStepRecord(event.payload) ?? parseTaskStepRecord(event.payload.step);
        if (!step) { void loadAll(false); return; }
        setSummary((prev) => {
          if (!prev || !prev.tasks.some((item) => item.id === step.task_id)) {
            void loadAll(false);
            return prev;
          }
          return {
            ...prev,
            tasks: prev.tasks.map((item) =>
              item.id === step.task_id
                ? { ...item, steps: upsertStep(item.steps, step), updated_at_unix: Math.max(item.updated_at_unix, step.created_at_unix) }
                : item,
            ),
          };
        });
        return;
      }

      if (event.kind === "artifact_created") {
        const artifact = parseTaskArtifactRecord(event.payload) ?? parseTaskArtifactRecord(event.payload.artifact);
        if (!artifact) { void loadAll(false); return; }
        setSummary((prev) => {
          if (!prev || !prev.tasks.some((item) => item.id === artifact.task_id)) {
            void loadAll(false);
            return prev;
          }
          return {
            ...prev,
            tasks: prev.tasks.map((item) =>
              item.id === artifact.task_id
                ? { ...item, artifacts: upsertArtifact(item.artifacts, artifact) }
                : item,
            ),
          };
        });
        return;
      }

      if (event.kind === "approval_required") {
        const fullTask = parseTaskCenterTaskEnvelope(event.payload);
        if (fullTask) {
          setSummary((prev) => (prev ? { ...prev, tasks: upsertById(prev.tasks, fullTask) } : prev));
          return;
        }
        const taskId = typeof event.payload.task_id === "string" ? event.payload.task_id : null;
        const approval =
          parseTaskApprovalPayload(event.payload.approval) ??
          parseTaskApprovalPayload(event.payload.pending_approval) ??
          parseTaskApprovalPayload(event.payload);
        if (!taskId || !approval) { void loadAll(false); return; }
        setSummary((prev) => {
          if (!prev || !prev.tasks.some((item) => item.id === taskId)) {
            void loadAll(false);
            return prev;
          }
          return {
            ...prev,
            tasks: prev.tasks.map((item) =>
              item.id === taskId
                ? {
                    ...item,
                    status: "awaiting_approval",
                    provenance: "awaiting_approval",
                    attention: "permission_required",
                    pending_approval: approval,
                    updated_at_unix: Math.max(item.updated_at_unix, event.occurred_at_unix),
                  }
                : item,
            ),
          };
        });
      }
    };

    const connect = () => {
      if (disposed) return;
      setStreamState(attempt === 0 ? "connecting" : "reconnecting");
      const nextSource = new EventSource("/api/v1/task-center/stream");
      source = nextSource;

      nextSource.onopen = () => {
        if (disposed || !mountedRef.current) return;
        attempt = 0;
        setStreamState("live");
      };

      const onNamedEvent = (ev: Event) => {
        const msg = ev as MessageEvent<string>;
        if (typeof msg.data === "string") applyStreamEvent(msg.data);
      };
      for (const name of TASK_CENTER_SSE_EVENTS) {
        nextSource.addEventListener(name, onNamedEvent);
      }
      nextSource.onmessage = (msg) => {
        if (typeof msg.data === "string") applyStreamEvent(msg.data);
      };
      nextSource.onerror = () => {
        if (disposed || !mountedRef.current) return;
        nextSource.close();
        if (source === nextSource) source = null;
        setStreamState("reconnecting");
        const delay = Math.min(MAX_BACKOFF_MS, BASE_BACKOFF_MS * Math.pow(2, attempt));
        attempt += 1;
        reconnectTimer = setTimeout(connect, delay);
      };
    };

    connect();
    return () => {
      disposed = true;
      if (reconnectTimer !== null) clearTimeout(reconnectTimer);
      source?.close();
      source = null;
    };
  }, [loadAll]);

  const submitTask = useCallback(
    async (input: CreateTaskRequest): Promise<TaskCenterTask | null> => {
      if (submittingTask) return null;
      const fingerprint = JSON.stringify(input);
      const idempotencyKey =
        pendingDraftRef.current?.fingerprint === fingerprint
          ? pendingDraftRef.current.key
          : generateClientUuid();
      pendingDraftRef.current = { fingerprint, key: idempotencyKey };
      setSubmittingTask(true);
      setError(null);
      setNotice(null);

      try {
        const res = await fetchJson("/api/v1/task-center/tasks", {
          method: "POST",
          headers: { "Content-Type": "application/json", "Idempotency-Key": idempotencyKey },
          body: JSON.stringify(input),
        });
        if (!res.ok) {
          if (res.status === 503 || res.status === 409 || res.status < 500) {
            pendingDraftRef.current = null;
          }
          setError(
            extractErrorMessage(
              res.body,
              res.status === 503
                ? "AI runtime is unconfigured. No task was executed."
                : res.status === 409
                  ? "Task rejected due to budget or policy conflict."
                  : "Task submission was not confirmed by the server.",
            ),
          );
          if (res.status === 409 || res.status === 503) void loadAll(false);
          return null;
        }
        const task = parseTaskCenterTaskEnvelope(res.body);
        if (!task) {
          setError("Server returned an invalid task receipt. Refreshing state to verify.");
          void loadAll(false);
          return null;
        }
        pendingDraftRef.current = null;
        setSummary((prev) => (prev ? { ...prev, tasks: upsertById(prev.tasks, task) } : prev));
        setNotice(
          task.status === "awaiting_approval"
            ? "Task queued for owner approval before execution."
            : `Task accepted with status: ${task.status}.`,
        );
        return task;
      } catch {
        setError("Network error while submitting task. Retry will reuse the same Idempotency-Key to avoid duplicates.");
        return null;
      } finally {
        if (mountedRef.current) setSubmittingTask(false);
      }
    },
    [loadAll, submittingTask],
  );

  const createSpace = useCallback(
    async (input: CreateSpaceRequest): Promise<SpaceRecord | null> => {
      if (creatingSpace) return null;
      setCreatingSpace(true);
      setError(null);
      setNotice(null);
      try {
        const res = await fetchJson("/api/v1/task-center/spaces", {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify(input),
        });
        if (!res.ok) {
          setError(extractErrorMessage(res.body, "Unable to create Space."));
          return null;
        }
        const space = parseSpaceEnvelope(res.body);
        if (!space) { void loadAll(false); return null; }
        setSummary((prev) => (prev ? { ...prev, spaces: upsertById(prev.spaces, space) } : prev));
        setNotice(`Space "${space.name}" created.`);
        return space;
      } catch {
        setError("Network error while creating Space.");
        return null;
      } finally {
        if (mountedRef.current) setCreatingSpace(false);
      }
    },
    [creatingSpace, loadAll],
  );

  const archiveSpace = useCallback(
    async (spaceId: string, archived: boolean): Promise<boolean> => {
      setError(null);
      setNotice(null);
      try {
        const res = await fetchJson(`/api/v1/task-center/spaces/${encodeURIComponent(spaceId)}`, {
          method: "PATCH",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ action: archived ? "archive" : "unarchive" }),
        });
        if (!res.ok) {
          setError(extractErrorMessage(res.body, "Unable to update Space state."));
          return false;
        }
        const space = parseSpaceEnvelope(res.body);
        if (space) setSummary((prev) => (prev ? { ...prev, spaces: upsertById(prev.spaces, space) } : prev));
        else void loadAll(false);
        setNotice(archived ? "Space archived." : "Space restored.");
        return true;
      } catch {
        setError("Network error while updating Space.");
        return false;
      }
    },
    [loadAll],
  );

  const decideApproval = useCallback(
    async (taskId: string, decision: ApproveTaskRequest): Promise<boolean> => {
      setTaskBusy(taskId, true);
      setApprovalConflict(null);
      setError(null);
      setNotice(null);
      try {
        const res = await fetchJson(
          `/api/v1/task-center/tasks/${encodeURIComponent(taskId)}/approve`,
          {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify(decision),
          },
        );
        if (res.status === 409) {
          setApprovalConflict(
            extractErrorMessage(
              res.body,
              "Approval payload changed or was already decided (409 Conflict). Reloading latest task state.",
            ),
          );
          void loadAll(false);
          return false;
        }
        if (!res.ok) {
          setError(extractErrorMessage(res.body, "Approval decision could not be recorded."));
          return false;
        }
        const task = parseTaskCenterTaskEnvelope(res.body);
        if (task) setSummary((prev) => (prev ? { ...prev, tasks: upsertById(prev.tasks, task) } : prev));
        else void loadAll(false);
        setNotice(decision.approved ? "Approval confirmed and dispatched." : "Proposed action rejected.");
        return true;
      } catch {
        setError("Network error while recording approval decision.");
        return false;
      } finally {
        if (mountedRef.current) setTaskBusy(taskId, false);
      }
    },
    [loadAll, setTaskBusy],
  );

  const cancelTask = useCallback(
    async (taskId: string): Promise<boolean> => {
      setTaskBusy(taskId, true);
      setError(null);
      setNotice(null);
      try {
        const res = await fetchJson(
          `/api/v1/task-center/tasks/${encodeURIComponent(taskId)}/cancel`,
          { method: "POST", headers: { "Content-Type": "application/json" }, body: "{}" },
        );
        if (!res.ok) {
          setError(extractErrorMessage(res.body, "Unable to cancel task."));
          return false;
        }
        const task = parseTaskCenterTaskEnvelope(res.body);
        if (task) {
          setSummary((prev) => (prev ? { ...prev, tasks: upsertById(prev.tasks, task) } : prev));
          setNotice(
            task.status === "outcome_unknown"
              ? "Task had already reached an external provider; status is outcome_unknown pending reconciliation."
              : "Task cancelled.",
          );
        } else {
          void loadAll(false);
        }
        return true;
      } catch {
        setError("Network error while cancelling task.");
        return false;
      } finally {
        if (mountedRef.current) setTaskBusy(taskId, false);
      }
    },
    [loadAll, setTaskBusy],
  );

  const sendFollowUp = useCallback(
    async (taskId: string, prompt: string): Promise<boolean> => {
      setTaskBusy(taskId, true);
      setError(null);
      setNotice(null);
      try {
        const res = await fetchJson(
          `/api/v1/task-center/tasks/${encodeURIComponent(taskId)}/messages`,
          { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ prompt }) },
        );
        if (!res.ok) {
          setError(extractErrorMessage(res.body, "Follow-up turn could not be executed."));
          return false;
        }
        const task = parseTaskCenterTaskEnvelope(res.body);
        if (task) setSummary((prev) => (prev ? { ...prev, tasks: upsertById(prev.tasks, task) } : prev));
        else void loadAll(false);
        setNotice("Follow-up turn recorded.");
        return true;
      } catch {
        setError("Network error while sending follow-up instruction.");
        return false;
      } finally {
        if (mountedRef.current) setTaskBusy(taskId, false);
      }
    },
    [loadAll, setTaskBusy],
  );

  const togglePinArtifact = useCallback(
    async (taskId: string, artifactId: string, spaceId: string, pinned: boolean): Promise<boolean> => {
      setTaskBusy(taskId, true);
      setError(null);
      setNotice(null);
      try {
        const res = await fetchJson(
          `/api/v1/task-center/tasks/${encodeURIComponent(taskId)}/artifacts/${encodeURIComponent(artifactId)}/pin`,
          {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ space_id: spaceId, pinned }),
          },
        );
        if (!res.ok) {
          setError(extractErrorMessage(res.body, "Unable to update pinned artifact."));
          return false;
        }
        void loadAll(false);
        setNotice(pinned ? "Artifact pinned to Space." : "Artifact unpinned.");
        return true;
      } catch {
        setError("Network error while updating pinned artifact.");
        return false;
      } finally {
        if (mountedRef.current) setTaskBusy(taskId, false);
      }
    },
    [loadAll, setTaskBusy],
  );

  const updateStandingPolicy = useCallback(
    async (
      department: string,
      policy: { auto_approve_max_micros: number; allow_external_send: boolean; allow_financial_commit: boolean },
    ): Promise<boolean> => {
      setError(null);
      setNotice(null);
      try {
        const res = await fetchJson(
          `/api/v1/task-center/policies/${encodeURIComponent(department)}`,
          { method: "PUT", headers: { "Content-Type": "application/json" }, body: JSON.stringify(policy) },
        );
        if (!res.ok) {
          setError(extractErrorMessage(res.body, "Unable to update standing authority policy."));
          return false;
        }
        const saved = parsePolicyEnvelope(res.body);
        if (saved) {
          setSummary((prev) => {
            if (!prev) return prev;
            return {
              ...prev,
              standing_policies: [
                ...prev.standing_policies.filter((p) => p.department !== saved.department),
                saved,
              ],
            };
          });
        } else {
          void loadAll(false);
        }
        setNotice(`Standing policy updated for ${department}.`);
        return true;
      } catch {
        setError("Network error while saving standing authority policy.");
        return false;
      }
    },
    [loadAll],
  );

  return {
    summary, catalog, loading, refreshing, error, notice, approvalConflict,
    streamState, submittingTask, creatingSpace, busyTaskIds,
    reload, submitTask, createSpace, archiveSpace, decideApproval,
    cancelTask, sendFollowUp, togglePinArtifact, updateStandingPolicy,
    clearApprovalConflict: () => setApprovalConflict(null),
  };
}
