# Launch a Chromium instance and load a blank page so V8's JIT +
# Playwright's browser handshake are captured in the snapshot's
# memory image. Second fork skips the ~1s browser spin-up.
node -e "
const { chromium } = require('playwright');
(async () => {
  const browser = await chromium.launch({ headless: true });
  const page = await browser.newPage();
  await page.goto('about:blank');
  const title = await page.title();
  console.log('warm: playwright', require('playwright/package.json').version, 'title=', title);
  await browser.close();
})();
"
