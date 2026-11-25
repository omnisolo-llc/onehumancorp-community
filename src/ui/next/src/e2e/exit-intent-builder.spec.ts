import { test, expect } from '../../../../e2e/fixtures';
import { createGrowthOwner } from '../../../../e2e/growth_owner';

test('exit-intent template editing and copying work directly in Community Edition', async ({ page, context, baseURL }) => {
  await createGrowthOwner(page, baseURL);
  await context.grantPermissions(['clipboard-read', 'clipboard-write']);
  const capabilitiesLoaded = page.waitForResponse(
    (response) => new URL(response.url()).pathname === '/api/v1/capabilities' && response.status() === 200,
  );
  await page.goto('/exit-intent-builder');
  await capabilitiesLoaded;
  await expect(page.getByRole('heading', { name: 'Exit-Intent Pop-up Builder' })).toBeVisible();
  await expect(page.getByText(/Offer text is a draft/)).toBeVisible();
  await page.getByPlaceholder('Wait! Before you go...').fill('Owner supplied offer');
  await page.getByPlaceholder('Get 10% off your first order...').fill('Ask us about our current offer.');
  await expect(page.locator('.max-w-4xl').filter({ hasText: 'Live Preview' }).getByRole('heading', { name: 'Owner supplied offer' })).toBeVisible();
  const toggle = page.getByRole('switch', { name: 'Remove OmniSolo Branding' });
  await expect(toggle).toBeEnabled();
  await toggle.click();
  await expect(toggle).toHaveAttribute('aria-checked', 'true');
  await page.getByRole('button', { name: 'Copy to Clipboard' }).click();
  await expect(page.getByRole('button', { name: 'Copied!' })).toBeVisible();
  const copied = await page.evaluate(() => navigator.clipboard.readText());
  expect(copied).toContain('Owner supplied offer');
  expect(copied).toContain('Ask us about our current offer.');
  expect(copied).not.toContain('Powered by OmniSolo');
});
