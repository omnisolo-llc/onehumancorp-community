import { act, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { registerPlanResolver } from '@/components/ExtensionProviders';
import { COMMUNITY_DEFAULT_CAPABILITIES } from '@/lib/capabilities';
import { invalidateQueueOwner, readQueueOwner } from '@/lib/sync/queueIdentity';
import { useProPlan } from './useProPlan';

const a = { userId: 'plan-user-a', tenantId: 'plan-tenant-a' };
const b = { userId: 'plan-user-b', tenantId: 'plan-tenant-b' };
const CLOUD_BILLING_CAPABILITIES = {
  edition: 'cloud' as const,
  features: { cloud_sync: false, oidc: false, saas_billing: true, multitenant: true },
};
let current = a;
let capabilitiesReply: () => Promise<Response>;

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>(yes => { resolve = yes; });
  return { promise, resolve };
}

function Harness() {
  const plan = useProPlan();
  return (
    <>
      <output aria-label="Pro">{String(plan.hasPro)}</output>
      <output aria-label="PlanName">{plan.currentPlan ?? ''}</output>
      <button onClick={() => void plan.refreshPlan()}>Refresh plan</button>
      <button onClick={() => void plan.claimTrial()}>Check trial availability</button>
      <p role="status" aria-label="Plan status">{plan.planError ?? plan.claimError ?? ''}</p>
    </>
  );
}

beforeEach(() => {
  localStorage.clear();
  registerPlanResolver(null);
  act(() => invalidateQueueOwner());
  current = a;
  Object.defineProperty(navigator, 'onLine', { configurable: true, value: true });
  capabilitiesReply = async () => Response.json(COMMUNITY_DEFAULT_CAPABILITIES);
  vi.stubGlobal('fetch', vi.fn(async input =>
    String(input) === '/api/v1/auth/session-identity'
      ? Response.json({ ...current, expiresAt: Date.now() + 60000 })
      : capabilitiesReply(),
  ));
});

