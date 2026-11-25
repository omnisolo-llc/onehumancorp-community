"use client";

import React, { FormEvent, useId, useState } from "react";
import Link from "next/link";
import {
  formatMicroUsd,
  formatPayerMode,
  formatUnixTimestamp,
  type ApproveTaskRequest,
  type ConnectorCatalogResponse,
  type SpaceRecord,
  type StandingPolicyRecord,
  type TaskArtifactRecord,
  type TaskCenterBudgetInfo,
  type TaskCenterRuntimeInfo,
  type TaskCenterTask,
} from "@/lib/taskCenterContract";
import { ApprovalCard } from "./ApprovalCard";
import { ProvenanceBadge, TaskStatusBadge } from "./ProvenanceBadge";

export type InspectorTab = "now" | "artifacts" | "budget";

export interface InspectorDrawerProps {
  task: TaskCenterTask | null;
  spaces: SpaceRecord[];
  runtime: TaskCenterRuntimeInfo | null;
  budget: TaskCenterBudgetInfo | null;
  standingPolicies: StandingPolicyRecord[];
  catalog: ConnectorCatalogResponse | null;
  activeTab: InspectorTab;
  busyTask: boolean;
  approvalConflict: string | null;
  onTabChange: (tab: InspectorTab) => void;
  onDecideApproval: (taskId: string, decision: ApproveTaskRequest) => Promise<void>;
  onCancelTask: (taskId: string) => Promise<void>;
  onSendFollowUp: (taskId: string, prompt: string) => Promise<boolean>;
  onTogglePinArtifact: (
    taskId: string,
    artifactId: string,
    spaceId: string,
    pinned: boolean,
  ) => Promise<void>;
  onUpdatePolicy: (
    department: string,
    policy: {
      auto_approve_max_micros: number;
      allow_external_send: boolean;
      allow_financial_commit: boolean;
    },
  ) => Promise<void>;
}

