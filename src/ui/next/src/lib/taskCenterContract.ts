export const TASK_CENTER_TASK_STATUSES = [
  "draft", "queued", "running", "awaiting_approval", "waiting_for_input",
  "completed", "failed", "cancelled", "outcome_unknown",
] as const;
export type TaskCenterTaskStatus = (typeof TASK_CENTER_TASK_STATUSES)[number];

export const PROVENANCE_STATES = [
  "draft", "awaiting_approval", "verified", "failed",
  "cancelled", "outcome_unknown", "unconfigured",
] as const;
export type ProvenanceState = (typeof PROVENANCE_STATES)[number];

export const ATTENTION_RINGS = [
  "none", "permission_required", "needs_input", "blocked", "completed_unread",
] as const;
export type AttentionRing = (typeof ATTENTION_RINGS)[number];

export const SPACE_KINDS = [
  "operations", "client_delivery", "cash_recovery", "research", "custom",
] as const;
export type SpaceKind = (typeof SPACE_KINDS)[number];

export const SPACE_SOURCE_HEALTHS = [
  "live", "offline", "revoked", "unconfigured",
] as const;
export type SpaceSourceHealth = (typeof SPACE_SOURCE_HEALTHS)[number];

export const ACTION_RISK_TIERS = ["auto_execute", "draft_for_review"] as const;
export type ActionRiskTier = (typeof ACTION_RISK_TIERS)[number];

export const CONNECTOR_STATUSES = [
  "verified", "verification_required", "revoked", "not_connected",
] as const;
export type ConnectorStatus = (typeof CONNECTOR_STATUSES)[number];

export const PAYER_MODES = [
  "managed_api", "byok_api", "local", "unconfigured",
] as const;
export type PayerMode = (typeof PAYER_MODES)[number];

export const TASK_STEP_KINDS = [
  "plan", "tool_call", "llm_turn", "approval_gate", "artifact", "system",
] as const;
export type TaskStepKind = (typeof TASK_STEP_KINDS)[number];

export const TASK_ARTIFACT_FORMATS = [
  "markdown", "table", "checklist", "space_card", "json",
] as const;
export type TaskArtifactFormat = (typeof TASK_ARTIFACT_FORMATS)[number];

export const APPROVAL_STATUSES = [
  "pending", "approved", "rejected", "expired",
] as const;
export type ApprovalStatus = (typeof APPROVAL_STATUSES)[number];

export const CONNECTOR_IDS = [
  "openai_api", "anthropic_api", "stripe", "resend", "google_workspace",
] as const;
export type ConnectorId = (typeof CONNECTOR_IDS)[number];

export const CONNECTOR_CATEGORIES = [
  "ai_provider", "payments", "email", "workspace",
] as const;
export type ConnectorCategory = (typeof CONNECTOR_CATEGORIES)[number];

export const CONNECTOR_AUTH_MODES = ["api_key", "oauth_bearer"] as const;
export type ConnectorAuthMode = (typeof CONNECTOR_AUTH_MODES)[number];

export const TASK_CENTER_SSE_EVENTS = [
  "task_updated", "step_appended", "approval_required",
  "artifact_created", "space_updated", "resync_required",
] as const;
export type TaskCenterStreamEventKind = (typeof TASK_CENTER_SSE_EVENTS)[number];

export interface SpaceRecord {
  id: string;
  name: string;
  description: string;
  kind: SpaceKind;
  prompt_template: string;
  department: string;
  reads: string[];
  shows: string[];
  cannot: string[];
  source_health: SpaceSourceHealth;
  pinned_artifact_ids: string[];
  archived: boolean;
  created_at_unix: number;
  updated_at_unix: number;
}

export interface TaskStepRecord {
  id: string;
  task_id: string;
  seq: number;
  kind: TaskStepKind;
  title: string;
  detail: string;
  status: TaskCenterTaskStatus;
  provenance: ProvenanceState;
  tool_name: string | null;
  cost_micros: number;
  created_at_unix: number;
}

export interface TaskApprovalPayload {
  approval_id: string;
  action_kind: string;
  department: string;
  risk: ActionRiskTier;
  summary: string;
  payload_preview: Record<string, unknown>;
  payload_sha256: string;
  estimated_cost_micros: number;
  standing_limit_micros: number;
  status: ApprovalStatus;
  decided_by: string | null;
  decided_at_unix: number | null;
}

