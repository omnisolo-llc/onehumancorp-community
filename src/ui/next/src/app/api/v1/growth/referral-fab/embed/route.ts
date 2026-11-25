import { NextResponse } from 'next/server';

const HEX_COLOR_RE = /^#(?:[0-9a-fA-F]{3}|[0-9a-fA-F]{6})$/;
const DEFAULT_THEME_COLOR = '#2563eb';

function safeThemeColor(raw: string | null): string {
    const trimmed = (raw || '').trim();
    return HEX_COLOR_RE.test(trimmed) ? trimmed.toLowerCase() : DEFAULT_THEME_COLOR;
}

function stripControlChars(value: string): string {
    let out = '';
    for (let i = 0; i < value.length; i += 1) {
        const code = value.charCodeAt(i);
        if (code >= 0x20 && code !== 0x7f) out += value[i];
    }
    return out;
}

function safeBoundedText(raw: string | null, fallback: string, maxLen: number): string {
    const cleaned = stripControlChars(raw || '')
        .trim()
        .slice(0, maxLen);
    return cleaned || fallback;
}

function serializeJsLiteral(value: unknown): string {
    return JSON.stringify(value)
        .replace(/</g, '\\u003c')
        .replace(/>/g, '\\u003e')
        .replace(/&/g, '\\u0026')
        .replace(/'/g, '\\u0027')
        .replace(/\//g, '\\u002f')
        .replace(/\u2028/g, '\\u2028')
        .replace(/\u2029/g, '\\u2029');
}

export async function GET(request: Request) {
    const { searchParams } = new URL(request.url);
    const tenant = safeBoundedText(searchParams.get('tenant'), 'demo', 80);
    const reward = safeBoundedText(searchParams.get('reward'), '$10', 60);
    const themeColor = safeThemeColor(searchParams.get('themeColor'));
    const removeBranding = searchParams.get('removeBranding') === 'true';

    const configLiteral = serializeJsLiteral({
        tenant,
        encodedTenant: encodeURIComponent(tenant),
        reward,
        themeColor,
        removeBranding,
    });

    const jsCode = `(function() {
    'use strict';
    if (document.getElementById('omnisolo-referral-fab')) return;

    const config = ${configLiteral};
    const SVG_NS = 'http://www.w3.org/2000/svg';

    function createIconSvg(pathD) {
        const svg = document.createElementNS(SVG_NS, 'svg');
        svg.setAttribute('viewBox', '0 0 24 24');
        svg.setAttribute('fill', 'none');
        svg.setAttribute('stroke', 'currentColor');
        svg.setAttribute('aria-hidden', 'true');
        svg.style.width = '24px';
        svg.style.height = '24px';
        const path = document.createElementNS(SVG_NS, 'path');
        path.setAttribute('stroke-linecap', 'round');
        path.setAttribute('stroke-linejoin', 'round');
        path.setAttribute('stroke-width', '2');
        path.setAttribute('d', pathD);
        svg.appendChild(path);
        return svg;
    }

    const GIFT_PATH = 'M12 8v13m0-13V6a2 2 0 112 2h-2zm0 0V5.5A2.5 2.5 0 109.5 8H12zm-7 4h14M5 12a2 2 0 110-4h14a2 2 0 110 4M5 12v7a2 2 0 002 2h10a2 2 0 002-2v-7';
    const CLOSE_PATH = 'M6 18L18 6M6 6l12 12';

    const wrapper = document.createElement('div');
    wrapper.id = 'omnisolo-referral-fab';
    wrapper.style.position = 'fixed';
    wrapper.style.bottom = '24px';
    wrapper.style.right = '24px';
    wrapper.style.zIndex = '999999';
    wrapper.style.fontFamily = '-apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, Helvetica, Arial, sans-serif';

    const fab = document.createElement('button');
    fab.type = 'button';
    fab.setAttribute('aria-label', 'Open referral rewards');
    fab.setAttribute('aria-expanded', 'false');
    fab.style.width = '56px';
    fab.style.height = '56px';
    fab.style.border = 'none';
    fab.style.borderRadius = '50%';
    fab.style.backgroundColor = config.themeColor;
    fab.style.boxShadow = '0 10px 15px -3px rgba(0, 0, 0, 0.1), 0 4px 6px -2px rgba(0, 0, 0, 0.05)';
    fab.style.display = 'flex';
    fab.style.alignItems = 'center';
    fab.style.justifyContent = 'center';
    fab.style.cursor = 'pointer';
    fab.style.transition = 'transform 0.2s ease';
    fab.style.color = 'white';
    fab.appendChild(createIconSvg(GIFT_PATH));

    fab.addEventListener('mouseover', () => { fab.style.transform = 'scale(1.05)'; });
    fab.addEventListener('mouseout', () => { fab.style.transform = 'scale(1)'; });

    const popover = document.createElement('div');
    popover.setAttribute('role', 'dialog');
    popover.setAttribute('aria-label', 'Referral rewards');
    popover.style.position = 'absolute';
    popover.style.bottom = '70px';
    popover.style.right = '0';
    popover.style.width = '300px';
    popover.style.backgroundColor = 'white';
    popover.style.borderRadius = '16px';
    popover.style.boxShadow = '0 20px 25px -5px rgba(0, 0, 0, 0.1), 0 10px 10px -5px rgba(0, 0, 0, 0.04)';
    popover.style.padding = '20px';
    popover.style.display = 'none';
    popover.style.transformOrigin = 'bottom right';
    popover.style.transition = 'all 0.2s ease';
    popover.style.opacity = '0';
    popover.style.transform = 'scale(0.95)';
    popover.style.border = '1px solid #f3f4f6';

    const heading = document.createElement('h3');
    heading.style.margin = '0 0 8px 0';
    heading.style.color = '#111827';
    heading.style.fontSize = '16px';
    heading.style.fontWeight = '700';
    heading.textContent = 'Get ' + config.reward;

    const description = document.createElement('p');
    description.style.margin = '0 0 16px 0';
    description.style.color = '#4b5563';
    description.style.fontSize = '14px';
    description.style.lineHeight = '1.4';
    description.textContent = 'Give a friend ' + config.reward + ' off their first order, and get ' + config.reward + ' when they buy!';

    const form = document.createElement('form');
    form.id = 'omnisolo-referral-form';
    form.style.display = 'flex';
    form.style.flexDirection = 'column';
    form.style.gap = '12px';
    form.style.margin = '0';

    const emailInput = document.createElement('input');
    emailInput.type = 'email';
    emailInput.id = 'omnisolo-referral-email';
    emailInput.placeholder = 'Enter your email';
    emailInput.required = true;
    emailInput.setAttribute('aria-label', 'Email address');
    emailInput.style.width = '100%';
    emailInput.style.padding = '10px 12px';
    emailInput.style.border = '1px solid #e5e7eb';
    emailInput.style.borderRadius = '8px';
    emailInput.style.fontSize = '14px';
    emailInput.style.boxSizing = 'border-box';
    emailInput.style.background = '#f9fafb';

    const submitBtn = document.createElement('button');
    submitBtn.type = 'submit';
    submitBtn.textContent = 'Get Share Link';
    submitBtn.style.width = '100%';
    submitBtn.style.padding = '10px';
    submitBtn.style.backgroundColor = config.themeColor;
    submitBtn.style.color = 'white';
    submitBtn.style.border = 'none';
    submitBtn.style.borderRadius = '8px';
    submitBtn.style.fontWeight = '600';
    submitBtn.style.cursor = 'pointer';
    submitBtn.style.fontSize = '14px';

    form.appendChild(emailInput);
    form.appendChild(submitBtn);

    const successDiv = document.createElement('div');
    successDiv.id = 'omnisolo-referral-success';
    successDiv.style.display = 'none';
    successDiv.style.flexDirection = 'column';
    successDiv.style.gap = '12px';

    const successTitle = document.createElement('div');
    successTitle.style.color = '#10b981';
    successTitle.style.fontWeight = '500';
    successTitle.style.fontSize = '14px';
    successTitle.textContent = 'Here is your link!';

    const linkInput = document.createElement('input');
    linkInput.type = 'text';
    linkInput.id = 'omnisolo-referral-link';
    linkInput.readOnly = true;
    linkInput.value = '/share?ref=' + config.encodedTenant;
    linkInput.setAttribute('aria-label', 'Your referral link');
    linkInput.style.width = '100%';
    linkInput.style.padding = '10px 12px';
    linkInput.style.border = '1px solid #e5e7eb';
    linkInput.style.borderRadius = '8px';
    linkInput.style.fontSize = '12px';
    linkInput.style.boxSizing = 'border-box';
    linkInput.style.background = '#f3f4f6';
    linkInput.style.color = '#374151';

    const copyBtn = document.createElement('button');
    copyBtn.type = 'button';
    copyBtn.id = 'omnisolo-copy-btn';
    copyBtn.textContent = 'Copy Link';
    copyBtn.style.width = '100%';
    copyBtn.style.padding = '10px';
    copyBtn.style.backgroundColor = '#f3f4f6';
    copyBtn.style.color = '#374151';
    copyBtn.style.border = 'none';
    copyBtn.style.borderRadius = '8px';
    copyBtn.style.fontWeight = '600';
    copyBtn.style.cursor = 'pointer';
    copyBtn.style.fontSize = '14px';

    successDiv.appendChild(successTitle);
    successDiv.appendChild(linkInput);
    successDiv.appendChild(copyBtn);

    popover.appendChild(heading);
    popover.appendChild(description);
    popover.appendChild(form);
    popover.appendChild(successDiv);

    if (!config.removeBranding) {
        const brandingWrap = document.createElement('div');
        brandingWrap.style.marginTop = '12px';
        brandingWrap.style.textAlign = 'center';
        brandingWrap.style.fontSize = '11px';
        brandingWrap.style.fontWeight = '500';

        const brandingLink = document.createElement('a');
        brandingLink.href = '/api/v1/growth/referrals/click?target=/onboarding&ref=' + config.encodedTenant;
        brandingLink.target = '_blank';
        brandingLink.rel = 'noopener noreferrer';
        brandingLink.style.color = '#9ca3af';
        brandingLink.style.textDecoration = 'none';
        brandingLink.textContent = 'Powered by OmniSolo';
        brandingWrap.appendChild(brandingLink);
        popover.appendChild(brandingWrap);
    }

    wrapper.appendChild(popover);
    wrapper.appendChild(fab);
    document.body.appendChild(wrapper);

    let isOpen = false;
    fab.addEventListener('click', () => {
        isOpen = !isOpen;
        fab.setAttribute('aria-expanded', String(isOpen));
        if (isOpen) {
            popover.style.display = 'block';
            setTimeout(() => {
                popover.style.opacity = '1';
                popover.style.transform = 'scale(1)';
            }, 10);
            fab.replaceChildren(createIconSvg(CLOSE_PATH));
        } else {
            popover.style.opacity = '0';
            popover.style.transform = 'scale(0.95)';
            setTimeout(() => {
                popover.style.display = 'none';
            }, 200);
            fab.replaceChildren(createIconSvg(GIFT_PATH));
        }
    });

    form.addEventListener('submit', (e) => {
        e.preventDefault();
        const email = emailInput.value.trim();
        if (!email) return;
        fetch('/api/v1/growth/referrals/generate', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ tenantId: config.tenant, email: email })
        }).then((res) => {
            if (res.ok) return res.json();
            throw new Error('Backend failed');
        }).then((data) => {
            form.style.display = 'none';
            successDiv.style.display = 'flex';
            if (
                data &&
                typeof data.link === 'string' &&
                (data.link.startsWith('https://') ||
                    data.link.startsWith('http://') ||
                    (data.link.startsWith('/') && !data.link.startsWith('//')))
            ) {
                linkInput.value = data.link;
            }
        }).catch((err) => {
            console.error('Failed to generate referral', err);
        });
    });

    copyBtn.addEventListener('click', () => {
        const originalText = copyBtn.textContent || 'Copy Link';
        const onCopied = () => {
            copyBtn.textContent = 'Copied!';
            setTimeout(() => {
                copyBtn.textContent = originalText;
            }, 2000);
        };
        if (navigator.clipboard && typeof navigator.clipboard.writeText === 'function') {
            navigator.clipboard.writeText(linkInput.value).then(onCopied).catch(() => {
                linkInput.select();
                onCopied();
            });
        } else {
            linkInput.select();
            onCopied();
        }
    });
})();
`;

    return new NextResponse(jsCode, {
        headers: {
            'Content-Type': 'application/javascript; charset=utf-8',
            'X-Content-Type-Options': 'nosniff',
            'Cache-Control': 'public, max-age=300, s-maxage=300',
        },
    });
}
