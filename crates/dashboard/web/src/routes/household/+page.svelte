<script lang="ts">
 import {onMount} from 'svelte';
 import {request} from '$lib/api';
 import PushSettings from '$lib/PushSettings.svelte';
 type View={member:{name:string,role:string},trips:{id:string,title:string}[],bookings:{id:string,title:string,starts_at:string,status:string}[],tasks:{id:string,title:string,completed_at:string|null}[]};
 let token=$state('');let view=$state<View|null>(null);let error=$state('');
 onMount(()=>{token=new URLSearchParams(window.location.hash.slice(1)).get('token')||'';history.replaceState(null,'',window.location.pathname);if(!token)void load();});
 async function load(){try{view=await request<View>('/v1/household/overview')}catch{error='Sign in using your invitation.'}}
 async function login(){const response=await fetch('/api/v1/session',{method:'POST',headers:{Authorization:`Bearer ${token}`}});token='';if(response.ok){error='';await load()}else{error='This invitation is unavailable.'}}
 async function toggle(id:string){try{await request(`/v1/household/tasks/${encodeURIComponent(id)}/toggle`,{method:'POST'});await load()}catch(e){error=String(e)}}
</script>
<main><h1>Family travel</h1>{#if error}<p role="alert">{error}</p>{/if}
{#if !view}<label>Invitation access token <input type="password" bind:value={token}/></label><button onclick={login}>Open my trips</button>
{:else}<p>Welcome, {view.member.name}.</p><PushSettings />{#each view.trips as trip}<h2>{trip.title}</h2>{/each}
{#each view.bookings as booking}<article><h3>{booking.title}</h3><p>{booking.starts_at} · {booking.status}</p></article>{/each}
<h2>Checklist</h2>{#each view.tasks as task}<label><input type="checkbox" checked={Boolean(task.completed_at)} disabled={view.member.role!=='editor'} onchange={()=>toggle(task.id)}/>{task.title}</label>{/each}{/if}</main>
<style>main{padding:2rem;overflow:auto}article{border-bottom:1px solid #ddd;padding:1rem}label{display:block;margin:1rem 0}button,input{padding:.6rem}</style>
