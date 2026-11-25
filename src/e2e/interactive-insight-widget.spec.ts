import { test, expect } from '@playwright/test';

test.describe('Interactive Insight Widget Growth Feature', () => {
  test('should display builder, update live preview, and unlock white-label embed without a paywall in Community Edition', async ({ page }) => {
    // Navigate to the widget builder page
    await page.goto('/interactive-insight-widget');

    // Verify title and page header
    await expect(page).toHaveTitle(/Insight Widget \| OmniSolo/);
    await expect(page.getByRole('heading', { name: 'Insight Widget Builder' })).toBeVisible();

    // Verify default metric label and value
    const metricLabelInput = page.getByLabel('Metric Label');
    await expect(metricLabelInput).toHaveValue('Projects Completed');
    const metricValueInput = page.getByLabel('Metric Value');
    await expect(metricValueInput).toHaveValue('150+');

    // Verify Live Preview reflects default values
    await expect(page.getByText('Projects Completed', { exact: true })).toBeVisible();
    await expect(page.getByText('150+', { exact: true })).toBeVisible();

    // Community Edition unlocks branding removal without a Cloud SaaS soft paywall.
    const removeBrandingCheckbox = page.getByLabel(/Remove .* Badge/);
    await expect(removeBrandingCheckbox).toBeChecked();
    await expect(page.getByRole('link', { name: /OmniSolo/ })).toHaveCount(0);

    // Update metric label and value
    await metricLabelInput.fill('Happy Customers');
    await metricValueInput.fill('99%');

    // Verify Live Preview reflects new values
    await expect(page.getByText('Happy Customers', { exact: true })).toBeVisible();
    await expect(page.getByText('99%', { exact: true })).toBeVisible();

    // Interacting with branding control does not open a Cloud paywall modal
    await removeBrandingCheckbox.click();
    await expect(page.getByRole('heading', { name: 'Upgrade to Remove Branding' })).toHaveCount(0);

    const embedPreview = page.locator('pre');
    await expect(embedPreview).toContainText('label=Happy%20Customers');
    await expect(embedPreview).toContainText('value=99%25');
    await expect(embedPreview).toContainText('branding=false');
  });
});
