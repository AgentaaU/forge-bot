const status = document.getElementById('status');
const log = document.getElementById('log');
const recipientSelect = document.getElementById('recipient');
try {
  const saved = window.localStorage.getItem('forge-bot-recipient');
  if (saved && Array.from(recipientSelect.options).some(function (option) { return option.value === saved; })) {
    recipientSelect.value = saved;
  }
} catch (_) { /* Storage may be disabled. */ }
let registration = null;
let pushConfig = null;
let configurationError = '';
let pushUpdates = Promise.resolve();

async function loadPushConfig() {
  try {
    const response = await fetch('/notifications/push');
    if (!response.ok) { throw new Error('Configuration returned ' + response.status); }
    pushConfig = await response.json();
    if (!['web_push', 'polling'].includes(pushConfig.transport)) {
      throw new Error('Unknown notification transport');
    }
    configurationError = pushConfig.error || '';
    refreshDeliveryStatus();
  } catch (error) {
    pushConfig = null;
    configurationError = 'Could not load notification mode: ' + error.message;
    refreshDeliveryStatus();
  }
}

function syncPush(disable = false, create = false) {
  // Serialize selection changes so the final server binding matches the UI.
  pushUpdates = pushUpdates.catch(function () {}).then(async function () {
    if (!pushConfig || pushConfig.transport !== 'web_push') { return; }
    if (pushConfig.error) { setDeliveryStatus(pushConfig.error); return; }
    if (!registration || !registration.pushManager) {
      setDeliveryStatus('Web Push requires an active service worker and Push API.');
      return;
    }
    let subscription = await registration.pushManager.getSubscription();
    if (disable) {
      if (subscription) {
        const response = await fetch('/notifications/push', {
          method: 'DELETE', headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ endpoint: subscription.endpoint }),
        });
        if (!response.ok) { throw new Error('Could not remove push subscription'); }
        await subscription.unsubscribe();
      }
      setDeliveryStatus('Web Push disabled. Enable notifications to subscribe again.');
      return;
    }
    if (!pushConfig || pushConfig.transport !== 'web_push' ||
        typeof Notification === 'undefined' || Notification.permission !== 'granted') { return; }
    const recipient = recipientSelect.value;
    if (!recipient) { return; }
    if (!subscription && !create) { setDeliveryStatus('Web Push is not subscribed. Click Enable notifications.'); return; }
    if (!subscription) {
      const encoded = pushConfig.public_key.replace(/-/g, '+').replace(/_/g, '/');
      const bytes = Uint8Array.from(atob(encoded), function (character) { return character.charCodeAt(0); });
      subscription = await registration.pushManager.subscribe({
        userVisibleOnly: true, applicationServerKey: bytes,
      });
    }
    const response = await fetch('/notifications/push', {
      method: 'POST', headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ recipient: recipient, subscription: subscription.toJSON() }),
    });
    if (!response.ok) { throw new Error('Push subscription returned ' + response.status); }
    if (recipientSelect.value === recipient) {
      setDeliveryStatus('');
      setStatus('Browser subscribed for ' + recipient + '.');
    }
  }).catch(function (error) {
    setDeliveryStatus('Web Push failed: ' + error.message);
  });
  return pushUpdates;
}

// Why the service worker registration failed, if it did. `register()` rejects
// for environment-specific reasons (site data blocked, insecure context, a
// transient network error) that the page otherwise swallows, and the Android
// notification failure cannot be diagnosed without it.
let serviceWorkerError = null;
let generation = null;
let requestSeq = 0;
// One cursor per human, so switching accounts never hides the new account's
// pending notifications behind the previous account's high-water mark. A
// cursor only advances once the entry has actually been displayed, so a
// rejected `showNotification` (for example before permission is granted on a
// fresh Android install) is retried instead of being silently consumed.
const cursors = {};
// Ids already drawn in the log, so retrying a failed display does not duplicate
// the visible entries.
const rendered = new Set();
// Per recipient, ids whose system notification has already been raised. A later
// entry can succeed while an earlier one is still failing, so these are kept
// apart from the cursor: the cursor may not pass the first failure, and a retry
// must not raise an already-displayed entry a second time.
const delivered = {};
// Per recipient, ids whose `display` is still in flight. Overlapping polls
// share one promise per id so the same entry is never shown twice.
const displaying = {};

function setStatus(text) { status.textContent = text; }

