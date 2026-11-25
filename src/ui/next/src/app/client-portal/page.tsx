// SPDX-License-Identifier: Apache-2.0
"use client";
import React from "react";

import {
  CreditCard,
  FileText,
  LifeBuoy,
  MessageSquareText,
  RefreshCw,
  Repeat2,
} from "lucide-react";
import Link from "next/link";
import { useCallback, useEffect, useState } from "react";

type UsageSummary = {
  total_micros?: number;
  limit_micros?: number | null;
  total_requests?: number;
};

type SubscriptionOverview = {
  plans: Array<{ id: string; name: string; active: boolean }>;
  subscribers: Array<{ id: string; status: string }>;
  batches: Array<{ id: string; status: string }>;
};

type PortalData = {
  usage: UsageSummary | null;
  subscriptions: SubscriptionOverview | null;
};

const EMPTY_DATA: PortalData = { usage: null, subscriptions: null };

const portalActions = [
  {
    href: "/quoting",
    label: "Quotes and proposals",
    description: "Review quote details and approval status.",
    icon: FileText,
  },
  {
    href: "/subscriptions",
    label: "Subscriptions",
    description: "View plans, subscribers, and fulfillment batches.",
    icon: Repeat2,
  },
  {
    href: "/finance",
    label: "Invoices and ledger",
    description: "Manage customer invoices, payments, and ledger entries.",
    icon: CreditCard,
  },
  {
    href: "/inbox",
    label: "Support messages",
    description: "Continue customer and support conversations.",
    icon: MessageSquareText,
  },
] as const;

async function readJson<T>(url: string, signal: AbortSignal): Promise<T> {
  const response = await fetch(url, { signal, cache: "no-store" });
  if (!response.ok) throw new Error(`${url} returned HTTP ${response.status}`);
  return response.json() as Promise<T>;
}

function formatMicros(micros: number | null | undefined): string {
  if (typeof micros !== "number") return "Not reported";
  return new Intl.NumberFormat("en-US", {
    style: "currency",
    currency: "USD",
  }).format(micros / 1_000_000);
}

export default function ClientPortalPage() {
  const [data, setData] = useState<PortalData>(EMPTY_DATA);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [reloadKey, setReloadKey] = useState(0);

  const reload = useCallback(() => setReloadKey((key) => key + 1), []);

  useEffect(() => {
    const controller = new AbortController();

    async function loadPortal() {
      setLoading(true);
      setError(null);
      const [usage, subscriptions] = await Promise.allSettled([
        readJson<UsageSummary>("/api/v1/usage", controller.signal),
        readJson<SubscriptionOverview>("/api/v1/subscriptions", controller.signal),
      ]);
      if (controller.signal.aborted) return;

      const nextData = {
        usage: usage.status === "fulfilled" ? usage.value : null,
        subscriptions:
          subscriptions.status === "fulfilled" ? subscriptions.value : null,
      };
      setData(nextData);
      if (nextData.usage === null && nextData.subscriptions === null) {
        setError("Client data is currently unavailable.");
      }
      setLoading(false);
    }

    void loadPortal();
    return () => controller.abort();
  }, [reloadKey]);

  const activePlans =
    data.subscriptions?.plans.filter((plan) => plan.active).length;

  return (
    <main
      className="space-y-6"
      aria-labelledby="client-portal-heading"
      data-client-portal-state={loading ? "loading" : "settled"}
    >
      <section className="flex flex-col gap-4 border-b border-[#D2D2D7] pb-5 sm:flex-row sm:items-center sm:justify-between">
        <div>
          <p className="app-eyebrow">Customer workspace</p>
          <h2 id="client-portal-heading" className="text-xl font-semibold text-[#1D1D1F]">
            Account overview
          </h2>
          <p className="mt-1 text-sm text-[#6E6E73]">
            Live subscription, usage, invoice, and support status.
          </p>
        </div>
        <button
          type="button"
          className="app-button min-h-[44px] self-start"
          onClick={reload}
          disabled={loading}
        >
          <RefreshCw aria-hidden="true" size={16} />
          Refresh
        </button>
      </section>

      {loading ? (
        <div className="app-panel p-5 text-sm text-[#6E6E73]" role="status">
          Loading client data...
        </div>
      ) : null}

      {error ? (
        <div className="app-panel border-[#FF3B30] p-5" role="alert">
          <div className="flex items-start gap-3">
            <LifeBuoy aria-hidden="true" className="mt-0.5 text-[#C9342C]" size={18} />
            <div>
              <h3 className="font-semibold text-[#1D1D1F]">Unable to load account data</h3>
              <p className="mt-1 text-sm text-[#6E6E73]">{error}</p>
            </div>
          </div>
        </div>
      ) : null}

      {!loading && (data.usage || data.subscriptions) ? (
        <section className="grid gap-3 sm:grid-cols-2 lg:grid-cols-4" aria-label="Account status">
          <div className="app-card p-4">
            <p className="text-xs font-semibold text-[#6E6E73]">Workspace edition</p>
            <p className="mt-2 text-lg font-semibold text-[#1D1D1F]">
              Community
            </p>
          </div>
          <div className="app-card p-4">
            <p className="text-xs font-semibold text-[#6E6E73]">Recorded usage</p>
            <p className="mt-2 text-lg font-semibold text-[#1D1D1F] tabular-nums">
              {formatMicros(data.usage?.total_micros)}
            </p>
          </div>
          <div className="app-card p-4">
            <p className="text-xs font-semibold text-[#6E6E73]">Active plans</p>
            <p className="mt-2 text-lg font-semibold text-[#1D1D1F] tabular-nums">
              {activePlans ?? "Not reported"}
            </p>
          </div>
          <div className="app-card p-4">
            <p className="text-xs font-semibold text-[#6E6E73]">Spending limit</p>
            <p className="mt-2 text-lg font-semibold text-[#1D1D1F] tabular-nums">
              {formatMicros(data.usage?.limit_micros)}
            </p>
          </div>
        </section>
      ) : null}

      <section aria-labelledby="client-actions-heading">
        <h3 id="client-actions-heading" className="mb-3 text-sm font-semibold text-[#1D1D1F]">
          Account tools
        </h3>
        <div className="grid gap-3 md:grid-cols-2">
          {portalActions.map((action) => {
            const Icon = action.icon;
            return (
              <Link key={action.href} href={action.href} className="app-card p-4 transition-colors hover:border-[#0066FF]">
                <div className="flex items-start gap-3">
                  <span className="flex h-9 w-9 shrink-0 items-center justify-center rounded-md bg-[#EAF2FF] text-[#0066FF]">
                    <Icon aria-hidden="true" size={18} />
                  </span>
                  <span>
                    <span className="block font-semibold text-[#1D1D1F]">{action.label}</span>
                    <span className="mt-1 block text-sm text-[#6E6E73]">{action.description}</span>
                  </span>
                </div>
              </Link>
            );
          })}
        </div>
      </section>
    </main>
  );
}
