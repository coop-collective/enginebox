import { defineConfig } from '@playwright/test';
export default defineConfig({
  testDir:'./tests/browser',fullyParallel:true,forbidOnly:!!process.env.CI,retries:0,
  use:{baseURL:'http://127.0.0.1:4178',trace:'retain-on-failure'},
  webServer:{command:'node build.mjs && wrangler dev --config wrangler.jsonc --config tests/fixtures/dashboard/wrangler.jsonc --port 4178 --local',url:'http://127.0.0.1:4178',reuseExistingServer:false,timeout:60000},
  projects:[{name:'desktop',use:{viewport:{width:1440,height:1000}}},{name:'mobile',use:{viewport:{width:390,height:844},isMobile:true,hasTouch:true}}],
});
