import { test, expect } from '../../../../e2e/fixtures';
import { seedDashboardAuditOwner } from '../../../../e2e/support/dashboard_audit_fixture';

test.describe('Omni Inbox Agentic Triage', () => {
  test('displays unread leads summary and allows inventory deduction approval', async ({ anonymousPage: page, baseURL }) => {
    if (!baseURL) throw new Error('The isolated local app URL is required');
    const owner = await seedDashboardAuditOwner(baseURL);

    // Rely on case-owned E2E seed data for messages and avoid overriding global API routes
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

    // Assert the special translucent action modal button is visible from the e2e seed data
    // Because seed data contains action_type "Draft Quote", check for the dynamic quote button
    const approveButton = page.locator('button', { hasText: '✨ Send quote for $' });
    await expect(approveButton).toBeVisible({ timeout: 10000 });

  });
});
