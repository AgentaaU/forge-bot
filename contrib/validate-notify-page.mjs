#!/usr/bin/env node
// Dynamic regression for the emitted /notifications page script.
//
// The page is plain JavaScript with no build step, so this harness runs the
// real `web/notifications.js` against a deterministic browser-API stub. It
// covers the regressions that are hard to catch with static assertions:
//
//   1. Delayed activation: the registration resolves while the worker is still
//      `installing`, so polling/display must wait for `activated`.
//   2. Missing permission: the first batch must not be consumed while
//      permission is `default`; after the user grants it, the same batch is
//      displayed and only then does the cursor advance.
//   3. Partial display failure: a later entry succeeding in a batch must not
//      let the cursor pass an earlier entry that failed, and retrying the
//      earlier entry must not raise the already-displayed later entry again.
//   4. Recipient switch while a display is pending: the abandoned batch must
//      not render, raise or advance anything on the newly selected page.
//   5. Restart-generation reset while a display is pending: the abandoned old
//      batch must not restore a stale cursor or repopulate the reset log.
//
// Run from the repository root with: node contrib/validate-notify-page.mjs

import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import vm from 'node:vm';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const source = readFileSync(join(root, 'web', 'notifications.js'), 'utf8');

function assert(condition, message) {
  if (!condition) throw new Error(message);
}

const tick = () => new Promise((resolve) => setImmediate(resolve));
async function settle() {
  await tick();
  await tick();
  await tick();
}

// Run the actual worker behind the page's MessageChannel protocol. Cache state
// can be shared by replacement worker instances to exercise durable receipts.
function workerHarness(registration, records = new Map()) {
  const handlers = {};
  const cache = {
    async match(key) { return records.get(key); },
    async put(key, value) { records.set(key, value); },
    async keys() { return Array.from(records.keys()); },
    async delete(key) { return records.delete(key); },
  };
  const self = {
    location: { origin: 'https://forge.example' }, registration,
    caches: { async open() { return cache; } },
    addEventListener(type, handler) { handlers[type] = handler; },
    skipWaiting() {}, clients: { async claim() {}, async openWindow() {} },
  };
  vm.runInNewContext(readFileSync(join(root, 'web', 'notifications-sw.js'), 'utf8'), { self, URL, Response });
  return {
    handlers, records,
    message(data, ports) {
      let pending;
      handlers.message({ data, ports, waitUntil(value) { pending = value; } });
      return pending;
    },
    push(entry) {
      let pending;
      handlers.push({ data: { json: () => entry }, waitUntil(value) { pending = value; } });
      return pending;
    },
  };
}
class TestMessageChannel {
  constructor() {
    this.port1 = { close() {} };
    this.port2 = { close() {}, postMessage: (data) => {
      Promise.resolve().then(() => this.port1.onmessage({ data }));
    } };
  }
}

function notification(id, recipient, message, generation = 'gen-1') {
  return {
    id,
    generation,
    recipient,
    author: 'shylock-bot',
    repository: 'shylock/forge-bot',
    location: `https://forge.example/shylock/forge-bot/issues/${id}`,
    message,
    created_at: '2026-01-01T00:00:00Z',
  };
}

