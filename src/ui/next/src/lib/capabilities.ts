// SPDX-License-Identifier: Apache-2.0

export type Edition = "community" | "cloud";

export type CapabilityFeatures = Readonly<{
  cloud_sync: boolean;
  oidc: boolean;
  saas_billing: boolean;
  multitenant: boolean;
}>;

export type RuntimeCapabilities = Readonly<{
  edition: Edition;
  features: CapabilityFeatures;
}>;

export const CAPABILITIES_ENDPOINT = "/api/v1/capabilities";

export const COMMUNITY_DEFAULT_CAPABILITIES: RuntimeCapabilities = Object.freeze({
  edition: "community",
  features: Object.freeze({
    cloud_sync: false,
    oidc: false,
    saas_billing: false,
    multitenant: false,
  }),
});

export function parseCapabilities(value: unknown): RuntimeCapabilities {
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    return COMMUNITY_DEFAULT_CAPABILITIES;
  }

  const candidate = value as Record<string, unknown>;
  if (candidate.edition !== "cloud") {
    return COMMUNITY_DEFAULT_CAPABILITIES;
  }

  const rawFeatures = candidate.features;
  if (rawFeatures === null || typeof rawFeatures !== "object" || Array.isArray(rawFeatures)) {
    return Object.freeze({
      edition: "cloud",
      features: COMMUNITY_DEFAULT_CAPABILITIES.features,
    });
  }

  const features = rawFeatures as Record<string, unknown>;
  return Object.freeze({
    edition: "cloud",
    features: Object.freeze({
      cloud_sync: features.cloud_sync === true,
      oidc: features.oidc === true,
      saas_billing: features.saas_billing === true,
      multitenant: features.multitenant === true,
    }),
  });
}

export type FetchCapabilitiesOptions = Readonly<{
  signal?: AbortSignal;
  fetchImpl?: typeof fetch;
  url?: string;
}>;

export async function fetchCapabilities(
  options: FetchCapabilitiesOptions = {},
): Promise<RuntimeCapabilities> {
  const fetcher = options.fetchImpl ?? globalThis.fetch;
  if (typeof fetcher !== "function") {
    return COMMUNITY_DEFAULT_CAPABILITIES;
  }

  try {
    const response = await fetcher(options.url ?? CAPABILITIES_ENDPOINT, {
      method: "GET",
      credentials: "same-origin",
      cache: "no-store",
      redirect: "error",
      signal: options.signal,
    });
    if (!response.ok) {
      return COMMUNITY_DEFAULT_CAPABILITIES;
    }
    const payload: unknown = await response.json();
    return parseCapabilities(payload);
  } catch {
    return COMMUNITY_DEFAULT_CAPABILITIES;
  }
}
