// SPDX-License-Identifier: Apache-2.0
"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import { getRegisteredPlanResolver } from "@/components/ExtensionProviders";
import { fetchCapabilities } from "@/lib/capabilities";
import {
  hasVerifiedOfflineQueueOwner,
  QUEUE_IDENTITY_EPOCH_KEY,
  readQueueOwner,
  sameOwner,
  subscribeQueueIdentityReadiness,
  type QueueOwner,
} from "@/lib/sync/queueIdentity";

/**
 * Resolves workspace access after verifying the active authenticated owner.
 *
 * In the single-tenant Apache-2.0 Community Edition (`saas_billing: false`),
 * every verified workspace owner receives complete single-tenant functionality
 * without calling any Cloud SaaS billing endpoint. Cloud builds may register
 * a plan resolver via `registerPlanResolver(...)` in `ExtensionProviders`.
 */
export function useProPlan() {
  const [plan, setPlan] = useState<string | null>(null);
  const [hasProAccess, setHasProAccess] = useState(false);
  const [identityReady, setIdentityReady] = useState(false);
  const [planError, setPlanError] = useState<string | null>(null);
  const [claimError, setClaimError] = useState<string | null>(null);
  const owner = useRef<QueueOwner | null>(null);
  const epoch = useRef(0);
  const active = useRef(false);
  const request = useRef<AbortController | null>(null);

  const retire = useCallback(() => {
    ++epoch.current;
    request.current?.abort();
    owner.current = null;
    setPlan(null);
    setHasProAccess(false);
    setIdentityReady(false);
    setClaimError(null);
    setPlanError("Your session changed. Refresh to verify the current workspace.");
  }, []);

  const readiness = useCallback(() => {
    const intended = owner.current;
    const ready = hasVerifiedOfflineQueueOwner(intended);
    setIdentityReady(ready);
    if (intended && hasVerifiedOfflineQueueOwner() && !ready) retire();
  }, [retire]);

  const refreshPlan = useCallback(async (): Promise<boolean> => {
    if (!active.current) return false;
    const version = ++epoch.current;
    request.current?.abort();
    const controller = new AbortController();
    request.current = controller;
    setPlan(null);
    setHasProAccess(false);
    setPlanError(null);
    setIdentityReady(false);
    const current = () => active.current && version === epoch.current;

    try {
      const verified = await readQueueOwner();
      if (!current()) return false;
      if (owner.current && !sameOwner(owner.current, verified)) {
        retire();
        return false;
      }
      owner.current = { ...verified };
      readiness();
      if (!current()) return false;

      const capabilities = await fetchCapabilities({ signal: controller.signal });
      if (!current()) return false;

      const resolver = getRegisteredPlanResolver();
      if (capabilities.features.saas_billing && resolver) {
        const resolved = await resolver({
          userId: verified.userId,
          tenantId: verified.tenantId,
          signal: controller.signal,
          capabilities,
        });
        if (!current()) return false;
        if (resolved === "session-changed") {
          retire();
          return false;
        }
        readiness();
        if (!current()) return false;
        setPlan(resolved.currentPlan);
        setHasProAccess(resolved.hasPro);
        setPlanError(resolved.planError);
        return hasVerifiedOfflineQueueOwner(verified) && resolved.hasPro;
      }

      readiness();
      if (!current()) return false;
      setPlan("Community");
      setHasProAccess(true);
      setPlanError(null);
      return hasVerifiedOfflineQueueOwner(verified);
    } catch {
      if (current()) {
        setPlan(null);
        setHasProAccess(false);
        setPlanError("Current workspace access could not be verified. Refresh to check again.");
      }
      return false;
    }
  }, [readiness, retire]);

  useEffect(() => {
    active.current = true;
    const unsubscribe = subscribeQueueIdentityReadiness(readiness);
    const storage = (event: StorageEvent) => {
      if (event.key === null || event.key === QUEUE_IDENTITY_EPOCH_KEY) retire();
    };
    window.addEventListener("omnisolo_auth_changed", retire);
    window.addEventListener("pagehide", retire);
    window.addEventListener("storage", storage);
    void refreshPlan();
    return () => {
      active.current = false;
      ++epoch.current;
      request.current?.abort();
      owner.current = null;
      unsubscribe();
      window.removeEventListener("omnisolo_auth_changed", retire);
      window.removeEventListener("pagehide", retire);
      window.removeEventListener("storage", storage);
    };
  }, [readiness, refreshPlan, retire]);

  const claimTrial = useCallback(async () => {
    if (active.current) {
      setClaimError(
        "Trial activation is unavailable because a durable grant is not verified. Self-hosted Community workspaces include full single-tenant access.",
      );
    }
    return false;
  }, []);

  return {
    ownerRevision: epoch.current,
    hasPro: identityReady && hasProAccess,
    currentPlan: identityReady ? plan : null,
    verifiedOwner: identityReady && owner.current ? { ...owner.current } : null,
    planError,
    claimError,
    claimTrial,
    refreshPlan,
  };
}
