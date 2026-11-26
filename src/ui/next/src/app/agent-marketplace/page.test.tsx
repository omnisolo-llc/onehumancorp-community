import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { act, cleanup, render, screen, fireEvent, waitFor } from '@testing-library/react';
import { notifyQueueIdentityChange } from '@/lib/sync/queueIdentity';
import { marketplaceBackend } from './marketplace.test-support';
import Page from './page';
let backend: ReturnType<typeof marketplaceBackend>;
beforeEach(() => { backend = marketplaceBackend(); });
afterEach(() => { cleanup(); notifyQueueIdentityChange(); vi.unstubAllGlobals(); });
it('renders the marketplace header and search', async () => {
 render(<Page/>); expect(screen.getByRole('heading',{name:'Agent Marketplace'})).toBeVisible();
 expect(screen.getByPlaceholderText('Search for agents...')).toBeVisible(); await screen.findByText('Senior Rust Developer');
});
it('fetches actual definitions and displays their source and content', async () => {
 render(<Page/>); await screen.findByText('Senior Rust Developer');
 expect(screen.getAllByText('Drafts for owner review')).toHaveLength(2); expect(screen.getAllByText(/First-party definition/)).toHaveLength(2);
 expect(backend.fetch.mock.calls.some(([url]) => url.startsWith('/api/v1/agents/definitions?'))).toBe(true);
});
it('displays no results only after a verified empty response', async () => {
 backend.state.definitions=[]; render(<Page/>); await screen.findByText('No agents found.'); expect(screen.queryByRole('alert')).toBeNull();
});
it('updates the actual search query when typing', async () => {
 render(<Page/>); await screen.findByText('Technical Writer'); fireEvent.change(screen.getByPlaceholderText('Search for agents...'),{target:{value:'Rust'}});
 await waitFor(() => expect(backend.fetch.mock.calls.some(([url,init]) => url==='/api/v1/agents/definitions?q=Rust' && init?.signal instanceof AbortSignal)).toBe(true));
 await screen.findByText('Senior Rust Developer'); expect(screen.queryByText('Technical Writer')).toBeNull();
});
it('shows service failure without replacing it with empty results', async () => {
 backend.fetch.mockImplementation(async(url,init)=>url.includes('/definitions')?new Response('',{status:503}):backend.route(url,init));
 render(<Page/>); await screen.findByRole('button',{name:'Retry marketplace'}); expect(screen.getByRole('alert')).toHaveTextContent('Failed to fetch agents'); expect(screen.queryByText('No agents found.')).toBeNull();
});
it('retries the current query after a service failure', async () => {
 let fail=true; backend.fetch.mockImplementation(async(url,init)=>url.includes('/definitions')&&fail?new Response('',{status:503}):backend.route(url,init));
 render(<Page/>); await screen.findByRole('button',{name:'Retry marketplace'}); fireEvent.change(screen.getByPlaceholderText('Search for agents...'),{target:{value:'Writer'}});
 await waitFor(()=>expect(backend.fetch.mock.calls.some(([url])=>url.endsWith('q=Writer'))).toBe(true)); fail=false;
 await act(async()=>fireEvent.click(screen.getByRole('button',{name:'Retry marketplace'}))); await screen.findByText('Technical Writer'); expect(screen.queryByText('Senior Rust Developer')).toBeNull();
});
it('keeps main aria-busy through session verification and catalogue loading until Install Agent actions are enabled', async () => {
 let releaseIdentity!:(response:Response)=>void;
 let identityReleased=false;
 let releaseCatalogue!:(response:Response)=>void;
 backend.fetch.mockImplementation(async(url,init)=>{
  if(url==='/api/v1/auth/session-identity'&&!identityReleased){
   return new Promise<Response>(resolve=>{releaseIdentity=response=>{identityReleased=true;resolve(response);};});
  }
  if(url.startsWith('/api/v1/agents/definitions?')){
   const payload=await backend.route(url,init);
   return new Promise<Response>(resolve=>{releaseCatalogue=resolve;void payload;});
  }
  return backend.route(url,init);
 });
 render(<Page/>);
 const main=screen.getByRole('main');
 expect(main).toHaveAttribute('aria-busy','true');
 expect(screen.queryByRole('button',{name:'Install Agent'})).toBeNull();
 await waitFor(()=>expect(releaseIdentity).toBeDefined());
 await act(async()=>{releaseIdentity(Response.json({...backend.state.owner,expiresAt:Date.now()+60_000}));});
 await waitFor(()=>expect(releaseCatalogue).toBeDefined());
 expect(main).toHaveAttribute('aria-busy','true');
 const catalogue=await backend.route('/api/v1/agents/definitions?q=',{headers:{'x-ohc-expected-user':backend.state.owner.userId,'x-ohc-expected-tenant':backend.state.owner.tenantId}});
 await act(async()=>{releaseCatalogue(catalogue);});
 const buttons=await screen.findAllByRole('button',{name:'Install Agent'});
 expect(buttons).toHaveLength(2);
 for(const button of buttons)expect(button).toBeEnabled();
 expect(main).toHaveAttribute('aria-busy','false');
});
it('clears main aria-busy when session verification fails so the page never stays stuck busy', async () => {
 backend.fetch.mockImplementation(async()=>new Response('',{status:401}));
 render(<Page/>);
 const main=screen.getByRole('main');
 await waitFor(()=>expect(main).toHaveAttribute('aria-busy','false'));
 expect(screen.queryByRole('button',{name:'Install Agent'})).toBeNull();
});