// Keep delivery feedback separate from polling/subscription activity. Browsers
// cannot observe whether Android actually surfaces an accepted notification.
let deliveryGuidance = '';
const deliveryFailures = {};
const deliveryWarnings = {};
let permissionGuidance = false;
function refreshDeliveryStatus() {
  const failures = deliveryFailures[recipientSelect.value];
  document.getElementById('delivery-status').textContent = configurationError ||
    (failures && failures.size ? failures.values().next().value.text
      : (deliveryWarnings[recipientSelect.value] || {}).text || deliveryGuidance);
}
function setDeliveryStatus(text, permissionWarning = false) {
  permissionGuidance = permissionWarning;
  deliveryGuidance = text;
  refreshDeliveryStatus();
}

// Permission is origin-wide. Retire warnings for every recipient, including
// consumed denied entries, but keep real display errors until their retry succeeds.
function permissionGranted() {
  for (const recipient of Object.keys(deliveryWarnings)) {
    if (deliveryWarnings[recipient].permission) { delete deliveryWarnings[recipient]; }
  }
  for (const failures of Object.values(deliveryFailures)) {
    for (const [id, feedback] of failures) {
      if (feedback.permission) { failures.delete(id); }
    }
  }
  if (permissionGuidance) { deliveryGuidance = ''; permissionGuidance = false; }
  refreshDeliveryStatus();
}

async function setupServiceWorker() {
  // Required on Android and iOS, whose browsers throw on `new Notification`.
  // Registration only succeeds in a secure context (HTTPS, or localhost).
  if (!('serviceWorker' in navigator)) { return; }
  try {
    // A broad scope so `navigator.serviceWorker.ready` (which only resolves
    // for a worker covering the current page) works on both `/notifications`
    // and the `/notify` alias.
    const pending = await navigator.serviceWorker.register('/notifications/sw.js', { scope: '/' });
    // `register()` resolves while a new worker may still be installing, and
    // `showNotification` rejects until the worker is active. Wait for the
    // active state before polling; otherwise the first batch advances the
    // cursor without ever raising a notification.
    registration = await activeRegistration(pending);
    if (!registration) {
      serviceWorkerError = 'registered but never activated';
    }
  } catch (error) {
    registration = null;
    serviceWorkerError = error.name + ': ' + error.message;
  }
}

function activeRegistration(pending) {
  if (pending.active) { return Promise.resolve(pending); }
  const worker = pending.installing || pending.waiting;
  if (!worker) {
    // No worker object to watch: fall back to the container's readiness
    // promise, whose scope now covers both page paths.
    if (navigator.serviceWorker.ready) {
      return navigator.serviceWorker.ready.then(
        function (ready) { return ready; },
        function () { return null; },
      );
    }
    return Promise.resolve(null);
  }
  return new Promise(function (resolve) {
    function settle() {
      if (worker.state === 'activated') { resolve(pending); }
      else if (worker.state === 'redundant') { resolve(null); }
    }
    worker.addEventListener('statechange', settle);
    settle();
  });
}

async function enable() {
  if (!('Notification' in window)) {
    setDeliveryStatus('This browser has no Notification API. On mobile, serve over HTTPS and add the page to the Home Screen.');
    return;
  }
  if (!window.isSecureContext) {
    setDeliveryStatus('Browser notifications require HTTPS; open this page over a secure connection.');
    return;
  }
  const permission = await Notification.requestPermission();
  setDeliveryStatus('Notification permission: ' + permission, true);
  // Permission often arrives after earlier polls already fetched the batch;
  // poll again now so those entries are delivered instead of waiting five
  // seconds (or being lost if the cursor had advanced).
  if (!pushConfig) { await loadPushConfig(); }
  if (permission === 'granted') { permissionGranted(); await syncPush(false, true); await poll(); }
}

// Deliver a synthetic notification on demand. This lets a person confirm that
// the current browser can actually raise system notifications (and see the
// error if it cannot) without waiting for the next real request.
async function testNotification() {
  if (typeof Notification === 'undefined') {
    setDeliveryStatus('This browser has no Notification API.');
    return;
  }
  if (Notification.permission !== 'granted') {
    const permission = await Notification.requestPermission();
    if (permission !== 'granted') {
      setDeliveryStatus('Notification permission: ' + permission, true);
      return;
    }
  }
  permissionGranted();
  const result = await display({
    id: 0,
    repository: 'forge-bot test',
    message: 'This is a test notification from forge-bot.',
    location: location.href,
  });
  if (result.raised) {
    setDeliveryStatus('Test notification accepted by the browser. Check your system notification shade. If it is missing, check the notification settings below.');
  }
}

