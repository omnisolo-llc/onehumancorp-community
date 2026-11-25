import { readFileSync } from "node:fs";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { GET } from "./route";

describe("GET /api/v1/growth/embed/widget", () => {
  it("HTML-escapes tenant fallback output and sets strict CSP + nosniff headers", async () => {
    const response = await GET(
      new Request(
        `https://app.example.com/api/v1/growth/embed/widget?tenant=${encodeURIComponent(
          '<img src=x onerror=alert(1)>" onclick="evil()',
        )}&type=booking&theme=dark`,
      ),
    );

    expect(response.status).toBe(200);
    expect(response.headers.get("content-type")).toBe("text/html; charset=utf-8");
    expect(response.headers.get("content-security-policy")).toContain("script-src 'none'");
    expect(response.headers.get("x-content-type-options")).toBe("nosniff");

    const html = await response.text();
    expect(html).not.toContain("<img src=x onerror=alert(1)>");
    expect(html).toContain("&lt;img src=x onerror=alert(1)&gt;");
    expect(html).toContain(
      "ref=%3Cimg%20src%3Dx%20onerror%3Dalert(1)%3E%22%20onclick%3D%22evil()",
    );
  });

  it("does not append url.search onto backendPath passed to proxyBackendRequest", () => {
    const source = readFileSync(
      join(process.cwd(), "src/app/api/v1/growth/embed/widget/route.ts"),
      "utf8",
    );
    expect(source).toContain('proxyBackendRequest(request, "/api/v1/growth/embed/widget"');
    expect(source).not.toContain("embed/widget${url.search}");
  });
});
