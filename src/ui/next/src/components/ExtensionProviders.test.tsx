// SPDX-License-Identifier: Apache-2.0
import React from "react";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import {
  ExtensionProviders,
  LoginExtensionsSlot,
  getRegisteredPlanResolver,
  registerExtensionProvider,
  registerLoginExtension,
  registerPlanResolver,
  useCapabilities,
  type ExtensionProviderProps,
  type LoginExtensionProps,
} from "./ExtensionProviders";
import { COMMUNITY_DEFAULT_CAPABILITIES } from "@/lib/capabilities";

function CapabilitiesProbe() {
  const { capabilities, loaded } = useCapabilities();
  return (
    <output data-testid="capabilities-probe">
      {`${capabilities.edition}:${String(loaded)}:${String(capabilities.features.oidc)}:${String(capabilities.features.cloud_sync)}`}
    </output>
  );
}

afterEach(() => {
  cleanup();
  registerPlanResolver(null);
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe("ExtensionProviders", () => {
  it("acts as a clean single-tenant community pass-through when no extensions are registered", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => new Response("Not Found", { status: 404 })),
    );

    render(
      <ExtensionProviders>
        <div data-testid="app-content">Workspace Ready</div>
        <CapabilitiesProbe />
      </ExtensionProviders>,
    );

    expect(screen.getByTestId("app-content")).toHaveTextContent("Workspace Ready");
    await waitFor(() =>
      expect(screen.getByTestId("capabilities-probe")).toHaveTextContent("community:true:false:false"),
    );
  });

  it("composes registered extension providers with negotiated runtime capabilities", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () =>
        Response.json({
          edition: "cloud",
          features: { cloud_sync: true, oidc: true, saas_billing: true, multitenant: true },
        }),
      ),
    );

    function TestWrapper({ children, capabilities }: ExtensionProviderProps) {
      return (
        <section data-testid="extension-wrapper" data-edition={capabilities.edition}>
          {children}
        </section>
      );
    }

    const unregister = registerExtensionProvider(TestWrapper);
    try {
      render(
        <ExtensionProviders>
          <CapabilitiesProbe />
        </ExtensionProviders>,
      );

      await waitFor(() =>
        expect(screen.getByTestId("extension-wrapper")).toHaveAttribute("data-edition", "cloud"),
      );
      expect(screen.getByTestId("capabilities-probe")).toHaveTextContent("cloud:true:true:true");
    } finally {
      unregister();
    }
  });

  it("renders registered login extensions inside LoginExtensionsSlot and cleans up on unregister", () => {
    function SampleSsoSlot({ next, capabilities }: LoginExtensionProps) {
      return (
        <a href={`/sso?next=${encodeURIComponent(next)}`} data-edition={capabilities.edition}>
          Enterprise SSO
        </a>
      );
    }

    const unregister = registerLoginExtension(SampleSsoSlot);
    try {
      render(
        <ExtensionProviders initialCapabilities={COMMUNITY_DEFAULT_CAPABILITIES} skipCapabilityFetch>
          <LoginExtensionsSlot next="/dashboard" onAuthNavigation={() => {}} />
        </ExtensionProviders>,
      );
      expect(screen.getByRole("link", { name: "Enterprise SSO" })).toHaveAttribute(
        "href",
        "/sso?next=%2Fdashboard",
      );
    } finally {
      unregister();
    }

    cleanup();
    render(
      <ExtensionProviders initialCapabilities={COMMUNITY_DEFAULT_CAPABILITIES} skipCapabilityFetch>
        <LoginExtensionsSlot next="/dashboard" onAuthNavigation={() => {}} />
      </ExtensionProviders>,
    );
    expect(screen.queryByRole("link", { name: "Enterprise SSO" })).toBeNull();
  });

  it("registers and unregisters a pluggable plan resolver cleanly", () => {
    const resolver = async () => ({ hasPro: true, currentPlan: "Pro", planError: null });
    expect(getRegisteredPlanResolver()).toBeNull();
    const unregister = registerPlanResolver(resolver);
    expect(getRegisteredPlanResolver()).toBe(resolver);
    unregister();
    expect(getRegisteredPlanResolver()).toBeNull();
  });
});
