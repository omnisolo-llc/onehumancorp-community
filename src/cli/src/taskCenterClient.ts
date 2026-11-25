// SPDX-License-Identifier: Apache-2.0
import { randomUUID } from 'node:crypto';
import type { ToolItem } from './components/ToolProgress.js';
import { buildAuthHeaders, type CliConfig } from './config.js';

export type TaskCenterStatus =
  | 'draft'
  | 'queued'
  | 'running'
  | 'awaiting_approval'
  | 'waiting_for_input'
  | 'completed'
  | 'failed'
  | 'cancelled'
  | 'outcome_unknown';

export interface TaskCenterStep {
  id: string;
  seq: number;
  kind: string;
  title: string;
  detail: string;
  status: TaskCenterStatus;
  tool_name: string | null;
}

export interface TaskCenterArtifact {
  id: string;
  title: string;
  format: string;
  content: string;
}

export interface TaskCenterApproval {
  approval_id: string;
  action_kind: string;
  department: string;
  summary: string;
  payload_sha256: string;
  status: string;
}

export interface TaskCenterTask {
  id: string;
  title: string;
  prompt: string;
  department: string;
  status: TaskCenterStatus;
  output_summary: string | null;
  error_message: string | null;
  pending_approval: TaskCenterApproval | null;
  steps?: TaskCenterStep[];
  artifacts?: TaskCenterArtifact[];
}

export interface CreateTaskPayload {
  prompt: string;
  department?: string;
  requested_action?: string;
}

export interface TaskExecutionOutcome {
  task: TaskCenterTask;
  statusLabel: string;
  output: string;
  tools: ToolItem[];
}

export interface MarketplaceAgent {
  id: string;
  name: string;
  description: string;
  author: string;
  downloads: number;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === 'object' && !Array.isArray(value);
}

async function parseJsonSafe(response: Response): Promise<unknown> {
  try {
    return await response.json();
  } catch {
    return null;
  }
}

function extractApiError(body: unknown, status: number): string {
  if (isRecord(body)) {
    if (typeof body.error === 'string' && body.error.trim()) {
      return body.error.trim();
    }
    if (typeof body.message === 'string' && body.message.trim()) {
      return body.message.trim();
    }
  }
  return `HTTP error! status: ${status}`;
}

export function mapStepsToTools(steps: TaskCenterStep[] | undefined): ToolItem[] {
  if (!Array.isArray(steps)) return [];
  return steps.map((step) => {
    let status: ToolItem['status'] = 'pending';
    if (step.status === 'completed') {
      status = 'success';
    } else if (
      step.status === 'failed' ||
      step.status === 'cancelled' ||
      step.status === 'outcome_unknown'
    ) {
      status = 'error';
    }
    return {
      name: step.tool_name || step.title,
      status,
    };
  });
}

function parseTaskEnvelope(body: unknown): TaskCenterTask {
  const candidate = isRecord(body) && isRecord(body.task) ? body.task : body;
  if (
    !isRecord(candidate) ||
    typeof candidate.id !== 'string' ||
    !candidate.id.trim() ||
    typeof candidate.status !== 'string'
  ) {
    throw new Error('Server returned an invalid Task Center receipt.');
  }
  return candidate as unknown as TaskCenterTask;
}

