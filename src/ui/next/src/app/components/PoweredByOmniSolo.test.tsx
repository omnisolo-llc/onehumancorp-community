/**
 * @vitest-environment jsdom
 */

import React from 'react';
import { render, screen, fireEvent, cleanup } from '@testing-library/react';
import { PoweredByOmniSolo } from './PoweredByOmniSolo';
import { describe, it, expect, afterEach } from 'vitest';

describe('PoweredByOmniSolo Component', () => {
  afterEach(() => {
    cleanup();
  });
  const testTenantId = 'test-org-123';

  it('renders the base button correctly', () => {
    render(<PoweredByOmniSolo tenantId={testTenantId} />);

    // The main base link should be rendered
    const linkElements = screen.getAllByRole('link');
    // Find the one that has the text Powered by OmniSolo
    const baseLink = linkElements.find(el => el.textContent?.includes('Powered by OmniSolo'));

    expect(baseLink).toBeDefined();
    expect(baseLink?.getAttribute('href')).toBe(`/onboarding?ref=${testTenantId}&source=footer_widget`);
  });

  it('shows the visitor popover when hovered and not owner', async () => {
    render(<PoweredByOmniSolo tenantId={testTenantId} />);

    // The popover content shouldn't be there initially
    expect(screen.queryByText(/Built with OmniSolo/i)).toBeNull();

    // Find the container wrapper and trigger hover
    const wrapper = screen.getAllByRole('link')[0].parentElement;
    if (!wrapper) throw new Error("Wrapper not found");

    fireEvent.mouseEnter(wrapper);

    // Wait for the popover content to appear
    expect(screen.getByText(/Built with OmniSolo/i)).toBeDefined();

    // There should now be two links to the onboarding URL (one in base, one in popover CTA)
    const links = screen.getAllByRole('link');
    expect(links.length).toBe(2);
    expect(links[0].getAttribute('href')).toBe(`/onboarding?ref=${testTenantId}&source=footer_widget`);
    expect(links[1].getAttribute('href')).toBe(`/onboarding?ref=${testTenantId}&source=footer_widget`);

    // CTA button text should be present
    expect(screen.getByText(/Create Your Own/i)).toBeDefined();
  });

  it('shows the branding settings popover when hovered and is owner', async () => {
    render(<PoweredByOmniSolo tenantId={testTenantId} isOwner={true} />);

    // The popover content shouldn't be there initially
    expect(screen.queryByText(/Customize Branding/i)).toBeNull();

    // Find the container wrapper and trigger hover
    const wrapper = screen.getAllByRole('link')[0].parentElement;
    if (!wrapper) throw new Error("Wrapper not found");

    fireEvent.mouseEnter(wrapper);

    // Wait for the owner popover content to appear
    expect(screen.getByText(/Customize Branding/i)).toBeDefined();

    // There should now be two links to workspace settings (one in base, one in popover CTA)
    const links = screen.getAllByRole('link');
    expect(links.length).toBe(2);
    expect(links[0].getAttribute('href')).toBe(`/settings?source=footer_widget_branding`);
    expect(links[1].getAttribute('href')).toBe(`/settings?source=footer_widget_branding`);

    // CTA button text should be present
    expect(screen.getAllByText(/Open Branding Settings/i).length).toBeGreaterThan(0);
  });
});
