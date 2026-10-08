// Only push and click handlers: never cache signed-in pages, balances or API responses.
self.addEventListener('install', () => self.skipWaiting());
self.addEventListener('activate', (event) => event.waitUntil(self.clients.claim()));
self.addEventListener('push', (event) => {
  let payload;
  try { payload = event.data.json(); } catch { return; }
  if (!/^[a-f0-9]{32}$/.test(payload?.id ?? '')) return;
  event.waitUntil(Promise.all([
    self.registration.showNotification('Atlas', {
      body: 'You have a new Atlas money update.', icon: '/atlas-icon.png',
      badge: '/atlas-icon.png', tag: payload.id, data: { url: '/notifications' },
    }),
    self.clients.matchAll({ type: 'window', includeUncontrolled: true }).then((clients) => {
      clients.forEach((client) => client.postMessage({ type: 'atlas-money-update' }));
    }),
  ]));
});
self.addEventListener('notificationclick', (event) => {
  event.notification.close();
  const url = new URL('/notifications', self.location.origin).href;
  event.waitUntil(self.clients.matchAll({ type: 'window', includeUncontrolled: true }).then(async (clients) => {
    const client = clients.find((c) => new URL(c.url).origin === self.location.origin);
    if (client) { await client.navigate(url); await client.focus(); }
    else await self.clients.openWindow(url);
  }));
});
