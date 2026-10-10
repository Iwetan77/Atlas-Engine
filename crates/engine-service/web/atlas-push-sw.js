// Only push and click handlers: never cache signed-in pages, balances or API responses.
self.addEventListener('install', () => self.skipWaiting());
self.addEventListener('activate', (event) => event.waitUntil(self.clients.claim()));
self.addEventListener('push', (event) => {
  let payload;
  try { payload = event.data.json(); } catch { return; }
  if (!/^[a-f0-9]{32}$/.test(payload?.id ?? '')) return;
  event.waitUntil(Promise.all([
    self.registration.showNotification(notificationText(payload.title, 100, 'Atlas'), {
      body: notificationText(payload.body, 240, 'Open Atlas to see the details.'), icon: '/atlas-icon.png',
      badge: '/atlas-icon.png', tag: payload.id, data: { url: notificationPath(payload.url) },
    }),
    self.clients.matchAll({ type: 'window', includeUncontrolled: true }).then((clients) => {
      clients.forEach((client) => client.postMessage({ type: 'atlas-money-update' }));
    }),
  ]));
});
self.addEventListener('notificationclick', (event) => {
  event.notification.close();
  const url = new URL(notificationPath(event.notification.data?.url), self.location.origin).href;
  event.waitUntil(self.clients.matchAll({ type: 'window', includeUncontrolled: true }).then(async (clients) => {
    const client = clients.find((c) => new URL(c.url).origin === self.location.origin);
    if (client) { await client.navigate(url); await client.focus(); }
    else await self.clients.openWindow(url);
  }));
});

function notificationText(value, limit, fallback) {
  return Array.from((typeof value === 'string' ? value : '').replace(/[\u0000-\u001f\u007f]/g, ' ').replace(/\s+/g, ' ').trim()).slice(0, limit).join('') || fallback;
}
function notificationPath(value) {
  if (typeof value !== 'string' || !value.startsWith('/') || value.startsWith('//') || /[\\\r\n]/.test(value)) return '/notifications';
  try {
    const url = new URL(value, self.location.origin);
    return url.origin === self.location.origin ? url.pathname + url.search : '/notifications';
  } catch { return '/notifications'; }
}
