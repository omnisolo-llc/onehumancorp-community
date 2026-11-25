import React from 'react';
import { act, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import InboxPage from './page';

const queryState = vi.hoisted(() => ({
  data: [] as Array<Record<string, string>>,
  search: "",
}));

vi.mock('next/navigation', () => ({ useRouter: () => ({ push: vi.fn() }), useSearchParams: () => new URLSearchParams(queryState.search) }));

vi.mock('../components/AppShell', () => ({
  AppShell: ({ children, actions }: { children: React.ReactNode; actions?: Array<{ label: string; href: string }> }) => <div><nav>{actions?.map(action => <a key={action.href} href={action.href}>{action.label}</a>)}</nav>{children}</div>,
}));

function defaultInboxFetch(url: RequestInfo | URL): Promise<Response> {
  const target = String(url);
  if (target === '/api/v1/ui/omni_inbox') return Promise.resolve(Response.json(queryState.data));
  if (target.startsWith('/api/v1/agents/approvals')) return Promise.resolve(Response.json({ pending_approvals: [] }));
  return Promise.resolve(Response.json([]));
}

async function renderInbox() {
  const view = render(<InboxPage />);
  await screen.findByTestId('inbox-settled');
  return view;
}

beforeEach(() => {
  queryState.data = [];
  queryState.search = "";
  vi.spyOn(globalThis, 'fetch').mockImplementation((url) => defaultInboxFetch(url));
});

afterEach(() => {
  vi.restoreAllMocks();
});

test('the audit action opens the existing audit dashboard document', async () => {
  await renderInbox();
  expect(screen.getByRole('link', { name: 'Audit' })).toHaveAttribute('href', '/agent-audit-dashboard.html');
});

test('renders a stable empty state when the inbox API returns no messages', async () => {
  const { container } = await renderInbox();

  expect(screen.getByText('No inbox messages found for this tenant.')).toBeInTheDocument();
  expect(screen.getByText('Select a database-backed message to inspect it.')).toBeInTheDocument();
  expect(container.textContent).not.toContain('\\n');
});

test('renders message markup as text while preserving safe HTTPS media', async () => {
  queryState.data = [{
    id: 'message-1',
    content: '<script>window.compromised = true</script>\n![Receipt](https://cdn.example.test/receipt.png)',
    draft_reply: '[Media: application/pdf - https://cdn.example.test/invoice.pdf]',
    status: 'resolved',
  }];

  const { container } = await renderInbox();

  expect(screen.getByText('<script>window.compromised = true</script>')).toBeInTheDocument();
  expect(container.querySelector('script')).toBeNull();
  expect(screen.getByRole('img', { name: 'Receipt' })).toHaveAttribute(
    'src',
    'https://cdn.example.test/receipt.png',
  );
  expect(screen.getByRole('link', { name: 'Attached Media (application/pdf)' })).toHaveAttribute(
    'href',
    'https://cdn.example.test/invoice.pdf',
  );
});


test('marks the actual pending API surface busy until the workspace is committed', async () => {
  let release!: (response: Response) => void;
  const pending = new Promise<Response>(resolve => { release = resolve; });
  vi.mocked(globalThis.fetch).mockImplementation(async url => String(url) === '/api/v1/ui/omni_inbox' ? pending : Response.json([]));
  const { container } = render(<InboxPage />);
  const loading = screen.getByText('Loading inbox messages...').closest('[aria-busy="true"]');
  expect(loading).not.toBeNull();
  expect(container.querySelector('[data-testid="inbox-settled"]')).toBeNull();
  await act(async () => { release(Response.json([])); });
  await waitFor(() => expect(screen.getByText('No inbox messages found for this tenant.')).toBeVisible());
  expect(container.querySelector('[aria-busy="true"]')).toBeNull();
  expect(container.querySelector('[data-testid="inbox-settled"]')).not.toBeNull();
});

test('loads the real omni inbox records without an unused request to a different inbox table', async () => {
  const fetcher = vi.mocked(globalThis.fetch);
  fetcher.mockImplementation(async (url) => Response.json(String(url) === '/api/v1/ui/omni_inbox' ? [{ id: 'owned-message', content: 'Actual owned inbox content', status: 'pending' }] : []));
  render(<InboxPage />);
  await screen.findAllByText('Actual owned inbox content');
  expect(fetcher.mock.calls.filter(([url]) => url === '/api/v1/ui/inbox/messages')).toHaveLength(0);
  expect(fetcher.mock.calls.some(([url]) => url === '/api/v1/ui/omni_inbox')).toBe(true);
});

const detail = () => screen.getByText('Conversation Detail').closest('section')!;
test('opens the exact searched message from the fetched rows instead of the first inbox item', async () => {
  queryState.search = 'messageId=second';
  queryState.data = [{ id: 'first', content: 'First conversation', status: 'resolved' }, { id: 'second', content: 'Searched conversation', status: 'resolved' }];
  await renderInbox();
  expect(within(detail()).getByText('Searched conversation')).toBeVisible();
  expect(within(detail()).queryByText('First conversation')).toBeNull();
});
test('does not substitute another message for an absent or foreign search result', async () => {
  queryState.search = 'messageId=not-in-this-workspace';
  queryState.data = [{ id: 'first', content: 'Unrelated conversation', status: 'pending' }];
  await renderInbox();
  expect(within(detail()).getByText('The requested message is unavailable in this workspace.')).toBeVisible();
  expect(within(detail()).queryByText('Unrelated conversation')).toBeNull();
  expect(screen.queryByRole('button', { name: 'Send Reply' })).toBeNull();
  fireEvent.click(screen.getByRole('button', { name: /Unrelated conversation/ }));
  expect(within(detail()).getByText('Unrelated conversation')).toBeVisible();
});
test('uses a changed search destination immediately and clears an earlier reply draft', async () => {
  queryState.search = 'messageId=first';
  queryState.data = [{ id: 'first', content: 'First conversation', status: 'pending' }, { id: 'second', content: 'Second conversation', status: 'pending' }];
  const view = await renderInbox();
  fireEvent.change(screen.getByPlaceholderText('Type your reply here...'), { target: { value: 'Draft for first only' } });
  queryState.search = 'messageId=second'; view.rerender(<InboxPage />);
  expect(within(detail()).getByText('Second conversation')).toBeVisible();
  expect(screen.getByPlaceholderText('Type your reply here...')).toHaveValue('');
});

test('keeps unsent replies attached to their original conversation when search navigation changes', async () => {
  queryState.search = 'messageId=first';
  queryState.data = [{ id: 'first', content: 'First conversation', status: 'pending' }, { id: 'second', content: 'Second conversation', status: 'pending' }];
  const view = await renderInbox();
  fireEvent.change(screen.getByPlaceholderText('Type your reply here...'), { target: { value: 'First draft' } });
  queryState.search = 'messageId=second'; view.rerender(<InboxPage />);
  fireEvent.change(screen.getByPlaceholderText('Type your reply here...'), { target: { value: 'Second draft' } });
  queryState.search = 'messageId=first'; view.rerender(<InboxPage />);
  expect(screen.getByPlaceholderText('Type your reply here...')).toHaveValue('First draft');
});

test.each(['another message', 'newer text'])('does not erase %s when an older send finishes', async destination => {
  queryState.search = 'messageId=first';
  queryState.data = [{ id: 'first', content: 'First conversation', status: 'pending' }, { id: 'second', content: 'Second conversation', status: 'pending' }];
  let finish!: (response: Response) => void;
  vi.mocked(globalThis.fetch).mockImplementation(async (url, options) => options?.method === 'POST'
    ? new Promise<Response>(resolve => { finish = resolve; }) : defaultInboxFetch(url));
  const view = await renderInbox();
  fireEvent.change(screen.getByPlaceholderText('Type your reply here...'), { target: { value: 'Submitted first draft' } });
  fireEvent.click(screen.getByRole('button', { name: 'Send Reply' }));
  if (destination === 'another message') { queryState.search = 'messageId=second'; view.rerender(<InboxPage />); }
  fireEvent.change(screen.getByPlaceholderText('Type your reply here...'), { target: { value: 'Keep this newer draft' } });
  await act(async () => finish(Response.json({ success: true })));
  expect(screen.getByPlaceholderText('Type your reply here...')).toHaveValue('Keep this newer draft');
  if (destination === 'another message') expect(screen.queryByText('Manual reply sent.')).toBeNull();
});

test.each(['omnisolo_auth_changed', 'storage'])('clears mounted reply drafts and retires an old completion on %s', async event => {
  queryState.search = 'messageId=first';
  queryState.data = [{ id: 'first', content: 'First conversation', status: 'pending' }];
  let finish!: (response: Response) => void;
  vi.mocked(globalThis.fetch).mockImplementation(async (url, options) => options?.method === 'POST'
    ? new Promise<Response>(resolve => { finish = resolve; }) : defaultInboxFetch(url));
  await renderInbox();
  fireEvent.change(screen.getByPlaceholderText('Type your reply here...'), { target: { value: 'Private old draft' } });
  fireEvent.click(screen.getByRole('button', { name: 'Send Reply' }));
  act(() => window.dispatchEvent(event === 'storage' ? new StorageEvent('storage', { key: 'omnisolo_queue_identity_epoch_v2' }) : new Event(event)));
  expect(screen.getByPlaceholderText('Type your reply here...')).toHaveValue('');
  await act(async () => finish(Response.json({ success: true })));
  expect(screen.queryByText('Manual reply sent.')).toBeNull();
});

test('retires a manual selection when a searched destination changes and returns', async () => {
  queryState.search = 'messageId=first';
  queryState.data = [{ id: 'first', content: 'First conversation', status: 'resolved' }, { id: 'second', content: 'Second conversation', status: 'resolved' }];
  const view = await renderInbox();
  fireEvent.click(screen.getByRole('button', { name: /Second conversation/ }));
  queryState.search = 'messageId=second'; view.rerender(<InboxPage />);
  queryState.search = 'messageId=first'; view.rerender(<InboxPage />);
  expect(within(detail()).getByText('First conversation')).toBeVisible();
  expect(within(detail()).queryByText('Second conversation')).toBeNull();
});

test('resets original-language feedback when refreshed rows change the default selected message', async () => {
  queryState.data = [{ id: 'first', content: 'First translation', original_content: 'First original', translated_from_language: 'Spanish', status: 'resolved' }];
  await renderInbox();
  fireEvent.click(screen.getByRole('button', { name: 'Original Spanish' }));
  expect(within(detail()).getByText('First original')).toBeVisible();
  queryState.data = [{ id: 'second', content: 'Second translation', original_content: 'Second original', translated_from_language: 'French', status: 'resolved' }];
  await act(async () => { window.dispatchEvent(new Event('omnisolo_inbox_refresh')); });
  await waitFor(() => expect(within(detail()).getByText('Second translation')).toBeVisible());
  expect(screen.getByRole('button', { name: 'Original French' })).toBeVisible();
});

const manualRequestId = '10b562b2-4c75-4f85-b6fa-43a99d9b655a';
const manualReceipt = (body: Record<string, unknown>, state = 'pending', extra = {}) => ({
  request_id: body.request_id ?? manualRequestId, message_id: body.message_id ?? 'first',
  state, provider_message_id: state === 'accepted' ? 'provider-123' : null,
  detail: null, draft_reply: body.edited_reply ?? 'Submitted reply', ...extra,
});
function manualInbox() {
  queryState.search = 'messageId=first';
  queryState.data = [{ id: 'first', content: 'First conversation', status: 'pending' }, { id: 'second', content: 'Second conversation', status: 'pending' }];
}
function typeReply(value = 'Submitted reply') {
  fireEvent.change(screen.getByPlaceholderText('Type your reply here...'), { target: { value } });
}

test('prepares an immutable UUID request then clears only a provider-accepted reply', async () => {
  manualInbox();
  const actions: Record<string, unknown>[] = [];
  vi.mocked(globalThis.fetch).mockImplementation(async (url, options) => {
    if (options?.method !== 'POST') return defaultInboxFetch(url);
    const body = JSON.parse(String(options.body)); actions.push(body);
    return Response.json(manualReceipt(body, body.prepare_only ? 'pending' : 'accepted'));
  });
  await renderInbox(); typeReply();
  fireEvent.click(screen.getByRole('button', { name: 'Send Reply' }));
  await screen.findByText(/Provider accepted the reply/);
  expect(actions).toHaveLength(2);
  expect(actions[0]).toMatchObject({ message_id: 'first', approved: true, edited_reply: 'Submitted reply', prepare_only: true });
  expect(actions[0].request_id).toMatch(/^[\da-f]{8}(-[\da-f]{4}){3}-[\da-f]{12}$/i);
  expect(actions[1]).toEqual({ message_id: 'first', approved: true, edited_reply: 'Submitted reply', request_id: actions[0].request_id });
  expect(screen.getByPlaceholderText('Type your reply here...')).toHaveValue('');
  expect(screen.queryByText('Manual reply sent.')).toBeNull();
});

test.each([
  ['malformed', { success: true }],
  ['missing provider ID', { state: 'accepted', provider_message_id: null }],
  ['wrong message', { state: 'accepted', message_id: 'second' }],
  ['wrong request', { state: 'accepted', request_id: 'different-request' }],
  ['changed canonical reply', { state: 'accepted', draft_reply: 'Different reply' }],
])('keeps the submitted draft after a %s response and requires readback before another send', async (_label, invalid) => {
  manualInbox(); let posts = 0;
  vi.mocked(globalThis.fetch).mockImplementation(async (url, options) => {
    if (options?.method !== 'POST') return defaultInboxFetch(url);
    const body = JSON.parse(String(options.body)); posts += 1;
    if (body.prepare_only) return Response.json(manualReceipt(body));
    return Response.json(_label === 'malformed' ? invalid : manualReceipt(body, 'accepted', invalid));
  });
  await renderInbox(); typeReply();
  fireEvent.click(screen.getByRole('button', { name: 'Send Reply' }));
  await screen.findByText(/Check saved reply status before/);
  expect(screen.getByPlaceholderText('Type your reply here...')).toHaveValue('Submitted reply');
  fireEvent.click(screen.getByRole('button', { name: 'Send Reply' }));
  expect(posts).toBe(2);
});

test.each(['navigation', 'auth change', 'newer draft', 'pagehide'])('never sends a late preparation after %s', async reason => {
  manualInbox(); let finish!: () => void; let posts = 0;
  vi.mocked(globalThis.fetch).mockImplementation(async (url, options) => {
    if (options?.method !== 'POST') return defaultInboxFetch(url);
    posts += 1; const body = JSON.parse(String(options.body));
    return new Promise<Response>(resolve => { finish = () => resolve(Response.json(manualReceipt(body))); });
  });
  const view = await renderInbox(); typeReply();
  fireEvent.click(screen.getByRole('button', { name: 'Send Reply' }));
  fireEvent.click(screen.getByRole('button', { name: 'Send Reply' }));
  expect(posts).toBe(1);
  if (reason === 'navigation') { queryState.search = 'messageId=second'; view.rerender(<InboxPage />); queryState.search = 'messageId=first'; view.rerender(<InboxPage />); }
  if (reason === 'auth change') act(() => window.dispatchEvent(new Event('omnisolo_auth_changed')));
  if (reason === 'pagehide') act(() => window.dispatchEvent(new Event('pagehide')));
  if (reason === 'newer draft') typeReply('Keep newer draft');
  await act(async () => finish());
  expect(posts).toBe(1);
  if (reason === 'newer draft') expect(screen.getByPlaceholderText('Type your reply here...')).toHaveValue('Keep newer draft');
});

test('reads an unknown saved receipt after remount and restores its draft without replaying', async () => {
  manualInbox(); const actionCalls: string[] = [];
  vi.mocked(globalThis.fetch).mockImplementation(async (url, options) => {
    if (!String(url).includes('/omni_inbox/action')) return defaultInboxFetch(url);
    actionCalls.push(options?.method ?? 'GET');
    return Response.json(manualReceipt({}, 'unknown'));
  });
  const view = await renderInbox(); view.unmount(); await renderInbox();
  fireEvent.click(screen.getByRole('button', { name: 'Check saved reply status' }));
  await screen.findByText(/Provider outcome is unknown/);
  expect(screen.getByPlaceholderText('Type your reply here...')).toHaveValue('Submitted reply');
  typeReply('Changed reply');
  fireEvent.click(screen.getByRole('button', { name: 'Send Reply' }));
  expect(actionCalls).toEqual(['GET']);
});

test('recovers an accepted receipt after transport loss without a second provider attempt', async () => {
  manualInbox(); let submitted: Record<string, unknown> = {}; let posts = 0;
  vi.mocked(globalThis.fetch).mockImplementation(async (url, options) => {
    if (!String(url).includes('/omni_inbox/action')) return defaultInboxFetch(url);
    if (options?.method === 'POST') {
      submitted = JSON.parse(String(options.body)); posts += 1;
      if (submitted.prepare_only) return Response.json(manualReceipt(submitted));
      throw new Error('Connection lost');
    }
    expect(new URL(String(url), 'http://localhost').searchParams.get('request_id')).toBe(submitted.request_id);
    return Response.json(manualReceipt(submitted, 'accepted'));
  });
  await renderInbox(); typeReply();
  fireEvent.click(screen.getByRole('button', { name: 'Send Reply' }));
  await screen.findByText(/Check saved reply status before/);
  fireEvent.click(screen.getByRole('button', { name: 'Check saved reply status' }));
  await screen.findByText(/Provider accepted the reply/);
  expect(screen.getByPlaceholderText('Type your reply here...')).toHaveValue('');
  expect(posts).toBe(2);
});

test.each(['another message', 'auth change', 'newer draft'])('never overwrites %s with a late saved-receipt read', async reason => {
  manualInbox(); let finish!: () => void;
  vi.mocked(globalThis.fetch).mockImplementation(async (url) => {
    if (!String(url).includes('/omni_inbox/action')) return defaultInboxFetch(url);
    return new Promise<Response>(resolve => { finish = () => resolve(Response.json(manualReceipt({}, 'unknown'))); });
  });
  const view = await renderInbox();
  fireEvent.click(screen.getByRole('button', { name: 'Check saved reply status' }));
  if (reason === 'another message') { queryState.search = 'messageId=second'; view.rerender(<InboxPage />); }
  if (reason === 'auth change') act(() => window.dispatchEvent(new Event('omnisolo_auth_changed')));
  typeReply('Preserved new draft');
  await act(async () => finish());
  expect(screen.getByPlaceholderText('Type your reply here...')).toHaveValue('Preserved new draft');
  if (reason !== 'newer draft') expect(screen.queryByText(/Provider outcome is unknown/)).toBeNull();
});

test('dismissal retires pending preparation without clearing its local draft or sending it later', async () => {
  manualInbox(); let finish!: () => void; const posts: Record<string, unknown>[] = [];
  vi.mocked(globalThis.fetch).mockImplementation(async (url, options) => {
    if (options?.method !== 'POST') return defaultInboxFetch(url);
    const body = JSON.parse(String(options.body)); posts.push(body);
    if (body.prepare_only) return new Promise<Response>(resolve => { finish = () => resolve(Response.json(manualReceipt(body))); });
    return Response.json(manualReceipt(body, 'dismissed'));
  });
  await renderInbox(); typeReply();
  fireEvent.click(screen.getByRole('button', { name: 'Send Reply' }));
  fireEvent.click(screen.getByRole('button', { name: 'Dismiss message' }));
  await screen.findByText(/Message dismissed\. A previously started send cannot be recalled/);
  await act(async () => finish());
  expect(posts).toHaveLength(2);
  expect(posts[1]).toMatchObject({ message_id: 'first', approved: false });
  expect(posts[1].request_id).not.toBe(posts[0].request_id);
  expect(screen.getByPlaceholderText('Type your reply here...')).toHaveValue('Submitted reply');
  expect(screen.getByRole('button', { name: 'Send Reply' })).toBeDisabled();
});

test.each(['resolved', 'dismissed'])('retires a pending preparation when server rows report %s', async status => {
  manualInbox(); let finish!: () => void; let posts = 0;
  vi.mocked(globalThis.fetch).mockImplementation(async (url, options) => {
    if (options?.method !== 'POST') return defaultInboxFetch(url);
    posts += 1; const body = JSON.parse(String(options.body));
    return new Promise<Response>(resolve => { finish = () => resolve(Response.json(manualReceipt(body))); });
  });
  await renderInbox(); typeReply();
  fireEvent.click(screen.getByRole('button', { name: 'Send Reply' }));
  queryState.data = queryState.data.map(message => message.id === 'first' ? { ...message, status } : message);
  await act(async () => { window.dispatchEvent(new Event('omnisolo_inbox_refresh')); });
  await act(async () => finish());
  expect(posts).toBe(1);
});

test.each(['pending', 'retired', 'unknown', 'rejected', 'blocked'])('never clears a draft for a %s send receipt', async state => {
  manualInbox();
  vi.mocked(globalThis.fetch).mockImplementation(async (url, options) => {
    if (options?.method !== 'POST') return defaultInboxFetch(url);
    const body = JSON.parse(String(options.body));
    return Response.json(manualReceipt(body, body.prepare_only ? 'pending' : state));
  });
  await renderInbox(); typeReply(); fireEvent.click(screen.getByRole('button', { name: 'Send Reply' }));
  await waitFor(() => expect(screen.getByRole('status')).not.toHaveTextContent(/Preparing reply|Requesting provider/));
  expect(screen.getByPlaceholderText('Type your reply here...')).toHaveValue('Submitted reply');
});

test.each(['another message', 'newer text'])('preserves %s when a validated older acceptance arrives', async destination => {
  manualInbox(); let finish!: () => void;
  vi.mocked(globalThis.fetch).mockImplementation(async (url, options) => {
    if (options?.method !== 'POST') return defaultInboxFetch(url);
    const body = JSON.parse(String(options.body));
    if (body.prepare_only) return Response.json(manualReceipt(body));
    return new Promise<Response>(resolve => { finish = () => resolve(Response.json(manualReceipt(body, 'accepted'))); });
  });
  const view = await renderInbox(); typeReply(); fireEvent.click(screen.getByRole('button', { name: 'Send Reply' }));
  await screen.findByText('Requesting provider acceptance...');
  if (destination === 'another message') { queryState.search = 'messageId=second'; view.rerender(<InboxPage />); }
  typeReply('Keep newer draft');
  await act(async () => finish());
  expect(screen.getByPlaceholderText('Type your reply here...')).toHaveValue('Keep newer draft');
  if (destination === 'another message') expect(screen.queryByText(/Provider accepted the reply/)).toBeNull();
});

test('rejects another message receipt on readback without exposing its canonical draft', async () => {
  manualInbox();
  vi.mocked(globalThis.fetch).mockImplementation(async url => String(url).includes('/omni_inbox/action')
    ? Response.json(manualReceipt({ message_id: 'second', edited_reply: 'Private other draft' }, 'unknown')) : defaultInboxFetch(url));
  await renderInbox();
  fireEvent.click(screen.getByRole('button', { name: 'Check saved reply status' }));
  await screen.findByText(/Saved reply status is unavailable/);
  expect(screen.getByPlaceholderText('Type your reply here...')).toHaveValue('');
  expect(screen.queryByText('Private other draft')).toBeNull();
});

test('reuses the canonical pending request after reload only when the user explicitly sends it', async () => {
  manualInbox(); const posts: Record<string, unknown>[] = [];
  vi.mocked(globalThis.fetch).mockImplementation(async (url, options) => {
    if (!String(url).includes('/omni_inbox/action')) return defaultInboxFetch(url);
    if (options?.method !== 'POST') return Response.json(manualReceipt({}));
    const body = JSON.parse(String(options.body)); posts.push(body);
    return Response.json(manualReceipt(body, body.prepare_only ? 'pending' : 'accepted'));
  });
  await renderInbox();
  fireEvent.click(screen.getByRole('button', { name: 'Check saved reply status' }));
  await screen.findByText(/Reply prepared, but not sent/);
  expect(posts).toHaveLength(0);
  fireEvent.click(screen.getByRole('button', { name: 'Send Reply' }));
  await screen.findByText(/Provider accepted the reply/);
  expect(posts).toHaveLength(2);
  expect(posts.every(body => body.request_id === manualRequestId && body.edited_reply === 'Submitted reply')).toBe(true);
});

test('does not let a late attachment for another conversation cancel the current preparation', async () => {
  manualInbox(); let fileComplete!: () => void; let prepared!: () => void; const posts: Record<string, unknown>[] = [];
  const reader = vi.spyOn(FileReader.prototype, 'readAsDataURL').mockImplementation(function (this: FileReader) {
    fileComplete = () => this.onload?.({ target: { result: 'data:image/png;base64,example' } } as unknown as ProgressEvent<FileReader>);
  });
  vi.mocked(globalThis.fetch).mockImplementation(async (url, options) => {
    if (options?.method !== 'POST') return defaultInboxFetch(url);
    const body = JSON.parse(String(options.body)); posts.push(body);
    if (body.prepare_only) return new Promise<Response>(resolve => { prepared = () => resolve(Response.json(manualReceipt(body))); });
    return Response.json(manualReceipt(body, 'accepted'));
  });
  try {
    const view = await renderInbox();
    const input = view.container.querySelector('input[type=file]')!;
    fireEvent.change(input, { target: { files: [new File(['test'], 'test.png', { type: 'image/png' })] } });
    queryState.search = 'messageId=second'; view.rerender(<InboxPage />); typeReply('Second submitted reply');
    fireEvent.click(screen.getByRole('button', { name: 'Send Reply' }));
    await act(async () => { fileComplete(); });
    await act(async () => prepared());
    await screen.findByText(/Provider accepted the reply/);
    expect(posts).toHaveLength(2);
    expect(posts.every(body => body.message_id === 'second')).toBe(true);
  } finally { reader.mockRestore(); }
});


test('recovers a legacy request receipt without issuing another send', async () => {
  manualInbox(); const legacy = `legacy-${'a'.repeat(64)}`; const reads: string[] = [];
  vi.mocked(globalThis.fetch).mockImplementation(async (url, options) => {
    if (!String(url).includes('/omni_inbox/action')) return defaultInboxFetch(url);
    expect(options?.method).not.toBe('POST'); reads.push(String(url));
    return Response.json(manualReceipt({ request_id: legacy }, 'unknown'));
  });
  await renderInbox(); fireEvent.click(screen.getByRole('button', { name: 'Check saved reply status' }));
  await screen.findByText(/Provider outcome is unknown/);
  expect(screen.getByPlaceholderText('Type your reply here...')).toHaveValue('Submitted reply');
  fireEvent.click(screen.getByRole('button', { name: 'Check saved reply status' }));
  await waitFor(() => expect(reads).toHaveLength(2));
  expect(new URL(reads[1], 'http://localhost').searchParams.get('request_id')).toBe(legacy);
});

test.each(['resolved', 'dismissed'])('keeps provider-receipt readback available after a message becomes %s', async status => {
  manualInbox(); queryState.data[0].status = status;
  vi.mocked(globalThis.fetch).mockImplementation(async url => String(url).includes('/omni_inbox/action')
    ? Response.json(manualReceipt({}, 'unknown')) : defaultInboxFetch(url));
  await renderInbox();
  fireEvent.click(screen.getByRole('button', { name: 'Check saved reply status' }));
  await screen.findByText(/Provider outcome is unknown/);
  expect(screen.queryByRole('button', { name: 'Send Reply' })).toBeNull();
});

test.each(['accepted', 'unknown'])('shows prior %s provider evidence after dismissal readback without clearing the current draft', async state => {
  manualInbox();
  vi.mocked(globalThis.fetch).mockImplementation(async url => String(url).includes('/omni_inbox/action')
    ? Response.json(manualReceipt({}, 'dismissed', { prior_send: {
      request_id: 'prior-send', state, provider_message_id: state === 'accepted' ? 'prior-provider-id' : null,
      detail: null, draft_reply: 'Previously submitted provider reply',
    } })) : defaultInboxFetch(url));
  await renderInbox(); typeReply('Current preserved draft');
  fireEvent.click(screen.getByRole('button', { name: 'Check saved reply status' }));
  const evidence = await screen.findByRole('region', { name: 'Previous reply attempt' });
  expect(within(evidence).getByText('Previously submitted provider reply')).toBeVisible();
  expect(within(evidence).getByText(state === 'accepted' ? /Provider accepted the reply/ : /Provider outcome is unknown/)).toBeVisible();
  expect(screen.getByPlaceholderText('Type your reply here...')).toHaveValue('Current preserved draft');
  expect(screen.getByRole('button', { name: 'Send Reply' })).toBeDisabled();
});

test('blocks a new send after failed recovery until an authoritative read succeeds', async () => {
  manualInbox(); let readable = false; let posts = 0;
  vi.mocked(globalThis.fetch).mockImplementation(async (url, options) => {
    if (!String(url).includes('/omni_inbox/action')) return defaultInboxFetch(url);
    if (options?.method === 'POST') { posts += 1; return Response.json({}); }
    return readable ? Response.json({}, { status: 404 }) : Response.json({ success: true });
  });
  await renderInbox(); typeReply();
  fireEvent.click(screen.getByRole('button', { name: 'Check saved reply status' }));
  await screen.findByText(/Saved reply status is unavailable/);
  expect(screen.getByRole('button', { name: 'Send Reply' })).toBeDisabled();
  expect(posts).toBe(0);
  readable = true;
  fireEvent.click(screen.getByRole('button', { name: 'Check saved reply status' }));
  await screen.findByText(/No saved reply request was found/);
  expect(screen.getByRole('button', { name: 'Send Reply' })).toBeEnabled();
});

test.each(['accepted', 'unknown'])('recovers the other tab’s %s receipt after a conflicting prepare without clearing the unsubmitted draft', async state => {
  manualInbox(); const posts: Record<string, unknown>[] = []; const reads: URL[] = [];
  vi.mocked(globalThis.fetch).mockImplementation(async (url, options) => {
    if (!String(url).includes('/omni_inbox/action')) return defaultInboxFetch(url);
    if (options?.method === 'POST') {
      posts.push(JSON.parse(String(options.body)));
      return Response.json({ error: 'Inbox already has a provider outcome' }, { status: 409 });
    }
    const query = new URL(String(url), 'http://localhost'); reads.push(query);
    return query.searchParams.get('request_id') === posts[0]?.request_id ? Response.json({}, { status: 404 })
      : Response.json(manualReceipt({ request_id: 'other-tab-request', edited_reply: 'Submitted reply' }, state));
  });
  await renderInbox(); typeReply();
  fireEvent.click(screen.getByRole('button', { name: 'Send Reply' }));
  await screen.findByText(/Check saved reply status before/);
  fireEvent.click(screen.getByRole('button', { name: 'Check saved reply status' }));
  await screen.findByText(state === 'accepted' ? /Provider accepted the reply/ : /Provider outcome is unknown/);
  expect(reads).toHaveLength(2);
  expect(reads[0].searchParams.get('request_id')).toBe(posts[0].request_id);
  expect(reads[1].searchParams.has('request_id')).toBe(false);
  expect(reads.every(url => url.searchParams.get('message_id') === 'first')).toBe(true);
  expect(posts).toHaveLength(1);
  expect(posts[0].prepare_only).toBe(true);
  expect(screen.getByPlaceholderText('Type your reply here...')).toHaveValue('Submitted reply');
  expect(screen.getByRole('button', { name: 'Send Reply' })).toBeDisabled();
  expect(screen.getByRole('status')).not.toHaveClass('good');
  fireEvent.click(screen.getByRole('button', { name: 'Check saved reply status' }));
  await screen.findByText(state === 'accepted' ? /Provider accepted the reply/ : /Provider outcome is unknown/);
  expect(reads).toHaveLength(3);
  expect(reads[2].searchParams.get('request_id')).toBe('other-tab-request');
  expect(screen.getByPlaceholderText('Type your reply here...')).toHaveValue('Submitted reply');
});

test.each([404, 503])('allows another prepare only after a genuine latest-request absence (%s)', async latestStatus => {
  manualInbox(); const reads: URL[] = [];
  vi.mocked(globalThis.fetch).mockImplementation(async (url, options) => {
    if (!String(url).includes('/omni_inbox/action')) return defaultInboxFetch(url);
    if (options?.method === 'POST') return Response.json({}, { status: 409 });
    const query = new URL(String(url), 'http://localhost'); reads.push(query);
    return Response.json({}, { status: query.searchParams.has('request_id') ? 404 : latestStatus });
  });
  await renderInbox(); typeReply();
  fireEvent.click(screen.getByRole('button', { name: 'Send Reply' }));
  await screen.findByText(/Check saved reply status before/);
  fireEvent.click(screen.getByRole('button', { name: 'Check saved reply status' }));
  await screen.findByText(latestStatus === 404 ? /No saved reply request was found/ : /Saved reply status is unavailable/);
  expect(reads).toHaveLength(2);
  expect(screen.getByRole('button', { name: 'Send Reply' }).hasAttribute('disabled')).toBe(latestStatus !== 404);
  expect(screen.getByPlaceholderText('Type your reply here...')).toHaveValue('Submitted reply');
});

test('does not give an unconfirmed send status a success tone', async () => {
  manualInbox();
  vi.mocked(globalThis.fetch).mockImplementation(async (url, options) => options?.method === 'POST'
    ? Response.json({}, { status: 503 }) : defaultInboxFetch(url));
  await renderInbox(); typeReply(); fireEvent.click(screen.getByRole('button', { name: 'Send Reply' }));
  await screen.findByText(/Check saved reply status before/);
  expect(screen.getByRole('status')).not.toHaveClass('good');
});

test('marks the active auto-selected thread disabled with aria-current while keeping other threads selectable', async () => {
  queryState.data = [
    { id: 'first', source: 'instagram', content: 'Do you have vegan options for Saturday?', status: 'pending' },
    { id: 'second', source: 'email', content: 'Can I update my pickup time?', status: 'open' },
  ];
  await renderInbox();

  const firstThread = screen.getByRole('button', { name: /Do you have vegan options for Saturday\?/ });
  const secondThread = screen.getByRole('button', { name: /Can I update my pickup time\?/ });

  expect(firstThread).toBeDisabled();
  expect(firstThread).toHaveAttribute('aria-current', 'true');
  expect(secondThread).toBeEnabled();
  expect(secondThread).not.toHaveAttribute('aria-current');
  expect(within(detail()).getByText('Do you have vegan options for Saturday?')).toBeVisible();

  fireEvent.click(secondThread);

  expect(secondThread).toBeDisabled();
  expect(secondThread).toHaveAttribute('aria-current', 'true');
  expect(firstThread).toBeEnabled();
  expect(firstThread).not.toHaveAttribute('aria-current');
  expect(within(detail()).getByText('Can I update my pickup time?')).toBeVisible();

  fireEvent.click(firstThread);

  expect(firstThread).toBeDisabled();
  expect(firstThread).toHaveAttribute('aria-current', 'true');
  expect(secondThread).toBeEnabled();
  expect(secondThread).not.toHaveAttribute('aria-current');
  expect(within(detail()).getByText('Do you have vegan options for Saturday?')).toBeVisible();
});
