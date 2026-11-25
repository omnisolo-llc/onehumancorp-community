import { expect, test } from '@playwright/test';

test.describe('Unified Agent Feed Viewport Constraint', () => {
  test.use({ viewport: { width: 375, height: 812 } });

  test('should render properly and not exceed 375px', async ({ page }) => {
    test.setTimeout(180000);

    await page.goto('/dashboard');
    const dashboardH1 = page.locator('h1#dashboard-title');
    await expect(dashboardH1).toBeAttached({ timeout: 25000 });
    const container = page.locator('.app-panel').first();
    await expect(container).toBeAttached();


    const feedContainer = page.locator('.glassmorphism').first();
    await expect(feedContainer).toBeAttached({ timeout: 15000 });

    // We expect the body not to scroll horizontally
    const bodyWidth = await page.evaluate(() => document.body.scrollWidth);
    expect(bodyWidth).toBeLessThanOrEqual(375);

    // Verify touch targets of rendered buttons in the feed atomically so live
    // feed refreshes from concurrent workers cannot invalidate a cached count.
    const boxes = await page.locator('div.glassmorphism button').evaluateAll((elements) =>
      elements
        .map((el) => {
          const rect = el.getBoundingClientRect();
          return {
            width: rect.width,
            height: rect.height,
            label: (el.getAttribute('aria-label') || el.textContent || '').trim(),
          };
        })
        .filter((box) => box.width > 0 && box.height > 0),
    );
    for (const box of boxes) {
      expect(box.width, `button "${box.label}" width`).toBeGreaterThanOrEqual(44);
      expect(box.height, `button "${box.label}" height`).toBeGreaterThanOrEqual(44);
    }
  });
});
