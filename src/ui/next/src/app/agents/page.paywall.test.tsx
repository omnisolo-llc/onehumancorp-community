import { act, cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import AgentsPage from './page';
import { registerPlanResolver, type PlanStatus } from '@/components/ExtensionProviders';
import { TooltipProvider } from '@/components/TooltipRegistry';
import { COMMUNITY_DEFAULT_CAPABILITIES } from '@/lib/capabilities';
import { invalidateQueueOwner, readQueueOwner } from '@/lib/sync/queueIdentity';
import { invalidateOnboardingSession } from '../onboarding/draftSession';

// Exercise the actual page, plan hook, capability negotiation, canonical identity and business readers.
// Only HTTP is supplied by fixtures; no plan or identity implementation is mocked.
const owner = { userId: 'paywall-owner', tenantId: 'paywall-tenant' };
const CLOUD_BILLING_CAPABILITIES = {
  edition: 'cloud' as const,
  features: { cloud_sync: false, oidc: false, saas_billing: true, multitenant: true },
};
let currentOwner = owner;
let currentPlan: string | null = 'Pro';
let identityReply: (() => Promise<Response>) | null;
let capabilitiesReply: (() => Promise<Response>) | null;
let planReply: (() => Promise<PlanStatus | 'session-changed'>) | null;
const fetchMock = vi.fn<typeof fetch>();
const toggle = () => screen.getByRole('button', { name: 'Toggle Pro Mode' });
const paywall = () => screen.queryByRole('heading', { name: 'Upgrade to Pro' });

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>(done => { resolve = done; });
  return { promise, resolve };
}

function planStatusFromName(plan: string | null): PlanStatus {
  if (plan === 'Pro' || plan === 'Business') return { currentPlan: plan, hasPro: true, planError: null };
  if (plan === 'Free' || plan === 'Starter') return { currentPlan: plan, hasPro: false, planError: null };
  return { currentPlan: null, hasPro: false, planError: 'Current plan data is unavailable. Refresh to check again.' };
}

const identity = () => Response.json({ ...currentOwner, expiresAt: Date.now() + 60_000 });

async function open() {
  render(<TooltipProvider><AgentsPage /></TooltipProvider>);
  await waitFor(() => expect(fetchMock.mock.calls.some(([url]) => url === '/api/v1/capabilities')).toBe(true));
  await act(async () => {});
}

beforeEach(() => {
  localStorage.clear();
  registerPlanResolver(null);
  invalidateQueueOwner();
  invalidateOnboardingSession(false);
  currentOwner = owner;
  currentPlan = 'Pro';
  identityReply = null;
  capabilitiesReply = null;
  planReply = null;
  Object.defineProperty(navigator, 'onLine', { configurable: true, value: true });
  registerPlanResolver(async () => (planReply ? planReply() : planStatusFromName(currentPlan)));
  fetchMock.mockReset();
  fetchMock.mockImplementation(async (input) => {
    const url = String(input);
    if (url === '/api/v1/auth/session-identity') return identityReply ? identityReply() : identity();
    if (url === '/api/v1/capabilities') return capabilitiesReply ? capabilitiesReply() : Response.json(CLOUD_BILLING_CAPABILITIES);
    if (url === '/api/v1/agents/execution-policy') return Response.json({ available: false, mode: 'text_analysis', workspace_access: false, tools: [], policy: null });
    if (url === '/api/v1/agents/approvals' || url === '/api/v1/agents/approvals/activity') return Response.json({ pending_approvals: [] });
    if (url === '/api/v1/agents/workflows') return Response.json({ workflows: [], next_cursor: null });
    throw new Error(`Unexpected HTTP boundary: ${url}`);
  });
  vi.stubGlobal('fetch', fetchMock);
});

afterEach(() => {
  cleanup();
  registerPlanResolver(null);
  invalidateQueueOwner();
  invalidateOnboardingSession(false);
  vi.unstubAllGlobals();
});

