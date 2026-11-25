import { proxyBackendRequest } from "@/lib/auth/backendTransport";
import {
  isValidTaskCenterId,
  parseTaskCenterTaskEnvelope,
} from "@/lib/taskCenterContract";

export const dynamic = "force-dynamic";

function privateJson(status: number, body: unknown): Response {
  return Response.json(body, {
    status,
    headers: {
      "cache-control": "private, no-store",
      pragma: "no-cache",
      "x-content-type-options": "nosniff",
    },
  });
}

export async function GET(
  request: Request,
  context: { params: Promise<{ id: string }> },
): Promise<Response> {
  const { id } = await context.params;
  if (!isValidTaskCenterId(id)) {
    return privateJson(400, { error: "invalid task ID" });
  }

  const response = await proxyBackendRequest(
    request,
    `/api/v1/task-center/tasks/${id}`,
    { forwardQuery: true },
  );
  if (!response.ok) return response;

  const payload: unknown = await response.json().catch(() => null);
  if (parseTaskCenterTaskEnvelope(payload) === null) {
    return privateJson(502, { error: "invalid task center task response" });
  }
  return privateJson(response.status, payload);
}
