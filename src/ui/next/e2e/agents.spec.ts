import { test, expect } from '../../../e2e/fixtures';
import { e2eDbQuery } from '../../../e2e/db_utils';
import { createGrowthOwner } from '../../../e2e/growth_owner';

// Preserve the first failure's event ordering when CI retries are disabled.
test.use({ trace: 'retain-on-failure' });

test.describe('AI Agent Department Architecture', () => {
  test('should display approval inbox and activity feed', async ({ page, baseURL }) => {
    if (!baseURL) throw new Error('Playwright baseURL is required for the agent records test.');
    const origin = new URL(baseURL).origin;
    const owner = await createGrowthOwner(page, baseURL);
    const activityId = `${owner.tenantId}-activity`;
    const approvalId = `${owner.tenantId}-approval`;
    const activityDescription = 'Recorded owner decision';
    const approvalDescription = 'Review the owner proposal';
    const inserted = await e2eDbQuery(`INSERT INTO agent_feed_items
      (id, tenant_id, event_source, context_payload, proposed_action, lifecycle_state, created_at, updated_at)
      VALUES ($1, $3, 'operations', jsonb_build_object('description', $4::text), '{}'::jsonb, 'APPROVED', NOW(), NOW()),
             ($2, $3, 'operations', jsonb_build_object('description', $5::text), '{}'::jsonb, 'PENDING_APPROVAL', NOW(), NOW())
      RETURNING id`, [activityId, approvalId, owner.tenantId, activityDescription, approvalDescription]);
    expect(inserted).toEqual([{ id: activityId }, { id: approvalId }]);

    const activityRead = page.waitForResponse(response => response.request().method() === 'GET'
      && new URL(response.url()).origin === origin
      && new URL(response.url()).pathname === '/api/v1/agents/approvals/activity');
    const approvalsRead = page.waitForResponse(response => response.request().method() === 'GET'
      && new URL(response.url()).origin === origin
      && new URL(response.url()).pathname === '/api/v1/agents/approvals');
    await page.goto('/agents');

    await expect(page.getByRole('button', { name: 'My Team', exact: true })).toBeVisible();
    await expect(page.getByRole('button', { name: 'The Manager', exact: true })).toBeVisible();
    const [activityResponse, approvalsResponse] = await Promise.all([activityRead, approvalsRead]);
    expect(activityResponse.status()).toBe(200);
    expect(approvalsResponse.status()).toBe(200);
    expect((await activityResponse.json()).pending_approvals).toEqual([expect.objectContaining({
      id: activityId, tenant_id: owner.tenantId, description: activityDescription, status: 'Approved',
    })]);
    expect((await approvalsResponse.json()).pending_approvals).toEqual([expect.objectContaining({
      id: approvalId, tenant_id: owner.tenantId, description: approvalDescription, status: 'PendingApproval',
    })]);

    await page.getByRole('button', { name: 'Activity Feed', exact: true }).click();
    await expect(page.getByRole('heading', { name: 'Activity Feed', exact: true })).toBeVisible();
    await expect(page.getByText(activityDescription, { exact: true })).toBeVisible();
    await expect(page.getByText(approvalDescription, { exact: true })).toHaveCount(0);

    await page.getByRole('button', { name: 'Needs Approval', exact: true }).click();
    await expect(page.getByRole('heading', { name: 'Needs Approval', exact: true })).toBeVisible();
    await expect(page.getByText(approvalDescription, { exact: true })).toBeVisible();
    await expect(page.getByRole('button', { name: 'Approve & Send', exact: true })).toBeVisible();
    await expect(page.getByText(activityDescription, { exact: true })).toHaveCount(0);
  });

  test('Community Edition workspace enables Pro Mode without Cloud SaaS paywall prompts', async ({ page, baseURL }) => {
    if (!baseURL) throw new Error('Playwright baseURL is required.');
    await createGrowthOwner(page, baseURL);
    await page.goto('/agents');
    const toggle = page.getByRole('button', { name: 'Toggle Pro Mode' });
    await expect(toggle).toBeEnabled();
    await expect(toggle).toHaveAttribute('aria-pressed', 'true');
    await expect(page.getByRole('heading', { name: 'Upgrade to Pro' })).not.toBeVisible();
  });
});
