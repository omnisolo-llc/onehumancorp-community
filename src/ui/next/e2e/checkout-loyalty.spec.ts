import { test, expect } from '@playwright/test';

test.describe('Checkout Loyalty UI', () => {
  test('should display loyalty points toggle and update checkout total', async ({ page }) => {
    // Navigate to checkout
    await page.goto('/checkout');

    // Wait for the page to load
    await expect(page.getByRole('heading', { name: 'Secure Checkout' })).toBeVisible();

    // Check if the loyalty points section exists with OSS single-tenant copy
    const loyaltyHeading = page.getByText('Loyalty Points', { exact: true });
    await expect(loyaltyHeading).toBeVisible();
    await expect(page.getByText('-10% off • Apply eligible loyalty points')).toBeVisible();

    const loyaltyCheckbox = page.getByRole('checkbox', { name: 'Loyalty Points', exact: true });
    await expect(loyaltyCheckbox).not.toBeChecked();
    await expect(page.getByText('Due: $45.00')).toBeVisible();

    await loyaltyCheckbox.click();
    await expect(loyaltyCheckbox).toBeChecked();
    await expect(page.getByText('Due: $42.75')).toBeVisible();

    await loyaltyCheckbox.click();
    await expect(loyaltyCheckbox).not.toBeChecked();
    await expect(page.getByText('Due: $45.00')).toBeVisible();
  });
});
