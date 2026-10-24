import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { execFileSync } from 'node:child_process';
test('build renders the selected release without leaving template placeholders',async()=>{
 execFileSync(process.execPath,['build.mjs']);
 const release=JSON.parse(await readFile('src/release.json','utf8'));
 const html=await readFile('dist/index.html','utf8');
 assert.ok(html.includes(release.headline));assert.ok(html.includes(`VERSION ${release.version}`));assert.ok(!html.includes('{{'));
});