export interface TaskArtifactRecord {
  id: string;
  task_id: string;
  space_id: string | null;
  title: string;
  format: TaskArtifactFormat;
  content: string;
  provenance: ProvenanceState;
  pinned: boolean;
  created_at_unix: number;
}

export interface TaskCenterTask {
  id: string;
  space_id: string | null;
  title: string;
  prompt: string;
  department: string;
  status: TaskCenterTaskStatus;
  provenance: ProvenanceState;
  attention: AttentionRing;
  risk: ActionRiskTier;
  payer_mode: PayerMode;
  estimated_cost_micros: number;
  actual_cost_micros: number | null;
  idempotency_key: string;
  receipt_id: string | null;
  output_summary: string | null;
  error_message: string | null;
  pending_approval: TaskApprovalPayload | null;
  steps: TaskStepRecord[];
  artifacts: TaskArtifactRecord[];
  archived: boolean;
  created_at_unix: number;
  updated_at_unix: number;
}

export interface StandingPolicyRecord {
  department: string;
  auto_approve_max_micros: number;
  allow_external_send: boolean;
  allow_financial_commit: boolean;
  updated_at_unix: number;
}

export interface TaskCenterRuntimeInfo {
  llm_configured: boolean;
  provider: string | null;
  model: string | null;
  payer_mode: PayerMode;
  reason: string | null;
}

export interface TaskCenterBudgetInfo {
  configured: boolean;
  limit_micros: number;
  spent_micros: number;
  reserved_micros: number;
  currency: "USD";
  unit: "micro_usd";
  customer_direct_inference_is_not_rebilled: true;
}

export interface TaskCenterSummaryResponse {
  spaces: SpaceRecord[];
  tasks: TaskCenterTask[];
  standing_policies: StandingPolicyRecord[];
  runtime: TaskCenterRuntimeInfo;
  budget: TaskCenterBudgetInfo;
}

export interface TaskCenterStreamEvent {
  event_id: string;
  kind: TaskCenterStreamEventKind;
  tenant_id: string;
  occurred_at_unix: number;
  payload: Record<string, unknown>;
}

export interface ConnectorCatalogItem {
  id: ConnectorId;
  name: string;
  category: ConnectorCategory;
  auth_mode: ConnectorAuthMode;
  read_scopes: string[];
  write_scopes: string[];
  status: ConnectorStatus;
  usable: boolean;
  verified_at: number | null;
  expires_at: number | null;
}

export interface ConnectorCatalogResponse {
  success: boolean;
  connectors: ConnectorCatalogItem[];
  mcp_server: {
    endpoint: "/api/v1/integrations/mcp";
    protocol_version: "2025-03-26";
    default_mode: "read_only";
    write_opt_in_header: "x-ohc-mcp-write";
    tool_count: number;
  };
}

export interface CreateSpaceRequest {
  name: string;
  description: string;
  kind: SpaceKind;
  prompt_template: string;
  department: string;
  reads: string[];
  shows: string[];
  cannot: string[];
}

export interface CreateTaskRequest {
  prompt: string;
  space_id?: string | null;
  department?: string;
  requested_action?: string;
  estimated_cost_micros?: number;
}

export interface ApproveTaskRequest {
  approved: boolean;
  payload_sha256: string;
  reason?: string;
}

const SAFE_ID_REGEX = /^[A-Za-z0-9._-]{1,128}$/;
const UUID_REGEX =
  /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
const MAX_PROMPT_LENGTH = 16_000;
const MAX_ARTIFACT_CONTENT_BYTES = 65_536;

const isRecord = (v: unknown): v is Record<string, unknown> =>
  v !== null && typeof v === "object" && !Array.isArray(v);
const isString = (v: unknown): v is string => typeof v === "string";
const isNullableString = (v: unknown): v is string | null =>
  v === null || typeof v === "string";
const isSafeInt = (v: unknown): v is number =>
  typeof v === "number" && Number.isSafeInteger(v);
const isNonNegativeInt = (v: unknown): v is number => isSafeInt(v) && v >= 0;
const isNullableInt = (v: unknown): v is number | null =>
  v === null || isSafeInt(v);
