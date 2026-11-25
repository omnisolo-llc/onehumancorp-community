import {
  proxyBackendRequest,
  stripBrowserIdentityJsonRequestBody,
} from "@/lib/auth/backendTransport";
import {
  isUuid,
  parseTaskCenterTaskEnvelope,
  parseTaskCenterTaskList,
  validateCreateTaskPayload,
} from "@/lib/taskCenterContract";

export const dynamic = "force-dynamic";

const UTF8_DECODER = new TextDecoder("utf-8", { fatal: true });
const UTF8_ENCODER = new TextEncoder();

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

function sanitizeAndValidateCreateTaskBody(
  body: Uint8Array<ArrayBuffer>,
): Uint8Array<ArrayBuffer> {
  const stripped = stripBrowserIdentityJsonRequestBody(body);
  const parsed: unknown = JSON.parse(UTF8_DECODER.decode(stripped));
  const validated = validateCreateTaskPayload(parsed);
  if (validated === null) {
    throw new Error("invalid task payload");
  }
  return UTF8_ENCODER.encode(JSON.stringify(validated));
}

export async function GET(request: Request): Promise<Response> {
  const response = await proxyBackendRequest(request, "/api/v1/task-center/tasks", {
    forwardQuery: true,
  });
  if (!response.ok) return response;
  const payload: unknown = await response.json().catch(() => null);
  if (parseTaskCenterTaskList(payload) === null) {
    return privateJson(502, { error: "invalid task center task list" });
  }
  return privateJson(response.status, payload);
}

export async function POST(request: Request): Promise<Response> {
  const idempotencyKey = request.headers.get("idempotency-key")?.trim() ?? "";
  if (!isUuid(idempotencyKey)) {
    return privateJson(400, {
      error: "A valid UUID Idempotency-Key header is required.",
    });
  }

  const response = await proxyBackendRequest(request, "/api/v1/task-center/tasks", {
    requestContentType: "application/json",
    transformRequestBody: sanitizeAndValidateCreateTaskBody,
  });
  if (!response.ok) return response;

  const payload: unknown = await response.json().catch(() => null);
  if (parseTaskCenterTaskEnvelope(payload) === null) {
    return privateJson(502, { error: "invalid task center task response" });
  }
  return privateJson(response.status, payload);
}
