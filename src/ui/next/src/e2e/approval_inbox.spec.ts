import { expect, test } from '@playwright/test';
import { createDashboardAuditCase } from '../../../../e2e/support/dashboard_audit_fixture';

test.describe('Unified Agent Feed Mobile Test', () => {
  test.use({ viewport: { width: 375, height: 812 } });

  test('should render properly and handle tabs', async ({ browser, baseURL }) => {
    test.setTimeout(180000);
    if (!baseURL) throw new Error('The isolated app base URL is required');
    const owned = await createDashboardAuditCase(browser, baseURL, { width: 375, height: 812 });
    try {
      await owned.navigate('/dashboard');
      const page = owned.page;
      await expect(page.locator('h1', { hasText: 'Dashboard' }).first()).toBeVisible({ timeout: 25000 });

      // Wait for hydrated, enabled tab controls after the dashboard baseline settles.
      const proposalsTab = page.locator('#tab-proposals');
      const activityTab = page.locator('#tab-activity');
      await expect(proposalsTab).toBeVisible({ timeout: 15000 });
      await expect(proposalsTab).toBeEnabled();
      await expect(proposalsTab).toHaveAttribute('aria-pressed', 'true');
      await expect(activityTab).toBeVisible();
      await expect(activityTab).toBeEnabled();

      // Switch tabs without force-clicking past layout/actionability checks.
      await activityTab.scrollIntoViewIfNeeded();
      await activityTab.click();
      await expect(activityTab).toHaveAttribute('aria-pressed', 'true');

      // Verify glassmorphism CSS
      const feedContainer = page.locator('.glassmorphism').first();
      await expect(feedContainer).toBeVisible();
      await expect(feedContainer).toHaveCSS('backdrop-filter', /blur\(30px\)|none/);

      // Switch back to Proposals and verify selection + seeded proposal cards.
      await proposalsTab.scrollIntoViewIfNeeded();
      await proposalsTab.click();
      await expect(proposalsTab).toHaveAttribute('aria-pressed', 'true');

      // Verify the backend-backed feed produced at least one actionable proposal.
      await expect(page.locator('[data-testid*="triage-card-"]').first()).toBeVisible();
    } finally {
      await owned.close();
    }
  });

  test('should display Action Needed tag correctly', async ({ browser, baseURL }) => {
    if (!baseURL) throw new Error('The isolated app base URL is required');
    const owned = await createDashboardAuditCase(browser, baseURL, { width: 375, height: 812 });
    try {
      await owned.navigate('/dashboard');
      await expect(owned.page.locator('#tab-proposals')).toBeVisible({ timeout: 15000 });

      // Look for Action Needed tag
      const actionNeededTag = owned.page.locator('span', { hasText: 'Action Needed' }).first();
      await expect(actionNeededTag).toBeVisible();
      await expect(actionNeededTag).toHaveClass(/bg-green-100/);
    } finally {
      await owned.close();
    }
  });

  test('should display Approval tag correctly', async ({ browser, baseURL }) => {
    if (!baseURL) throw new Error('The isolated app base URL is required');
    const owned = await createDashboardAuditCase(browser, baseURL, { width: 375, height: 812 });
    try {
      await owned.navigate('/dashboard');
      await expect(owned.page.locator('#tab-proposals')).toBeVisible({ timeout: 15000 });

      // Look for Approval tag
      const approvalTag = owned.page.locator('span', { hasText: 'Approval' }).first();
      await expect(approvalTag).toBeVisible();
      await expect(approvalTag).toHaveClass(/bg-\[#0066FF\]\/10/);
    } finally {
      await owned.close();
    }
  });

  test('should display action buttons for the recorded generic proposal', async ({ browser, baseURL }) => {
    if (!baseURL) throw new Error('The isolated app base URL is required');
    const owned = await createDashboardAuditCase(browser, baseURL, { width: 375, height: 812 });
    try {
      await owned.navigate();
      const proposal = owned.page.getByTestId(`triage-card-${owned.actor.namespace}-e2e-feed-reschedule`);
      await expect(proposal).toBeVisible();
      // Quote drafts have a distinct blue Approve & Send action. These existing
      // assertions belong to the recorded generic proposal, not the first card.
      const approveButton = proposal.getByRole('button', { name: 'Approve proposal', exact: true });
      await expect(approveButton).toBeVisible();
      await expect(approveButton).toHaveClass(/bg-green-500/);
      await expect(proposal.getByRole('button', { name: 'Edit proposal', exact: true })).toBeVisible();
      const denyButton = proposal.getByRole('button', { name: 'Reject proposal', exact: true });
      await expect(denyButton).toBeVisible();
      await expect(denyButton).toHaveClass(/bg-red-100/);
    } finally { await owned.close(); }
  });

  test('should display empty state or loading state in Activity Feed correctly', async ({ browser, baseURL }) => {
    if (!baseURL) throw new Error('The isolated app base URL is required');
    const owned = await createDashboardAuditCase(browser, baseURL, { width: 375, height: 812 });
    try {
      await owned.navigate('/dashboard');
      const activityTab = owned.page.locator('#tab-activity');
      await expect(activityTab).toBeVisible({ timeout: 15000 });
      await expect(activityTab).toBeEnabled();

      // Switch tabs
      await activityTab.scrollIntoViewIfNeeded();
      await activityTab.click();
      await expect(activityTab).toHaveAttribute('aria-pressed', 'true');

      // Loading, empty, or populated activity surfaces must all be rendered with the shared glass treatment.
      const activityFeedSurface = owned.page.locator('[data-testid^="activity-feed-"]').first();
      await expect(activityFeedSurface).toBeVisible();
      await expect(activityFeedSurface).toHaveClass(/glassmorphism/);
    } finally {
      await owned.close();
    }
  });
});
