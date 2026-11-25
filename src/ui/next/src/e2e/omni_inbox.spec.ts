import { test, expect } from '../../../../e2e/fixtures';
import { seedDashboardAuditOwner } from '../../../../e2e/support/dashboard_audit_fixture';

test.describe('Omnichannel Inbox UI', () => {
  test('Owner sees sender id and known customer in inbox', async ({ anonymousPage: page, baseURL }) => {
    if (!baseURL) throw new Error('The isolated local app URL is required');
    const owner = await seedDashboardAuditOwner(baseURL);

    await page.goto('/login');
    await page.getByPlaceholder('Email or Username').fill(owner.email);
    await page.getByPlaceholder('Password').fill(owner.password);
    await page.getByLabel(/Organization/).fill(owner.tenantId);
    await page.getByRole('button', { name: /Log in/i }).click();
    await expect(page.getByRole('heading', { name: 'Dashboard' }).first()).toBeVisible({ timeout: 20000 });

    await page.goto('/inbox');
    await expect(page.getByTestId('inbox-settled')).toBeVisible({ timeout: 10000 });

    // Check if the case-owned inbox message is rendered with sender 'maya_bakes'
    const msg = page.locator('.app-list-item', { hasText: 'Do you have vegan options' }).first();
    await expect(msg).toBeVisible({ timeout: 10000 });
    if (await msg.isEnabled()) {
      await msg.click();
    }
    await expect(msg).toHaveAttribute('aria-current', 'true');

    await expect(page.getByText('maya_bakes')).toBeVisible();
    await expect(page.getByText('Known Customer')).toBeVisible();
  });
});