function serializeDelivered() {
  const out = {};
  for (const key of Object.keys(delivered)) {
    out[key] = Array.from(delivered[key]);
  }
  return out;
}

// Collect browser state relevant to notification delivery. Android system
// permissions and notification channels are not exposed to this page. The page uploads this to `/notifications/diagnostics` so the
// operator can read it even when the notification never appears.
async function collectDiagnostics() {
  const report = {
    at: new Date().toISOString(),
    userAgent: navigator.userAgent,
    platform: navigator.platform || null,
    href: location.href,
    origin: location.origin,
    visibility: document.visibilityState,
    secureContext: window.isSecureContext,
    notificationApi: 'Notification' in window,
    notificationPermission: typeof Notification === 'undefined' ? null : Notification.permission,
    maxActions: typeof Notification === 'undefined' ? null : (Notification.maxActions || 0),
    serviceWorkerApi: 'serviceWorker' in navigator,
    serviceWorkerError: serviceWorkerError,
    controller: null,
    registrations: [],
    ready: null,
    permissionState: null,
    displayTest: null,
    notificationDeliveryPath: pushConfig && pushConfig.transport === 'web_push'
      ? 'web-push' : pushConfig ? 'direct-showNotification' : 'unavailable',
    generation: generation,
    cursors: cursors,
    delivered: serializeDelivered(),
  };
  if (navigator.permissions && navigator.permissions.query) {
    try {
      const state = await navigator.permissions.query({ name: 'notifications' });
      report.permissionState = state.state;
    } catch (error) {
      report.permissionState = 'error: ' + error.message;
    }
  }
  if ('serviceWorker' in navigator) {
    report.controller = navigator.serviceWorker.controller
      ? navigator.serviceWorker.controller.scriptURL
      : null;
    try {
      const registrations = await navigator.serviceWorker.getRegistrations();
      report.registrations = registrations.map(function (entry) {
        return {
          scope: entry.scope,
          active: entry.active ? entry.active.scriptURL : null,
          installing: entry.installing
            ? entry.installing.scriptURL + ' (' + entry.installing.state + ')'
            : null,
          waiting: entry.waiting
            ? entry.waiting.scriptURL + ' (' + entry.waiting.state + ')'
            : null,
        };
      });
    } catch (error) {
      report.registrations = 'error: ' + error.message;
    }
    try {
      const ready = await Promise.race([
        navigator.serviceWorker.ready,
        new Promise(function (_, reject) {
          setTimeout(function () { reject(new Error('ready timeout')); }, 5000);
        }),
      ]);
      report.ready = {
        scope: ready.scope,
        active: ready.active ? ready.active.scriptURL : null,
      };
    } catch (error) {
      report.ready = 'error: ' + error.message;
    }
  }
  let target = registration;
  if (!target && 'serviceWorker' in navigator) {
    target = await Promise.race([
      navigator.serviceWorker.ready,
      new Promise(function (resolve) { setTimeout(function () { resolve(null); }, 5000); }),
    ]).catch(function () { return null; });
  }
  if (target && typeof target.showNotification === 'function') {
    try {
      await target.showNotification('forge-bot diagnostics', {
        body: 'Diagnostic notification from forge-bot.',
        icon: '/notifications/icon.png',
        badge: '/notifications/badge.png',
        tag: 'forge-bot-diagnostics',
      });
      report.displayTest = 'resolved';
    } catch (error) {
      report.displayTest = error.name + ': ' + error.message;
    }
  } else {
    report.displayTest = 'no active registration';
  }
  return report;
}

async function runDiagnostics() {
  setStatus('Running diagnostics...');
  const report = await collectDiagnostics();
  const box = document.getElementById('diagnostics');
  box.textContent = JSON.stringify(report, null, 2);
  box.style.display = 'block';
  try {
    const response = await fetch('/notifications/diagnostics', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(report),
    });
    setStatus(response.ok
      ? 'Diagnostics uploaded; read them at /notifications/diagnostics.'
      : 'Diagnostics collected locally (upload returned ' + response.status + ').');
  } catch (error) {
    setStatus('Diagnostics collected locally (upload failed: ' + error + ').');
  }
}

