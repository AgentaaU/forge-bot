// Run with Playwright installed: node --test tests/table_scroll.cjs
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const { chromium, firefox } = require('playwright');

for (const [pageName, columns, offsets] of [
    ['status', { rows: 12 }, [500]],
    ['statistics', { models: 6, repositories: 7, threads: 8 }, [150, 300, 500]],
]) {
    for (const engine of [chromium, firefox]) {
        test(engine.name() + ' ' + pageName + ' retains each table offset on automatic refresh', async () => {
            const browser = await engine.launch();
            try {
                const page = await browser.newPage({ viewport: { width: 390, height: 844 } });
                let html = fs.readFileSync('web/' + pageName + '.html', 'utf8')
                    .replace('{table_scroll_script}', () => fs.readFileSync('web/table-scroll.js', 'utf8'))
                    .replace('content="60"', 'content="2"');
                for (const [slot, count] of Object.entries(columns)) {
                    const row = '<tr>' + '<td>long_repository_identifier</td>'.repeat(count) + '</tr>';
                    html = html.replace('{' + slot + '}', row);
                }
                await page.route('**/dashboard.css', route => route.fulfill({
                    contentType: 'text/css', body: fs.readFileSync('web/dashboard.css'),
                }));
                await page.route('**/' + pageName, route => route.fulfill({ contentType: 'text/html', body: html }));
                await page.goto('http://dashboard.test/' + pageName);
                const tables = page.locator('.table-scroll');
                const readOffsets = () => tables.evaluateAll(nodes => nodes.map(node => node.scrollLeft));
                await tables.evaluateAll((nodes, values) => {
                    nodes.forEach((node, index) => { node.scrollLeft = values[index]; });
                }, offsets);
                assert.deepEqual(await readOffsets(), offsets);
                await page.waitForEvent('load', { timeout: 25000 });
                assert.deepEqual(await readOffsets(), offsets);
            } finally {
                await browser.close();
            }
        });
    }
}