// `entries` is filtered by recipient and `after` exactly like the real
// endpoint, so a test can prove which cursor the page asked for. `onShow`
// receives the entry, the per-entry attempt count and the options; it may
// resolve, reject (to simulate a failed display) or return a promise the test
// resolves later (to hold a display in flight).
function createHarness({ permission, requestedPermission = 'granted', notificationApi = true, secureContext = true, entries = [], onShow, registrationError, push = false, existingPush = false, failPush = false, configError = false, configUnavailable = false } = {}) {
  const pushRequests = [];
  let subscription = existingPush ? makeSubscription() : null;
  function makeSubscription() {
    return { endpoint: "https://fcm.googleapis.com/test", toJSON() { return { endpoint: this.endpoint, keys: {} }; }, async unsubscribe() { subscription = null; return true; } };
  }
  const requests = [];
  const shown = [];
  const attempts = new Map();
  let currentPermission = permission;
  let currentGeneration = 'gen-1';
  let constructorCalls = 0;
  let workerMessages = 0;
  let worker;

  function element() {
    return {
      _text: '',
      value: 'alice',
      children: [],
      href: '',
      listeners: {},
      get textContent() {
        return this._text;
      },
      set textContent(value) {
        this._text = value;
        // The real DOM clears children when textContent is assigned.
        if (value === '') { this.children = []; }
      },
      append(...nodes) {
        this.children.push(...nodes);
      },
      prepend(node) {
        this.children.unshift(node);
      },
      addEventListener(type, handler) {
        this.listeners[type] = handler;
      },
    };
  }

  const elements = {
    status: element(),
    'delivery-status': element(),
    log: element(),
    recipient: element(),
    enable: element(),
    'disable-push': element(),
    test: element(),
    diagnostics: element(),
    'diagnostics-button': element(),
    refresh: element(),
  };

  class Notification {
    static get permission() {
      return currentPermission;
    }
    static requestPermission() {
      currentPermission = requestedPermission;
      return Promise.resolve(requestedPermission);
    }
    constructor() {
      constructorCalls += 1;
      throw new TypeError('Illegal constructor on mobile');
    }
  }

  const registration = {
    pushManager: push ? {
      async getSubscription() { return subscription; },
      async subscribe(options) {
        assert(options.userVisibleOnly && options.applicationServerKey.length === 65, 'invalid push subscription options');
        subscription = makeSubscription();
        return subscription;
      },
    } : undefined,
    active: null,
    installing: null,
    showNotification(title, options) {
      const entry = entries.find((candidate) => candidate.message === options.body);
      const id = entry ? entry.id : 0;
      const attempt = (attempts.get(id) || 0) + 1;
      attempts.set(id, attempt);
      return Promise.resolve()
        .then(() => (onShow ? onShow(entry || { id }, attempt, options) : undefined))
        .then(() => {
          shown.push({ id, title, body: options.body });
        });
    },
  };

  const background = workerHarness(registration);

  const permissionState = { state: permission, onchange: null };
  const sandbox = {
    window: { Notification, isSecureContext: secureContext },
    atob: (value) => Buffer.from(value, "base64").toString("binary"),
    Uint8Array,
    MessageChannel: TestMessageChannel,
    console,
    Promise,
    Object,
    Math,
    Date,
    encodeURIComponent,
    decodeURIComponent,
    setTimeout,
    clearTimeout,
    location: { href: 'https://forge.example/notifications', origin: 'https://forge.example' },
    document: {
      getElementById: (id) => elements[id],
      createElement: () => element(),
    },
    navigator: {
      permissions: { query: async () => permissionState },
      serviceWorker: {
        register(script, options) {
          if (script !== '/notifications/sw.js') throw new Error(`unexpected script ${script}`);
          if (!options || options.scope !== '/') {
            throw new Error('service worker not registered at root scope');
          }
          if (registrationError) {
            return Promise.reject(registrationError);
          }
          worker = {
            state: 'installing',
            postMessage: (data, ports) => { workerMessages++; return background.message(data, ports); },
            listeners: {},
            addEventListener(type, handler) {
              this.listeners[type] = handler;
            },
            activate() {
              this.state = 'activated';
              registration.active = this;
              if (this.listeners.statechange) this.listeners.statechange();
            },
          };
          registration.installing = worker;
          return Promise.resolve(registration);
        },
        // Never resolves, so the script must use the registration's worker
        // state rather than the container readiness promise.
        ready: new Promise(() => {}),
      },
    },
    Notification,
    fetch(url, options) {
      if (url === '/notifications/push') {
        if (!options && configUnavailable) { return Promise.reject(new Error('offline')); }
        if (options) { pushRequests.push({ method: options.method, body: JSON.parse(options.body) }); }
        return Promise.resolve({ ok: !options || !failPush, status: failPush ? 500 : 200, json: async () => (push
          ? { transport: 'web_push', error: configError ? 'Missing VAPID key' : undefined, public_key: Buffer.alloc(65, 1).toString('base64url') }
          : { transport: 'polling' }) });
      }
      requests.push(url);
      const recipient = /[?&]recipient=([^&]+)/.exec(url);
      const after = /[?&]after=(\d+)/.exec(url);
      const selected = recipient ? decodeURIComponent(recipient[1]) : '';
      const cursor = after ? Number(after[1]) : 0;
      return Promise.resolve({
        ok: true,
        json: () =>
          Promise.resolve({
            generation: currentGeneration,
            notifications: entries.filter(
              (entry) => entry.recipient === selected && entry.id > cursor,
            ),
          }),
      });
    },
    setInterval() {},
  };

  if (!notificationApi) { delete sandbox.Notification; delete sandbox.window.Notification; }
  vm.runInNewContext(source, sandbox, { filename: 'notifications.js' });

  return {
    elements,
    pushRequests,
    setConfigUnavailable: (value) => { configUnavailable = value; },
    push: (entry) => background.push(entry),
    expireSubscription: () => { subscription = null; },
    requests,
    shown,
    attempts,
    entries,
    activate: () => worker.activate(),
    permissionChange: () => { permissionState.state = currentPermission; permissionState.onchange(); },
    setPermission: (value) => { currentPermission = value; },
    grant: () => {
      currentPermission = 'granted';
    },
    setGeneration: (value) => {
      currentGeneration = value;
    },
    poll: () => elements.refresh.listeners.click(),
    constructorCalls: () => constructorCalls,
    workerMessages: () => workerMessages,
  };
}