it('unlocks Pro Mode directly in Community Edition without calling Cloud billing', async () => {
  registerPlanResolver(null);
  capabilitiesReply = async () => Response.json(COMMUNITY_DEFAULT_CAPABILITIES);
  await open();
  await waitFor(() => expect(toggle()).toHaveAttribute('aria-pressed', 'true'));
  expect(screen.getByRole('status', { name: 'Pro Mode readiness' })).toHaveTextContent('Current plan: Community');
  fireEvent.click(toggle());
  expect(paywall()).not.toBeInTheDocument();
  expect(toggle()).toHaveAttribute('aria-pressed', 'false');
  expect(screen.getByRole('status', { name: 'Pro Mode readiness' })).toHaveTextContent('Current plan: Community (Pro Mode paused)');
  fireEvent.click(toggle());
  expect(toggle()).toHaveAttribute('aria-pressed', 'true');
  expect(screen.getByRole('status', { name: 'Pro Mode readiness' })).toHaveTextContent('Current plan: Community');
  expect(paywall()).not.toBeInTheDocument();
});

it.each(['Pro', 'Business'])('does not open a false upgrade prompt during a %s owner recheck', async plan => {
  currentPlan = plan;
  await open();
  await waitFor(() => expect(toggle()).toHaveAttribute('aria-pressed', 'true'));
  const pending = deferred<Response>();
  identityReply = () => pending.promise;
  let checking!: Promise<unknown>;
  act(() => { checking = readQueueOwner(); });
  expect(toggle()).toHaveAttribute('aria-pressed', 'mixed');
  fireEvent.click(toggle());
  expect(paywall()).not.toBeInTheDocument();
  expect(toggle()).toBeDisabled();
  await act(async () => { pending.resolve(identity()); await checking; });
  expect(toggle()).toHaveAttribute('aria-pressed', 'true');
  expect(toggle()).toBeEnabled();
  expect(paywall()).not.toBeInTheDocument();
  fireEvent.click(toggle());
  expect(paywall()).not.toBeInTheDocument();
  expect(fetchMock.mock.calls.filter(([, options]) => options?.method === 'POST')).toHaveLength(0);
});

it.each(['Free', 'Starter'])('keeps a verified %s plan gated and trial activation unavailable', async plan => {
  currentPlan = plan;
  await open();
  await waitFor(() => expect(toggle()).toBeEnabled());
  expect(toggle()).toHaveAttribute('aria-pressed', 'false');
  fireEvent.click(toggle());
  expect(paywall()).toBeVisible();
  fireEvent.click(screen.getByRole('button', { name: 'Check trial availability' }));
  expect(screen.getByRole('alert')).toHaveTextContent('durable grant is not verified');
  expect(toggle()).toHaveAttribute('aria-pressed', 'false');
  fireEvent.click(screen.getByRole('button', { name: 'Close' }));
  expect(paywall()).not.toBeInTheDocument();
  expect(fetchMock.mock.calls.filter(([, options]) => options?.method === 'POST')).toHaveLength(0);
});

it('holds the toggle until the initial plan resolution is verified', async () => {
  const pending = deferred<PlanStatus>();
  planReply = () => pending.promise;
  await open();
  expect(toggle()).toHaveAttribute('aria-pressed', 'mixed');
  fireEvent.click(toggle());
  expect(paywall()).not.toBeInTheDocument();
  expect(toggle()).toBeDisabled();
  await act(async () => pending.resolve(planStatusFromName('Pro')));
  await waitFor(() => expect(toggle()).toHaveAttribute('aria-pressed', 'true'));
  expect(toggle()).toBeEnabled();
  expect(paywall()).not.toBeInTheDocument();
});

it.each(['Unknown', null])('does not label an unverified plan %j as an upgrade opportunity', async plan => {
  currentPlan = plan;
  await open();
  expect(toggle()).toBeDisabled();
  fireEvent.click(toggle());
  expect(paywall()).not.toBeInTheDocument();
  expect(toggle()).toHaveAttribute('aria-pressed', 'mixed');
});

