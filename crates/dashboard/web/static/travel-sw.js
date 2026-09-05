self.addEventListener('push', event => {
  let data = {}; try { data = event.data.json(); } catch {}
  event.waitUntil(self.registration.showNotification(data.title || 'Travel update', {
    body: data.body || 'Open Travel to review your trip.', data: { url: data.url === '/household' ? '/household' : '/travel' }, tag: 'travel-update'
  }));
});
self.addEventListener('notificationclick', event => {
  event.notification.close(); event.waitUntil(clients.openWindow(event.notification.data.url));
});
