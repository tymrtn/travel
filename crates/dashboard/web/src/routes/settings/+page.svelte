<script lang="ts">
  import { request } from '$lib/api';
  import PushSettings from '$lib/PushSettings.svelte';
  let notice = $state('');
  let type = $state('api');
  let endpoint = $state('https://openrouter.ai/api/v1/chat/completions');
  let model = $state('');
  let key = $state('TRAVEL_MODEL_API_KEY');
  let executable = $state('');
  let args = $state('[]');
  let unsandboxed = $state(false);
  let limit = $state(20);
  let accessToken = $state('');
  let memberName = $state('');
  let role = $state('viewer');
  let trips = $state('');
  let invitation = $state('');
  async function run(action: () => Promise<unknown>) { try { await action(); notice = 'Saved.'; } catch (error) { notice = String(error); } }
  async function login() {
    const response = await fetch('/api/v1/session', {method:'POST', headers:{Authorization:`Bearer ${accessToken}`}});
    accessToken=''; if (!response.ok) throw new Error('Access token was not accepted.');
  }
  async function saveModel() {
    await request('/v1/intelligence',{method:'PUT',body:{daily_call_limit:limit,timeout_seconds:90,provider:type==='api'?{type:'api',endpoint,model,api_key_env:key}:{type:'cli',executable,args:JSON.parse(args),environment:[],allow_unsandboxed:unsandboxed}}});
  }
  async function connectGoogle() {
    const result = await request<{authorization_url:string}>('/v1/sources/gmail/authorize',{method:'POST'});
    window.location.assign(result.authorization_url);
  }
  async function invite() {
    const result=await request<{enrollment_path:string}>('/v1/members',{method:'POST',body:{name:memberName,role,trips:trips.split(',').map(v=>v.trim()).filter(Boolean)}});
    invitation=window.location.origin+result.enrollment_path;
  }
  async function importFile(event:Event) {
    const file=(event.target as HTMLInputElement).files?.[0];if(!file)return;
    const csrf=await request<{token:string}>('/csrf');
    const result=await fetch(`/api/v1/documents/import/${encodeURIComponent(file.name)}`,{method:'POST',headers:{'X-Travel-CSRF':csrf.token},body:file});
    if(!result.ok)throw new Error((await result.json()).message || 'File preserved but could not be extracted.');
    notice='Receipt imported. Open Travel to review it.';
  }
</script>
<svelte:head><title>Travel settings</title></svelte:head>
<main>
  <h1>Your Travel installation</h1>
  <p>Data stays with this installation. External services are used only when you configure them.</p>
  <PushSettings />
  {#if notice}<p role="status">{notice}</p>{/if}
  <section><h2>Remote access</h2><label>Owner access token <input type="password" bind:value={accessToken} /></label><button onclick={()=>run(login)}>Sign in on this browser</button></section>
  <section><h2>Receipts and calendars</h2><label>Import an email, text receipt, or PDF <input type="file" accept=".eml,.txt,.pdf" onchange={(event)=>run(()=>importFile(event))}/></label><p><a href="/api/v1/calendar.ics">Download private calendar with alarms and action links</a></p><button onclick={()=>run(connectGoogle)}>Connect Gmail with OAuth</button><p>OAuth uses the Google application configuration installed on your server.</p></section>
  <section><h2>Optional intelligence</h2><p>Unfamiliar receipts go to the provider you choose. Known formats use saved parsers without a model call.</p>
  <label>Connection <select bind:value={type}><option value="api">Model API</option><option value="cli">Command-line adapter</option></select></label>
  {#if type==='api'}<label>API endpoint <input bind:value={endpoint}/></label><label>Model <input bind:value={model}/></label><label>Credential environment variable <input bind:value={key}/></label>
  {:else}<label>Executable path <input bind:value={executable}/></label><label>Arguments (JSON array) <input bind:value={args}/></label><label><input type="checkbox" bind:checked={unsandboxed}/> Allow this executable to run without an OS sandbox</label><p>Only enable executables you trust. Receipt data is supplied as JSON on standard input.</p>{/if}
  <label>Maximum calls per day <input type="number" min="0" max="10000" bind:value={limit}/></label><button onclick={()=>run(saveModel)}>Save intelligence connection</button></section>
  <section><h2>Household access</h2><label>Name <input bind:value={memberName}/></label><label>Role <select bind:value={role}><option value="viewer">Viewer</option><option value="editor">Editor</option></select></label><label>Allowed trip IDs (comma-separated) <input bind:value={trips}/></label><button onclick={()=>run(invite)}>Create invitation link</button>{#if invitation}<textarea readonly value={invitation} aria-label="Invitation link"></textarea>{/if}</section>
</main>
<style>main{padding:2rem;max-width:850px;overflow:auto}section{border-top:1px solid #ddd;padding:1.5rem 0}label{display:block;margin:.8rem 0}input:not([type=checkbox]),select,textarea{display:block;padding:.6rem;width:100%;max-width:650px}button{padding:.6rem 1rem;cursor:pointer}h1,h2{font-weight:600}p{line-height:1.5}</style>
