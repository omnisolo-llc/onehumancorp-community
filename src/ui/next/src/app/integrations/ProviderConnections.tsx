"use client";

import { FormEvent, useCallback, useEffect, useRef, useState } from "react";
import type { ConnectorAuthMode, ConnectorCategory, ConnectorId } from "@/lib/taskCenterContract";

type ProviderId = ConnectorId;
type Connection = { id: ProviderId; status: string; usable: boolean };

interface ProviderDefinition {
  id: ProviderId;
  title: string;
  category: ConnectorCategory;
  authMode: ConnectorAuthMode;
  credentialLabel: string;
  description: string;
  scopeNote: string;
}

const PROVIDERS: readonly ProviderDefinition[] = [
  {
    id: "openai_api",
    title: "OpenAI API",
    category: "ai_provider",
    authMode: "api_key",
    credentialLabel: "OpenAI API API key",
    description:
      "Use your own API account for supported AI requests. API usage is billed by your provider; a ChatGPT subscription is not an API key.",
    scopeNote: "Inference requests billed directly to your OpenAI API account (not rebilled by OHC).",
  },
  {
    id: "anthropic_api",
    title: "Anthropic API",
    category: "ai_provider",
    authMode: "api_key",
    credentialLabel: "Anthropic API API key",
    description:
      "Use your own Anthropic API key for supported AI analysis and task steps. Consumer Claude subscriptions are not accepted as API keys.",
    scopeNote: "Inference requests billed directly to your Anthropic API account (not rebilled by OHC).",
  },
  {
    id: "stripe",
    title: "Stripe payments",
    category: "payments",
    authMode: "api_key",
    credentialLabel: "Stripe payments API key",
    description:
      "Create real payment requests in your business account. Connecting does not charge a customer or confirm that an invoice is paid.",
    scopeNote: "Financial commitments still require owner approval or explicit standing authority.",
  },
  {
    id: "resend",
    title: "Resend email",
    category: "email",
    authMode: "api_key",
    credentialLabel: "Resend email API key",
    description:
      "Send verified business emails through your Resend account. Connecting does not send messages automatically; external sends require approval.",
    scopeNote: "Outbound email delivery is gated behind owner approval unless explicitly authorized.",
  },
  {
    id: "google_workspace",
    title: "Google Workspace",
    category: "workspace",
    authMode: "oauth_bearer",
    credentialLabel: "Google Workspace OAuth bearer token",
    description:
      "Verify a read-only OAuth bearer token to list Drive files in your business workspace. Consumer subscription tokens are not accepted.",
    scopeNote: "Read-only Drive listing by default; write or send actions remain separately gated.",
  },
];

const PROVIDER_IDS = new Set<string>(PROVIDERS.map((provider) => provider.id));

function isProviderId(value: unknown): value is ProviderId {
  return typeof value === "string" && PROVIDER_IDS.has(value);
}

const confirmed = (
  value: unknown,
): value is { success: true; usable: true; status: "verified" } => {
  if (!value || typeof value !== "object") return false;
  const item = value as Record<string, unknown>;
  return (
    item.success === true &&
    item.usable === true &&
    item.status === "verified"
  );
};

function statusLabel(state: Connection | undefined): string {
  if (state?.usable === true) return "Verified connection";
  if (state?.status === "revoked") return "Disconnected";
  if (
    state?.status === "reauthentication_required" ||
    state?.status === "verification_required"
  ) {
    return "Verification required";
  }
  return "Not connected";
}

function statusBadgeTone(state: Connection | undefined): string {
  if (state?.usable === true) return "app-badge good";
  if (state?.status === "revoked") return "app-badge bad";
  if (
    state?.status === "reauthentication_required" ||
    state?.status === "verification_required"
  ) {
    return "app-badge warn";
  }
  return "app-badge";
}

