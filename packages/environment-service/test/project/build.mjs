import { readFile, mkdir, writeFile } from 'node:fs/promises';
const release = JSON.parse(await readFile(new URL('./src/release.json', import.meta.url)));
if (!/^\d+$/.test(release.version) || typeof release.headline !== 'string' || !release.headline.length) throw Error('Invalid release');
const template = await readFile(new URL('./src/index.html', import.meta.url), 'utf8');
const escape = s => s.replace(/[&<>"']/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));
await mkdir(new URL('./dist/', import.meta.url), { recursive: true });
await writeFile(new URL('./dist/index.html', import.meta.url), template.replaceAll('{{headline}}', escape(release.headline)).replaceAll('{{message}}', escape(release.message)).replaceAll('{{version}}', release.version));
await writeFile(new URL('./dist/release.json', import.meta.url), JSON.stringify(release));
console.log(`Built app version ${release.version}`);
