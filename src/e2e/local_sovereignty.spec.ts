import { test, expect } from './fixtures';

test.describe('Standalone local sovereignty & privacy', () => {
  test.beforeEach(async ({ page, loginAs, adminUser }) => {
    await loginAs(page, adminUser);
    await page.goto('/settings');
    await expect(page.getByRole('heading', { name: 'Workspace Settings', exact: true })).toBeVisible();
  });

  test('advertises standalone Community Edition capabilities without hosted telemetry or SaaS billing', async ({ page }) => {
    const response = await page.request.get('/api/v1/capabilities');
    expect(response.status()).toBe(200);
    expect(await response.json()).toEqual({
      edition: 'community',
      features: {
        cloud_sync: false,
        oidc: false,
        saas_billing: false,
        multitenant: false,
      },
    });
  });

  test('displays the local sovereignty privacy notice with product telemetry disabled', async ({ page }) => {
    await expect(page.getByText('Local Sovereignty & Privacy', { exact: true })).toBeVisible();
    await expect(
      page.getByText('Standalone workspaces keep operational data local with cloud telemetry disabled.', { exact: true }),
    ).toBeVisible();
    await expect(
      page.getByText('Product telemetry is disabled in self-hosted single-tenant Community Edition workspaces.', { exact: true }),
    ).toBeVisible();
    await expect(page.getByRole('checkbox', { name: 'Enable Product Telemetry (Standalone Mode)', exact: true })).toHaveCount(0);
  });

  test('reloads workspace settings without issuing telemetry network traffic', async ({ page }) => {
    const telemetryRequests: string[] = [];
    page.on('request', request => {
      if (new URL(request.url()).pathname.startsWith('/api/v1/settings/telemetry')) {
        telemetryRequests.push(`${request.method()} ${new URL(request.url()).pathname}`);
      }
    });

    await page.reload();
    await expect(page.getByRole('heading', { name: 'Workspace Settings', exact: true })).toBeVisible();
    await expect(page.getByText('Local Sovereignty & Privacy', { exact: true })).toBeVisible();
    expect(telemetryRequests).toEqual([]);
  });
});
