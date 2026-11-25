// SPDX-License-Identifier: Apache-2.0
import { describe, expect, it } from "vitest";
import {
  CAPABILITIES_ENDPOINT,
  COMMUNITY_DEFAULT_CAPABILITIES,
  fetchCapabilities,
  parseCapabilities,
} from "./capabilities";

describe("runtime capability negotiation", () => {
  it("parses valid cloud edition capabilities strictly", () => {
    expect(
      parseCapabilities({
        edition: "cloud",
        features: {
          cloud_sync: true,
          oidc: true,
          saas_billing: false,
          multitenant: true,
        },
      }),
    ).toEqual({
      edition: "cloud",
      features: {
        cloud_sync: true,
        oidc: true,
        saas_billing: false,
        multitenant: true,
      },
    });
  });

  it("fails closed to single-tenant community defaults for community edition even if flags are injected", () => {
    expect(
      parseCapabilities({
        edition: "community",
        features: {
          cloud_sync: true,
          oidc: true,
          saas_billing: true,
          multitenant: true,
        },
      }),
    ).toEqual(COMMUNITY_DEFAULT_CAPABILITIES);
  });

  it.each([
    null,
    undefined,
    "cloud",
    [],
    {},
    { edition: "enterprise", features: { multitenant: true } },
    { edition: "cloud", features: null },
    { edition: "cloud", features: { cloud_sync: "true", oidc: 1 } },
  ])("fails closed on malformed or unknown capability payload %j", (payload) => {
    const parsed = parseCapabilities(payload);
    if (
      payload !== null &&
      typeof payload === "object" &&
      !Array.isArray(payload) &&
      (payload as { edition?: unknown }).edition === "cloud"
    ) {
      expect(parsed).toEqual({
        edition: "cloud",
        features: COMMUNITY_DEFAULT_CAPABILITIES.features,
      });
    } else {
      expect(parsed).toEqual(COMMUNITY_DEFAULT_CAPABILITIES);
    }
  });

  it("negotiates GET /api/v1/capabilities and falls back to community defaults when absent or failing", async () => {
    const requested: Array<{ url: string; init?: RequestInit }> = [];
    const okCloud = await fetchCapabilities({
      fetchImpl: async (input, init) => {
        requested.push({ url: String(input), init });
        return Response.json({
          edition: "cloud",
          features: { cloud_sync: false, oidc: true, saas_billing: true, multitenant: true },
        });
      },
    });
    expect(requested).toHaveLength(1);
    expect(requested[0].url).toBe(CAPABILITIES_ENDPOINT);
    expect(requested[0].init).toMatchObject({
      method: "GET",
      credentials: "same-origin",
      cache: "no-store",
      redirect: "error",
    });
    expect(okCloud).toEqual({
      edition: "cloud",
      features: { cloud_sync: false, oidc: true, saas_billing: true, multitenant: true },
    });

    const notFound = await fetchCapabilities({
      fetchImpl: async () => new Response("Not Found", { status: 404 }),
    });
    expect(notFound).toEqual(COMMUNITY_DEFAULT_CAPABILITIES);

    const networkFailure = await fetchCapabilities({
      fetchImpl: async () => {
        throw new Error("connection refused");
      },
    });
    expect(networkFailure).toEqual(COMMUNITY_DEFAULT_CAPABILITIES);

    const invalidJson = await fetchCapabilities({
      fetchImpl: async () => new Response("not-json", { status: 200 }),
    });
    expect(invalidJson).toEqual(COMMUNITY_DEFAULT_CAPABILITIES);
  });
});
