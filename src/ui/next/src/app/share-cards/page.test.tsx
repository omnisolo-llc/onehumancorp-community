import { render, screen, act, waitFor } from '@testing-library/react';
import ShareCardsPage from './page';
import { describe, it, expect, vi, beforeEach } from 'vitest';

vi.mock('next/navigation', () => ({
  useRouter: () => ({ push: vi.fn() })
}));

describe('ShareCardsPage', () => {
  beforeEach(() => {
    vi.clearAllMocks();
    global.fetch = vi.fn().mockResolvedValue({ ok: true, json: async () => ({ current_plan: 'free' }) });
  });
  it('renders direct social share links', () => {
    render(<ShareCardsPage />);

    const twitterLink = screen.getByText(/Share on X/i);
    expect(twitterLink).toBeInTheDocument();
    expect(twitterLink.closest('a')).toHaveAttribute('href', expect.stringContaining('twitter.com/intent/tweet'));

    const facebookLink = screen.getByText(/Share on Facebook/i);
    expect(facebookLink).toBeInTheDocument();
    expect(facebookLink.closest('a')).toHaveAttribute('href', expect.stringContaining('facebook.com/sharer/sharer.php'));
  });

  it('renders Powered by OmniSolo branding in preview', () => {
    render(<ShareCardsPage />);
    const brandingElements = screen.getAllByText(/Powered by OmniSolo/i);
    expect(brandingElements.length).toBeGreaterThan(0);
  });

  it('shows soft paywall when trying to remove branding without Pro', () => {
    render(<ShareCardsPage />);
    const toggle = screen.getByRole('checkbox');
    act(() => {
        toggle.click();
    });
    expect(screen.getAllByText('Upgrade to Pro').length).toBeGreaterThan(0);
  });

  it('allows removing branding when capabilities confirm Community Edition', async () => {
    global.fetch = vi.fn(async url => Response.json(url === '/api/v1/auth/session-identity' ? { userId: 'owner', tenantId: 'tenant', expiresAt: Date.now() + 60000 } : { edition: 'community', features: { cloud_sync: false, oidc: false, saas_billing: false, multitenant: false } }));
    render(<ShareCardsPage />);
    await waitFor(() => expect(global.fetch).toHaveBeenCalledWith('/api/v1/capabilities', expect.objectContaining({ credentials: 'same-origin' })));
    await act(async () => { await new Promise(r => setTimeout(r, 0)); });
    const toggle = screen.getByRole('checkbox');
    act(() => {
        toggle.click();
    });
    expect(screen.queryByText('Powered by OmniSolo')).toBeNull();
  });
});
