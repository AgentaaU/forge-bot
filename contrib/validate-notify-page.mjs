#!/usr/bin/env node
// Dynamic regression for the emitted /notifications page script.
//
// The page is plain JavaScript with no build step, so this harness runs the
// real `src/notifications.js` against a deterministic browser-API stub. It
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
const source = readFileSync(join(root, 'src', 'notifications.js'), 'utf8');

function assert(condition, message) {
  if (!condition) throw new Error(message);
}

const tick = () => new Promise((resolve) => setImmediate(resolve));
async function settle() {
  await tick();
  await tick();
  await tick();
}

function notification(id, recipient, message) {
  return {
    id,
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
function createHarness({ permission, entries = [], onShow, registrationError } = {}) {
  const requests = [];
  const shown = [];
  const attempts = new Map();
  let currentPermission = permission;
  let currentGeneration = 'gen-1';
  let constructorCalls = 0;
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
    log: element(),
    recipient: element(),
    enable: element(),
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
      currentPermission = 'granted';
      return Promise.resolve('granted');
    }
    constructor() {
      constructorCalls += 1;
      throw new TypeError('Illegal constructor on mobile');
    }
  }

  const registration = {
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

  const sandbox = {
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
    fetch(url) {
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

  vm.runInNewContext(source, sandbox, { filename: 'notifications.js' });

  return {
    elements,
    requests,
    shown,
    attempts,
    entries,
    activate: () => worker.activate(),
    grant: () => {
      currentPermission = 'granted';
    },
    setGeneration: (value) => {
      currentGeneration = value;
    },
    poll: () => elements.refresh.listeners.click(),
    constructorCalls: () => constructorCalls,
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
  h.entries.push(notification(1, 'alice', 'new-one'));
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
    !h.elements.status.textContent.includes('Test notification sent'),
    `the test button falsely reported success: ${h.elements.status.textContent}`,
  );
  assert(
    h.elements.status.textContent.includes('cannot raise system notifications'),
    `the test button did not report the failure: ${h.elements.status.textContent}`,
  );
  console.log('registration failure: test button does not falsely claim success');
}

console.log('VALIDATION OK');