// Scenario 1: permission already granted, worker activates late.
{
  const h = createHarness({
    permission: 'granted',
    entries: [notification(1, 'alice', 'please rotate the key')],
  });
  await settle();
  assert(h.requests.length === 0, `polled before the worker was active: ${JSON.stringify(h.requests)}`);
  assert(h.shown.length === 0, `displayed before the worker was active: ${JSON.stringify(h.shown)}`);

  h.activate();
  await settle();
  assert(h.requests.length === 1, `expected one poll after activation, got ${JSON.stringify(h.requests)}`);
  assert(h.requests[0].includes('after=0'), `first poll must start at after=0: ${h.requests[0]}`);
  assert(h.shown.length === 1, `expected the notification to be displayed: ${JSON.stringify(h.shown)}`);
  assert(h.constructorCalls() === 0, 'used the unsupported Notification constructor');
  assert(h.elements.log.children.length === 1, 'notification was not rendered in the log');
  console.log('delayed activation: waited, then delivered via showNotification');
}

// Scenario 2: permission is still "default" for the first batch. The entry is
// shown in the log but must not be consumed until it is actually displayed.
{
  const h = createHarness({
    permission: 'default',
    entries: [notification(1, 'alice', 'please rotate the key')],
  });
  h.activate();
  await settle();
  assert(h.requests.length === 1, `expected the initial poll, got ${JSON.stringify(h.requests)}`);
  assert(h.shown.length === 0, 'displayed without permission');
  assert(h.elements.log.children.length === 1, 'pending entry missing from the log');

  // Grant permission and poll again: the same entry is delivered and the
  // cursor finally advances.
  h.grant();
  await h.poll();
  await settle();
  assert(h.shown.length === 1, `granting permission did not deliver the entry: ${JSON.stringify(h.shown)}`);

  // The next poll starts after the delivered id.
  await h.poll();
  await settle();
  assert(
    h.requests.at(-1).includes('after=1'),
    `cursor did not advance after delivery: ${JSON.stringify(h.requests)}`,
  );
  assert(h.constructorCalls() === 0, 'used the unsupported Notification constructor');
  console.log('missing permission: kept the entry pending, delivered after grant');
}

