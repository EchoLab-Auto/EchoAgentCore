// 会话面板 UI 排版自动化检查：截图 + 布局指标（溢出/重叠/裁切/尺寸）
const { chromium } = require('playwright-core');

const EXEC = '/home/evence/.cache/ms-playwright/chromium_headless_shell-1228/chrome-headless-shell-linux64/chrome-headless-shell';

(async () => {
  const browser = await chromium.launch({ executablePath: EXEC });
  const page = await browser.newPage({ viewport: { width: 1440, height: 900 } });
  await page.goto('http://127.0.0.1:8080/', { waitUntil: 'networkidle' }).catch(() => {});
  await page.waitForTimeout(2500);

  // 打开 会话 视图
  const clicked = await page.evaluate(() => {
    const btn = [...document.querySelectorAll('button')].find((b) => b.textContent.trim() === '会话');
    if (btn) { btn.click(); return true; }
    return false;
  });
  await page.waitForTimeout(1200);
  console.log('会话按钮点击:', clicked);

  // 截图1：会话-上下文
  await page.screenshot({ path: '/tmp/ui-check/session-context.png' });
  // 切到 系统提示词
  await page.evaluate(() => {
    const item = [...document.querySelectorAll('[role="menuitem"], .nm-menu__item')].find((n) => n.textContent.includes('系统提示词'));
    if (item) (item.closest('[role="menuitem"]') || item).click();
  });
  await page.waitForTimeout(600);
  await page.screenshot({ path: '/tmp/ui-check/session-prompt.png' });
  // 切到 日志
  await page.evaluate(() => {
    const item = [...document.querySelectorAll('[role="menuitem"], .nm-menu__item')].find((n) => n.textContent.includes('日志'));
    if (item) (item.closest('[role="menuitem"]') || item).click();
  });
  await page.waitForTimeout(1500);
  await page.screenshot({ path: '/tmp/ui-check/session-logs.png' });

  // 布局指标
  const report = await page.evaluate(() => {
    const issues = [];
    const vw = window.innerWidth, vh = window.innerHeight;
    const all = [...document.querySelectorAll('div, span, pre, button, h2, h3, label, input, textarea')];
    for (const el of all) {
      if (el.children.length > 0 && !/^DIV$/i.test(el.tagName)) continue;
      if (el.textContent.trim().length === 0 && el.tagName !== 'PRE') continue;
      const r = el.getBoundingClientRect();
      if (r.width === 0 || r.height === 0) continue;
      const cs = getComputedStyle(el);
      if (cs.display === 'none' || cs.visibility === 'hidden') continue;
      // 水平溢出视口
      if (r.right > vw + 2 || r.left < -2) issues.push({ t: el.tagName, txt: el.textContent.trim().slice(0, 40), issue: 'viewport-x-overflow', rect: [Math.round(r.left), Math.round(r.top), Math.round(r.width), Math.round(r.height)] });
      // 内部滚动溢出（文本被裁切）
      if (el.scrollWidth > el.clientWidth + 2 && cs.overflowX !== 'auto' && cs.overflowX !== 'scroll') issues.push({ t: el.tagName, txt: el.textContent.trim().slice(0, 40), issue: 'text-x-clip', sw: el.scrollWidth, cw: el.clientWidth });
      if (el.scrollHeight > el.clientHeight + 2 && cs.overflowY === 'hidden') issues.push({ t: el.tagName, txt: el.textContent.trim().slice(0, 40), issue: 'text-y-clip', sh: el.scrollHeight, ch: el.clientHeight });
    }
    // 菜单项信息
    const menuItems = [...document.querySelectorAll('[role="menuitem"]')].map((n) => {
      const r = n.getBoundingClientRect();
      return { text: n.textContent.trim().slice(0, 30), rect: [Math.round(r.left), Math.round(r.top), Math.round(r.width), Math.round(r.height)] };
    });
    return { issues: issues.slice(0, 40), menuItems, vw, vh };
  });

  console.log(JSON.stringify(report, null, 2));
  await browser.close();
})();