const isNullableNonNegativeInt = (v: unknown): v is number | null =>
  v === null || isNonNegativeInt(v);
const isStringArray = (v: unknown): v is string[] =>
  Array.isArray(v) && v.every(isString);
const isOneOf = <T extends readonly string[]>(
  allowed: T,
  v: unknown,
): v is T[number] =>
  typeof v === "string" && (allowed as readonly string[]).includes(v);

function parseArray<T>(
  list: unknown,
  parser: (item: unknown) => T | null,
): T[] | null {
  if (!Array.isArray(list)) return null;
  const out: T[] = [];
  for (const item of list) {
    const parsed = parser(item);
    if (parsed === null) return null;
    out.push(parsed);
  }
  return out;
}

export function isValidTaskCenterId(value: unknown): value is string {
  return (
    typeof value === "string" &&
    value !== "." &&
    value !== ".." &&
    SAFE_ID_REGEX.test(value)
  );
}

export function isUuid(value: unknown): value is string {
  return typeof value === "string" && UUID_REGEX.test(value);
}

export function parseSpaceRecord(v: unknown): SpaceRecord | null {
  if (!isRecord(v)) return null;
  if (
    !isString(v.id) || !v.id.trim() ||
    !isString(v.name) || v.name.trim().length < 1 || v.name.trim().length > 80 ||
    !isString(v.description) || !isOneOf(SPACE_KINDS, v.kind) ||
    !isString(v.prompt_template) || !isString(v.department) ||
    !isStringArray(v.reads) || !isStringArray(v.shows) || !isStringArray(v.cannot) ||
    !isOneOf(SPACE_SOURCE_HEALTHS, v.source_health) ||
    !isStringArray(v.pinned_artifact_ids) || typeof v.archived !== "boolean" ||
    !isSafeInt(v.created_at_unix) || !isSafeInt(v.updated_at_unix)
  ) {
    return null;
  }
  return {
    id: v.id, name: v.name, description: v.description, kind: v.kind,
    prompt_template: v.prompt_template, department: v.department,
    reads: v.reads, shows: v.shows, cannot: v.cannot,
    source_health: v.source_health, pinned_artifact_ids: v.pinned_artifact_ids,
    archived: v.archived, created_at_unix: v.created_at_unix,
    updated_at_unix: v.updated_at_unix,
  };
}

export function parseTaskStepRecord(v: unknown): TaskStepRecord | null {
  if (!isRecord(v)) return null;
  if (
    !isString(v.id) || !isString(v.task_id) || !isNonNegativeInt(v.seq) ||
    !isOneOf(TASK_STEP_KINDS, v.kind) || !isString(v.title) || !isString(v.detail) ||
    !isOneOf(TASK_CENTER_TASK_STATUSES, v.status) ||
    !isOneOf(PROVENANCE_STATES, v.provenance) || !isNullableString(v.tool_name) ||
    !isNonNegativeInt(v.cost_micros) || !isSafeInt(v.created_at_unix)
  ) {
    return null;
  }
  return {
    id: v.id, task_id: v.task_id, seq: v.seq, kind: v.kind,
    title: v.title, detail: v.detail, status: v.status, provenance: v.provenance,
    tool_name: v.tool_name, cost_micros: v.cost_micros,
    created_at_unix: v.created_at_unix,
  };
}

export function parseTaskApprovalPayload(v: unknown): TaskApprovalPayload | null {
  if (!isRecord(v)) return null;
  if (
    !isString(v.approval_id) || !v.approval_id.trim() ||
    !isString(v.action_kind) || !isString(v.department) ||
    !isOneOf(ACTION_RISK_TIERS, v.risk) || !isString(v.summary) ||
    !isRecord(v.payload_preview) || !isString(v.payload_sha256) || !v.payload_sha256.trim() ||
    !isNonNegativeInt(v.estimated_cost_micros) || !isNonNegativeInt(v.standing_limit_micros) ||
    !isOneOf(APPROVAL_STATUSES, v.status) || !isNullableString(v.decided_by) ||
    !isNullableInt(v.decided_at_unix)
  ) {
    return null;
  }
  return {
    approval_id: v.approval_id, action_kind: v.action_kind, department: v.department,
    risk: v.risk, summary: v.summary, payload_preview: v.payload_preview,
    payload_sha256: v.payload_sha256, estimated_cost_micros: v.estimated_cost_micros,
    standing_limit_micros: v.standing_limit_micros, status: v.status,
    decided_by: v.decided_by, decided_at_unix: v.decided_at_unix,
  };
}

