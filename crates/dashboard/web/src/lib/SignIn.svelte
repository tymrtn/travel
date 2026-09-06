<script lang="ts">
  import { onMount } from 'svelte';
  import { resetCsrf } from '$lib/api';
  let { onSignedIn = () => window.location.assign('/travel') }: { onSignedIn?: () => void } = $props();
  let accessToken = $state('');
  let busy = $state(false);
  let error = $state('');
  let manual = $state(false);

  async function finish(response: Response) {
    if (!response.ok) throw new Error('Sign-in was not accepted. Open Travel from the Mac menu bar to try again, or check your access token.');
    const result = await response.json();
    resetCsrf();
    if (result.destination === '/household') window.location.assign('/household');
    else onSignedIn();
  }
  onMount(() => {
    const code = new URLSearchParams(window.location.hash.slice(1)).get('handoff');
    if (!code) return;
    // Clear before making any request. Never store the code or owner token.
    window.history.replaceState(null, '', window.location.pathname);
    busy = true;
    void fetch('/api/v1/browser-handoff/consume', {
      method: 'POST', credentials: 'same-origin', cache: 'no-store',
      headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ code })
    }).then(finish).catch(() => {
      error = 'This sign-in link expired or was already used. Choose Open Travel from the Mac menu bar for a new link.';
    }).finally(() => { busy = false; });
  });
  async function signIn(event: SubmitEvent) {
    event.preventDefault();
    if (busy || !accessToken.trim()) return;
    const credential = accessToken.trim();
    accessToken = ''; busy = true; error = '';
    try {
      await finish(await fetch('/api/v1/session', {
        method: 'POST', credentials: 'same-origin', cache: 'no-store',
        headers: { Authorization: `Bearer ${credential}` }
      }));
    } catch (failure) { error = failure instanceof Error ? failure.message : 'Could not reach Travel. Check that the app is running.'; }
    finally { busy = false; }
  }
</script>

<svelte:head><meta name="referrer" content="no-referrer" /></svelte:head>
<section class="sign-in" aria-labelledby="sign-in-title">
  <img src="/travel-mark.svg" alt="" width="52" height="52" />
  <h2 id="sign-in-title">{busy ? 'Signing you in…' : 'Open your Travel'}</h2>
  <p>Your trips and receipts are private. Open the Mac app to sign this browser in with your saved connection.</p>
  <a class="open-app" href="travel://open">Open Travel for Mac</a>
  <p class="hint">Or choose <strong>Open Travel</strong> from the airplane icon in the Mac menu bar. This opens the browser used by your Mac.</p>
  {#if error}<p class="error" role="alert">{error}</p>{/if}
  <details bind:open={manual}>
    <summary>Using another browser or a remote server?</summary>
    <p>Use your owner access token here. On the host Mac, Travel settings has a Copy token button. Family members should use their invitation link.</p>
    <form onsubmit={signIn}>
      <label for="travel-access-token">Access token</label>
      <input id="travel-access-token" type="password" autocomplete="off" bind:value={accessToken} required />
      <button disabled={busy || !accessToken.trim()} type="submit">Sign in</button>
    </form>
  </details>
</section>

<style>
  .sign-in { max-width: 520px; margin: 2rem auto; padding: 2rem; border: 1px solid #dddcd5; border-radius: 12px; background: #fff; }
  h2 { font-size: 1.75rem; margin: 1rem 0 .75rem; }
  p { line-height: 1.55; color: #535854; }
  .open-app, button { display: inline-block; padding: .8rem 1.1rem; border: none; border-radius: 6px; background: #123e43; color: #fff; font-weight: 600; text-decoration: none; cursor: pointer; }
  .hint { font-size: .88rem; }
  details { border-top: 1px solid #dddcd5; margin-top: 1.5rem; padding-top: 1rem; }
  summary { cursor: pointer; }
  label, input { display: block; }
  input { width: 100%; box-sizing: border-box; margin: .5rem 0 1rem; padding: .7rem; border: 1px solid #888; border-radius: 4px; }
  .error { color: #942e16; }
  button:disabled { opacity: .6; cursor: default; }
</style>
