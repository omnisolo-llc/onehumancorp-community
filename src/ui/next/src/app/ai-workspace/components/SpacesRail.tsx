"use client";

import React, { FormEvent, useId, useState } from "react";
import {
  SPACE_KINDS,
  type CreateSpaceRequest,
  type SpaceKind,
  type SpaceRecord,
  type TaskCenterTask,
} from "@/lib/taskCenterContract";
import { SpaceHealthBadge } from "./ProvenanceBadge";

export interface SpacesRailProps {
  spaces: SpaceRecord[];
  tasks: TaskCenterTask[];
  selectedSpaceId: string | null;
  attentionOnly: boolean;
  creatingSpace: boolean;
  onSelectSpace: (spaceId: string | null) => void;
  onToggleAttentionOnly: () => void;
  onCreateSpace: (input: CreateSpaceRequest) => Promise<boolean>;
  onArchiveSpace: (spaceId: string, archived: boolean) => Promise<void>;
  onApplyTemplate: (template: string, spaceId: string, department: string) => void;
}

const DEFAULT_MANIFEST_BY_KIND: Record<
  SpaceKind,
  { reads: string[]; shows: string[]; cannot: string[]; department: string }
> = {
  operations: {
    reads: ["Tenant tasks", "Daily operations summary"],
    shows: ["Live task stream", "Pending approvals"],
    cannot: ["Spend without approval", "Send external email automatically"],
    department: "operations",
  },
  client_delivery: {
    reads: ["Client tasks", "Verified Drive files"],
    shows: ["Deliverable checklists", "Pinned artifacts"],
    cannot: ["Publish externally without approval", "Share private tenant records"],
    department: "operations",
  },
  cash_recovery: {
    reads: ["Invoice drafts", "Usage budget ledger"],
    shows: ["Overdue follow-up queue", "Cost ceiling guardrails"],
    cannot: ["Charge customers without approval", "Issue refunds autonomously"],
    department: "finance",
  },
  research: {
    reads: ["Workspace records", "Verified connected docs"],
    shows: ["Structured markdown briefs", "Source health badges"],
    cannot: ["Modify production data", "Execute unapproved side effects"],
    department: "marketing",
  },
  custom: {
    reads: ["Tenant task center records"],
    shows: ["Declarative Space cards"],
    cannot: ["Spend without approval", "Run unsandboxed HTML or scripts"],
    department: "operations",
  },
};

function parseListInput(raw: string): string[] {
  return raw
    .split(/[\n,]/)
    .map((item) => item.trim())
    .filter(Boolean);
}

function spaceKindLabel(kind: SpaceKind): string {
  switch (kind) {
    case "operations":
      return "Operations";
    case "client_delivery":
      return "Client Delivery";
    case "cash_recovery":
      return "Cash Recovery";
    case "research":
      return "Research";
    case "custom":
      return "Custom";
  }
}