export default function ProviderConnections() {
  const [connections, setConnections] = useState<
    Partial<Record<ProviderId, Connection>>
  >({});
  const [editing, setEditing] = useState<ProviderId | null>(null);
  const [disconnecting, setDisconnecting] = useState<ProviderId | null>(null);
  const [secret, setSecret] = useState("");
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState("");
  const generation = useRef(0);

  const load = useCallback(async () => {
    const expected = ++generation.current;
    try {
      const response = await fetch("/api/v1/integrations");
      const data: unknown = await response.json();
      if (
        !response.ok ||
        !data ||
        typeof data !== "object" ||
        !Array.isArray((data as { integrations?: unknown }).integrations)
      ) {
        throw new Error("Connection status unavailable");
      }
      const next: Partial<Record<ProviderId, Connection>> = {};
      for (const value of (data as { integrations: unknown[] }).integrations) {
        if (!value || typeof value !== "object") continue;
        const row = value as Record<string, unknown>;
        if (isProviderId(row.id) && typeof row.status === "string") {
          next[row.id] = {
            id: row.id,
            status: row.status,
            usable: row.usable === true && row.status === "verified",
          };
        }
      }
      if (generation.current === expected) setConnections(next);
    } catch {
      if (generation.current === expected) {
        setMessage(
          "Provider connection status is unavailable. No connection has been assumed.",
        );
      }
    }
  }, []);

  useEffect(() => {
    void load();
    return () => {
      generation.current++;
    };
  }, [load]);

  async function save(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (!editing || !secret.trim() || busy) return;
    const provider = editing;
    generation.current++;
    setBusy(true);
    setMessage("");
    try {
      const response = await fetch(`/api/v1/integrations/${provider}/connect`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ api_token: secret.trim() }),
      });
      const result: unknown = await response.json();
      if (!response.ok || !confirmed(result)) {
        throw new Error("Provider verification failed");
      }
      setEditing(null);
      setMessage(
        "The provider verified your credential. It is stored encrypted for this business.",
      );
      await load();
    } catch {
      // Never display an upstream body that could echo credentials.
      setMessage(
        "The key could not be verified. Owner access and a valid provider API key or OAuth token are required; no new connection was confirmed.",
      );
    } finally {
      setSecret("");
      setBusy(false);
    }
  }

  async function recheck(provider: ProviderId) {
    if (busy) return;
    generation.current++;
    setBusy(true);
    setMessage("");
    try {
      const response = await fetch(`/api/v1/integrations/${provider}/verify`, {
        method: "POST",
      });
      const result: unknown = await response.json();
      if (!response.ok || !confirmed(result)) {
        throw new Error("Verification unavailable");
      }
      setMessage("Connection verified again with the provider.");
    } catch {
      setMessage(
        "Reverification did not succeed. New requests may remain paused until the connection is verified.",
      );
    } finally {
      await load();
      setBusy(false);
    }
  }

  async function disconnect(provider: ProviderId) {
    if (busy) return;
    generation.current++;
    setBusy(true);
    setMessage("");
    try {
      const response = await fetch(`/api/v1/integrations/${provider}`, {
        method: "DELETE",
      });
      const result: unknown = await response.json();
      if (
        !response.ok ||
        !result ||
        typeof result !== "object" ||
        (result as { success?: unknown }).success !== true ||
        (result as { status?: unknown }).status !== "revoked"
      ) {
        throw new Error("Revocation unavailable");
      }
      setDisconnecting(null);
      setMessage(
        "Disconnected for new requests. Requests already accepted by the provider may still finish.",
      );
      await load();
    } catch {
      setMessage(
        "The connection could not be revoked. It has not been marked disconnected; try again.",
      );
    } finally {
      setBusy(false);
    }
  }

  const verifiedCount = PROVIDERS.filter(
    (provider) => connections[provider.id]?.usable === true,
  ).length;

  return (
    <section
      className="space-y-4 mb-8"
      aria-labelledby="verified-provider-title"
    >
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div>
          <h2 id="verified-provider-title" className="text-xl font-semibold">
            Verified business connections
          </h2>
          <p className="text-sm text-slate-600 dark:text-slate-300">
            Only an owner can connect, replace, verify or revoke these
            credentials. Secrets are never saved in browser storage and expire
            from active verification after 7 days without recheck.
          </p>
        </div>
        <span className="app-badge tabular-nums">
          {verifiedCount}/{PROVIDERS.length} verified
        </span>
      </div>

      {message && (
        <p role="status" aria-live="polite" className="app-card p-3 text-sm">
          {message}
        </p>
      )}

      <div className="grid gap-4 md:grid-cols-2 lg:grid-cols-3">
        {PROVIDERS.map((provider) => {
          const state = connections[provider.id];
          const active = state?.usable === true;
          const label = statusLabel(state);
          return (
            <article
              key={provider.id}
              aria-label={provider.title}
              className="app-card p-4 rounded-2xl space-y-3 flex flex-col justify-between"
            >
              <div className="space-y-2">
                <div className="flex items-start justify-between gap-2">
                  <div>
                    <h3 className="font-semibold text-slate-900 dark:text-slate-100">
                      {provider.title}
                    </h3>
                    <span className="text-xs text-slate-600 dark:text-slate-400">
                      {provider.authMode === "oauth_bearer"
                        ? "OAuth bearer token"
                        : "API key"}
                    </span>
                  </div>
                  <span className={statusBadgeTone(state)}>{label}</span>
                </div>
                <p className="text-sm text-slate-700 dark:text-slate-200">
                  {provider.description}
                </p>
                <p className="text-xs text-slate-600 dark:text-slate-400">
                  {provider.scopeNote}
                </p>
              </div>

              <div className="space-y-3 pt-1">
                <div className="flex flex-wrap gap-2">
                  <button
                    type="button"
                    className="glass-control px-3 min-h-[44px] rounded-lg text-sm font-medium focus-visible:outline-2 focus-visible:outline-[#2784ff] focus-visible:outline-offset-2"
                    disabled={busy}
                    onClick={() => {
                      setSecret("");
                      setEditing(provider.id);
                      setDisconnecting(null);
                      setMessage("");
                    }}
                  >
                    {active
                      ? `Replace ${provider.title} key`
                      : `Configure ${provider.title}`}
                  </button>
                  {state && state.status !== "revoked" && (
                    <>
                      <button
                        type="button"
                        className="glass-control px-3 min-h-[44px] rounded-lg text-sm font-medium focus-visible:outline-2 focus-visible:outline-[#2784ff] focus-visible:outline-offset-2"
                        disabled={busy}
                        onClick={() => void recheck(provider.id)}
                      >
                        Recheck {provider.title}
                      </button>
                      <button
                        type="button"
                        className="glass-control px-3 min-h-[44px] rounded-lg text-sm font-medium focus-visible:outline-2 focus-visible:outline-[#2784ff] focus-visible:outline-offset-2"
                        disabled={busy}
                        onClick={() => {
                          setEditing(null);
                          setSecret("");
                          setDisconnecting(provider.id);
                        }}
                      >
                        Disconnect {provider.title}
                      </button>
                    </>
                  )}
                </div>

                {editing === provider.id && (
                  <form onSubmit={save} className="space-y-3 pt-2 border-t border-slate-200/70 dark:border-slate-700/70">
                    <label
                      className="block text-sm font-medium"
                      htmlFor={`provider-key-${provider.id}`}
                    >
                      {provider.credentialLabel}
                    </label>
                    <input
                      id={`provider-key-${provider.id}`}
                      className="glass-control block w-full px-3 min-h-[44px] rounded-lg focus-visible:outline-2 focus-visible:outline-[#2784ff] focus-visible:outline-offset-2"
                      type="password"
                      autoComplete="new-password"
                      spellCheck={false}
                      maxLength={4096}
                      value={secret}
                      onChange={(event) => setSecret(event.target.value)}
                      disabled={busy}
                      required
                    />
                    <div className="flex gap-2 flex-wrap">
                      <button
                        type="submit"
                        className="app-button primary px-3 min-h-[44px] rounded-lg disabled:opacity-50"
                        disabled={busy || !secret.trim()}
                      >
                        Verify and save key
                      </button>
                      <button
                        type="button"
                        className="glass-control px-3 min-h-[44px] rounded-lg"
                        disabled={busy}
                        onClick={() => {
                          setEditing(null);
                          setSecret("");
                        }}
                      >
                        Cancel key change
                      </button>
                    </div>
                  </form>
                )}

                {disconnecting === provider.id && (
                  <div
                    role="group"
                    aria-label={`Confirm disconnect ${provider.title}`}
                    className="space-y-2 pt-2 border-t border-slate-200/70 dark:border-slate-700/70"
                  >
                    <p className="text-sm text-slate-700 dark:text-slate-200">
                      Stop new requests through this connection? This cannot
                      cancel an action the provider already accepted.
                    </p>
                    <div className="flex flex-wrap gap-2">
                      <button
                        type="button"
                        className="glass-control px-3 min-h-[44px] rounded-lg font-medium text-red-700 dark:text-red-300"
                        disabled={busy}
                        onClick={() => void disconnect(provider.id)}
                      >
                        Confirm disconnect
                      </button>
                      <button
                        type="button"
                        className="glass-control px-3 min-h-[44px] rounded-lg"
                        disabled={busy}
                        onClick={() => setDisconnecting(null)}
                      >
                        Keep connection
                      </button>
                    </div>
                  </div>
                )}
              </div>
            </article>
          );
        })}
      </div>
    </section>
  );
}
