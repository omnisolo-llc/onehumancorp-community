import { proxyBackendRequest } from "@/lib/auth/backendTransport";

const EMBED_HTML_CSP =
  "default-src 'none'; script-src 'none'; style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'";

function escapeHtml(unsafe: string): string {
  return unsafe
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#x27;");
}

function stripControlChars(value: string): string {
  let out = "";
  for (let i = 0; i < value.length; i += 1) {
    const code = value.charCodeAt(i);
    if (code >= 0x20 && code !== 0x7f) out += value[i];
  }
  return out;
}

export async function GET(request: Request) {
  const url = new URL(request.url);
  const rawTenant =
    stripControlChars(
      url.searchParams.get("tenant_id") ||
        url.searchParams.get("tenant") ||
        "workspace",
    )
      .trim()
      .slice(0, 80) || "workspace";
  const widgetType = url.searchParams.get("type") || "quote";
  const theme = url.searchParams.get("theme") || "light";

  try {
    const res = await proxyBackendRequest(request, "/api/v1/growth/embed/widget", {
      suppressRequestBody: true,
    });
    if (res.ok) {
      return res;
    }
  } catch {
    // Fall back to static escaped widget shell if backend proxy is unavailable
  }

  const bgColor = theme === "dark" ? "#1d1d1f" : "#ffffff";
  const textColor = theme === "dark" ? "#f5f5f7" : "#1d1d1f";

  const actionText =
    widgetType === "quote"
      ? "Request a quote"
      : widgetType === "booking"
        ? "Book an appointment"
        : "Connect with us";

  const safeTenantText = escapeHtml(rawTenant);
  const encodedTenantRef = encodeURIComponent(rawTenant);
  const safeActionText = escapeHtml(actionText);

  const html = `<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8">
  <title>OmniSolo Embed Widget</title>
  <style>
    body {
      font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, Helvetica, Arial, sans-serif;
      background-color: ${bgColor};
      color: ${textColor};
      margin: 0;
      padding: 16px;
      display: flex;
      flex-direction: column;
      align-items: center;
      justify-content: center;
      box-sizing: border-box;
    }
    .widget-container {
      width: 100%;
      max-width: 320px;
      padding: 20px;
      border-radius: 12px;
      box-shadow: 0 4px 6px rgba(0,0,0,0.1);
      text-align: center;
      border: 1px solid rgba(128,128,128,0.2);
    }
    .widget-title {
      font-size: 1.1rem;
      font-weight: 600;
      margin-bottom: 8px;
    }
    .widget-desc {
      font-size: 0.85rem;
      color: #888888;
      margin-bottom: 16px;
    }
    .btn {
      background-color: #0066FF;
      color: white;
      border: none;
      padding: 10px 16px;
      border-radius: 6px;
      font-weight: 500;
      cursor: pointer;
      width: 100%;
    }
    .watermark {
      margin-top: 16px;
      font-size: 0.75rem;
      color: #888888;
      text-decoration: none;
      display: flex;
      align-items: center;
      justify-content: center;
      gap: 4px;
    }
  </style>
</head>
<body>
  <div class="widget-container">
    <div class="widget-title">${safeActionText}</div>
    <div class="widget-desc">Workspace: ${safeTenantText}</div>
    <button type="button" class="btn">${safeActionText}</button>
    <a href="/api/v1/growth/referrals/click?target=/onboarding&amp;ref=${encodedTenantRef}" target="_blank" rel="noopener noreferrer" class="watermark">
      <span>⚡ OmniSolo</span>
    </a>
  </div>
</body>
</html>`;

  return new Response(html, {
    status: 200,
    headers: {
      "content-type": "text/html; charset=utf-8",
      "content-security-policy": EMBED_HTML_CSP,
      "x-content-type-options": "nosniff",
      "referrer-policy": "strict-origin-when-cross-origin",
    },
  });
}