// Display one entry. Resolves to `{ handled, raised }`: `handled` means the
// entry may be consumed (accepted, blocked, or impossible to show) so the
// cursor can advance. `raised` means the browser accepted the request; the
// page cannot verify whether the OS surfaced it.
async function display(notification, report = setDeliveryStatus) {
  if (typeof Notification === 'undefined') {
    report('This browser has no Notification API; showing notifications on the page only.');
    return { handled: true, raised: false };
  }
  const permission = Notification.permission;
  if (permission === 'denied') {
    // The user blocked this origin; the log below is all we can offer.
    report('Browser notifications are blocked for this site; showing them on the page only.', true);
    return { handled: true, raised: false };
  }
  if (permission !== 'granted') {
    report('Click "Enable notifications" to receive browser notifications.', true);
    return { handled: false, raised: false };
  }
  const title = 'forge-bot: ' + notification.repository;
  // Android needs a small monochrome badge for the status bar and some OEM
  // builds will not surface a notification without an icon, so always provide
  // both.
  const options = {
    body: notification.message,
    tag: 'forge-bot-' + (notification.generation || notification.created_at) + '-' + notification.id,
    data: notification.location,
    icon: '/notifications/icon.png',
    badge: '/notifications/badge.png',
  };
  if (registration && typeof registration.showNotification === 'function') {
    try {
      await registration.showNotification(title, options);
      return { handled: true, raised: true };
    } catch (error) {
      // Android rejects `new Notification` and can reject `showNotification`
      // while the worker is settling; retry rather than consume the entry.
      report('Could not show a notification: ' + error.message);
      return { handled: false, raised: false };
    }
  }
  try {
    new Notification(title, options);
    return { handled: true, raised: true };
  } catch (error) {
    // No service worker and no usable constructor: keep the log entry and
    // advance so the same batch is not fetched forever, but report honestly
    // that nothing was raised (the mobile constructor throws).
    report('This browser cannot raise system notifications; showing them on the page only.');
    return { handled: true, raised: false };
  }
}

function render(notification) {
  if (rendered.has(notification.id)) { return; }
  rendered.add(notification.id);
  const li = document.createElement('li');
  const link = document.createElement('a');
  link.href = notification.location;
  link.textContent = notification.repository + ' · ' + notification.author;
  const body = document.createElement('div');
  body.textContent = notification.message;
  const when = document.createElement('small');
  when.textContent = notification.created_at;
  li.append(link, body, when);
  log.prepend(li);
}

function cursorFor(recipient) {
  return cursors[recipient] || 0;
}

function deliveredFor(recipient) {
  return delivered[recipient] || (delivered[recipient] = new Set());
}

function displayingFor(recipient) {
  return displaying[recipient] || (displaying[recipient] = new Map());
}

function isCurrent(seq, recipient) {
  return seq === requestSeq && recipientSelect.value === recipient;
}

// Move the cursor over the contiguous prefix of handled entries and forget the
// ids it now covers. A later entry displayed while an earlier one failed stays
// in `delivered`, so retrying the earlier entry does not raise it twice.
function commitCursor(recipient, committed) {
  if (committed > cursorFor(recipient)) {
    cursors[recipient] = committed;
  }
  const set = delivered[recipient];
  if (set) {
    const cursor = cursorFor(recipient);
    for (const id of set) {
      if (id <= cursor) { set.delete(id); }
    }
  }
}

// Display one entry at most once at a time. Concurrent polls for the same
// recipient await one shared promise, and an entry that was already delivered
// is skipped instead of being raised again.
async function handle(recipient, notification) {
  const deliveredSet = deliveredFor(recipient);
  if (deliveredSet.has(notification.id)) { return true; }
  const inFlight = displayingFor(recipient);
  let pending = inFlight.get(notification.id);
  if (!pending) {
    const displayGeneration = generation;
    const failures = deliveryFailures[recipient] || (deliveryFailures[recipient] = new Map());
    let feedback = null;
    pending = display(notification, function (text, permission = false) { feedback = { text, permission }; }).then(
      function (result) {
        inFlight.delete(notification.id);
        if (result.handled) { deliveredSet.add(notification.id); }
        // Only this entry's acceptance resolves its feedback. A later success
        // must not hide an earlier failure, nor erase synthetic test guidance.
        if (generation === displayGeneration) {
          if (result.raised) { failures.delete(notification.id); }
          else if (feedback) {
            if (result.handled) {
              failures.delete(notification.id);
              deliveryWarnings[recipient] = feedback;
            } else { failures.set(notification.id, feedback); }
          }
          if (recipientSelect.value === recipient) { refreshDeliveryStatus(); }
        }
        return result.handled;
      },
      function (error) {
        inFlight.delete(notification.id);
        throw error;
      },
    );
    inFlight.set(notification.id, pending);
  }
  return pending;
}