export function parseTaskArtifactRecord(v: unknown): TaskArtifactRecord | null {
  if (!isRecord(v)) return null;
  if (
    !isString(v.id) || !isString(v.task_id) || !isNullableString(v.space_id) ||
    !isString(v.title) || !isOneOf(TASK_ARTIFACT_FORMATS, v.format) ||
    !isString(v.content) || v.content.length > MAX_ARTIFACT_CONTENT_BYTES ||
    !isOneOf(PROVENANCE_STATES, v.provenance) || typeof v.pinned !== "boolean" ||
    !isSafeInt(v.created_at_unix)
  ) {
    return null;
  }
  return {
    id: v.id, task_id: v.task_id, space_id: v.space_id, title: v.title,
    format: v.format, content: v.content, provenance: v.provenance,
    pinned: v.pinned, created_at_unix: v.created_at_unix,
  };
}

export function parseTaskCenterTask(v: unknown): TaskCenterTask | null {
  if (!isRecord(v)) return null;
  if (
    !isString(v.id) || !v.id.trim() || !isNullableString(v.space_id) ||
    !isString(v.title) || !isString(v.prompt) || !isString(v.department) ||
    !isOneOf(TASK_CENTER_TASK_STATUSES, v.status) ||
    !isOneOf(PROVENANCE_STATES, v.provenance) ||
    !isOneOf(ATTENTION_RINGS, v.attention) || !isOneOf(ACTION_RISK_TIERS, v.risk) ||
    !isOneOf(PAYER_MODES, v.payer_mode) || !isNonNegativeInt(v.estimated_cost_micros) ||
    !isNullableNonNegativeInt(v.actual_cost_micros) || !isString(v.idempotency_key) ||
    !isNullableString(v.receipt_id) || !isNullableString(v.output_summary) ||
    !isNullableString(v.error_message) || typeof v.archived !== "boolean" ||
    !isSafeInt(v.created_at_unix) || !isSafeInt(v.updated_at_unix)
  ) {
    return null;
  }
  const pendingApproval =
    v.pending_approval === null ? null : parseTaskApprovalPayload(v.pending_approval);
  if (v.pending_approval !== null && !pendingApproval) return null;
  const steps = parseArray(v.steps, parseTaskStepRecord);
  const artifacts = parseArray(v.artifacts, parseTaskArtifactRecord);
  if (!steps || !artifacts) return null;

  return {
    id: v.id, space_id: v.space_id, title: v.title, prompt: v.prompt,
    department: v.department, status: v.status, provenance: v.provenance,
    attention: v.attention, risk: v.risk, payer_mode: v.payer_mode,
    estimated_cost_micros: v.estimated_cost_micros,
    actual_cost_micros: v.actual_cost_micros, idempotency_key: v.idempotency_key,
    receipt_id: v.receipt_id, output_summary: v.output_summary,
    error_message: v.error_message, pending_approval: pendingApproval,
    steps, artifacts, archived: v.archived,
    created_at_unix: v.created_at_unix, updated_at_unix: v.updated_at_unix,
  };
}

export function parseTaskCenterTaskEnvelope(v: unknown): TaskCenterTask | null {
  return parseTaskCenterTask(v) ?? (isRecord(v) && "task" in v ? parseTaskCenterTask(v.task) : null);
}

export function parseTaskCenterTaskList(v: unknown): TaskCenterTask[] | null {
  const raw = Array.isArray(v) ? v : isRecord(v) && Array.isArray(v.tasks) ? v.tasks : null;
  return raw ? parseArray(raw, parseTaskCenterTask) : null;
}

