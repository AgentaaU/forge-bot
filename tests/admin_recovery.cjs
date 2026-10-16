// Run with Playwright installed: node --test tests/admin_recovery.cjs
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const { chromium } = require('playwright');

test('repository recovery only resets existing overrides and permits retry', async () => {
    const browser = await chromium.launch({ headless: true });
    try {
        const page = await browser.newPage();
        const errors = [];
        page.on('pageerror', error => errors.push(error.message));
        const html = fs.readFileSync('web/admin-recovery.html', 'utf8')
            .replaceAll('{repository}', 'owner/typo')
            .replace('{error}', 'Repository not found. Check spelling and access.')
            .replace('{reset_rows}', '<li>reviewer <button data-reset-user="reviewer">Use bot defaults</button></li>')
            .replaceAll('{{', '{').replaceAll('}}', '}');
        await page.route('**/admin?repository=owner/typo', route => route.fulfill({ contentType: 'text/html', body: html }));
        const requests = [];
        await page.route('**/admin/agent-settings', route => {
            requests.push(route.request().postDataJSON());
            return route.fulfill({ status: requests.length === 1 ? 500 : 204 });
        });
        await page.goto('http://admin.test/admin?repository=owner/typo');
        assert.equal(await page.locator('select, input, button[data-user]').count(), 0);
        const reset = page.getByRole('button', { name: 'Use bot defaults' });
        await reset.click();
        await page.getByRole('status').filter({ hasText: 'Reset failed' }).waitFor();
        assert.equal(await reset.isEnabled(), true);
        await reset.click();
        await page.getByRole('status').filter({ hasText: 'Override removed' }).waitFor();
        assert.equal(await reset.count(), 0);
        assert.deepEqual(requests, Array(2).fill({ repository: 'owner/typo', user: 'reviewer', reset: true }));
        assert.deepEqual(errors, []);
    } finally {
        await browser.close();
    }
});
