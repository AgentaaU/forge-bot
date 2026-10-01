const status = document.getElementById('status');
const log = document.getElementById('log');
const recipientSelect = document.getElementById('recipient');
let registration = null;
let generation = null;
let requestSeq = 0;
// One cursor per human, so switching accounts never hides the new account's
// pending notifications behind the previous account's high-water mark.
const cursors = {};

function setStatus(text) { status.textContent = text; }

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
  } catch (error) {
    registration = null;
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
    setStatus('This browser has no Notification API. On mobile, serve over HTTPS and add the page to the Home Screen.');
    return;
  }
  const permission = await Notification.requestPermission();
  setStatus('Notification permission: ' + permission);
}

function display(notification) {
  const title = 'forge-bot: ' + notification.repository;
  const options = { body: notification.message, data: notification.location };
  // One failed notification must not abort the rest of the batch: the cursor
  // has already advanced past this entry.
  try {
    if (registration && typeof registration.showNotification === 'function') {
      registration.showNotification(title, options).catch(function () {});
    } else if (Notification.permission === 'granted') {
      new Notification(title, options);
    }
  } catch (error) {
    // Ignore: the entry is still visible in the log below.
  }
}

function render(notification) {
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
    if (seq !== requestSeq) { return; }
    if (data.generation && data.generation !== generation) {
      // The server restarted and ids began again, so every saved cursor is
      // stale. Start over instead of silently skipping the new entries.
      generation = data.generation;
      for (const key of Object.keys(cursors)) { delete cursors[key]; }
      log.textContent = '';
    }
    for (const notification of data.notifications) {
      cursors[recipient] = Math.max(cursorFor(recipient), notification.id);
      render(notification);
      display(notification);
    }
    setStatus('Watching ' + recipient + ' · ' + new Date().toLocaleTimeString());
  } catch (error) {
    setStatus('Polling failed: ' + error);
  }
}

recipientSelect.addEventListener('change', function () {
  // Abandon an in-flight response for the previous recipient, then show only
  // this recipient's log with its own cursor.
  requestSeq++;
  log.textContent = '';
  poll();
});
document.getElementById('enable').addEventListener('click', enable);
document.getElementById('refresh').addEventListener('click', poll);
// Wait for an active worker before the first poll so mobile browsers never
// fall back to the unsupported `Notification` constructor on initial load.
setupServiceWorker().then(function () {
  poll();
  setInterval(poll, 5000);
});