export function parseStandingPolicyRecord(v: unknown): StandingPolicyRecord | null {
  if (!isRecord(v)) return null;
  if (
    !isString(v.department) || !v.department.trim() ||
    !isNonNegativeInt(v.auto_approve_max_micros) ||
    typeof v.allow_external_send !== "boolean" ||
    typeof v.allow_financial_commit !== "boolean" ||
    !isSafeInt(v.updated_at_unix)
  ) {
    return null;
  }
  return {
    department: v.department,
    auto_approve_max_micros: v.auto_approve_max_micros,
    allow_external_send: v.allow_external_send,
    allow_financial_commit: v.allow_financial_commit,
    updated_at_unix: v.updated_at_unix,
  };
}

export function parseTaskCenterSummaryResponse(
  v: unknown,
): TaskCenterSummaryResponse | null {
  if (!isRecord(v) || !isRecord(v.runtime) || !isRecord(v.budget)) return null;
  const spaces = parseArray(v.spaces, parseSpaceRecord);
  const tasks = parseArray(v.tasks, parseTaskCenterTask);
  const standingPolicies = parseArray(v.standing_policies, parseStandingPolicyRecord);
  if (!spaces || !tasks || !standingPolicies) return null;

  const rt = v.runtime;
  const bg = v.budget;
  if (
    typeof rt.llm_configured !== "boolean" || !isNullableString(rt.provider) ||
    !isNullableString(rt.model) || !isOneOf(PAYER_MODES, rt.payer_mode) ||
    !isNullableString(rt.reason) || typeof bg.configured !== "boolean" ||
    !isNonNegativeInt(bg.limit_micros) || !isNonNegativeInt(bg.spent_micros) ||
    !isNonNegativeInt(bg.reserved_micros) || bg.currency !== "USD" ||
    bg.unit !== "micro_usd" || bg.customer_direct_inference_is_not_rebilled !== true
  ) {
    return null;
  }
  return {
    spaces, tasks, standing_policies: standingPolicies,
    runtime: {
      llm_configured: rt.llm_configured, provider: rt.provider,
      model: rt.model, payer_mode: rt.payer_mode, reason: rt.reason,
    },
    budget: {
      configured: bg.configured, limit_micros: bg.limit_micros,
      spent_micros: bg.spent_micros, reserved_micros: bg.reserved_micros,
      currency: "USD", unit: "micro_usd",
      customer_direct_inference_is_not_rebilled: true,
    },
  };
}

export function parseTaskCenterStreamEvent(v: unknown): TaskCenterStreamEvent | null {
  if (!isRecord(v)) return null;
  if (
    !isString(v.event_id) || !isOneOf(TASK_CENTER_SSE_EVENTS, v.kind) ||
    !isString(v.tenant_id) || !isSafeInt(v.occurred_at_unix) || !isRecord(v.payload)
  ) {
    return null;
  }
  return {
    event_id: v.event_id, kind: v.kind, tenant_id: v.tenant_id,
    occurred_at_unix: v.occurred_at_unix, payload: v.payload,
  };
}

export function parseConnectorCatalogItem(v: unknown): ConnectorCatalogItem | null {
  if (!isRecord(v)) return null;
  if (
    !isOneOf(CONNECTOR_IDS, v.id) || !isString(v.name) ||
    !isOneOf(CONNECTOR_CATEGORIES, v.category) ||
    !isOneOf(CONNECTOR_AUTH_MODES, v.auth_mode) ||
    !isStringArray(v.read_scopes) || !isStringArray(v.write_scopes) ||
    !isOneOf(CONNECTOR_STATUSES, v.status) || typeof v.usable !== "boolean" ||
    !isNullableInt(v.verified_at) || !isNullableInt(v.expires_at)
  ) {
    return null;
  }
  return {
    id: v.id, name: v.name, category: v.category, auth_mode: v.auth_mode,
    read_scopes: v.read_scopes, write_scopes: v.write_scopes,
    status: v.status, usable: v.usable && v.status === "verified",
    verified_at: v.verified_at, expires_at: v.expires_at,
  };
}

