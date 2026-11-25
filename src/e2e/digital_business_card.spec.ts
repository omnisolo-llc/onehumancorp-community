import { test, expect } from './fixtures';

test.describe('Digital Business Card Generator E2E', () => {
  test('should generate digital business card and allow branding control without a paywall in Community Edition', async ({ page }) => {
    await test.step('Navigate and verify header', async () => {
      const capabilitiesRead = page.waitForResponse(
        response => new URL(response.url()).pathname === '/api/v1/capabilities' && response.request().method() === 'GET',
      );
      await page.goto('/digital-business-card');
      await expect(page.locator('h1', { hasText: 'Digital Business Card Generator' })).toBeVisible({ timeout: 15000 });
      const capabilitiesResponse = await capabilitiesRead;
      expect(capabilitiesResponse.status()).toBe(200);
      await capabilitiesResponse.finished();
    });

    await test.step('Fill out form and generate link', async () => {
      await page.fill('input[placeholder="e.g. Jane Doe"]', 'Carlos Repair');
      await page.fill('input[placeholder="e.g. Founder & CEO"]', 'Owner');
      await page.fill('input[placeholder="e.g. Acme Corp"]', 'Carlos Home Repair');
      await page.fill('input[placeholder="e.g. +1 (555) 123-4567"]', '+15559876543');

      await page.getByRole('button', { name: 'Generate Shareable Link' }).click();

      // Verify link is generated
      await expect(page.locator('input[readonly]').first()).toBeVisible();
      await expect(page.getByRole('button', { name: 'Copy' }).first()).toBeVisible();
    });

    await test.step('Toggle branding without SaaS paywall in Community Edition', async () => {
      const brandingCheckbox = page.getByRole('checkbox', { name: 'Remove Powered by OmniSolo branding' });
      const previewWatermark = page.getByText('⚡ Powered by OmniSolo');

      await expect(previewWatermark).toBeVisible();
      await brandingCheckbox.click();

      await expect(brandingCheckbox).toBeChecked();
      await expect(page.getByRole('heading', { name: 'Upgrade to Pro' })).toHaveCount(0);
      await expect(previewWatermark).toHaveCount(0);

      // Re-enable branding so the generated view exercises the viral footer
      await brandingCheckbox.click();
      await expect(brandingCheckbox).not.toBeChecked();
      await expect(previewWatermark).toBeVisible();
    });

    await test.step('Verify generated view and viral loop', async () => {
      // Re-generate link just in case
      await page.getByRole('button', { name: 'Generate Shareable Link' }).first().click();

      // Get the link
      const linkInput = page.locator('input[readonly]').first();
      await expect(linkInput).toBeVisible();
      const generatedUrl = await linkInput.inputValue();

      // Navigate to generated URL
      await page.goto(generatedUrl);

      // Verify VCard data renders
      await expect(page.getByRole('heading', { name: 'Carlos Repair' }).first()).toBeVisible({ timeout: 15000 });
      await expect(page.getByText('Carlos Home Repair')).toBeVisible();
      await expect(page.getByText('+15559876543')).toBeVisible();

      // Verify Save to Contacts button
      await expect(page.getByRole('button', { name: 'Save to Contacts' })).toBeVisible();

      // Verify viral loop footer
      await expect(page.getByText('Create your own free digital business card')).toBeVisible();
      await expect(page.getByText('OmniSolo')).toBeVisible();
    });
  });
});
