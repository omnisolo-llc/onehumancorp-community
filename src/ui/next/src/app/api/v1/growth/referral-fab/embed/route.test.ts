import { describe, expect, it } from "vitest";
import { GET } from "./route";

describe("GET /api/v1/growth/referral-fab/embed", () => {
  it("neutralizes JS template-literal, quote, and tag injection and emits valid JavaScript", async () => {
    const rewardPayload = "` + alert(1) + `${fetch('/pwn')}<script>alert(2)</script>'\"\\";
    const tenantPayload = "acme';alert(3);//";
    const colorPayload = "#2563eb';alert(4);//";

    const url =
      `https://app.example.com/api/v1/growth/referral-fab/embed?` +
      `tenant=${encodeURIComponent(tenantPayload)}` +
      `&reward=${encodeURIComponent(rewardPayload)}` +
      `&themeColor=${encodeURIComponent(colorPayload)}`;

    const response = await GET(new Request(url));
    expect(response.status).toBe(200);
    expect(response.headers.get("content-type")).toContain("application/javascript");
    expect(response.headers.get("x-content-type-options")).toBe("nosniff");

    const js = await response.text();
    // Must parse cleanly as JavaScript without syntax breakout.
    expect(() => new Function(js)).not.toThrow();
    // Never uses innerHTML or interpolates unescaped script tags / invalid color.
    expect(js).not.toContain("innerHTML");
    expect(js).not.toContain("<script>");
    expect(js).not.toContain("alert(4)");
    expect(js).toContain('"themeColor":"#2563eb"');
  });

  it("preserves valid 3-char and 6-char hex theme colors", async () => {
    const response = await GET(
      new Request(
        "https://app.example.com/api/v1/growth/referral-fab/embed?tenant=bakery&reward=20%25&themeColor=%23059669&removeBranding=true",
      ),
    );
    const js = await response.text();
    expect(() => new Function(js)).not.toThrow();
    expect(js).toContain('"themeColor":"#059669"');
    expect(js).toContain('"removeBranding":true');
  });
});
