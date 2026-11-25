// SPDX-License-Identifier: Apache-2.0
import { parseAuthRuntimeConfig } from "@/lib/auth/runtimeConfig";
import {
  CAPABILITIES_ENDPOINT,
  COMMUNITY_DEFAULT_CAPABILITIES,
  parseCapabilities,
} from "@/lib/capabilities";

export const dynamic = "force-dynamic";

const TIMEOUT_MS = 5_000;

function communityJson(payload: unknown): Response {
  return new Response(JSON.stringify(payload), {
    status: 200,
    headers: {
      "content-type": "application/json; charset=utf-8",
      "cache-control": "private, no-store",
      pragma: "no-cache",
      "x-content-type-options": "nosniff",
    },
  });
}

export async function GET(): Promise<Response> {
  try {
    const config = parseAuthRuntimeConfig(process.env);
    const target = new URL(CAPABILITIES_ENDPOINT, `${config.backendOrigin}/`);
    if (target.origin !== config.backendOrigin || target.pathname !== CAPABILITIES_ENDPOINT) {
      return communityJson(COMMUNITY_DEFAULT_CAPABILITIES);
    }
    const response = await fetch(target, {
      method: "GET",
      headers: { accept: "application/json" },
      cache: "no-store",
      redirect: "manual",
      signal: AbortSignal.timeout(TIMEOUT_MS),
    });
    if (!response.ok) {
      return communityJson(COMMUNITY_DEFAULT_CAPABILITIES);
    }
    const payload: unknown = await response.json().catch(() => null);
    return communityJson(parseCapabilities(payload));
  } catch {
    return communityJson(COMMUNITY_DEFAULT_CAPABILITIES);
  }
}
