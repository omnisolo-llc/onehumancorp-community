import { test, expect } from '@playwright/test';

test.describe('Checkout Loyalty UI', () => {
  test('should display loyalty points toggle and update checkout total', async ({ page }) => {
    // Navigate to checkout
    await page.goto('/checkout');

    // Wait for the page to load
    await expect(page.getByRole('heading', { name: 'Secure Checkout' })).toBeVisible();

    // Single-tenant checkout exposes Loyalty Points (cross-tenant collectives are Cloud-only)
    const loyaltyToggle = page.getByText('Loyalty Points', { exact: true });
    await expect(loyaltyToggle).toBeVisible();
    await expect(page.getByText(/-10% off/).first()).toBeVisible();
    await expect(page.getByText(/Apply eligible loyalty points/).first()).toBeVisible();

    const loyaltyCheckbox = page.getByRole('checkbox', { name: 'Loyalty Points', exact: true });
    await expect(loyaltyCheckbox).not.toBeChecked();
    await expect(page.getByText('Due: $45.00')).toBeVisible();

    // Toggle loyalty discount via the card label (parent card handles toggling)
    await loyaltyToggle.click();

    await expect(loyaltyCheckbox).toBeChecked();
    await expect(page.getByText('Total', { exact: true })).toBeVisible();
    await expect(page.getByText('Due: $42.75')).toBeVisible();
  });

  test('should display Taxes and Fees', async ({ page }) => {
    // Navigate to checkout
    await page.goto('/checkout');

    // Wait for the page to load by waiting for a visible element
    await page.waitForSelector('text=Total');

    // Check if Taxes and Fees line item exists
    const taxesAndFees = page.locator('text=Taxes and Fees');
    await expect(taxesAndFees).toBeVisible();

    // Check if Calculated at checkout exists
    const calculatedAtCheckout = page.locator('text=Calculated at checkout');
    await expect(calculatedAtCheckout).toBeVisible();
  });
});
