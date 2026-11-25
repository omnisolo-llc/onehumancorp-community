import { describe, expect, it } from "vitest";
import { GET } from "./route";

describe("GET /api/v1/growth/milestone/card", () => {
  it("XML-escapes SVG interpolations and emits strict scriptless CSP + nosniff", async () => {
    const response = await GET(
      new Request(
        `https://app.example.com/api/v1/growth/milestone/card?milestone_type=${encodeURIComponent(
          "</text><script>alert(1)</script>",
        )}&business_name=${encodeURIComponent('<svg onload="alert(2)"> & Co')}`,
      ),
    );

    expect(response.status).toBe(200);
    expect(response.headers.get("content-type")).toBe("image/svg+xml; charset=utf-8");
    expect(response.headers.get("content-security-policy")).toBe(
      "default-src 'none'; script-src 'none'; style-src 'unsafe-inline'; base-uri 'none'",
    );
    expect(response.headers.get("x-content-type-options")).toBe("nosniff");

    const svg = await response.text();
    expect(svg).not.toContain("<script>");
    expect(svg).not.toContain('<svg onload="alert(2)">');
    expect(svg).toContain("&lt;/text&gt;&lt;script&gt;alert(1)&lt;/script&gt;");
    expect(svg).toContain("&lt;svg onload=&quot;alert(2)&quot;&gt; &amp; Co");
  });
});
