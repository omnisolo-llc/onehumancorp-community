import { test, expect } from '../../../../e2e/fixtures';

test.describe('Digital Business Card Generator E2E', () => {
  test('should generate digital business card and toggle branding directly in Community Edition', async ({ page }) => {
    await test.step('Navigate and verify header', async () => {
      const capabilitiesLoaded = page.waitForResponse(
        (response) => new URL(response.url()).pathname === '/api/v1/capabilities' && response.status() === 200,
      );
      await page.goto('/digital-business-card');
      await capabilitiesLoaded;
      await expect(page.locator('h1.app-title')).toBeVisible({ timeout: 15000 });
    });

    await test.step('Fill out form and generate shareable link', async () => {
      await page.waitForSelector('input[placeholder="e.g. Jane Doe"]', { state: 'visible' });
      await page.fill('input[placeholder="e.g. Jane Doe"]', 'Carlos Repair');
      await page.fill('input[placeholder="e.g. Founder & CEO"]', 'Owner');
      await page.fill('input[placeholder="e.g. Acme Corp"]', 'Carlos Home Repair');
      await page.fill('input[placeholder="e.g. +1 (555) 123-4567"]', '+15559876543');

      page.on('dialog', (dialog) => dialog.accept());

      await page.getByRole('button', { name: 'Generate Shareable Link' }).click();

      await expect(page.locator('input[readonly]')).toBeVisible({ timeout: 10000 });
      await expect(page.getByRole('button', { name: 'Copy' })).toBeVisible();
    });

    await test.step('Verify Community Edition allows removing branding without a SaaS paywall', async () => {
      const brandingCheckbox = page.getByRole('checkbox', { name: 'Remove Powered by OmniSolo branding' });
      await expect(brandingCheckbox).not.toBeChecked();
      await brandingCheckbox.check();
      await expect(brandingCheckbox).toBeChecked();
      await expect(page.locator('h2', { hasText: 'Upgrade to Pro' })).toHaveCount(0);

      // Re-enable branding so the generated view link below verifies default footer rendering as well
      await brandingCheckbox.uncheck();
      await expect(brandingCheckbox).not.toBeChecked();
    });

    await test.step('Verify generated view and viral loop', async () => {
      await page.getByRole('button', { name: 'Generate Shareable Link' }).click();

      const linkInput = page.locator('input[value*="digital-business-card/view?data="]');
      await expect(linkInput).toBeVisible();
      const generatedUrl = await linkInput.inputValue();

      await page.goto(generatedUrl);

      await expect(page.locator('h1').filter({ hasText: 'Carlos Repair' })).toBeVisible({ timeout: 15000 });
      await expect(page.getByText('Carlos Home Repair')).toBeVisible();
      await expect(page.getByText('+15559876543')).toBeVisible();

      await expect(page.getByRole('button', { name: /Save to Contacts|Save vCard/i })).toBeVisible();
      await expect(page.getByText('OmniSolo').first()).toBeVisible();
    });
  });
});