// Scenario 3: a two-entry batch where the first display fails and the second
// succeeds. The cursor must stay before the failure so entry 1 is retried, and
// the successful entry 2 must not be raised again on that retry.
{
  let failedOnce = false;
  const h = createHarness({
    permission: 'granted',
    entries: [
      notification(1, 'alice', 'first'),
      notification(2, 'alice', 'second'),
    ],
    onShow: (entry) => {
      if (entry.id === 1 && !failedOnce) {
        failedOnce = true;
        throw new Error('service worker is not active yet');
      }
    },
  });
  h.activate();
  await settle();
  assert(
    h.shown.length === 1 && h.shown[0].id === 2,
    `expected only entry 2 on the first batch: ${JSON.stringify(h.shown)}`,
  );
  assert(h.elements.log.children.length === 2, 'both entries should be rendered in the log');
  assert(
    h.requests.at(-1).includes('after=0'),
    `a later success must not consume an earlier failure: ${h.requests.at(-1)}`,
  );

  // Retry: entry 1 is delivered, entry 2 is not displayed a second time.
  await h.poll();
  await settle();
  assert(
    h.shown.filter((entry) => entry.id === 1).length === 1,
    `entry 1 was not delivered on retry: ${JSON.stringify(h.shown)}`,
  );
  assert(
    h.shown.filter((entry) => entry.id === 2).length === 1,
    `entry 2 was displayed again on retry: ${JSON.stringify(h.shown)}`,
  );
  assert(h.attempts.get(2) === 1, `entry 2 was displayed ${h.attempts.get(2)} times`);

  // Only now does the cursor pass both entries.
  await h.poll();
  await settle();
  assert(
    h.requests.at(-1).includes('after=2'),
    `cursor did not advance after the prefix was handled: ${h.requests.at(-1)}`,
  );
  console.log('partial failure: retried entry 1, did not redisplay entry 2, cursor advanced to 2');
}

// Scenario 4: switch recipients while Alice's first display is still pending.
// The abandoned batch must not render Alice's second entry or claim Alice is
// being watched on Bob's page.
{
  let releaseFirst;
  const h = createHarness({
    permission: 'granted',
    entries: [
      notification(1, 'alice', 'alice-one'),
      notification(2, 'alice', 'alice-two'),
    ],
    onShow: (entry) => {
      if (entry.id === 1) {
        return new Promise((resolve) => {
          releaseFirst = resolve;
        });
      }
      return undefined;
    },
  });
  h.activate();
  await settle();
  assert(typeof releaseFirst === 'function', 'the first display was not held in flight');

  h.elements.recipient.value = 'bob';
  h.elements.recipient.listeners.change();
  await settle();

  releaseFirst();
  await settle();

  assert(
    h.attempts.get(2) === undefined,
    `the abandoned batch displayed a later entry: attempts=${h.attempts.get(2)}`,
  );
  assert(h.elements.log.children.length === 0, 'the abandoned batch rendered on the new page');
  assert(
    h.elements.status.textContent.includes('bob'),
    `status leaked the old recipient: ${h.elements.status.textContent}`,
  );
  console.log('recipient switch: pending display did not leak into the new recipient');
}

// Scenario 5: a server restart changes the generation while Alice's old
// display is pending and the new generation reuses the same ids. The old batch
// must not restore a stale cursor or repopulate the reset log.
{
  let releaseOld;
  const h = createHarness({
    permission: 'granted',
    entries: [
      notification(1, 'alice', 'old-one'),
      notification(2, 'alice', 'old-two'),
    ],
    onShow: (entry) => {
      if (entry.message === 'old-one') {
        return new Promise((resolve) => {
          releaseOld = resolve;
        });
      }
      return undefined;
    },
  });
  h.activate();
  await settle();
  assert(typeof releaseOld === 'function', 'the old display was not held in flight');

  // The server restarts: ids begin again and the generation changes.
  h.entries.length = 0;
  h.entries.push(notification(1, 'alice', 'new-one', 'gen-2'));
  h.setGeneration('gen-2');
  await h.poll();
  await settle();
  assert(
    h.shown.some((entry) => entry.body === 'new-one'),
    `the fresh-generation entry was not displayed: ${JSON.stringify(h.shown)}`,
  );
  assert(h.elements.log.children.length === 1, 'the reset log has stale entries');

  releaseOld();
  await settle();
  assert(h.elements.log.children.length === 1, 'the old batch rendered after the reset');

  await h.poll();
  await settle();
  assert(
    h.requests.at(-1).includes('after=1'),
    `the old batch restored a stale cursor: ${h.requests.at(-1)}`,
  );
  console.log('generation reset: pending old display abandoned, cursor stayed fresh');
}

