import { render, screen, fireEvent, waitFor, cleanup } from '@testing-library/svelte';
import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import SignIn from './SignIn.svelte';
import Travel from '../routes/travel/+page.svelte';

beforeEach(() => { window.history.replaceState(null, '', '/travel'); });
afterEach(() => { cleanup(); vi.unstubAllGlobals(); window.history.replaceState(null, '', '/travel'); });

it('shows an actionable sign-in panel for 401, not a service failure', async () => {
  vi.stubGlobal('fetch', vi.fn(async () => new Response(JSON.stringify({ error: 'sign_in_required' }), { status: 401 })));
  render(Travel);
  expect(await screen.findByText('Open your Travel')).toBeTruthy();
  expect(screen.getByRole('link', { name: 'Open Travel for Mac' }).getAttribute('href')).toBe('travel://open');
  expect(screen.queryByText('sign_in_required')).toBeNull();
  expect(screen.queryByText('Couldn’t load your trips')).toBeNull();
});

it('clears the URL fragment before exchanging the single-use code', async () => {
  window.history.replaceState(null, '', '/signin#handoff=synthetic-code');
  const fetchMock = vi.fn(async (_url: string, init: RequestInit) => {
    expect(window.location.hash).toBe('');
    expect(init.body).toBe(JSON.stringify({ code: 'synthetic-code' }));
    expect(init.headers).not.toHaveProperty('Authorization');
    return new Response(JSON.stringify({ destination: '/travel' }), { status: 200 });
  });
  vi.stubGlobal('fetch', fetchMock);
  const signedIn = vi.fn();
  render(SignIn, { props: { onSignedIn: signedIn } });
  await waitFor(() => expect(signedIn).toHaveBeenCalledOnce());
  expect(fetchMock).toHaveBeenCalledOnce();
  expect(fetchMock.mock.calls[0][0]).toBe('/api/v1/browser-handoff/consume');
});

it('explains expired handoffs without retrying or retaining them', async () => {
  window.history.replaceState(null, '', '/signin#handoff=expired');
  const fetchMock = vi.fn(async () => new Response('{}', { status: 401 }));
  vi.stubGlobal('fetch', fetchMock);
  render(SignIn);
  expect(await screen.findByRole('alert')).toHaveTextContent('expired or was already used');
  expect(window.location.hash).toBe('');
  expect(fetchMock).toHaveBeenCalledOnce();
});

it('supports manual sign-in, clearing the field and retaining no token in the URL', async () => {
  const fetchMock = vi.fn(async (_url: string, _init: RequestInit) => new Response(JSON.stringify({ destination: '/travel' }), { status: 200 }));
  vi.stubGlobal('fetch', fetchMock);
  const signedIn = vi.fn();
  render(SignIn, { props: { onSignedIn: signedIn } });
  await fireEvent.click(screen.getByText('Using another browser or a remote server?'));
  const field = screen.getByLabelText('Access token') as HTMLInputElement;
  await fireEvent.input(field, { target: { value: 'synthetic-test-credential' } });
  await fireEvent.submit(field.closest('form')!);
  await waitFor(() => expect(signedIn).toHaveBeenCalledOnce());
  expect(field.value).toBe('');
  expect(window.location.href).not.toContain('synthetic-test-credential');
  expect(fetchMock.mock.calls[0][1]).toMatchObject({ headers: { Authorization: 'Bearer synthetic-test-credential' } });
});