function renderArtifactContent(artifact: TaskArtifactRecord): React.ReactNode {
  if (artifact.format === "json") {
    let formatted: string;
    try {
      formatted = JSON.stringify(JSON.parse(artifact.content), null, 2);
    } catch {
      formatted = artifact.content;
    }
    return (
      <pre className="max-h-56 overflow-auto rounded-lg bg-slate-950 text-slate-100 p-3 text-xs font-mono whitespace-pre-wrap break-words">
        {formatted}
      </pre>
    );
  }

  if (artifact.format === "checklist") {
    const items = artifact.content
      .split("\n")
      .map((line) => line.replace(/^[-*]\s*/, "").trim())
      .filter(Boolean);
    return (
      <ul className="space-y-1.5 text-xs text-slate-800 dark:text-slate-200">
        {items.map((item, idx) => (
          <li key={`${artifact.id}-i-${idx}`} className="flex items-start gap-2">
            <span aria-hidden="true" className="text-[#2784ff] font-bold">•</span>
            <span>{item}</span>
          </li>
        ))}
      </ul>
    );
  }

  if (artifact.format === "table") {
    const rows = artifact.content
      .split("\n")
      .map((line) => line.split("|").map((c) => c.trim()).filter(Boolean))
      .filter((cells) => cells.length > 0 && !cells.every((c) => /^-+$/.test(c)));
    if (rows.length > 0) {
      const [header, ...bodyRows] = rows;
      return (
        <div className="app-table-wrap rounded-lg border border-slate-200 dark:border-slate-700">
          <table className="app-table text-xs">
            <thead>
              <tr>{header.map((col, idx) => <th key={`${artifact.id}-h-${idx}`}>{col}</th>)}</tr>
            </thead>
            <tbody>
              {bodyRows.map((row, rIdx) => (
                <tr key={`${artifact.id}-r-${rIdx}`}>
                  {row.map((cell, cIdx) => <td key={`${artifact.id}-c-${rIdx}-${cIdx}`}>{cell}</td>)}
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      );
    }
  }

  return (
    <div className="rounded-lg border border-slate-200/80 dark:border-slate-700/80 bg-white/75 dark:bg-slate-900/60 p-3 text-xs text-slate-800 dark:text-slate-200 whitespace-pre-wrap break-words">
      {artifact.content}
    </div>
  );
}

function connectorBadgeClass(status: string, usable: boolean): string {
  if (usable && status === "verified") return "app-badge good";
  if (status === "verification_required") return "app-badge warn";
  if (status === "revoked") return "app-badge bad";
  return "app-badge";
}

function connectorStatusLabel(status: string, usable: boolean): string {
  if (usable && status === "verified") return "Verified";
  if (status === "verification_required") return "Verification required";
  if (status === "revoked") return "Revoked";
  return "Not connected";
}

export function InspectorDrawer({
  task, spaces, runtime, budget, standingPolicies, catalog, activeTab,
  busyTask, approvalConflict, onTabChange, onDecideApproval, onCancelTask,
  onSendFollowUp, onTogglePinArtifact, onUpdatePolicy,
}: InspectorDrawerProps) {
  const [followUp, setFollowUp] = useState("");
  const [pinTargetByArtifact, setPinTargetByArtifact] = useState<Record<string, string>>({});
  const [editingPolicyDept, setEditingPolicyDept] = useState<string | null>(null);
  const [policyMaxMicros, setPolicyMaxMicros] = useState("0");
  const [policyExternalSend, setPolicyExternalSend] = useState(false);
  const [policyFinancialCommit, setPolicyFinancialCommit] = useState(false);

  const followUpId = useId();
  const hasPendingApproval = task?.pending_approval?.status === "pending";
  const isOutcomeUnknown =
    task?.status === "outcome_unknown" || task?.provenance === "outcome_unknown";
  const canCancel =
    task !== null &&
    (task.status === "queued" || task.status === "running" ||
      task.status === "awaiting_approval" || task.status === "waiting_for_input");
  const llmConfigured = runtime?.llm_configured === true;

  async function handleFollowUpSubmit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (!task || !followUp.trim() || busyTask || !llmConfigured || isOutcomeUnknown) return;
    if (await onSendFollowUp(task.id, followUp.trim())) setFollowUp("");
  }

  function startPolicyEdit(policy: StandingPolicyRecord) {
    setEditingPolicyDept(policy.department);
    setPolicyMaxMicros(String(policy.auto_approve_max_micros));
    setPolicyExternalSend(policy.allow_external_send);
    setPolicyFinancialCommit(policy.allow_financial_commit);
  }

  async function savePolicy(department: string) {
    const parsed = Number.parseInt(policyMaxMicros, 10);
    if (!Number.isSafeInteger(parsed) || parsed < 0) return;
    await onUpdatePolicy(department, {
      auto_approve_max_micros: parsed,
      allow_external_send: policyExternalSend,
      allow_financial_commit: policyFinancialCommit,
    });
    setEditingPolicyDept(null);
  }

  const remainingMicros = budget
    ? Math.max(0, budget.limit_micros - budget.spent_micros - budget.reserved_micros)
    : 0;
  const usedPercent =
    budget && budget.limit_micros > 0
      ? Math.min(100, Math.round(((budget.spent_micros + budget.reserved_micros) / budget.limit_micros) * 100))
      : 0;

  return (
    <aside aria-label="Contextual task inspector" className="app-panel flex flex-col gap-4 p-4 h-fit">
      {/* Active approval cards are NEVER covered by artifacts or tab switches */}
      {task && hasPendingApproval && task.pending_approval && (
        <ApprovalCard
          taskId={task.id}
          taskTitle={task.title}
          approval={task.pending_approval}
          busy={busyTask}
          conflictMessage={approvalConflict}
          onDecision={onDecideApproval}
        />
      )}

      {task && isOutcomeUnknown && (
        <div
          role="alert"
          data-testid="outcome-unknown-warning"
          className="rounded-xl border-2 border-orange-500 bg-orange-50 dark:bg-orange-950/60 p-3.5 space-y-1.5"
        >
          <p className="text-xs font-bold uppercase tracking-wide text-orange-950 dark:text-orange-100">
            Provider Outcome Unknown — Do Not Blindly Retry
          </p>
          <p className="text-xs text-orange-900 dark:text-orange-200">
            This task reached an external provider before an indeterminate failure. Reconcile the provider receipt
            {task.receipt_id ? ` (${task.receipt_id})` : ""} before initiating any replacement request. Automatic retry
            is disabled to prevent duplicate charges or duplicate external sends.
          </p>
        </div>
      )}

      <nav aria-label="Inspector sections" className="grid grid-cols-3 gap-1.5 border-b border-slate-200 dark:border-slate-700 pb-3">
        {(
          [
            ["now", "Now"],
            ["artifacts", `Artifacts (${task?.artifacts.length ?? 0})`],
            ["budget", "Budget & Tools"],
          ] as const
        ).map(([tabKey, label]) => (
          <button
            key={tabKey}
            type="button"
            aria-pressed={activeTab === tabKey}
            onClick={() => onTabChange(tabKey)}
            className={`min-h-[44px] rounded-lg px-2 text-xs font-semibold tabular-nums transition-colors motion-reduce:transition-none focus-visible:outline-2 focus-visible:outline-[#2784ff] focus-visible:outline-offset-2 ${
              activeTab === tabKey
                ? "bg-[#2784ff] text-white"
                : "bg-slate-100/80 dark:bg-slate-800/80 text-slate-800 dark:text-slate-200 hover:bg-slate-200/80 dark:hover:bg-slate-700"
            }`}
          >
            {label}
          </button>
        ))}
      </nav>

      {activeTab === "now" && (
        <div className="space-y-4">
          {!task ? (
            <p className="text-xs text-slate-600 dark:text-slate-400">
              Select a task from the stream to inspect live steps, receipts, approvals, and artifacts.
            </p>
          ) : (
            <>
              <div className="space-y-2">
                <div className="flex flex-wrap items-center gap-1.5">
                  <TaskStatusBadge status={task.status} />
                  <ProvenanceBadge provenance={task.provenance} compact />
                  <span className="app-badge capitalize">{task.department}</span>
                </div>
                <h3 className="text-sm font-bold text-slate-900 dark:text-slate-100">{task.title}</h3>
                <p className="text-xs text-slate-700 dark:text-slate-300">{task.prompt}</p>

                <dl className="grid grid-cols-2 gap-2 rounded-lg border border-slate-200/80 dark:border-slate-700/80 bg-white/70 dark:bg-slate-900/50 p-2.5 text-[11px]">
                  <div>
                    <dt className="text-slate-500 dark:text-slate-400">Payer</dt>
                    <dd className="font-semibold text-slate-900 dark:text-slate-100">{formatPayerMode(task.payer_mode)}</dd>
                  </div>
                  <div>
                    <dt className="text-slate-500 dark:text-slate-400">Cost (est / actual)</dt>
                    <dd className="font-semibold text-slate-900 dark:text-slate-100 tabular-nums">
                      {formatMicroUsd(task.estimated_cost_micros)} /{" "}
                      {task.actual_cost_micros !== null ? formatMicroUsd(task.actual_cost_micros) : "—"}
                    </dd>
                  </div>
                  <div className="col-span-2">
                    <dt className="text-slate-500 dark:text-slate-400">Receipt &amp; Idempotency Key</dt>
                    <dd className="font-mono text-[11px] text-slate-700 dark:text-slate-300 break-all">
                      {task.receipt_id ?? "No dispatch receipt yet"} · {task.idempotency_key}
                    </dd>
                  </div>
                </dl>

                {task.output_summary && (
                  <div className="rounded-lg border border-emerald-200 dark:border-emerald-800 bg-emerald-50/70 dark:bg-emerald-950/30 p-3 text-xs text-slate-900 dark:text-slate-100 whitespace-pre-wrap">
                    <p className="font-bold text-emerald-900 dark:text-emerald-200 mb-1">Output summary</p>
                    {task.output_summary}
                  </div>
                )}

                {task.error_message && (
                  <div role="alert" className="rounded-lg border border-rose-300 dark:border-rose-700 bg-rose-50 dark:bg-rose-950/40 p-3 text-xs text-rose-900 dark:text-rose-100">
                    {task.error_message}
                  </div>
                )}

                {canCancel && (
                  <button
                    type="button"
                    disabled={busyTask}
                    onClick={() => void onCancelTask(task.id)}
                    className="glass-control min-h-[44px] px-3 text-xs font-semibold text-rose-800 dark:text-rose-200 disabled:opacity-50"
                  >
                    {busyTask ? "Updating..." : "Cancel Task"}
                  </button>
                )}
              </div>

              <div className="space-y-2">
                <h4 className="text-xs font-bold uppercase tracking-wide text-slate-600 dark:text-slate-400">
                  Live Step Trace ({task.steps.length})
                </h4>
                {task.steps.length === 0 ? (
                  <p className="text-xs text-slate-600 dark:text-slate-400">No execution steps recorded yet.</p>
                ) : (
                  <ol className="space-y-2">
                    {[...task.steps].sort((a, b) => a.seq - b.seq).map((step) => (
                      <li key={step.id} className="rounded-lg border border-slate-200/80 dark:border-slate-700/80 bg-white/75 dark:bg-slate-900/60 p-2.5 space-y-1">
                        <div className="flex flex-wrap items-center justify-between gap-1.5">
                          <span className="text-xs font-semibold text-slate-900 dark:text-slate-100 tabular-nums">
                            #{step.seq} · {step.title}
                          </span>
                          <ProvenanceBadge provenance={step.provenance} compact />
                        </div>
                        {step.detail && (
                          <p className="text-xs text-slate-700 dark:text-slate-300 whitespace-pre-wrap">{step.detail}</p>
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

              <form onSubmit={handleFollowUpSubmit} className="space-y-2 pt-2 border-t border-slate-200 dark:border-slate-700">
                <label htmlFor={followUpId} className="block text-xs font-semibold text-slate-800 dark:text-slate-200">
                  Follow-up instruction
                </label>
                <div className="flex gap-2">
                  <input
                    id={followUpId}
                    type="text"
                    value={followUp}
                    onChange={(event) => setFollowUp(event.target.value)}
                    disabled={busyTask || !llmConfigured || isOutcomeUnknown}
                    placeholder={
                      !llmConfigured
                        ? "Configure AI runtime to send follow-ups..."
                        : isOutcomeUnknown
                          ? "Disabled while provider outcome is unknown"
                          : "Add follow-up context for this task..."
                    }
                    className="glass-control flex-1 px-3 min-h-[44px] text-xs"
                  />
                  <button
                    type="submit"
                    disabled={busyTask || !followUp.trim() || !llmConfigured || isOutcomeUnknown}
                    className="app-button primary min-h-[44px] px-3 text-xs disabled:opacity-50"
                  >
                    Send
                  </button>
                </div>
              </form>
            </>
          )}
        </div>
      )}

      {activeTab === "artifacts" && (
        <div className="space-y-3">
          {!task ? (
            <p className="text-xs text-slate-600 dark:text-slate-400">Select a task to view declarative artifacts.</p>
          ) : task.artifacts.length === 0 ? (
            <p className="text-xs text-slate-600 dark:text-slate-400">No artifacts generated for this task yet.</p>
          ) : (
            task.artifacts.map((artifact) => {
              const targetSpaceId =
                pinTargetByArtifact[artifact.id] ?? artifact.space_id ?? task.space_id ?? spaces[0]?.id ?? "";
              return (
                <article key={artifact.id} className="app-card p-3 space-y-2 border border-slate-200/80 dark:border-slate-700/80">
                  <header className="flex flex-wrap items-start justify-between gap-2">
                    <div>
                      <h3 className="text-xs font-bold text-slate-900 dark:text-slate-100">{artifact.title}</h3>
                      <p className="text-[11px] text-slate-600 dark:text-slate-400 tabular-nums">
                        {artifact.format} · {formatUnixTimestamp(artifact.created_at_unix)}
                      </p>
                    </div>
                    <ProvenanceBadge provenance={artifact.provenance} compact />
                  </header>
                  {renderArtifactContent(artifact)}
                  <div className="flex flex-wrap items-center gap-2 pt-1">
                    {spaces.length > 0 && !artifact.pinned && (
                      <select
                        aria-label={`Target Space for ${artifact.title}`}
                        value={targetSpaceId}
                        onChange={(event) => setPinTargetByArtifact((prev) => ({ ...prev, [artifact.id]: event.target.value }))}
                        className="glass-control min-h-[44px] px-2 text-xs"
                      >
                        {spaces.filter((s) => !s.archived).map((space) => (
                          <option key={space.id} value={space.id}>{space.name}</option>
                        ))}
                      </select>
                    )}
                    <button
                      type="button"
                      disabled={busyTask || (!artifact.pinned && !targetSpaceId)}
                      onClick={() => void onTogglePinArtifact(task.id, artifact.id, targetSpaceId, !artifact.pinned)}
                      className="glass-control min-h-[44px] px-3 text-xs font-semibold disabled:opacity-50"
                    >
                      {artifact.pinned ? "Unpin from Space" : "Pin to Space"}
                    </button>
                  </div>
                </article>
              );
            })
          )}
        </div>
      )}

      {activeTab === "budget" && (
        <div className="space-y-4">
          <section aria-label="Pre-execution budget and payer" className="app-card p-3.5 space-y-2.5 border border-slate-200/80 dark:border-slate-700/80">
            <div className="flex items-center justify-between gap-2">
              <h3 className="text-xs font-bold text-slate-900 dark:text-slate-100">Budget &amp; Payer Ledger</h3>
              <span className="app-badge">{runtime ? formatPayerMode(runtime.payer_mode) : "Unknown"}</span>
            </div>
            {budget ? (
              <>
                <dl className="grid grid-cols-2 gap-2 text-xs tabular-nums">
                  <div>
                    <dt className="text-slate-600 dark:text-slate-400">Limit</dt>
                    <dd className="font-bold text-slate-900 dark:text-slate-100">{formatMicroUsd(budget.limit_micros)}</dd>
                  </div>
                  <div>
                    <dt className="text-slate-600 dark:text-slate-400">Remaining</dt>
                    <dd className="font-bold text-slate-900 dark:text-slate-100">{formatMicroUsd(remainingMicros)}</dd>
                  </div>
                  <div>
                    <dt className="text-slate-600 dark:text-slate-400">Spent</dt>
                    <dd className="font-semibold text-slate-800 dark:text-slate-200">{formatMicroUsd(budget.spent_micros)}</dd>
                  </div>
                  <div>
                    <dt className="text-slate-600 dark:text-slate-400">Reserved</dt>
                    <dd className="font-semibold text-slate-800 dark:text-slate-200">{formatMicroUsd(budget.reserved_micros)}</dd>
                  </div>
                </dl>
                <div role="progressbar" aria-label="Budget utilization" aria-valuenow={usedPercent} aria-valuemin={0} aria-valuemax={100} className="h-2 w-full rounded-full bg-slate-200 dark:bg-slate-800 overflow-hidden">
                  <div className={`h-full transition-all motion-reduce:transition-none ${usedPercent >= 90 ? "bg-rose-600" : "bg-[#2784ff]"}`} style={{ width: `${usedPercent}%` }} />
                </div>
                <p className="text-[11px] text-slate-600 dark:text-slate-400">
                  Customer-direct BYOK inference is billed directly by your provider and never rebilled by OHC.
                </p>
              </>
            ) : (
              <p className="text-xs text-slate-600 dark:text-slate-400">Budget summary unavailable.</p>
            )}
          </section>

          <section aria-label="Department standing authority policies" className="app-card p-3.5 space-y-2.5 border border-slate-200/80 dark:border-slate-700/80">
            <h3 className="text-xs font-bold text-slate-900 dark:text-slate-100">Standing Authority Limits</h3>
            {standingPolicies.length === 0 ? (
              <p className="text-xs text-slate-600 dark:text-slate-400">
                No department standing policies configured. All external and costly actions require owner approval.
              </p>
            ) : (
              <ul className="space-y-2">
                {standingPolicies.map((policy) => (
                  <li key={policy.department} className="rounded-lg border border-slate-200/80 dark:border-slate-700/80 p-2.5 text-xs space-y-1.5">
                    <div className="flex items-center justify-between gap-2">
                      <span className="font-bold capitalize text-slate-900 dark:text-slate-100">{policy.department}</span>
                      <span className="font-semibold tabular-nums">Auto-approve ≤ {formatMicroUsd(policy.auto_approve_max_micros)}</span>
                    </div>
                    <div className="flex flex-wrap gap-2 text-[11px] text-slate-600 dark:text-slate-400">
                      <span>External send: {policy.allow_external_send ? "Allowed" : "Approval required"}</span>
                      <span>Financial commit: {policy.allow_financial_commit ? "Allowed" : "Approval required"}</span>
                    </div>
                    {editingPolicyDept === policy.department ? (
                      <div className="space-y-2 pt-1.5 border-t border-slate-200 dark:border-slate-700">
                        <label className="block text-[11px] font-medium">
                          Auto-approve ceiling (µUSD)
                          <input type="number" min={0} step={1000} value={policyMaxMicros} onChange={(e) => setPolicyMaxMicros(e.target.value)} className="glass-control mt-1 w-full px-2 min-h-[44px] text-xs tabular-nums" />
                        </label>
                        <label className="flex items-center gap-2 text-xs">
                          <input type="checkbox" checked={policyExternalSend} onChange={(e) => setPolicyExternalSend(e.target.checked)} />
                          Allow external send within ceiling
                        </label>
                        <label className="flex items-center gap-2 text-xs">
                          <input type="checkbox" checked={policyFinancialCommit} onChange={(e) => setPolicyFinancialCommit(e.target.checked)} />
                          Allow financial commit within ceiling
                        </label>
                        <div className="flex gap-2">
                          <button type="button" onClick={() => void savePolicy(policy.department)} className="app-button primary min-h-[44px] px-3 text-xs">Save policy</button>
                          <button type="button" onClick={() => setEditingPolicyDept(null)} className="glass-control min-h-[44px] px-3 text-xs">Cancel</button>
                        </div>
                      </div>
                    ) : (
                      <button type="button" onClick={() => startPolicyEdit(policy)} className="text-[11px] font-semibold text-[#2784ff] hover:underline">
                        Edit standing limit
                      </button>
                    )}
                  </li>
                ))}
              </ul>
            )}
          </section>

          <section aria-label="Verified connectors and MCP catalog" className="app-card p-3.5 space-y-2.5 border border-slate-200/80 dark:border-slate-700/80">
            <div className="flex items-center justify-between gap-2">
              <h3 className="text-xs font-bold text-slate-900 dark:text-slate-100">Verified Connectors</h3>
              <Link href="/integrations" className="text-xs font-semibold text-[#2784ff] hover:underline">Manage keys</Link>
            </div>
            {catalog ? (
              <>
                <ul className="space-y-2">
                  {catalog.connectors.map((connector) => (
                    <li key={connector.id} className="rounded-lg border border-slate-200/80 dark:border-slate-700/80 p-2 text-xs space-y-1">
                      <div className="flex items-center justify-between gap-2">
                        <span className="font-semibold text-slate-900 dark:text-slate-100">{connector.name}</span>
                        <span className={connectorBadgeClass(connector.status, connector.usable)}>
                          {connectorStatusLabel(connector.status, connector.usable)}
                        </span>
                      </div>
                      <p className="text-[11px] text-slate-600 dark:text-slate-400">
                        Read: {connector.read_scopes.join(", ") || "none"} · Write: {connector.write_scopes.join(", ") || "none"}
                      </p>
                    </li>
                  ))}
                </ul>
                <div className="rounded-lg bg-slate-100/80 dark:bg-slate-800/70 p-2.5 text-[11px] text-slate-700 dark:text-slate-300 space-y-0.5 tabular-nums">
                  <p className="font-semibold text-slate-900 dark:text-slate-100">
                    Inbound MCP Server ({catalog.mcp_server.protocol_version})
                  </p>
                  <p>
                    Endpoint: <code>{catalog.mcp_server.endpoint}</code> · {catalog.mcp_server.tool_count} tools
                  </p>
                  <p>
                    Default: {catalog.mcp_server.default_mode} (write opt-in: <code>{catalog.mcp_server.write_opt_in_header}</code>)
                  </p>
                </div>
              </>
            ) : (
              <p className="text-xs text-slate-600 dark:text-slate-400">
                Connector catalog unavailable. Manage provider credentials in{" "}
                <Link href="/integrations" className="underline text-[#2784ff]">Integrations</Link>.
              </p>
            )}
          </section>
        </div>
      )}
    </aside>
  );
}