export function extractTaskOutput(task: TaskCenterTask): string {
  if (typeof task.output_summary === 'string' && task.output_summary.trim()) {
    return task.output_summary;
  }
  if (Array.isArray(task.artifacts)) {
    const firstWithContent = task.artifacts.find(
      (artifact) => typeof artifact.content === 'string' && artifact.content.trim().length > 0,
    );
    if (firstWithContent) {
      return firstWithContent.content;
    }
  }
  if (task.status === 'awaiting_approval') {
    const summary = task.pending_approval?.summary?.trim();
    return summary
      ? `Approval required (${task.id}): ${summary}`
      : `Task ${task.id} is awaiting owner approval before execution.`;
  }
  if (task.status === 'waiting_for_input') {
    return `Task ${task.id} is waiting for additional input.`;
  }
  return 'No output received.';
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function isInFlight(status: TaskCenterStatus): boolean {
  return status === 'queued' || status === 'running' || status === 'draft';
}

export async function submitAndAwaitTask(
  config: CliConfig,
  payload: CreateTaskPayload,
  onProgress?: (task: TaskCenterTask) => void,
): Promise<TaskExecutionOutcome> {
  const prompt = payload.prompt.trim();
  if (!prompt) {
    throw new Error('Task prompt must not be empty.');
  }

  const idempotencyKey = randomUUID().toLowerCase();
  const requestBody: CreateTaskPayload = {
    prompt,
    department: payload.department ?? config.department,
    ...(payload.requested_action ? { requested_action: payload.requested_action } : {}),
  };

  const createResponse = await fetch(`${config.baseUrl}/api/v1/task-center/tasks`, {
    method: 'POST',
    headers: buildAuthHeaders(config, {
      'Content-Type': 'application/json',
      'Idempotency-Key': idempotencyKey,
    }),
    body: JSON.stringify(requestBody),
  });

  const createBody = await parseJsonSafe(createResponse);
  if (!createResponse.ok) {
    throw new Error(extractApiError(createBody, createResponse.status));
  }

  let task = parseTaskEnvelope(createBody);
  onProgress?.(task);

  const deadline = Date.now() + config.timeoutMs;
  while (isInFlight(task.status)) {
    if (Date.now() >= deadline) {
      throw new Error(
        `Timed out waiting for task ${task.id} (last status: ${task.status}). Inspect status before retrying.`,
      );
    }
    await sleep(config.pollIntervalMs);

    const pollResponse = await fetch(
      `${config.baseUrl}/api/v1/task-center/tasks/${encodeURIComponent(task.id)}`,
      {
        method: 'GET',
        headers: buildAuthHeaders(config),
      },
    );
    const pollBody = await parseJsonSafe(pollResponse);
    if (!pollResponse.ok) {
      throw new Error(extractApiError(pollBody, pollResponse.status));
    }
    task = parseTaskEnvelope(pollBody);
    onProgress?.(task);
  }

  if (task.status === 'failed') {
    throw new Error(task.error_message?.trim() || `Task ${task.id} failed.`);
  }
  if (task.status === 'cancelled') {
    throw new Error(task.error_message?.trim() || `Task ${task.id} was cancelled.`);
  }
  if (task.status === 'outcome_unknown') {
    throw new Error(
      task.error_message?.trim() ||
        `Task ${task.id} outcome is unknown; reconcile externally before starting new work.`,
    );
  }

  const statusLabel =
    task.status === 'awaiting_approval'
      ? 'Awaiting Approval'
      : task.status === 'waiting_for_input'
        ? 'Waiting For Input'
        : 'Complete';

  return {
    task,
    statusLabel,
    output: extractTaskOutput(task),
    tools: mapStepsToTools(task.steps),
  };
}

export async function searchMarketplaceAgents(
  config: CliConfig,
  query = '',
): Promise<MarketplaceAgent[]> {
  const url = new URL(`${config.baseUrl}/api/v1/agents/marketplace`);
  if (query.trim()) {
    url.searchParams.set('q', query.trim());
  }

  const response = await fetch(url.toString(), {
    method: 'GET',
    headers: buildAuthHeaders(config),
  });
  const body = await parseJsonSafe(response);
  if (!response.ok) {
    throw new Error(extractApiError(body, response.status));
  }

  const rawList = Array.isArray(body)
    ? body
    : isRecord(body) && Array.isArray(body.agents)
      ? body.agents
      : [];

  return rawList
    .filter((entry): entry is Record<string, unknown> => isRecord(entry))
    .map((entry) => ({
      id: typeof entry.id === 'string' ? entry.id : '',
      name: typeof entry.name === 'string' ? entry.name : '',
      description: typeof entry.description === 'string' ? entry.description : '',
      author: typeof entry.author === 'string' ? entry.author : 'unknown',
      downloads: typeof entry.downloads === 'number' && Number.isFinite(entry.downloads) ? entry.downloads : 0,
    }))
    .filter((agent) => agent.id.length > 0 && agent.name.length > 0);
}