export function parseConnectorCatalogResponse(
  v: unknown,
): ConnectorCatalogResponse | null {
  if (!isRecord(v) || typeof v.success !== "boolean" || !isRecord(v.mcp_server)) {
    return null;
  }
  const connectors = parseArray(v.connectors, parseConnectorCatalogItem);
  const mcp = v.mcp_server;
  if (
    !connectors ||
    mcp.endpoint !== "/api/v1/integrations/mcp" ||
    mcp.protocol_version !== "2025-03-26" ||
    mcp.default_mode !== "read_only" ||
    mcp.write_opt_in_header !== "x-ohc-mcp-write" ||
    !isNonNegativeInt(mcp.tool_count)
  ) {
    return null;
  }
  return {
    success: v.success,
    connectors,
    mcp_server: {
      endpoint: "/api/v1/integrations/mcp",
      protocol_version: "2025-03-26",
      default_mode: "read_only",
      write_opt_in_header: "x-ohc-mcp-write",
      tool_count: mcp.tool_count,
    },
  };
}

export function validateCreateTaskPayload(v: unknown): CreateTaskRequest | null {
  if (!isRecord(v) || !isString(v.prompt)) return null;
  const prompt = v.prompt.trim();
  if (!prompt || prompt.length > MAX_PROMPT_LENGTH || prompt.includes("\0")) return null;

  let spaceId: string | null | undefined;
  if (v.space_id !== undefined) {
    if (v.space_id === null) spaceId = null;
    else if (isValidTaskCenterId(v.space_id)) spaceId = v.space_id;
    else return null;
  }

  let department: string | undefined;
  if (v.department !== undefined) {
    if (!isString(v.department) || !v.department.trim() || v.department.length > 64) return null;
    department = v.department.trim();
  }

  let requestedAction: string | undefined;
  if (v.requested_action !== undefined) {
    if (!isString(v.requested_action) || !v.requested_action.trim() || v.requested_action.length > 128) return null;
    requestedAction = v.requested_action.trim();
  }

  let estimatedCostMicros: number | undefined;
  if (v.estimated_cost_micros !== undefined) {
    if (!isNonNegativeInt(v.estimated_cost_micros)) return null;
    estimatedCostMicros = v.estimated_cost_micros;
  }

  return {
    prompt,
    ...(spaceId !== undefined ? { space_id: spaceId } : {}),
    ...(department !== undefined ? { department } : {}),
    ...(requestedAction !== undefined ? { requested_action: requestedAction } : {}),
    ...(estimatedCostMicros !== undefined ? { estimated_cost_micros: estimatedCostMicros } : {}),
  };
}

export function formatMicroUsd(micros: number): string {
  if (!Number.isFinite(micros)) return "$0.00";
  const sign = micros < 0 ? "-" : "";
  const abs = Math.trunc(Math.abs(micros));
  const whole = Math.floor(abs / 1_000_000);
  const fracRaw = String(abs % 1_000_000).padStart(6, "0");
  const trimmed = fracRaw.replace(/0+$/, "");
  const frac = trimmed.length <= 2 ? fracRaw.slice(0, 2) : trimmed;
  return `${sign}$${whole.toLocaleString("en-US")}.${frac}`;
}

export function formatPayerMode(mode: PayerMode): string {
  switch (mode) {
    case "managed_api": return "Managed API";
    case "byok_api": return "Customer BYOK API";
    case "local": return "Local Runtime";
    case "unconfigured": return "Unconfigured";
  }
}

export function formatUnixTimestamp(unixSeconds: number): string {
  if (!Number.isFinite(unixSeconds) || unixSeconds <= 0) return "—";
  return new Date(unixSeconds * 1000).toLocaleString("en-US", {
    month: "short", day: "numeric", hour: "2-digit", minute: "2-digit",
  });
}

export function attentionPriority(attention: AttentionRing): number {
  switch (attention) {
    case "permission_required": return 4;
    case "blocked": return 3;
    case "needs_input": return 2;
    case "completed_unread": return 1;
    case "none": return 0;
  }
}

export function generateClientUuid(): string {
  if (typeof crypto !== "undefined" && typeof crypto.randomUUID === "function") {
    return crypto.randomUUID();
  }
  const bytes = new Uint8Array(16);
  if (typeof crypto !== "undefined" && typeof crypto.getRandomValues === "function") {
    crypto.getRandomValues(bytes);
  } else {
    for (let i = 0; i < 16; i += 1) bytes[i] = Math.floor(Math.random() * 256);
  }
  bytes[6] = (bytes[6] & 0x0f) | 0x40;
  bytes[8] = (bytes[8] & 0x3f) | 0x80;
  const hex = Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
}
