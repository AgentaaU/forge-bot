#!/usr/bin/env node
// Dynamic regression for the emitted /notifications page script.
//
// The page is plain JavaScript with no build step, so this harness runs the
// real `src/notifications.js` against a deterministic browser-API stub:
//   * notification permission is already granted,
//   * the service worker registration resolves while the worker is still
//     `installing`, then activates later.
//
// It asserts that no notification request or display happens before the
// worker is active, that the first poll uses `after=0`, and that the batch is
// delivered through `ServiceWorkerRegistration.showNotification` (never the
// unsupported `Notification` constructor).
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

const requests = [];
const shown = [];
let constructorCalls = 0;
let worker;

function element() {
  return {
    textContent: '',
    value: 'alice',
    children: [],
    href: '',
    listeners: {},
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
  refresh: element(),
};

class Notification {
  static permission = 'granted';
  static requestPermission() {
    return Promise.resolve('granted');
  }
  constructor() {
    constructorCalls += 1;
    throw new TypeError('Notification constructor is unsupported on mobile');
  }
}

const workerPromise = new Promise(() => {});

const registration = {
  active: null,
  installing: null,
  showNotification(title, options) {
    shown.push({ title, body: options && options.body });
    return Promise.resolve();
  },
};

const sandbox = {
  console,
  Promise,
  Object,
  Math,
  Date,
  encodeURIComponent,
  setTimeout,
  clearTimeout,
  document: {
    getElementById: (id) => elements[id],
    createElement: () => element(),
  },
  navigator: {
    serviceWorker: {
      register(script, options) {
        if (script !== '/notifications/sw.js') throw new Error(`unexpected script ${script}`);
        if (!options || options.scope !== '/') throw new Error('service worker not registered at root scope');
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
      ready: workerPromise,
    },
  },
  Notification,
  fetch(url) {
    requests.push(url);
    return Promise.resolve({
      ok: true,
      json: () =>
        Promise.resolve({
          generation: 'gen-1',
          notifications: [
            {
              id: 1,
              recipient: 'alice',
              author: 'shylock-bot',
              repository: 'shylock/forge-bot',
              location: 'https://forge.example/shylock/forge-bot/issues/1',
              message: 'please rotate the key',
              created_at: '2026-01-01T00:00:00Z',
            },
          ],
        }),
    });
  },
  setInterval() {},
};

vm.runInNewContext(source, sandbox, { filename: 'notifications.js' });

// Give the registration promise a chance to resolve; the worker is still
// installing, so the page must not poll or display anything yet.
await new Promise((resolve) => setImmediate(resolve));
await new Promise((resolve) => setImmediate(resolve));
assert(requests.length === 0, `polled before the worker was active: ${JSON.stringify(requests)}`);
assert(shown.length === 0, `displayed before the worker was active: ${JSON.stringify(shown)}`);

// Activate the worker; now the first poll may run.
worker.activate();
await new Promise((resolve) => setImmediate(resolve));
await new Promise((resolve) => setImmediate(resolve));
assert(requests.length === 1, `expected one poll after activation, got ${JSON.stringify(requests)}`);
assert(requests[0].includes('after=0'), `first poll must start at after=0: ${requests[0]}`);
assert(shown.length === 1, `expected the notification to be displayed: ${JSON.stringify(shown)}`);
assert(constructorCalls === 0, 'used the unsupported Notification constructor');
assert(elements.log.children.length === 1, 'notification was not rendered in the log');

console.log('validated: waits for an active worker, then delivers via showNotification');