export function SpacesRail({
  spaces,
  tasks,
  selectedSpaceId,
  attentionOnly,
  creatingSpace,
  onSelectSpace,
  onToggleAttentionOnly,
  onCreateSpace,
  onArchiveSpace,
  onApplyTemplate,
}: SpacesRailProps) {
  const [showCreateForm, setShowCreateForm] = useState(false);
  const [showArchived, setShowArchived] = useState(false);
  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [kind, setKind] = useState<SpaceKind>("operations");
  const [department, setDepartment] = useState("operations");
  const [promptTemplate, setPromptTemplate] = useState("");
  const [readsText, setReadsText] = useState(
    DEFAULT_MANIFEST_BY_KIND.operations.reads.join(", "),
  );
  const [showsText, setShowsText] = useState(
    DEFAULT_MANIFEST_BY_KIND.operations.shows.join(", "),
  );
  const [cannotText, setCannotText] = useState(
    DEFAULT_MANIFEST_BY_KIND.operations.cannot.join(", "),
  );
  const [formError, setFormError] = useState<string | null>(null);

  const nameId = useId();
  const kindId = useId();
  const deptId = useId();
  const descId = useId();
  const promptId = useId();
  const readsId = useId();
  const showsId = useId();
  const cannotId = useId();

  const attentionCount = tasks.filter(
    (task) => !task.archived && task.attention !== "none",
  ).length;

  const visibleSpaces = spaces.filter((space) =>
    showArchived ? true : !space.archived,
  );
  const selectedSpace =
    spaces.find((space) => space.id === selectedSpaceId) ?? null;

  function handleKindChange(nextKind: SpaceKind) {
    setKind(nextKind);
    const defaults = DEFAULT_MANIFEST_BY_KIND[nextKind];
    setDepartment(defaults.department);
    setReadsText(defaults.reads.join(", "));
    setShowsText(defaults.shows.join(", "));
    setCannotText(defaults.cannot.join(", "));
  }

  async function handleSubmit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    const trimmedName = name.trim();
    const reads = parseListInput(readsText);
    const shows = parseListInput(showsText);
    const cannot = parseListInput(cannotText);

    if (!trimmedName || trimmedName.length > 80) {
      setFormError("Space name must be between 1 and 80 characters.");
      return;
    }
    if (reads.length === 0 || shows.length === 0 || cannot.length === 0) {
      setFormError(
        "Specify at least one entry for Reads, Shows, and Cannot capabilities.",
      );
      return;
    }

    setFormError(null);
    const created = await onCreateSpace({
      name: trimmedName,
      description: description.trim() || `${spaceKindLabel(kind)} Space`,
      kind,
      prompt_template: promptTemplate.trim(),
      department: department.trim() || "operations",
      reads,
      shows,
      cannot,
    });
    if (created) {
      setName("");
      setDescription("");
      setPromptTemplate("");
      setShowCreateForm(false);
    }
  }

  return (
    <aside
      aria-label="Spaces and attention navigation"
      className="app-panel flex flex-col gap-4 p-4 h-fit"
    >
      <div className="flex items-center justify-between gap-2">
        <div>
          <h2 className="text-sm font-bold text-slate-900 dark:text-slate-100">
            Spaces
          </h2>
          <p className="text-xs text-slate-600 dark:text-slate-400">
            Declarative scoped views
          </p>
        </div>
        <button
          type="button"
          onClick={() => setShowCreateForm((prev) => !prev)}
          aria-expanded={showCreateForm}
          className="app-button primary min-h-[44px] px-3 text-xs focus-visible:outline-2 focus-visible:outline-[#2784ff] focus-visible:outline-offset-2"
        >
          {showCreateForm ? "Close" : "New Space"}
        </button>
      </div>

      <button
        type="button"
        aria-pressed={attentionOnly}
        onClick={onToggleAttentionOnly}
        className={`flex items-center justify-between rounded-lg border px-3 min-h-[44px] text-xs font-semibold transition-colors motion-reduce:transition-none focus-visible:outline-2 focus-visible:outline-[#2784ff] focus-visible:outline-offset-2 ${
          attentionOnly
            ? "border-[#2784ff] bg-[#2784ff]/10 text-[#0f4eb8] dark:text-blue-200"
            : "border-slate-200 dark:border-slate-700 bg-white/70 dark:bg-slate-900/60 text-slate-800 dark:text-slate-200 hover:bg-slate-50 dark:hover:bg-slate-800/70"
        }`}
      >
        <span>Needs attention only</span>
        <span className="app-badge warn tabular-nums">{attentionCount}</span>
      </button>

      {showCreateForm && (
        <form
          onSubmit={handleSubmit}
          aria-label="Create Space form"
          className="app-card p-3 space-y-2.5 border border-slate-200 dark:border-slate-700"
        >
          <p className="text-xs font-bold text-slate-900 dark:text-slate-100">
            Prompt-to-Space manifest
          </p>
          {formError && (
            <p role="alert" className="text-xs font-medium text-rose-700 dark:text-rose-300">
              {formError}
            </p>
          )}
          <div>
            <label htmlFor={nameId} className="block text-xs font-medium mb-1">
              Space name
            </label>
            <input
              id={nameId}
              type="text"
              value={name}
              onChange={(event) => setName(event.target.value)}
              maxLength={80}
              required
              placeholder="e.g. Overdue Invoice Recovery"
              className="glass-control w-full px-2.5 min-h-[44px] text-sm"
            />
          </div>
          <div className="grid grid-cols-2 gap-2">
            <div>
              <label htmlFor={kindId} className="block text-xs font-medium mb-1">
                Kind
              </label>
              <select
                id={kindId}
                value={kind}
                onChange={(event) =>
                  handleKindChange(event.target.value as SpaceKind)
                }
                className="glass-control w-full px-2 min-h-[44px] text-xs"
              >
                {SPACE_KINDS.map((option) => (
                  <option key={option} value={option}>
                    {spaceKindLabel(option)}
                  </option>
                ))}
              </select>
            </div>
            <div>
              <label htmlFor={deptId} className="block text-xs font-medium mb-1">
                Department
              </label>
              <select
                id={deptId}
                value={department}
                onChange={(event) => setDepartment(event.target.value)}
                className="glass-control w-full px-2 min-h-[44px] text-xs"
              >
                <option value="operations">Operations</option>
                <option value="finance">Finance</option>
                <option value="sales">Sales</option>
                <option value="marketing">Marketing</option>
              </select>
            </div>
          </div>
          <div>
            <label htmlFor={descId} className="block text-xs font-medium mb-1">
              Description
            </label>
            <input
              id={descId}
              type="text"
              value={description}
              onChange={(event) => setDescription(event.target.value)}
              maxLength={240}
              placeholder="What this Space tracks..."
              className="glass-control w-full px-2.5 min-h-[44px] text-xs"
            />
          </div>
          <div>
            <label htmlFor={promptId} className="block text-xs font-medium mb-1">
              Prompt template
            </label>
            <textarea
              id={promptId}
              rows={2}
              value={promptTemplate}
              onChange={(event) => setPromptTemplate(event.target.value)}
              placeholder="Optional reusable starting prompt..."
              className="glass-control w-full p-2 text-xs"
            />
          </div>
          <div>
            <label htmlFor={readsId} className="block text-xs font-medium mb-1">
              Reads (comma-separated)
            </label>
            <input
              id={readsId}
              type="text"
              value={readsText}
              onChange={(event) => setReadsText(event.target.value)}
              className="glass-control w-full px-2.5 min-h-[44px] text-xs"
            />
          </div>
          <div>
            <label htmlFor={showsId} className="block text-xs font-medium mb-1">
              Shows (comma-separated)
            </label>
            <input
              id={showsId}
              type="text"
              value={showsText}
              onChange={(event) => setShowsText(event.target.value)}
              className="glass-control w-full px-2.5 min-h-[44px] text-xs"
            />
          </div>
          <div>
            <label htmlFor={cannotId} className="block text-xs font-medium mb-1">
              Cannot (explicit guardrails)
            </label>
            <input
              id={cannotId}
              type="text"
              value={cannotText}
              onChange={(event) => setCannotText(event.target.value)}
              className="glass-control w-full px-2.5 min-h-[44px] text-xs"
            />
          </div>
          <button
            type="submit"
            disabled={creatingSpace || !name.trim()}
            className="app-button primary w-full min-h-[44px] text-xs disabled:opacity-50"
          >
            {creatingSpace ? "Creating Space..." : "Create Space"}
          </button>
        </form>
      )}

      <nav aria-label="Spaces list" className="flex flex-col gap-1.5">
        <button
          type="button"
          aria-pressed={selectedSpaceId === null}
          disabled={selectedSpaceId === null && !attentionOnly}
          onClick={() => onSelectSpace(null)}
          className={`flex items-center justify-between rounded-lg px-3 min-h-[44px] text-left text-xs font-semibold transition-colors motion-reduce:transition-none focus-visible:outline-2 focus-visible:outline-[#2784ff] focus-visible:outline-offset-2 ${
            selectedSpaceId === null
              ? "bg-[#2784ff] text-white disabled:cursor-default"
              : "text-slate-800 dark:text-slate-200 hover:bg-slate-100 dark:hover:bg-slate-800"
          }`}
        >
          <span>All Spaces</span>
          <span className="tabular-nums opacity-90">
            {tasks.filter((task) => !task.archived).length}
          </span>
        </button>

        {visibleSpaces.length === 0 ? (
          <p className="px-2 py-3 text-xs text-slate-600 dark:text-slate-400">
            No Spaces yet. Create a scoped Space to group tasks and pinned
            artifacts.
          </p>
        ) : (
          visibleSpaces.map((space) => {
            const spaceTasks = tasks.filter(
              (task) => task.space_id === space.id && !task.archived,
            );
            const active = selectedSpaceId === space.id;
            return (
              <button
                key={space.id}
                type="button"
                aria-pressed={active}
                onClick={() => onSelectSpace(space.id)}
                className={`flex flex-col gap-1 rounded-lg border px-3 py-2.5 text-left transition-colors motion-reduce:transition-none focus-visible:outline-2 focus-visible:outline-[#2784ff] focus-visible:outline-offset-2 ${
                  active
                    ? "border-[#2784ff] bg-[#2784ff]/10 text-slate-900 dark:text-white"
                    : "border-transparent hover:bg-slate-100/80 dark:hover:bg-slate-800/70 text-slate-800 dark:text-slate-200"
                }`}
              >
                <div className="flex items-center justify-between gap-2 w-full">
                  <span className="text-xs font-bold truncate">
                    {space.name}
                    {space.archived ? " (Archived)" : ""}
                  </span>
                  <span className="text-[11px] font-semibold tabular-nums text-slate-600 dark:text-slate-400">
                    {spaceTasks.length}
                  </span>
                </div>
                <div className="flex items-center justify-between gap-2 w-full">
                  <span className="text-[11px] text-slate-600 dark:text-slate-400 capitalize">
                    {space.department}
                  </span>
                  <SpaceHealthBadge health={space.source_health} />
                </div>
              </button>
            );
          })
        )}

        {spaces.some((space) => space.archived) && (
          <button
            type="button"
            onClick={() => setShowArchived((prev) => !prev)}
            className="mt-1 text-left px-2 py-1 text-xs font-medium text-slate-600 dark:text-slate-400 hover:underline"
          >
            {showArchived ? "Hide archived Spaces" : "Show archived Spaces"}
          </button>
        )}
      </nav>

      {selectedSpace && (
        <section
          aria-label={`Capability manifest for ${selectedSpace.name}`}
          data-testid="space-capability-card"
          className="app-card p-3.5 space-y-3 border border-slate-200/90 dark:border-slate-700/80"
        >
          <div className="flex items-start justify-between gap-2">
            <div className="min-w-0">
              <h3 className="text-xs font-bold text-slate-900 dark:text-slate-100 truncate">
                {selectedSpace.name}
              </h3>
              <p className="text-xs text-slate-600 dark:text-slate-400">
                {selectedSpace.description}
              </p>
            </div>
            <SpaceHealthBadge health={selectedSpace.source_health} />
          </div>

          <dl className="space-y-2 text-xs">
            <div>
              <dt className="font-bold text-emerald-800 dark:text-emerald-300">
                Reads
              </dt>
              <dd className="mt-0.5 text-slate-700 dark:text-slate-200">
                {selectedSpace.reads.length > 0
                  ? selectedSpace.reads.join(" · ")
                  : "None"}
              </dd>
            </div>
            <div>
              <dt className="font-bold text-blue-800 dark:text-blue-300">
                Shows
              </dt>
              <dd className="mt-0.5 text-slate-700 dark:text-slate-200">
                {selectedSpace.shows.length > 0
                  ? selectedSpace.shows.join(" · ")
                  : "None"}
              </dd>
            </div>
            <div>
              <dt className="font-bold text-rose-800 dark:text-rose-300">
                Cannot
              </dt>
              <dd className="mt-0.5 text-slate-700 dark:text-slate-200">
                {selectedSpace.cannot.length > 0
                  ? selectedSpace.cannot.join(" · ")
                  : "None declared"}
              </dd>
            </div>
          </dl>

          <div className="flex flex-wrap gap-2 pt-1">
            {selectedSpace.prompt_template.trim() && (
              <button
                type="button"
                onClick={() =>
                  onApplyTemplate(
                    selectedSpace.prompt_template,
                    selectedSpace.id,
                    selectedSpace.department,
                  )
                }
                className="glass-control px-2.5 min-h-[44px] text-xs font-semibold"
              >
                Use template
              </button>
            )}
            <button
              type="button"
              onClick={() =>
                void onArchiveSpace(selectedSpace.id, !selectedSpace.archived)
              }
              className="glass-control px-2.5 min-h-[44px] text-xs font-medium text-slate-700 dark:text-slate-300"
            >
              {selectedSpace.archived ? "Unarchive Space" : "Archive Space"}
            </button>
          </div>
        </section>
      )}
    </aside>
  );
}
