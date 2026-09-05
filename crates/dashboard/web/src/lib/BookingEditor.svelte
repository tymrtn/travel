<script lang="ts">
  import {request} from '$lib/api';
  import type {Booking} from '$lib/travel-api';
  let {booking, refreshed}:{booking:Booking,refreshed:()=>Promise<void>}=$props();
  let editing=$state(false);
  let title=$state(''), status=$state('confirmed'), start=$state(''), end=$state(''), location=$state('');
  let revision=$state(''), error=$state(''), busy=$state(false);
  let purpose=$state('manage'), label=$state(''), url=$state('');
  function open(){title=booking.title;status=booking.status||'confirmed';start=booking.starts_at||'';end=booking.ends_at||'';location=booking.location||'';revision=String(booking.updated_at||'');editing=true;error='';}
  async function save(event:SubmitEvent){
    event.preventDefault();busy=true;error='';
    try{await request(`/v1/bookings/${booking.id}`,{method:'PUT',body:{expected_updated_at:revision,title,status,starts_at:start||null,ends_at:end||null,location:location||null}});await refreshed();editing=false;}
    catch(e){error=String(e)}finally{busy=false}
  }
  async function addLink(event:SubmitEvent){
    event.preventDefault();busy=true;error='';
    try{await request(`/v1/bookings/${booking.id}/links`,{method:'POST',body:{purpose,label:label||purpose,url,visibility:'private'}});url='';label='';await refreshed();editing=false;}
    catch(e){error=String(e)}finally{busy=false}
  }
</script>
{#if !editing}<button class="edit" onclick={open}>Edit booking or add an action link</button>
{:else}
<div class="editor">
  <form onsubmit={save}>
    <label>Title<input bind:value={title} required maxlength="500"/></label>
    <label>Status<select bind:value={status}><option value="confirmed">Confirmed</option><option value="changed">Changed</option><option value="pending">Pending</option><option value="cancelled">Cancelled</option></select></label>
    <label>Start<input bind:value={start} placeholder="2026-10-01T10:00:00+02:00"/></label>
    <label>End<input bind:value={end} placeholder="2026-10-01T12:00:00+02:00"/></label>
    <p>Use a date (YYYY-MM-DD) or a full time with its UTC offset. Leave blank if unknown. Changing this record does not change your reservation with the provider.</p>
    <label>Location<input bind:value={location}/></label>
    <button disabled={busy}>Save booking</button> <button type="button" onclick={()=>editing=false}>Close</button>
  </form>
  <form onsubmit={addLink}>
    <h4>Private action link</h4>
    <label>Purpose<select bind:value={purpose}><option value="manage">Manage reservation</option><option value="check_in">Check in</option><option value="flight_status">Flight status</option><option value="directions">Directions</option><option value="ticket">Ticket</option><option value="cancel">Cancellation page</option></select></label>
    <label>Label<input bind:value={label}/></label>
    <label>URL<input type="url" bind:value={url} required/></label>
    <p>Travel saves this link without visiting it. Private links are excluded from family shares.</p>
    <button disabled={busy}>Add link to private calendar</button>
  </form>
  {#if error}<p role="alert">{error}</p>{/if}
</div>
{/if}
<style>.editor{padding:1rem;border:1px solid #aaa;margin:.8rem 0;max-width:600px}label{display:block;margin:.6rem 0}input,select{display:block;width:100%;padding:.45rem}p{font-size:.85rem;line-height:1.4}button{padding:.45rem;cursor:pointer}.edit{margin-top:.5rem;background:none;border:0;text-decoration:underline}form+form{border-top:1px solid #aaa;margin-top:1rem;padding-top:1rem}</style>