it('hides an open Free paywall when owner authority is retired', async () => {
  currentPlan = 'Free';
  await open();
  await waitFor(() => expect(toggle()).toBeEnabled());
  fireEvent.click(toggle());
  expect(paywall()).toBeVisible();
  act(() => window.dispatchEvent(new Event('pagehide')));
  expect(toggle()).toHaveAttribute('aria-pressed', 'mixed');
  expect(toggle()).toBeDisabled();
  expect(paywall()).not.toBeInTheDocument();
});

it('distinguishes an unknown plan from verified Free while its resolution is pending', async () => {
  localStorage.setItem('has_pro', 'true');
  const pending = deferred<PlanStatus>();
  planReply = () => pending.promise;
  await open();
  expect(toggle()).toBeDisabled();
  expect(toggle()).toHaveAttribute('aria-pressed', 'mixed');
  expect(toggle()).toHaveAttribute('aria-busy', 'true');
  expect(screen.getByRole('status', { name: 'Pro Mode readiness' })).toHaveTextContent('Checking current plan');
  fireEvent.click(toggle());
  expect(paywall()).not.toBeInTheDocument();
  await act(async () => pending.resolve(planStatusFromName('Free')));
  await waitFor(() => expect(toggle()).toBeEnabled());
  expect(toggle()).toHaveAttribute('aria-pressed', 'false');
  expect(toggle()).toHaveAttribute('aria-busy', 'false');
  expect(screen.getByRole('status', { name: 'Pro Mode readiness' })).toHaveTextContent('Current plan: Free');
  fireEvent.click(toggle());
  expect(paywall()).toBeVisible();
});

it('does not publish Free readiness until every overlapping identity read is settled', async () => {
  currentPlan = 'Free';
  await open();
  await waitFor(() => expect(toggle()).toBeEnabled());
  const first = deferred<Response>(), second = deferred<Response>();
  let reads = 0;
  identityReply = () => (++reads === 1 ? first.promise : second.promise);
  let checking!: Promise<unknown>;
  act(() => { checking = Promise.all([readQueueOwner(), readQueueOwner()]); });
  expect(toggle()).toBeDisabled();
  expect(toggle()).toHaveAttribute('aria-pressed', 'mixed');
  fireEvent.click(toggle());
  expect(paywall()).not.toBeInTheDocument();
  await act(async () => second.resolve(identity()));
  expect(toggle()).toBeDisabled();
  expect(toggle()).toHaveAttribute('aria-busy', 'true');
  await act(async () => { first.resolve(identity()); await checking; });
  expect(toggle()).toBeEnabled();
  expect(toggle()).toHaveAttribute('aria-pressed', 'false');
  fireEvent.click(toggle());
  expect(paywall()).toBeVisible();
});

it('requires a fresh verified plan after remount and does not trust a stored Pro flag', async () => {
  currentPlan = 'Free';
  await open();
  await waitFor(() => expect(toggle()).toBeEnabled());
  fireEvent.click(toggle());
  expect(paywall()).toBeVisible();
  cleanup();
  localStorage.setItem('has_pro', 'true');
  const pending = deferred<PlanStatus>();
  planReply = () => pending.promise;
  await open();
  expect(toggle()).toBeDisabled();
  expect(toggle()).toHaveAttribute('aria-pressed', 'mixed');
  expect(paywall()).not.toBeInTheDocument();
  await act(async () => pending.resolve(planStatusFromName('Free')));
  await waitFor(() => expect(toggle()).toBeEnabled());
  expect(toggle()).toHaveAttribute('aria-pressed', 'false');
  fireEvent.click(toggle());
  expect(paywall()).toBeVisible();
});

it('announces retired authority after an authentication change and ignores a late plan resolution', async () => {
  const pending = deferred<PlanStatus>();
  planReply = () => pending.promise;
  await open();
  act(() => window.dispatchEvent(new Event('omnisolo_auth_changed')));
  await act(async () => pending.resolve(planStatusFromName('Free')));
  expect(toggle()).toBeDisabled();
  expect(toggle()).toHaveAttribute('aria-pressed', 'mixed');
  expect(screen.getByRole('status', { name: 'Pro Mode readiness' })).toHaveTextContent('Your session changed');
  fireEvent.click(toggle());
  expect(paywall()).not.toBeInTheDocument();
});
