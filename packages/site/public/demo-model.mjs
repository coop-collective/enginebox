// Explanatory browser model. Never connects to an Engine Box controller.
export function initialState() {
  return { revision: 42, rows: 1284, awake: true, nextId: 3, forks: [
    { id: 1, agent: 'Claude Code', task: 'Try the new checkout', revision: 42, rows: 1284, changed: false },
    { id: 2, agent: 'Codex', task: 'Test a data migration', revision: 42, rows: 1284, changed: false },
  ], message: 'One prepared environment. Independent places to work.' };
}
export function transition(state, action, id) {
  switch (action) {
    case 'fork': {
      if (state.forks.length >= 3) return { ...state, message: 'This demo has three agent slots. Discard a fork to try another.' };
      return { ...state, nextId: state.nextId + 1, forks: [...state.forks, { id: state.nextId, agent: 'Cursor', task: 'Explore the next idea', revision: state.revision, rows: state.rows, changed: false }], message: 'New fork starts at revision ' + state.revision + '. Its writes belong only to it.' };
    }
    case 'edit': return { ...state, forks: state.forks.map(f => f.id === id ? { ...f, rows: f.rows + 12, changed: true } : f), message: '12 test records added to one fork. The core and other agents stay unchanged.' };
    case 'discard': return { ...state, forks: state.forks.filter(f => f.id !== id), message: 'The disposable fork is gone. Core data and private agent identity stay.' };
    case 'advance': return { ...state, revision: state.revision + 1, message: 'Main advanced. Core data stays; existing forks keep their starting revision.' };
    case 'sleep': return { ...state, awake: !state.awake, message: state.awake ? 'Core compute sleeps. Its data and existing agent forks stay.' : 'Core compute wakes against its existing data.' };
    case 'reset': return initialState();
    default: return state;
  }
}
