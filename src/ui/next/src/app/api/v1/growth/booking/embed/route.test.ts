import { describe, expect, it } from 'vitest';
import { GET } from './route';

describe('booking embed API', () => {
  it('sanitizes tenant/service values and never reflects Host or X-Forwarded-Proto headers', async () => {
    const response = await GET(new Request('http://localhost/api/v1/growth/booking/embed?tenant=<script>alert(1)</script>&service=<script>alert(2)</script>', {
      headers: {
        host: 'attacker.example',
        'x-forwarded-proto': 'http',
      },
    }));

    expect(response.status).toBe(200);
    const html = await response.text();

    expect(html).not.toContain('<script>alert(1)</script>');
    expect(html).not.toContain('<script>alert(2)</script>');
    expect(html).not.toContain('attacker.example');
    expect(html).toContain('href="/booking?tenant=%3Cscript%3Ealert(1)%3C%2Fscript%3E"');
    expect(html).toContain('&lt;script&gt;alert(2)&lt;/script&gt;');
  });

  it('sets hardening headers for embeddable HTML', async () => {
    const response = await GET(new Request('http://localhost/api/v1/growth/booking/embed?tenant=store'));

    expect(response.headers.get('content-security-policy')).toContain("default-src 'none'");
    expect(response.headers.get('content-security-policy')).toContain("script-src 'none'");
    expect(response.headers.get('x-content-type-options')).toBe('nosniff');
    expect(response.headers.get('referrer-policy')).toBe('strict-origin-when-cross-origin');
  });
});