// Scenario 6: the service worker fails to register, so `display` falls back to
// the mobile `Notification` constructor, which throws. The test button must
// report the failure instead of claiming the notification was sent.
{
  const h = createHarness({
    permission: 'granted',
    registrationError: new Error('site data is blocked'),
  });
  await settle();
  assert(h.shown.length === 0, 'nothing should be shown without a registration');

  h.elements.test.listeners.click();
  await settle();
  assert(
    !h.elements['delivery-status'].textContent.includes('Test notification accepted'),
    `the test button falsely reported success: ${h.elements.status.textContent}`,
  );
  assert(
    h.elements['delivery-status'].textContent.includes('cannot raise system notifications'),
    `the test button did not report the failure: ${h.elements.status.textContent}`,
  );
  console.log('registration failure: test button does not falsely claim success');
}

// Configured Web Push never displays polled entries as a fallback.
for (const failure of ['unsubscribed', 'provider failure', 'expired subscription', 'missing key', 'config unavailable']) {
  const h = createHarness({ permission: 'granted', push: true,
    existingPush: failure !== 'unsubscribed', failPush: failure === 'provider failure',
    configError: failure === 'missing key', configUnavailable: failure === 'config unavailable' });
  h.activate(); await settle();
  if (failure === 'expired subscription') { h.expireSubscription(); }
  h.entries.push(notification(1, 'alice', failure));
  await h.poll(); await h.poll();
  assert(h.shown.length === 0, failure + ': switched to polling delivery');
  assert(h.elements.log.children.length === 1, failure + ': missing page log');
  if (failure === 'config unavailable') { assert(h.requests.at(-1).includes('after=0'), 'unknown mode consumed entry'); }
  if (['missing key', 'provider failure', 'config unavailable'].includes(failure)) {
    assert(h.elements['delivery-status'].textContent.length > 0, failure + ': error missing');
  }
}
{
  const h = createHarness({ permission: 'granted', push: true });
  h.activate(); await settle();
  assert(h.pushRequests.length === 0, 'startup subscribed without explicit click');
  await h.elements.enable.listeners.click(); await settle();
  assert(h.pushRequests.at(-1).body.recipient === 'alice', 'enable did not subscribe');
  const entry = notification(1, 'alice', 'push delivery');
  h.entries.push(entry); await h.push(entry); await h.poll();
  assert(h.shown.length === 1 && h.workerMessages() === 0, 'page participated in push delivery');
  h.elements.recipient.value = 'bob'; h.elements.recipient.listeners.change(); await settle();
  assert(h.pushRequests.at(-1).body.recipient === 'bob', 'recipient did not rebind');
  await h.elements['disable-push'].listeners.click(); await settle();
  assert(h.pushRequests.at(-1).method === 'DELETE', 'disable did not unsubscribe');
  h.entries.push(notification(2, 'bob', 'disabled')); await h.poll();
  assert(h.shown.length === 1, 'disable switched to polling');
}
// Concurrent polling/push and a dismissed notification across worker restart.
{
  let count = 0;
  const registration = { async showNotification() { count++; }, async getNotifications() { return []; } };
  const worker = workerHarness(registration);
  const entry = notification(1, 'alice', 'concurrent display');
  let response;
  await Promise.all([
    worker.push(entry),
    worker.message({ type: 'display-notification', notification: entry }, [{ postMessage(value) { response = value; }, close() {} }]),
  ]);
  assert(count === 1 && response.displayed, 'concurrent transports did not share one successful display');
  const replacement = workerHarness(registration, worker.records);
  await replacement.push(entry);
  assert(count === 1, 'dismissed notification was redisplayed after worker restart');
}

