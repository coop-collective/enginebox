import test from 'node:test';
import assert from 'node:assert/strict';
import { initialState, transition } from '../public/demo-model.mjs';
test('data edits belong only to the selected agent fork',()=>{
  const original=initialState(); const edited=transition(original,'edit',1);
  assert.equal(edited.rows,1284); assert.equal(edited.forks[0].rows,1296); assert.equal(edited.forks[1].rows,1284);
  assert.equal(original.forks[0].rows,1284);
});
test('advancing main preserves data and existing revisions, new forks use the new revision',()=>{
  const advanced=transition(initialState(),'advance'); const forked=transition(advanced,'fork');
  assert.equal(advanced.rows,1284); assert.equal(advanced.forks[0].revision,42); assert.equal(forked.forks[2].revision,43);
});
test('sleep and disposable fork deletion preserve persistent state',()=>{
  const original=transition(initialState(),'edit',1); const sleeping=transition(original,'sleep');
  assert.equal(sleeping.awake,false); assert.deepEqual(sleeping.forks,original.forks);
  const discarded=transition(sleeping,'discard',1); assert.equal(discarded.rows,1284);assert.equal(discarded.forks.length,1);
  const awake=transition(discarded,'sleep');assert.equal(awake.awake,true);assert.deepEqual(awake.forks,discarded.forks);
});
