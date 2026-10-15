// Run with Playwright installed: node --test tests/admin_model_search.cjs
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const { chromium } = require('playwright');

test('admin model search preserves choices and follows catalog and agent changes', async () => {
    const browser = await chromium.launch({ headless: true });
    try {
        const page = await browser.newPage();
        const errors = [];
        page.on('pageerror', error => errors.push(error.message));
        let count = 10;
        await page.route('**/admin/agent-models', route => route.fulfill({
            json: { supported: true, models: Array.from({ length: count }, (_, i) => `Model-${i}`) }
        }));
        const row = fs.readFileSync('web/admin-user-row.html', 'utf8')
            .replaceAll('{user}', 'alice')
            .replace('{agent_options}', '<option value="one">One</option><option value="two">Two</option>')
            .replace('{model_options}', '<option value="">Agent default</option><option data-custom-model>Custom model…</option>')
            .replace('{effort_disabled}', '').replace('{effort_options}', '<option value="">Agent default</option>');
        const html = fs.readFileSync('web/admin.html', 'utf8')
            .replace('{user_rows}', row + row.replaceAll('alice', 'bob'))
            .replaceAll('{repository}', '').replace('{rows}', '').replace('{thread_rows}', '')
            .replace('{capacity}', '').replaceAll('{{', '{').replaceAll('}}', '}');
        await page.route('**/admin', route => route.fulfill({ contentType: 'text/html', body: html }));
        await page.goto('http://admin.test/admin');
        const alice = page.locator('tr').filter({ has: page.locator('button[data-user="alice"]') });
        const search = alice.locator('input[type=search]');
        const select = alice.locator('select[data-model]');
        await page.waitForFunction(() => document.querySelector('select[data-model]').options.length === 12);
        assert.equal(await search.isVisible(), false, '10 models do not enable search');
        count = 11;
        await alice.locator('select[data-user-agent]').selectOption('two');
        await search.waitFor({ state: 'visible' });
        await select.selectOption('Model-2');
        await search.fill('  MODEL-10  ');
        assert.deepEqual(await select.locator('option').allTextContents(), ['Agent default', 'Model-2', 'Model-10', 'Custom model…']);
        assert.equal(await select.inputValue(), 'Model-2');
        assert.equal(await page.locator('select[data-model]').nth(1).locator('option').count(), 12, 'other user is independent');
        await search.fill('missing');
        assert.match(await alice.textContent(), /No matching models/);
        assert.equal(await select.inputValue(), 'Model-2');
        page.once('dialog', dialog => dialog.accept('my-custom'));
        await select.selectOption({ label: 'Custom model…' });
        assert.equal(await select.inputValue(), 'my-custom');
        await search.fill('');
        assert.equal(await select.locator('option').count(), 14, 'clearing restores every choice');
        await search.fill('Model-1');
        page.once('dialog', dialog => dialog.dismiss());
        await select.selectOption({ label: 'Custom model…' });
        assert.equal(await select.inputValue(), 'my-custom', 'cancel preserves previous selection');
        count = 3;
        await alice.locator('select[data-user-agent]').selectOption('one');
        await search.waitFor({ state: 'hidden' });
        await page.waitForFunction(() => document.querySelector('select[data-model]').options.length === 5);
        assert.equal(await search.inputValue(), '');
        assert.equal(await select.inputValue(), '');
        assert.deepEqual(await select.locator('option').allTextContents(), ['Agent default', 'Model-0', 'Model-1', 'Model-2', 'Custom model…']);
        assert.deepEqual(errors, []);
    } finally {
        await browser.close();
    }
});

test('admin saves report success without discarding other rows and warn about unsaved edits', async () => {
    const browser = await chromium.launch({ headless: true });
    try {
        const page = await browser.newPage();
        const errors = [];
        page.on('pageerror', error => errors.push(error.message));
        const model = 'openrouter/nvidia/nemotron-3-ultra-550b-a55b:free';
        const saved = {};
        let fail = false;
        let loads = 0;
        await page.route('**/admin/agent-models', route => route.fulfill({
            json: { supported: true, models: [model] }
        }));
        await page.route('**/admin/agent-settings', async route => {
            if (fail) return route.fulfill({ status: 500, body: 'failed to save repository settings' });
            const body = route.request().postDataJSON();
            assert.equal(body.repository, 'shylock/agent-earn-money');
            saved[body.user] = body;
            await route.fulfill({ status: 204 });
        });
        await page.route(/\/admin\?/, route => {
            loads++;
            const rows = ['alice', 'bob'].map(user => fs.readFileSync('web/admin-user-row.html', 'utf8')
                .replaceAll('{user}', user)
                .replace('{agent_options}', '<option value="pi-rpc">pi-rpc</option>')
                .replace('{model_options}', `<option value="">Agent default</option><option value="${model}"${saved[user]?.model === model ? ' selected' : ''}>${model}</option><option data-custom-model>Custom model…</option>`)
                .replace('{effort_disabled}', '').replace('{effort_options}', '<option value="">Agent default</option><option value="high">high</option>')).join('');
            const html = fs.readFileSync('web/admin.html', 'utf8')
                .replace('{user_rows}', rows).replaceAll('{repository}', 'shylock/agent-earn-money')
                .replace('{rows}', '').replace('{thread_rows}', '').replace('{capacity}', '')
                .replaceAll('{{', '{').replaceAll('}}', '}');
            return route.fulfill({ contentType: 'text/html', body: html });
        });
        await page.goto('http://admin.test/admin?repository=shylock/agent-earn-money');
        const row = user => page.locator('tr').filter({ has: page.locator(`button[data-user="${user}"]`) });
        const alice = row('alice');
        const bob = row('bob');
        const status = alice.locator('[data-save-status]');
        await alice.locator('select[data-model]').selectOption(model);
        await bob.locator('select[data-effort]').selectOption('high');
        await page.waitForFunction(() => document.querySelectorAll('tr[data-dirty="true"]').length === 2);
        assert.match(await status.textContent(), /Unsaved changes/);
        const wouldWarn = () => page.evaluate(() => {
            const event = new Event('beforeunload', { cancelable: true });
            window.dispatchEvent(event);
            return event.defaultPrevented;
        });
        assert.equal(await wouldWarn(), true);
        fail = true;
        await alice.locator('button[data-user]').click();
        await status.filter({ hasText: 'Save failed:' }).waitFor();
        assert.equal(await alice.getAttribute('data-dirty'), 'true');
        assert.equal(await alice.locator('select[data-model]').inputValue(), model);
        fail = false;
        await alice.locator('button[data-user]').click();
        await status.filter({ hasText: 'Saved for shylock/agent-earn-money' }).waitFor();
        assert.equal(saved.alice.model, model);
        assert.equal(saved.alice.agent, 'pi-rpc');
        assert.equal(await alice.getAttribute('data-dirty'), 'false');
        assert.equal(await bob.locator('select[data-effort]').inputValue(), 'high');
        assert.equal(await bob.getAttribute('data-dirty'), 'true');
        assert.equal(loads, 1, 'saving must not reload and discard another user\'s edits');
        assert.equal(await wouldWarn(), true, 'another row still has unsaved edits');
        await bob.locator('select[data-effort]').selectOption('');
        await page.waitForFunction(() => !document.querySelector('tr[data-dirty="true"]'));
        assert.equal(await wouldWarn(), false, 'reverting edits clears the warning');
        await page.reload();
        assert.equal(await alice.locator('select[data-model]').inputValue(), model);
        assert.deepEqual(errors, []);
    } finally {
        await browser.close();
    }
});