// Storage failures still allow delivery, and receipt storage stays bounded.
{
  let count = 0;
  const registration = { async showNotification() { count++; }, async getNotifications() { return []; } };
  const records = new Map();
  for (let index = 0; index < 2050; index++) { records.set('old-' + index, {}); }
  const worker = workerHarness(registration, records);
  await worker.push(notification(3, 'alice', 'bounded receipts'));
  assert(records.size === 2048 && !records.has('old-0'), 'persistent receipt cache grew beyond its bound');
  const unavailable = { async match() { throw new Error('storage blocked'); }, async put() { throw new Error('storage blocked'); } };
  const degraded = workerHarness(registration, unavailable);
  const entry = notification(4, 'alice', 'storage unavailable');
  await degraded.push(entry);
  await degraded.push(entry);
  assert(count === 2, 'storage failure prevented display or in-memory deduplication');
}

// Exercise the real worker while no page is running.
{
  const handlers = {};
  const shown = [];
  const opened = [];
  const worker = { location: { origin: 'https://bot.example' },
    addEventListener: (type, handler) => { handlers[type] = handler; },
    skipWaiting() {},
    registration: { showNotification: async (title, options) => { shown.push({ title, options }); } },
    clients: { claim: async () => {}, openWindow: async (url) => { opened.push(url); } },
  };
  vm.runInNewContext(readFileSync(join(root, 'web', 'notifications-sw.js'), 'utf8'), { self: worker, URL });
  let pending;
  const entry = notification(7, 'alice', 'background message');
  handlers.push({ data: { json: () => entry }, waitUntil: (value) => { pending = value; } });
  await pending;
  assert(shown[0].options.body === entry.message, 'worker did not show pushed payload');
  assert(shown[0].options.icon && shown[0].options.badge, 'worker omitted mobile icons');
  let closed = false;
  handlers.notificationclick({ notification: { data: entry.location, close: () => { closed = true; } }, waitUntil: (value) => { pending = value; } });
  await pending;
  assert(closed && opened[0] === entry.location, 'click did not open the forge comment');
  handlers.notificationclick({ notification: { data: 'javascript:alert(1)', close() {} }, waitUntil() { throw new Error('unsafe URL opened'); } });
}
console.log('web push: configured transport, subscription lifecycle, background display and click validated');

