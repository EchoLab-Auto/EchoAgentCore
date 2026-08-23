// 会话面板视觉详情检查：关键区域 bbox、重叠检测、三个视图的元素清单
const { chromium } = require('playwright-core');
const EXEC = '/home/evence/.cache/ms-playwright/chromium_headless_shell-1228/chrome-headless-shell-linux64/chrome-headless-shell';

(async () => {
  const browser = await chromium.launch({ executablePath: EXEC });
  const page = await browser.newPage({ viewport: { width: 1440, height: 900 } });
  await page.goto('http://127.0.0.1:8080/', { waitUntil: 'networkidle' }).catch(() => {});
  await page.waitForTimeout(2500);
  await page.evaluate(() => {
    const btn = [...document.querySelectorAll('button')].find((b) => b.textContent.trim() === '会话');
    if (btn) btn.click();
  });
  await page.waitForTimeout(1200);

  async function collect(label) {
    return await page.evaluate((label) => {
      const out = { label, rects: [], overlaps: [] };
      const grab = (sel, name) => {
        const el = document.querySelector(sel);
        if (!el) { out.rects.push({ name, missing: true }); return null; }
        const r = el.getBoundingClientRect();
        out.rects.push({ name, rect: [Math.round(r.left), Math.round(r.top), Math.round(r.width), Math.round(r.height)] });
        return r;
      };
      const menu = grab('.session-side', 'submenu-side');
      const main = grab('.session-main', 'main-area');
      if (menu && main) {
        const overlapX = Math.max(0, Math.min(menu.right, main.right) - Math.max(menu.left, main.left));
        const overlapY = Math.max(0, Math.min(menu.bottom, main.bottom) - Math.max(menu.top, main.top));
        if (overlapX > 0 && overlapY > 0) out.overlaps.push({ a: 'submenu-side', b: 'main-area', x: Math.round(overlapX), y: Math.round(overlapY) });
      }
      return out;
    }, label);
  }

  // 视图1：上下文
  console.log(JSON.stringify(await collect('context')));
  await page.screenshot({ path: '/tmp/ui-check/ctx.png' });

  // 视图2：系统提示词
  await page.evaluate(() => {
    const item = [...document.querySelectorAll('[role="menuitem"]')].find((n) => n.textContent.includes('系统提示词'));
    if (item) item.click();
  });
  await page.waitForTimeout(500);
  console.log(JSON.stringify(await collect('prompt')));
  await page.screenshot({ path: '/tmp/ui-check/prompt.png' });

  // 视图3：日志
  await page.evaluate(() => {
    const item = [...document.querySelectorAll('[role="menuitem"]')].find((n) => n.textContent.includes('日志'));
    if (item) item.click();
  });
  await page.waitForTimeout(1500);
  console.log(JSON.stringify(await collect('logs')));
  await page.screenshot({ path: '/tmp/ui-check/logs.png' });

  // 整体页面截图（含顶部导航）
  await page.screenshot({ path: '/tmp/ui-check/full.png', fullPage: false });
  await browser.close();
})();
