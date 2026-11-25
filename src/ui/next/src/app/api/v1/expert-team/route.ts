// SPDX-License-Identifier: Apache-2.0
import { runtimeTextResponse } from "@/lib/auth/runtimeResponse";
import { proxyBackendRequest } from "@/lib/auth/backendTransport";
import { jsonRpcRequestTransform } from "@/lib/auth/jsonRpc";

export async function POST(request: Request) {
  try {
    const response = await proxyBackendRequest(request, "/api/v1/rpc", {
      requestContentType: "application/json",
      transformRequestBody: jsonRpcRequestTransform("run_expert_team", (input) => {
        if (typeof input.task !== "string" || input.task.trim().length === 0) {
          throw new Error("task is required");
        }
        return { message: input.task };
      }),
    });
    return runtimeTextResponse(response);
  } catch (error) {
    const message = error instanceof Error ? error.message : "invalid request";
    return Response.json({ error: message }, { status: 400 });
  }
}