for (const editResetRow of [false, true]) {
    test(`reset stays committed after dismissing unload (reset row edited: ${editResetRow})`, async () => {
        const browser = await chromium.launch({ headless: true });
        try {
            const page = await browser.newPage();
            const errors = [];
            page.on('pageerror', error => errors.push(error.message));
            let resets = 0;
            let loads = 0;
            let overridden = true;
            await page.route('**/admin/agent-models', route => route.fulfill({ json: { supported: true, models: [] } }));
            await page.route('**/admin/agent-settings', route => {
                const body = route.request().postDataJSON();
                assert.equal(body.user, 'alice');
                assert.equal(body.repository, 'owner/project');
                assert.equal(body.reset, true);
                resets++;
                overridden = false;
                return route.fulfill({ status: 204 });
            });
            await page.route(/\/admin\?/, route => {
                loads++;
                const rows = ['alice', 'bob'].map(user => fs.readFileSync('web/admin-user-row.html', 'utf8')
                    .replaceAll('{user}', user)
                    .replace('{agent_options}', '<option value="pi-rpc">pi-rpc</option>')
                    .replace('{model_options}', `<option value="">Agent default</option><option value="old-model"${user === 'alice' && overridden ? ' selected' : ''}>old-model</option><option data-custom-model>Custom model…</option>`)
                    .replace('{effort_disabled}', '').replace('{effort_options}', '<option value="">Agent default</option><option value="high">high</option>')).join('');
                const html = fs.readFileSync('web/admin.html', 'utf8')
                    .replace('{user_rows}', rows).replaceAll('{repository}', 'owner/project')
                    .replace('{rows}', '').replace('{thread_rows}', '').replace('{capacity}', '')
                    .replaceAll('{{', '{').replaceAll('}}', '}');
                return route.fulfill({ contentType: 'text/html', body: html });
            });
            await page.goto('http://admin.test/admin?repository=owner/project');
            const row = user => page.locator('tr').filter({ has: page.locator(`button[data-user="${user}"]`) });
            const alice = row('alice');
            const bob = row('bob');
            if (editResetRow) await alice.locator('select[data-effort]').selectOption('high');
            await bob.locator('select[data-effort]').selectOption('high');
            const stayed = new Promise(resolve => page.once('dialog', async dialog => {
                assert.equal(dialog.type(), 'beforeunload');
                await dialog.dismiss();
                resolve();
            }));
            await alice.locator('button[data-reset-user]').click();
            await stayed;
            assert.equal(resets, 1);
            assert.equal(overridden, false);
            assert.equal(loads, 1, 'Stay cancels reload');
            assert.match(await alice.locator('[data-save-status]').textContent(), /Reset saved.*Bot defaults are active/);
            assert.equal(await alice.getAttribute('data-dirty'), 'false');
            assert.equal(await alice.locator('select[data-model]').inputValue(), '');
            assert.equal(await alice.locator('button[data-user]').isDisabled(), true, 'removed override cannot be saved back');
            assert.equal(await alice.locator('select[data-model]').isDisabled(), true);
            assert.equal(await bob.locator('select[data-effort]').inputValue(), 'high');
            assert.equal(await bob.getAttribute('data-dirty'), 'true');
            assert.equal(await bob.locator('select[data-effort]').isEnabled(), true);
            // After clearing the other edit, reload restores live, effective defaults.
            await bob.locator('select[data-effort]').selectOption('');
            await page.reload();
            assert.equal(await alice.locator('select[data-model]').inputValue(), '');
            assert.equal(await alice.locator('button[data-user]').isEnabled(), true);
            assert.deepEqual(errors, []);
        } finally {
            await browser.close();
        }
    });
}
