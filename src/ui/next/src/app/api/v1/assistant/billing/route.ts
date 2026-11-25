// SPDX-License-Identifier: Apache-2.0
import { proxyBackendRequest } from "@/lib/auth/backendTransport";
import { privateJson } from "../assistantBackend";

export const dynamic = "force-dynamic";

function finiteNumber(value: unknown): number | null {
  return typeof value === "number" && Number.isFinite(value) ? value : null;
}

export async function GET(request: Request): Promise<Response> {
  const response = await proxyBackendRequest(request, "/api/v1/usage");
  if (!response.ok) return response;
  const value = await response.json().catch(() => null);
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    return privateJson(502, { error: "invalid usage response" });
  }
  const data = value as Record<string, unknown>;
  const totalMicros = finiteNumber(data.total_micros) ?? 0;
  const limitMicros = finiteNumber(data.limit_micros);
  const requests = finiteNumber(data.total_requests) ?? 0;

  return privateJson(200, {
    plan: "Community",
    aiActionsUsed: requests,
    aiActionsLimit: limitMicros ?? 0,
    storageUsedGB: 0,
    storageLimitGB: 0,
    estimatedNextBill: Number((totalMicros / 1_000_000).toFixed(4)),
  });
}
