import { test,expect } from '@playwright/test';
test('environment concept preserves core and independent forks',async({page})=>{
  const errors=[];page.on('pageerror',e=>errors.push(e.message));
  await page.goto('/'); await expect(page.getByRole('heading',{name:'Keep the world. Fork the work.'})).toBeVisible();
  const claude=page.getByRole('article',{name:'Claude Code fork 1'});const codex=page.getByRole('article',{name:'Codex fork 2'});
  await claude.getByRole('button',{name:'Edit Claude Code fork 1 data'}).click();
  await expect(claude.locator('[data-fork-rows]')).toContainText('1,296');await expect(codex.locator('[data-fork-rows]')).toContainText('1,284');await expect(page.locator('[data-core-rows]')).toHaveText('1,284');
  await page.getByRole('button',{name:'Advance main'}).click();await expect(page.locator('[data-core-revision]')).toHaveText('main · r43');await expect(claude).toContainText('fork / r42');
  await page.getByRole('button',{name:/Fork an environment/}).click();await expect(page.getByRole('article',{name:'Cursor fork 3'})).toContainText('fork / r43');
  await page.getByRole('button',{name:'Sleep core'}).click();await expect(page.locator('[data-core-status]')).toHaveText('Sleeping');await expect(claude.locator('[data-fork-rows]')).toContainText('1,296');
  await claude.getByRole('button',{name:'Discard Claude Code fork 1'}).click();await expect(claude).toHaveCount(0);await expect(page.locator('[data-core-rows]')).toHaveText('1,284');
  await page.getByRole('button',{name:'Wake core'}).click();await expect(page.locator('[data-core-status]')).toHaveText('Running');
  expect(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth)).toBe(true);expect(errors).toEqual([]);
});
test('docs search, code copy and theme persist across navigation',async({page})=>{
  await page.addInitScript(()=>{Object.defineProperty(navigator,'clipboard',{value:{writeText:async text=>{window.copiedText=text;}}});});
  await page.goto('/docs/quickstart/');await expect(page.getByRole('heading',{level:1})).toBeVisible();
  await page.getByRole('button',{name:'Copy code',exact:true}).first().click();expect(await page.evaluate(()=>window.copiedText)).toBeTruthy();
  await page.getByRole('button',{name:'Switch to dark theme'}).click();await expect(page.locator('html')).toHaveAttribute('data-theme','dark');
  await page.getByRole('button',{name:/Search docs|⌕/}).click();await page.getByRole('searchbox').fill('Cloudflare');
  await expect(page.locator('.search-result')).not.toHaveCount(0);await page.locator('.search-result').filter({hasText:/Cloudflare/}).first().click();
  await expect(page.locator('html')).toHaveAttribute('data-theme','dark');await expect(page.locator('.doc-content')).toBeVisible();
  expect(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth)).toBe(true);
});
test('docs navigation, availability and a real not-found page',async({page,isMobile})=>{
  await page.goto('/docs/');
  if(isMobile)await page.getByRole('button',{name:'Browse documentation'}).click();
  await page.locator('.doc-nav-group a[href="/docs/platform-support/"]').click();await expect(page.locator('h1')).toBeVisible();
  await expect(page.locator('.doc-content')).toContainText(/Linux/);
  const response=await page.goto('/missing-environment');expect(response.status()).toBe(404);await expect(page.getByRole('heading',{name:'This environment doesn’t exist.'})).toBeVisible();
});
