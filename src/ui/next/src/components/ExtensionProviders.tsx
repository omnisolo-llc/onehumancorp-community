// SPDX-License-Identifier: Apache-2.0
"use client";

import React, {
  createContext,
  useContext,
  useEffect,
  useState,
  type ComponentType,
  type ReactNode,
} from "react";
import * as nextNavigation from "next/navigation";
import {
  COMMUNITY_DEFAULT_CAPABILITIES,
  fetchCapabilities,
  parseCapabilities,
  type RuntimeCapabilities,
} from "@/lib/capabilities";

const useOptionalPathname: () => string | null =
  "usePathname" in nextNavigation && typeof nextNavigation.usePathname === "function"
    ? nextNavigation.usePathname
    : () => null;

export type CapabilitiesContextValue = Readonly<{
  capabilities: RuntimeCapabilities;
  loaded: boolean;
}>;

const DEFAULT_CONTEXT: CapabilitiesContextValue = Object.freeze({
  capabilities: COMMUNITY_DEFAULT_CAPABILITIES,
  loaded: false,
});

const CapabilitiesContext = createContext<CapabilitiesContextValue>(DEFAULT_CONTEXT);

export function CapabilitiesProvider({
  children,
  initialCapabilities,
  skipFetch = false,
}: {
  children: ReactNode;
  initialCapabilities?: RuntimeCapabilities;
  skipFetch?: boolean;
}) {
  const [capabilities, setCapabilities] = useState<RuntimeCapabilities>(
    () => (initialCapabilities ? parseCapabilities(initialCapabilities) : COMMUNITY_DEFAULT_CAPABILITIES),
  );
  const [loaded, setLoaded] = useState<boolean>(Boolean(initialCapabilities || skipFetch));
  const pathname = useOptionalPathname();

  useEffect(() => {
    // A share-card document is immediately replaced; skip startup capability
    // fetches here so navigation is never raced by abandoned requests.
    if (skipFetch || initialCapabilities || pathname === "/share-card") return;

    const controller = new AbortController();
    let active = true;

    void fetchCapabilities({ signal: controller.signal }).then((resolved) => {
      if (!active || controller.signal.aborted) return;
      setCapabilities(resolved);
      setLoaded(true);
    });

    return () => {
      active = false;
      controller.abort();
    };
  }, [initialCapabilities, pathname, skipFetch]);

  return (
    <CapabilitiesContext.Provider value={{ capabilities, loaded }}>
      {children}
    </CapabilitiesContext.Provider>
  );
}

export function useCapabilities(): CapabilitiesContextValue {
  return useContext(CapabilitiesContext);
}

export type ExtensionProviderProps = Readonly<{
  children: ReactNode;
  capabilities: RuntimeCapabilities;
}>;

export type ExtensionProviderComponent = ComponentType<ExtensionProviderProps>;

export type LoginExtensionProps = Readonly<{
  next: string;
  capabilities: RuntimeCapabilities;
  onAuthNavigation: () => void;
}>;

export type LoginExtensionComponent = ComponentType<LoginExtensionProps>;

export type PlanStatus = Readonly<{
  hasPro: boolean;
  currentPlan: string | null;
  planError: string | null;
}>;

export type PlanResolver = (context: Readonly<{
  userId: string;
  tenantId: string;
  signal: AbortSignal;
  capabilities: RuntimeCapabilities;
}>) => Promise<PlanStatus | "session-changed">;

const registeredProviders: ExtensionProviderComponent[] = [];
const registeredLoginExtensions: LoginExtensionComponent[] = [];
let registeredPlanResolver: PlanResolver | null = null;

/**
 * Registers a root provider component into the pluggable `<ExtensionProviders />`
 * slot. Community builds keep this registry empty; Cloud builds register
 * proprietary providers during bootstrap.
 */
export function registerExtensionProvider(provider: ExtensionProviderComponent): () => void {
  if (!registeredProviders.includes(provider)) {
    registeredProviders.push(provider);
  }
  return () => {
    const index = registeredProviders.indexOf(provider);
    if (index >= 0) registeredProviders.splice(index, 1);
  };
}

/**
 * Registers an edition-specific login extension component (for example,
 * enterprise SSO buttons in Cloud deployments).
 */
export function registerLoginExtension(component: LoginExtensionComponent): () => void {
  if (!registeredLoginExtensions.includes(component)) {
    registeredLoginExtensions.push(component);
  }
  return () => {
    const index = registeredLoginExtensions.indexOf(component);
    if (index >= 0) registeredLoginExtensions.splice(index, 1);
  };
}

/**
 * Registers an edition-specific plan resolver used when `saas_billing` is
 * enabled by runtime capabilities.
 */
export function registerPlanResolver(resolver: PlanResolver | null): () => void {
  registeredPlanResolver = resolver;
  return () => {
    if (registeredPlanResolver === resolver) {
      registeredPlanResolver = null;
    }
  };
}

export function getRegisteredPlanResolver(): PlanResolver | null {
  return registeredPlanResolver;
}

function ExtensionSlotComposer({
  children,
  extensions,
}: {
  children: ReactNode;
  extensions?: readonly ExtensionProviderComponent[];
}) {
  const { capabilities } = useCapabilities();
  const activeProviders = extensions ?? registeredProviders;

  return activeProviders.reduceRight<ReactNode>(
    (inner, Provider) => <Provider capabilities={capabilities}>{inner}</Provider>,
    children,
  );
}

/**
 * Pluggable root extension slot.
 *
 * In the open-source single-tenant Community Edition this acts as a lightweight
 * capability-aware pass-through with zero proprietary runtime dependencies.
 * Cloud builds register or pass proprietary providers without modifying the OSS
 * root layout.
 */
export function ExtensionProviders({
  children,
  extensions,
  initialCapabilities,
  skipCapabilityFetch = false,
}: {
  children: ReactNode;
  extensions?: readonly ExtensionProviderComponent[];
  initialCapabilities?: RuntimeCapabilities;
  skipCapabilityFetch?: boolean;
}) {
  return (
    <CapabilitiesProvider
      initialCapabilities={initialCapabilities}
      skipFetch={skipCapabilityFetch}
    >
      <ExtensionSlotComposer extensions={extensions}>
        {children}
      </ExtensionSlotComposer>
    </CapabilitiesProvider>
  );
}

export function LoginExtensionsSlot({
  next,
  onAuthNavigation,
  extensions,
}: {
  next: string;
  onAuthNavigation: () => void;
  extensions?: readonly LoginExtensionComponent[];
}) {
  const { capabilities } = useCapabilities();
  const activeExtensions = extensions ?? registeredLoginExtensions;
  if (activeExtensions.length === 0) return null;

  return (
    <>
      {activeExtensions.map((Extension, index) => (
        <Extension
          key={Extension.displayName || Extension.name || `login-ext-${index}`}
          next={next}
          capabilities={capabilities}
          onAuthNavigation={onAuthNavigation}
        />
      ))}
    </>
  );
}
