self.addEventListener('install', function () { self.skipWaiting(); });
self.addEventListener('activate', function (event) {
  event.waitUntil(self.clients.claim());
});

// Share one in-flight display per notification across both transports.
// Registration and provider acceptance are not proof of delivery; only showNotification success
// creates a receipt. Persistent receipts also cover dismissed notifications.
const inFlight = new Map();
const displayed = new Set();
const receiptLimit = 2048;
function displayOnce(notification) {
  const tag = 'forge-bot-' + (notification.generation || notification.created_at) + '-' + notification.id;
  if (inFlight.has(tag)) { return inFlight.get(tag); }
  const pending = Promise.resolve().then(async function () {
    const durable = Boolean(notification.created_at && notification.id);
    const key = new URL('/notifications/receipts/' + encodeURIComponent(tag), self.location.origin).href;
    let receipts = null;
    if (durable) {
      if (displayed.has(tag)) { return; }
      try {
        receipts = await self.caches.open('forge-bot-notification-receipts-v1');
        if (await receipts.match(key)) { return; }
      } catch (_) { /* Storage may be unavailable; keep in-memory receipts. */ }
      // Covers a worker restart when persistent storage was unavailable but
      // the notification is still visible, including a crash before saving.
      if (typeof self.registration.getNotifications === 'function') {
        const visible = await self.registration.getNotifications({ tag: tag });
        if (visible.length) { return; }
      }
    }
    await self.registration.showNotification('forge-bot: ' + (notification.repository || 'notification'), {
      body: notification.message || 'A forge-bot request needs your attention.',
      data: notification.location || '/notifications',
      icon: '/notifications/icon.png',
      badge: '/notifications/badge.png',
      tag: tag,
    });
    if (durable) {
      displayed.add(tag);
      if (displayed.size > receiptLimit) { displayed.delete(displayed.values().next().value); }
      if (receipts) {
        try {
          await receipts.put(key, new Response('displayed'));
          const keys = await receipts.keys();
          for (const stale of keys.slice(0, Math.max(0, keys.length - receiptLimit))) {
            await receipts.delete(stale);
          }
        } catch (_) { /* Successful display is still acknowledged. */ }
      }
    }
  });
  const tracked = pending.finally(function () { inFlight.delete(tag); });
  inFlight.set(tag, tracked);
  return tracked;
}

self.addEventListener('push', function (event) {
  event.waitUntil(displayOnce(event.data ? event.data.json() : {}));
});
self.addEventListener('message', function (event) {
  if (!event.data || event.data.type !== 'display-notification' || !event.ports[0]) { return; }
  const port = event.ports[0];
  event.waitUntil(displayOnce(event.data.notification).then(function () {
    port.postMessage({ displayed: true });
    port.close();
  }, function (error) {
    port.postMessage({ displayed: false, error: error.message });
    port.close();
  }));
});
self.addEventListener('notificationclick', function (event) {
  event.notification.close();
  const target = new URL(event.notification.data || '/notifications', self.location.origin);
  if (target.protocol !== 'https:' && target.protocol !== 'http:') { return; }
  event.waitUntil(self.clients.openWindow(target.href));
});