async function poll() {
  const recipient = recipientSelect.value;
  if (!recipient) { setStatus('No human configured.'); return; }
  // Responses are matched to the selection that requested them, so a slow
  // reply cannot repopulate the log after the human switched accounts.
  const seq = ++requestSeq;
  try {
    let url = '/notifications.json?recipient=' + encodeURIComponent(recipient) +
      '&after=' + cursorFor(recipient);
    if (generation) { url += '&generation=' + encodeURIComponent(generation); }
    const response = await fetch(url);
    if (!response.ok) { setStatus('Server error ' + response.status); return; }
    const data = await response.json();
    if (!isCurrent(seq, recipient)) { return; }
    if (data.generation && data.generation !== generation) {
      // The server restarted and ids began again, so every saved cursor is
      // stale. Start over instead of silently skipping the new entries. Delete
      // (rather than clear) the maps so an in-flight old-generation display
      // cannot write back into the fresh state.
      generation = data.generation;
      for (const key of Object.keys(cursors)) { delete cursors[key]; }
      for (const key of Object.keys(delivered)) { delete delivered[key]; }
      for (const key of Object.keys(displaying)) { delete displaying[key]; }
      for (const key of Object.keys(deliveryFailures)) { delete deliveryFailures[key]; }
      for (const key of Object.keys(deliveryWarnings)) { delete deliveryWarnings[key]; }
      refreshDeliveryStatus();
      log.textContent = '';
      rendered.clear();
    }
    const batchGeneration = generation;
    let committed = cursorFor(recipient);
    let handled = true;
    for (const notification of data.notifications) {
      // A recipient switch or a restart reset while a previous display was
      // pending invalidates the rest of this batch: it must not render, raise
      // or advance anything on the newly selected page.
      if (!isCurrent(seq, recipient) || generation !== batchGeneration) { return; }
      render(notification);
      // Web Push owns system delivery in push mode. This fetch updates only
      // the page log; provider/subscription errors never select polling delivery.
      const displayed = pushConfig && (pushConfig.transport === 'web_push' ||
        await handle(recipient, notification));
      if (!isCurrent(seq, recipient) || generation !== batchGeneration) { return; }
      if (displayed) {
        if (handled) { committed = notification.id; }
      } else {
        handled = false;
      }
    }
    if (!isCurrent(seq, recipient) || generation !== batchGeneration) { return; }
    commitCursor(recipient, committed);
    if (handled) {
      setStatus('Watching ' + recipient + ' · ' + new Date().toLocaleTimeString());
    }
  } catch (error) {
    setStatus('Polling failed: ' + error);
  }
}

recipientSelect.addEventListener('change', function () {
  // Abandon an in-flight response for the previous recipient, then show only
  // this recipient's log with its own cursor.
  try { window.localStorage.setItem('forge-bot-recipient', recipientSelect.value); } catch (_) {}
  requestSeq++;
  refreshDeliveryStatus();
  log.textContent = '';
  rendered.clear();
  syncPush().then(poll);
});
document.getElementById('disable-push').addEventListener('click', function () { syncPush(true); });
document.getElementById('enable').addEventListener('click', enable);
document.getElementById('test').addEventListener('click', testNotification);
document.getElementById('diagnostics-button').addEventListener('click', runDiagnostics);
document.getElementById('refresh').addEventListener('click', async function () {
  if (!pushConfig) { await loadPushConfig(); await syncPush(); }
  await poll();
});
// A permission change (for example the user unblocks notifications in browser
// settings) should deliver whatever is still pending without waiting.
if (navigator.permissions && navigator.permissions.query) {
  navigator.permissions.query({ name: 'notifications' }).then(function (state) {
    state.onchange = function () {
      if (state.state === 'granted') { permissionGranted(); poll(); }
    };
  }).catch(function () {});
}
// Wait for an active worker before the first poll so mobile browsers never
// fall back to the unsupported `Notification` constructor on initial load.
setupServiceWorker().then(async function () {
  await loadPushConfig();
  await syncPush();
  poll();
  setInterval(poll, 5000);
});
