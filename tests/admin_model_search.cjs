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
