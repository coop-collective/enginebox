// Runs only in the provisioned, private SDK helper. No caller SQL or code.
import assert from 'node:assert/strict';
export async function mergeFrozen(client, request) {
  const base = `${request.prefix}base`;
  await client.branch.fork(base, request.baseRef);
  const twin = `${request.prefix}twin`;
  await client.branch.fork(twin, request.baseRef);
  const verified = await client.branch.merge(base, twin, `${request.prefix}verified`, 'Enginebox immutable base verification');
  assert.equal(verified.root, request.baseRoot, 'Base reference root differs');
  let current = base;
  let merged = verified;
  for (let index=0; index<request.changes.length; index++) {
    const change=request.changes[index];
    const branch=`${request.prefix}input-${index}`;
    await client.branch.fork(branch,change.ref);
    const ancestry=await client.branch.merge(base,branch,`${request.prefix}ancestor-${index}`,'Enginebox exact-base verification');
    assert(ancestry.fastForward, 'Change is not descended from the supplied exact base');
    assert.equal(ancestry.mergeBase, request.baseRef, 'Change does not descend from exact base');
    assert.equal(ancestry.root,change.root,'Change reference root differs');
    merged=await client.branch.merge(current,branch,`${request.prefix}merged-${index}`,'Enginebox frozen integration');
    if ((merged.lww?.length ?? 0)>0 || merged.lwwTruncated) throw Object.assign(new Error('Last-writer-wins policy is prohibited for integration'),{code:'Conflict',detail:{lww:merged.lww}});
    assert(!merged.lwwTruncated && !merged.conflictsTruncated && !merged.conflicts?.length,'Incomplete or conflicting merge');
    current=merged.branch;
  }
  return {ref:merged.head ?? request.baseRef,root:merged.root,branch:current};
}
export async function inspectFrozen(client, reference, prefix) {
  const first=await client.branch.fork(`${prefix}read`,reference);
  const ref=first.head ?? first.base;
  await client.branch.fork(`${prefix}copy`,ref);
  const verified=await client.branch.merge(`${prefix}read`,`${prefix}copy`,`${prefix}verified`,'Enginebox immutable source snapshot');
  return {ref,root:verified.root};
}
