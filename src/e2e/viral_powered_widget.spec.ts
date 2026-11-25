import { test, expect } from './fixtures';

test.describe('Viral Powered By OmniSolo Widget', () => {
  test('should allow owner to configure the viral widget, toggle branding without a paywall, and copy embed code', async ({ page }) => {
    // 1. Navigate to dashboard
    await page.goto('/dashboard');

    // 2. Find and click the Viral Widget link in GrowBusinessCard
    const widgetLink = page.locator('a[href="/viral-powered-by-omnisolo-widget"]');
    await expect(widgetLink).toBeVisible();
    await widgetLink.click();

    // Verify page content
    await expect(page.getByRole('heading', { name: 'Viral Widget Builder' })).toBeVisible();

    // Community Edition resolves full single-tenant branding access (no Pro paywall).
    const removeBrandingCheckbox = page.getByRole('checkbox', { name: /Remove "OmniSolo" Badge/i });
    await expect(removeBrandingCheckbox).toBeChecked();

    // 3. Configure the widget
    const titleInput = page.getByRole('textbox');
    await titleInput.fill('Special Viral Offer');

    const themeSelect = page.getByRole('combobox');
    await themeSelect.selectOption('dark');

    const embedPreview = page.locator('pre');
    await expect(embedPreview).toContainText('theme=dark');
    await expect(embedPreview).toContainText('branding=false');

    // 4. Toggle branding freely without triggering a Cloud paywall
    await removeBrandingCheckbox.click();
    await expect(removeBrandingCheckbox).not.toBeChecked();
    await expect(page.getByRole('heading', { name: 'Upgrade to Remove Branding' })).toHaveCount(0);
    await expect(embedPreview).toContainText('branding=true');

    await removeBrandingCheckbox.click();
    await expect(removeBrandingCheckbox).toBeChecked();
    await expect(embedPreview).toContainText('branding=false');

    // 5. Copy the embed code
    const copyButton = page.getByRole('button', { name: 'Copy Embed Code' });
    await expect(copyButton).toBeVisible();

    await copyButton.click();
    await expect(page.getByText('Copied to Clipboard!')).toBeVisible();
  });
});
