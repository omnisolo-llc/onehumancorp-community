import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, test, vi } from "vitest";
import ClientPortalPage from "./page";

function response(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

describe("ClientPortalPage", () => {
  afterEach(() => vi.restoreAllMocks());

  test("renders data returned by the real usage and subscription API contracts", async () => {
    vi.spyOn(globalThis, "fetch").mockImplementation(async (input) => {
      const url = String(input);
      if (url.endsWith("/api/v1/usage")) {
        return response({
          total_micros: 29_000_000,
          limit_micros: 100_000_000,
          total_requests: 27,
        });
      }
      if (url.endsWith("/api/v1/subscriptions")) {
        return response({
          plans: [
            { id: "plan-1", name: "Monthly delivery", active: true },
            { id: "plan-2", name: "Paused delivery", active: false },
          ],
          subscribers: [],
          batches: [],
        });
      }
      return response({}, 404);
    });

    const { container } = render(<ClientPortalPage />);

    expect(await screen.findByText("Community")).toBeDefined();
    expect(container.querySelector('[data-client-portal-state="settled"]')).toBeTruthy();
    expect(screen.getByText("$29.00")).toBeDefined();
    expect(screen.getByText("$100.00")).toBeDefined();
    expect(screen.getByText("Quotes and proposals").closest("a")).toHaveAttribute(
      "href",
      "/quoting",
    );
  });

  test("fails honestly when both backend resources are unavailable and can retry", async () => {
    const fetch = vi.spyOn(globalThis, "fetch").mockResolvedValue(response({}, 503));
    render(<ClientPortalPage />);

    expect(await screen.findByRole("alert")).toHaveTextContent(
      "Client data is currently unavailable.",
    );
    const callsBeforeRefresh = fetch.mock.calls.length;
    fireEvent.click(screen.getByRole("button", { name: "Refresh" }));
    await waitFor(() =>
      expect(fetch.mock.calls.length).toBeGreaterThanOrEqual(callsBeforeRefresh + 2),
    );
  });
});
