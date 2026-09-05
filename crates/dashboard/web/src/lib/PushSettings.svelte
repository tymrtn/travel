<script lang="ts">
  import { request } from '$lib/api';
  let notice = $state('');
  let accepted = $state(false);
  let busy = $state(false);
  async function enable() {
    busy = true;
    try {
      if (!accepted) throw new Error('Please accept the browser transport dependency first.');
      if (!('serviceWorker' in navigator) || !('PushManager' in window)) throw new Error('Push is unavailable here. On iPhone, add Travel to your Home Screen and open it there.');
      if (await Notification.requestPermission() !== 'granted') throw new Error('Notification permission was not granted. You can change it in browser settings.');
      const registration = await navigator.serviceWorker.register('/travel-sw.js');
      await navigator.serviceWorker.ready;
      const { public_key } = await request<{public_key:string}>('/v1/household/push/enable', {method:'POST'});
      const padded = public_key.replace(/-/g,'+').replace(/_/g,'/') + '='.repeat((4-public_key.length%4)%4);
      const applicationServerKey = Uint8Array.from(atob(padded), c=>c.charCodeAt(0));
      const subscription = await registration.pushManager.getSubscription() || await registration.pushManager.subscribe({userVisibleOnly:true,applicationServerKey});
      const { id } = await request<{id:string}>('/v1/household/push/devices', {method:'POST',body:subscription.toJSON()});
      localStorage.setItem('travel-push-device',id);
      await request(`/v1/household/push/devices/${id}/test`,{method:'POST'});
      notice = 'Enabled. A test is queued for the next delivery cycle. Delivery is not guaranteed while your Travel host is asleep.';
    } catch(error) { notice = String(error); } finally { busy = false; }
  }
  async function disable() {
    try {
      const id = localStorage.getItem('travel-push-device');
      if (id) await request(`/v1/household/push/devices/${id}`,{method:'DELETE'});
      const registration = await navigator.serviceWorker.getRegistration('/');
      await (await registration?.pushManager.getSubscription())?.unsubscribe();
      localStorage.removeItem('travel-push-device');
      notice = 'Notifications disabled for this browser.';
    } catch(error) { notice = String(error); }
  }
</script>
<section>
  <h2>Optional browser notifications</h2>
  <p>Encrypted notifications travel through your browser vendor’s push service. The vendor can observe delivery metadata. Notifications contain a generic alert, not reservation details. No push connection is made until you enable it.</p>
  <label><input type="checkbox" bind:checked={accepted}/> I accept this optional browser-vendor dependency.</label>
  <p><button disabled={!accepted || busy} onclick={enable}>Enable and send a test</button> <button onclick={disable}>Disable on this browser</button></p>
  {#if notice}<p role="status">{notice}</p>{/if}
</section>
