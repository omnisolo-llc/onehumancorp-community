import { proxyBackendRequest } from "@/lib/auth/backendTransport";

const MILESTONE_SVG_CSP =
  "default-src 'none'; script-src 'none'; style-src 'unsafe-inline'; base-uri 'none'";

function stripControlChars(value: string): string {
  let out = "";
  for (let i = 0; i < value.length; i += 1) {
    const code = value.charCodeAt(i);
    if (code >= 0x20 && code !== 0x7f) out += value[i];
  }
  return out;
}

function escapeXml(unsafe: string, maxLen: number): string {
  return stripControlChars(unsafe)
    .trim()
    .slice(0, maxLen)
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&apos;");
}

export async function GET(request: Request) {
  try {
    const res = await proxyBackendRequest(request, "/api/v1/growth/milestone/card", {
      suppressRequestBody: true,
    });
    if (res.status === 200) {
      return res;
    }
  } catch {
    // Fall back to public milestone card rendering
  }

  const url = new URL(request.url);
  const title = escapeXml(
    url.searchParams.get("milestone_type") || "Milestone Reached",
    100,
  );
  const business = escapeXml(
    url.searchParams.get("business_name") || "OmniSolo Business",
    100,
  );
  const svg = `<svg viewBox="0 0 1200 630" width="100%" height="100%" xmlns="http://www.w3.org/2000/svg">
  <defs>
    <linearGradient id="g" x1="0%" y1="0%" x2="100%" y2="100%">
      <stop offset="0%" style="stop-color:#4F46E5;stop-opacity:1" />
      <stop offset="100%" style="stop-color:#7C3AED;stop-opacity:1" />
    </linearGradient>
  </defs>
  <rect width="1200" height="630" fill="url(#g)" rx="24" ry="24" />
  <g transform="translate(100, 70)">
    <rect width="1000" height="490" rx="32" ry="32" fill="rgba(255, 255, 255, 0.15)" stroke="rgba(255, 255, 255, 0.4)" stroke-width="2" />
    <text x="500" y="270" font-family="Outfit, sans-serif" font-size="64" font-weight="700" text-anchor="middle" fill="#ffffff">${title}</text>
    <text x="500" y="440" font-family="Outfit, sans-serif" font-size="28" font-weight="700" text-anchor="middle" fill="#ffffff">${business}</text>
  </g>
</svg>`;

  return new Response(svg, {
    status: 200,
    headers: {
      "Content-Type": "image/svg+xml; charset=utf-8",
      "Content-Security-Policy": MILESTONE_SVG_CSP,
      "X-Content-Type-Options": "nosniff",
      "Cache-Control": "public, max-age=3600",
    },
  });
}