// Polling activity must not erase blocked or unsupported delivery feedback.
for (const options of [
  { permission: 'denied' },
  { permission: 'granted', registrationError: new Error('storage blocked') },
]) {
  const h = createHarness({ ...options, entries: [notification(1, 'alice', 'blocked')] });
  await settle();
  if (!options.registrationError) { h.activate(); await settle(); }
  const feedback = h.elements['delivery-status'].textContent;
  assert(feedback.includes('page only'), 'missing delivery failure feedback');
  await h.poll();
  assert(h.elements['delivery-status'].textContent === feedback, 'poll erased delivery feedback');
}
{
  const h = createHarness({ permission: 'granted' });
  h.activate();
  await settle();
  await h.elements.test.listeners.click();
  assert(h.shown.length === 1, 'test did not ask the browser to show a notification');
  const feedback = h.elements['delivery-status'].textContent;
  assert(feedback.includes('accepted by the browser') && feedback.includes('notification shade'), 'test claimed verified OS delivery');
  await h.poll();
  assert(h.elements['delivery-status'].textContent === feedback, 'poll erased test guidance');
}
console.log('system delivery feedback: blocked states and Android guidance survive polling');
// Polling-only mode must bypass stale durable worker receipts. Seed a receipt
// by displaying through push, then poll the same entry through the page.
{
  const entry = notification(1, 'alice', 'polling-only regression');
  const h = createHarness({ permission: 'granted' });
  h.activate();
  await settle();
  await h.push(entry);
  h.entries.push(entry);
  await h.poll();
  assert(h.shown.length === 2, 'old worker receipt suppressed direct polling display');
  assert(h.workerMessages() === 0, 'polling-only mode used worker receipt protocol');
  await h.poll();
  assert(h.shown.length === 2, 'page cursor did not prevent repeated polling display');
}
{
  const h = createHarness({ permission: 'granted', push: true, existingPush: true });
  h.activate();
  await settle();
  h.entries.push(notification(1, 'alice', 'push mode updates log only'));
  await h.poll();
  assert(h.workerMessages() === 0, 'Web Push mode used page delivery');
  await h.elements.test.listeners.click();
  assert(h.workerMessages() === 0 && h.shown.length === 1, 'test button did not use a fresh direct display');
}
console.log('polling-only: direct display bypasses stale worker receipts; push mode avoids page delivery');
// Every failed user attempt replaces the preceding successful test result.
for (const button of ['test', 'enable']) {
  for (const options of [
    { permission: 'default', requestedPermission: 'denied' },
    { permission: 'default', requestedPermission: 'default' },
    { permission: 'granted', notificationApi: false },
    ...(button === 'enable' ? [{ permission: 'granted', secureContext: false }] : []),
  ]) {
    const h = createHarness(options);
    h.activate();
    await settle();
    await h.elements[button].listeners.click();
    const feedback = h.elements['delivery-status'].textContent;
    assert(feedback.includes('permission:') || feedback.includes('no Notification API') || feedback.includes('require HTTPS'),
      button + ': missing persistent early failure');
    assert(h.shown.length === 0, button + ': failed attempt requested display');
    await h.poll();
    assert(h.elements['delivery-status'].textContent === feedback, button + ': empty poll erased early failure');
  }
}
{
  const h = createHarness({ permission: 'granted', requestedPermission: 'denied' });
  h.activate();
  await settle();
  await h.elements.test.listeners.click();
  assert(h.elements['delivery-status'].textContent.includes('accepted by the browser'), 'missing prior success');
  h.setPermission('denied');
  await h.elements.test.listeners.click();
  await h.poll();
  assert(h.elements['delivery-status'].textContent === 'Notification permission: denied', 'denied test retained prior success');
  assert(h.shown.length === 1, 'denied second test requested display');
}
// Recovery belongs to the failed entry, including within a partial batch.
for (const partial of [false, true]) {
  let recoverSecond = false;
  const h = createHarness({ permission: 'granted',
    entries: [notification(1, 'alice', 'retry-one'), ...(partial ? [
      notification(2, 'alice', 'retry-two'), notification(3, 'alice', 'success-three'),
    ] : [])],
    onShow: (entry, attempt) => {
      if (entry.id === 1 && attempt === 1) { throw new Error('temporary failure'); }
      if (partial && entry.id === 2 && !recoverSecond) { throw new Error('still unresolved'); }
    },
  });
  h.activate();
  await settle();
  assert(h.elements['delivery-status'].textContent.includes('temporary failure'), 'later success hid first failure');
  await h.poll();
  if (partial) {
    assert(h.elements['delivery-status'].textContent.includes('still unresolved'), 'first recovery hid unresolved second entry');
    await h.poll();
    assert(h.requests.at(-1).includes('after=1'), 'cursor passed unresolved second entry');
    recoverSecond = true;
    await h.poll();
  }
  await h.poll();
  assert(h.elements['delivery-status'].textContent === '', 'accepted retry retained delivery error');
  assert(h.requests.at(-1).includes(partial ? 'after=3' : 'after=1'), 'recovered cursor did not advance');
  await h.elements.test.listeners.click();
  const guidance = h.elements['delivery-status'].textContent;
  h.entries.push(notification(partial ? 4 : 2, 'alice', 'unrelated success'));
  await h.poll();
  await h.poll();
  assert(h.elements['delivery-status'].textContent === guidance, 'unrelated or empty poll erased test guidance');
}
console.log('attempt feedback and per-entry retry recovery validated');
// Consumed denied entries cannot retry; origin-wide grants retire their warnings.
for (const grantFlow of ['enable', 'permission-change', 'test', 'test-request']) {
  const h = createHarness({ permission: 'denied', entries: [notification(1, 'alice', 'blocked old entry')] });
  h.activate(); await settle(); await h.poll();
  assert(h.requests.at(-1).includes('after=1'), 'denied entry was not consumed');
  assert(h.elements['delivery-status'].textContent.includes('blocked'), 'denied warning missing');
  h.elements.recipient.value = 'bob'; h.elements.recipient.listeners.change(); await settle();
  if (grantFlow === 'test-request') { h.setPermission('default'); } else { h.grant(); }
  if (grantFlow === 'permission-change') { h.permissionChange(); }
  else { await h.elements[grantFlow === 'test-request' ? 'test' : grantFlow].listeners.click(); }
  await settle();
  h.elements.recipient.value = 'alice'; h.elements.recipient.listeners.change(); await settle();
  h.entries.push(notification(2, 'alice', 'new accepted entry'));
  await h.poll(); await h.elements.test.listeners.click(); await h.poll();
  assert(h.shown.length === (grantFlow.startsWith('test') ? 3 : 2), 'grant failed to deliver new entry and test');
  assert(h.requests.at(-1).includes('after=2'), 'new entry cursor did not advance');
  const guidance = h.elements['delivery-status'].textContent;
  assert(guidance.includes('Test notification accepted'), 'consumed warning hid accepted test');
  await h.poll();
  assert(h.elements['delivery-status'].textContent === guidance, 'empty poll erased guidance');
}
// A grant must not clear a genuine unresolved display failure.
{
  const h = createHarness({ permission: 'granted', entries: [notification(1, 'alice', 'unresolved')],
    onShow: (entry) => { if (entry.id === 1) { throw new Error('real display failure'); } } });
  h.activate(); await settle();
  await h.elements.enable.listeners.click(); await settle();
  h.permissionChange(); await settle();
  await h.elements.test.listeners.click(); await h.poll();
  assert(h.elements['delivery-status'].textContent.includes('real display failure'), 'grant or test hid unresolved delivery');
}
console.log('consumed permission warnings retire across grant flows and recipients');
// Configuration failures survive permission guidance and recover through user actions.
for (const push of [false, true]) {
  for (const recovery of ['enable', 'refresh']) {
    const h = createHarness({ permission: 'granted', push, configUnavailable: true,
      entries: [notification(1, 'alice', 'pending during configuration outage')] });
    h.activate(); await settle();
    const error = h.elements['delivery-status'].textContent;
    assert(error.includes('Could not load notification mode'), 'configuration error missing');
    await h.elements.enable.listeners.click();
    await h.poll();
    assert(h.elements['delivery-status'].textContent === error, 'failed retry or permission grant hid configuration error');
    assert(h.shown.length === 0, 'unknown transport displayed pending entry');
    assert(h.requests.at(-1).includes('after=0'), 'unknown transport consumed pending entry');
    h.setConfigUnavailable(false);
    await h.elements[recovery].listeners.click(); await settle();
    await h.poll();
    assert(!h.elements['delivery-status'].textContent.includes('Could not load notification mode'), 'resolved configuration error remained');
    assert(h.shown.length === (push ? 0 : 1), 'recovered transport did not control automatic display');
    assert(h.requests.at(-1).includes('after=1'), 'recovered transport did not advance cursor');
    assert(h.elements.log.children.length === 1, 'configuration retry duplicated log entry');
    if (push && recovery === 'enable') {
      assert(h.pushRequests.some((request) => request.method === 'POST'), 'Enable did not subscribe after configuration recovery');
    }
  }
}
console.log('transient configuration failures recover through Enable and Refresh with transport honored');
console.log('VALIDATION OK');
