// Runs as the unprivileged LayerFS workspace user before extracting reviewed
// source. Only named source leaves are removed; retained directories must be
// empty before replacement. Never traverse an existing symlink parent.
const fs = require('node:fs');
const path = require('node:path');
const plan = JSON.parse(fs.readFileSync(process.argv[1], 'utf8'));
const root = process.argv[2] ?? '/project';
function destination(relative) {
  if (!relative || path.isAbsolute(relative) || relative.split('/').some(p => !p || p === '.' || p === '..')) throw Error('Unsafe source path');
  const parts = relative.split('/');
  for (let i = 1; i < parts.length; i++) {
    const parent = path.join(root, ...parts.slice(0, i));
    const stat = fs.lstatSync(parent, { throwIfNoEntry: false });
    if (stat && !stat.isDirectory()) throw Error('Source parent is not a real directory');
  }
  return path.join(root, relative);
}
for (const relative of plan.remove) fs.unlinkSync(destination(relative));
for (const relative of plan.write) {
  const file = destination(relative);
  const stat = fs.lstatSync(file, { throwIfNoEntry: false });
  if (stat?.isDirectory()) fs.rmdirSync(file);
  else if (stat) fs.unlinkSync(file);
}
