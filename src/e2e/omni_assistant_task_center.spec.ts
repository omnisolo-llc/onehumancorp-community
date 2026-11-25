import { randomUUID } from "node:crypto";
import { test, expect } from "./fixtures";

test.describe("Omni Assistant AI Task Center (real-stack persistence & guardrails)", () => {
  test("renders live 3-pane Task Center, persists a Space across reload, and enforces truthful runtime/idempotency guardrails", async ({
    page,
    adminUser,
    loginAs,
  }) => {
    await loginAs(page, adminUser);

    const navLink = page.getByRole("link", { name: "Omni Assistant" });
    await expect(navLink).toBeVisible();
    await expect(navLink).toHaveAttribute("href", "/ai-workspace");

    const summaryPromise = page.waitForResponse(
      (res) =>
        new URL(res.url()).pathname === "/api/v1/task-center/summary" &&
        res.request().method() === "GET",
    );
    const catalogPromise = page.waitForResponse(
      (res) =>
        new URL(res.url()).pathname === "/api/v1/integrations/catalog" &&
        res.request().method() === "GET",
    );

    await page.goto("/ai-workspace");

    const summaryResponse = await summaryPromise;
    expect(summaryResponse.status()).toBe(200);
    expect(summaryResponse.headers()["cache-control"] ?? "").toContain(
      "no-store",
    );
    const summaryData = (await summaryResponse.json()) as {
      runtime: { llm_configured: boolean };
      spaces: Array<{ id: string; name: string }>;
    };

    const catalogResponse = await catalogPromise;
    expect(catalogResponse.status()).toBe(200);
    expect(catalogResponse.headers()["cache-control"] ?? "").toContain(
      "no-store",
    );

    await expect(
      page.getByTestId("omni-assistant-task-center-grid"),
    ).toBeVisible();

    // Verify truthful fail-closed runtime banner when no LLM provider is configured
    if (!summaryData.runtime.llm_configured) {
      await expect(
        page.getByTestId("unconfigured-runtime-banner"),
      ).toBeVisible();
      await page
        .getByLabel("Task prompt")
        .fill("Attempt execution without configured provider");
      await expect(
        page.getByRole("button", { name: "Run Task" }),
      ).toBeDisabled();
    }

    // Create a real persisted Space with Reads / Shows / Cannot manifest
    const spaceName = `E2E Space ${randomUUID().slice(0, 8)}`;
    await page.getByRole("button", { name: "New Space" }).click();
    await page.getByLabel("Space name").fill(spaceName);

    const createSpaceResponsePromise = page.waitForResponse(
      (res) =>
        new URL(res.url()).pathname === "/api/v1/task-center/spaces" &&
        res.request().method() === "POST",
    );
    await page.getByRole("button", { name: "Create Space" }).click();

    const createSpaceResponse = await createSpaceResponsePromise;
    expect([200, 201]).toContain(createSpaceResponse.status());

    const capabilityCard = page.getByTestId("space-capability-card");
    await expect(capabilityCard).toBeVisible();
    await expect(capabilityCard).toContainText(spaceName);
    await expect(capabilityCard).toContainText("Reads");
    await expect(capabilityCard).toContainText("Shows");
    await expect(capabilityCard).toContainText("Cannot");

    // Reload and verify the persisted Space remains present from the real backend
    await page.reload();
    await expect(
      page.getByRole("button", { name: new RegExp(spaceName) }),
    ).toBeVisible();

    // Guardrail: POST /api/v1/task-center/tasks from a trusted same-origin browser request
    // rejects a missing Idempotency-Key with 400 (rather than CSRF 403).
    const missingKeyStatus = await page.evaluate(async () => {
      const response = await fetch("/api/v1/task-center/tasks", {
        method: "POST",
        credentials: "same-origin",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ prompt: "Guardrail check without UUID header" }),
      });
      return response.status;
    });
    expect(missingKeyStatus).toBe(400);

    // Guardrail: When unconfigured, POST with a valid UUID fails closed (never fakes completion)
    if (!summaryData.runtime.llm_configured) {
      const unconfiguredStatus = await page.evaluate(async (idempotencyKey) => {
        const response = await fetch("/api/v1/task-center/tasks", {
          method: "POST",
          credentials: "same-origin",
          headers: {
            "Content-Type": "application/json",
            "Idempotency-Key": idempotencyKey,
          },
          body: JSON.stringify({
            prompt: "Fail-closed runtime verification",
            department: "operations",
          }),
        });
        return response.status;
      }, randomUUID());
      expect([503, 409, 412]).toContain(unconfiguredStatus);
    }
  });

  test("renders all 5 verified provider connection cards truthfully on /integrations", async ({
    page,
    adminUser,
    loginAs,
  }) => {
    await loginAs(page, adminUser);
    await page.goto("/integrations");

    const providerSection = page.locator(
      'section[aria-labelledby="verified-provider-title"]',
    );
    await expect(providerSection.getByRole("article")).toHaveCount(5);

    for (const providerName of [
      "OpenAI API",
      "Anthropic API",
      "Stripe payments",
      "Resend email",
    ]) {
      await expect(
        providerSection.getByRole("article", { name: providerName }),
      ).toBeVisible();
    }
    await expect(
      providerSection.getByText("OAuth bearer token", { exact: true }),
    ).toBeVisible();
  });
});
