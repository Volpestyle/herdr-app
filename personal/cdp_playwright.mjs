// Playwright half of cdp_smoke.py: drive an agent pane through the guarded bridge.
// Usage: node cdp_playwright.mjs <playwright-core dir> <endpoint> <fixture url> <screenshot path>
import { createRequire } from 'node:module';

const [core, endpoint, fixture, shot] = process.argv.slice(2);
const { chromium } = createRequire(import.meta.url)(core);
const report = {};
const browser = await chromium.connectOverCDP(endpoint);
try {
  report.version = browser.version();
  const context = browser.contexts()[0];
  const pages = context.pages();
  report.pages = pages.map((p) => p.url());
  const page = pages.find((p) => p.url().startsWith(fixture));
  if (!page) throw new Error(`agent fixture page not visible: ${JSON.stringify(report.pages)}`);
  await page.goto(fixture + '?step=playwright');
  await page.getByRole('button', { name: 'Click me' }).click();
  report.clicked = await page.locator('#result').textContent();
  await page.getByLabel('Name').fill('Playwright');
  await page.getByLabel('Name').press('Enter');
  report.submitted = await page.locator('#result').textContent();
  report.evaluated = await page.evaluate(() => document.title);
  const consoleSeen = page.waitForEvent('console');
  await page.evaluate(() => console.log('hello from the page'));
  report.console = (await consoleSeen).text();
  const response = page.waitForResponse((r) => r.url().includes('/next'));
  await page.getByRole('link', { name: 'Next page' }).click();
  report.response = (await response).status();
  // Chromium restores from the back/forward cache without a load event; Playwright
  // normally turns that cache off with a launch flag it cannot pass to Herdr.
  await page.goBack({ waitUntil: 'commit' });
  await page.getByRole('button', { name: 'Click me' }).waitFor();
  await page.screenshot({ path: shot, fullPage: true });
  await context.addCookies([{ name: 'pw_added', value: 'agent-write', url: fixture }]);
  report.cookies = (await context.cookies()).map((c) => `${c.name}=${c.value}`);
  try { await context.clearCookies(); report.clearCookies = 'allowed'; } catch (e) { report.clearCookies = e.message; }
  const extra = await context.newPage();
  await extra.goto(fixture + '?step=new-page');
  report.newPage = extra.url();
  await extra.close();
} finally {
  await browser.close();
}
console.log(JSON.stringify(report));
