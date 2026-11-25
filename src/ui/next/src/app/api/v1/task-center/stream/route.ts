import { proxyBackendRequest } from "@/lib/auth/backendTransport";

export const dynamic = "force-dynamic";

export async function GET(request: Request): Promise<Response> {
  return proxyBackendRequest(request, "/api/v1/task-center/stream", {
    streamResponse: true,
    forwardQuery: true,
  });
}