afterEach(() => {
  registerPlanResolver(null);
  act(() => invalidateQueueOwner());
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

it('grants full Community workspace access after verifying owner without calling Cloud billing', async () => {
  render(<Harness />);
  await waitFor(() => expect(screen.getByLabelText('Pro')).toHaveTextContent('true'));
  expect(screen.getByLabelText('PlanName')).toHaveTextContent('Community');
  const calledUrls = vi.mocked(fetch).mock.calls.map(([input]) => String(input));
  expect(calledUrls).toEqual(['/api/v1/auth/session-identity', '/api/v1/capabilities']);
  const [, options] = vi.mocked(fetch).mock.calls.find(([input]) => input === '/api/v1/capabilities')!;
  expect(options).toMatchObject({ credentials: 'same-origin', redirect: 'error', cache: 'no-store' });
});

it('fails closed to Community defaults when capabilities endpoint returns 404', async () => {
  capabilitiesReply = async () => new Response('Not Found', { status: 404 });
  render(<Harness />);
  await waitFor(() => expect(screen.getByLabelText('Pro')).toHaveTextContent('true'));
  expect(screen.getByLabelText('PlanName')).toHaveTextContent('Community');
});

it.each(['Free', 'Starter', 'Pro', 'Business'])('delegates to registered Cloud plan resolver for %s when saas_billing is enabled', async current_plan => {
  capabilitiesReply = async () => Response.json(CLOUD_BILLING_CAPABILITIES);
  const resolver = vi.fn(async () => ({
    currentPlan: current_plan,
    hasPro: ['Pro', 'Business'].includes(current_plan),
    planError: null,
  }));
  registerPlanResolver(resolver);
  render(<Harness />);
  await waitFor(() => expect(resolver).toHaveBeenCalledOnce());
  expect(resolver).toHaveBeenCalledWith(expect.objectContaining({ userId: a.userId, tenantId: a.tenantId }));
  await waitFor(() => expect(screen.getByLabelText('Pro')).toHaveTextContent(String(['Pro', 'Business'].includes(current_plan))));
  expect(screen.getByLabelText('PlanName')).toHaveTextContent(current_plan);
});

it('fails closed when a registered Cloud plan resolver reports an unavailable plan', async () => {
  capabilitiesReply = async () => Response.json(CLOUD_BILLING_CAPABILITIES);
  registerPlanResolver(async () => ({
    currentPlan: null,
    hasPro: false,
    planError: 'Current plan data is unavailable. Refresh to check again.',
  }));
  render(<Harness />);
  await waitFor(() => expect(screen.getByRole('status', { name: 'Plan status' })).toHaveTextContent(/unavailable/i));
  expect(screen.getByLabelText('Pro')).toHaveTextContent('false');
});

it.each(['omnisolo_auth_changed', 'pagehide', 'storage'])('retires access immediately on %s', async event => {
  render(<Harness />);
  await waitFor(() => expect(screen.getByLabelText('Pro')).toHaveTextContent('true'));
  act(() => window.dispatchEvent(event === 'storage' ? new StorageEvent('storage', { key: 'omnisolo_queue_identity_epoch_v2' }) : new Event(event)));
  expect(screen.getByLabelText('Pro')).toHaveTextContent('false');
});

it('retires a no-event canonical owner replacement and rejects a late prior plan resolution', async () => {
  const pending = deferred<{ currentPlan: string; hasPro: boolean; planError: null }>();
  capabilitiesReply = async () => Response.json(CLOUD_BILLING_CAPABILITIES);
  registerPlanResolver(() => pending.promise);
  render(<Harness />);
  await waitFor(() => expect(vi.mocked(fetch).mock.calls.some(([url]) => url === '/api/v1/capabilities')).toBe(true));
  current = b;
  await act(async () => { await readQueueOwner(); });
  await act(async () => pending.resolve({ currentPlan: 'Pro', hasPro: true, planError: null }));
  expect(screen.getByLabelText('Pro')).toHaveTextContent('false');
});

it('a newer Free read cannot be overwritten by an older Pro resolution', async () => {
  const pending = deferred<{ currentPlan: string; hasPro: boolean; planError: null }>();
  let calls = 0;
  capabilitiesReply = async () => Response.json(CLOUD_BILLING_CAPABILITIES);
  registerPlanResolver(() => (++calls === 1 ? pending.promise : Promise.resolve({ currentPlan: 'Free', hasPro: false, planError: null })));
  render(<Harness />);
  await waitFor(() => expect(calls).toBe(1));
  await act(async () => fireEvent.click(screen.getByRole('button', { name: 'Refresh plan' })));
  await act(async () => pending.resolve({ currentPlan: 'Pro', hasPro: true, planError: null }));
  expect(screen.getByLabelText('Pro')).toHaveTextContent('false');
});

it('a trial check cannot flip entitlement or submit an unverified grant mutation', async () => {
  capabilitiesReply = async () => Response.json(CLOUD_BILLING_CAPABILITIES);
  registerPlanResolver(async () => ({ currentPlan: 'Free', hasPro: false, planError: null }));
  render(<Harness />);
  await waitFor(() => expect(screen.getByLabelText('PlanName')).toHaveTextContent('Free'));
  await act(async () => fireEvent.click(screen.getByRole('button', { name: 'Check trial availability' })));
  expect(screen.getByLabelText('Pro')).toHaveTextContent('false');
  expect(vi.mocked(fetch).mock.calls.some(([, options]) => options?.method === 'POST')).toBe(false);
  expect(screen.getByRole('status', { name: 'Plan status' })).toHaveTextContent(/unavailable|not verified/i);
});

it('holds confirmed access during a pending same-owner verification and restores it afterward', async () => {
  render(<Harness />);
  await waitFor(() => expect(screen.getByLabelText('Pro')).toHaveTextContent('true'));
  const pending = deferred<Response>();
  vi.mocked(fetch).mockImplementation(async input =>
    String(input) === '/api/v1/auth/session-identity' ? pending.promise : capabilitiesReply(),
  );
  let checking!: Promise<unknown>;
  act(() => { checking = readQueueOwner(); });
  expect(screen.getByLabelText('Pro')).toHaveTextContent('false');
  await act(async () => { pending.resolve(Response.json({ ...a, expiresAt: Date.now() + 60000 })); await checking; });
  expect(screen.getByLabelText('Pro')).toHaveTextContent('true');
});

it('retires access when a registered plan resolver reports session-changed', async () => {
  capabilitiesReply = async () => Response.json(CLOUD_BILLING_CAPABILITIES);
  registerPlanResolver(async () => 'session-changed');
  render(<Harness />);
  await waitFor(() => expect(screen.getByRole('status', { name: 'Plan status' })).toHaveTextContent(/session changed/i));
  expect(screen.getByLabelText('Pro')).toHaveTextContent('false');
});

it('an unmounted plan response cannot update the next view', async () => {
  const pending = deferred<{ currentPlan: string; hasPro: boolean; planError: null }>();
  let calls = 0;
  capabilitiesReply = async () => Response.json(CLOUD_BILLING_CAPABILITIES);
  registerPlanResolver(() => (++calls === 1 ? pending.promise : Promise.resolve({ currentPlan: 'Free', hasPro: false, planError: null })));
  const first = render(<Harness />);
  await waitFor(() => expect(calls).toBe(1));
  first.unmount();
  current = b;
  render(<Harness />);
  await act(async () => pending.resolve({ currentPlan: 'Pro', hasPro: true, planError: null }));
  expect(screen.getByLabelText('Pro')).toHaveTextContent('false');
});
