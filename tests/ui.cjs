const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');

test('polling does not overlap, recovers after failure, and uses valid edge names', async () => {
  const nodes = new Map();
  const makeNode = () => ({ dataset: {}, textContent: '', addEventListener() {}, setAttribute(k, v) { this[k] = v; } });
  const html = fs.readFileSync('ui/index.html', 'utf8');
  const edges = [...html.matchAll(/data-edge="([^"]+)"/g)].map(([, edge]) => ({ ...makeNode(), dataset: { edge } }));
  assert.deepEqual(edges.map(n => n.dataset.edge), ['Left', 'Right', 'Top', 'Bottom']);
  const requests = [];
  let poll;
  const context = vm.createContext({
    window: { __TAURI__: { core: { invoke: () => new Promise((resolve, reject) => requests.push({ resolve, reject })) } } },
    document: { getElementById: id => { if (!nodes.has(id)) nodes.set(id, makeNode()); return nodes.get(id); }, querySelectorAll: () => edges },
    console: { error() {} },
    setInterval(fn) { poll = fn; },
  });
  vm.runInContext(fs.readFileSync('ui/app.js', 'utf8'), context);
  await poll();
  assert.equal(requests.length, 1, 'a pending request must not accumulate more polls');
  requests[0].reject(new Error('temporary IPC failure'));
  await new Promise(setImmediate);
  const refresh = poll();
  assert.equal(requests.length, 2, 'polling must resume after failure');
  requests[1].resolve({ role: 'host', status: 'disconnected', edge: 'Right', log: [], next_seq: 0 });
  await refresh;
  assert.equal(nodes.get('link').textContent, 'disconnected');
  assert.equal(edges[1]['aria-pressed'], 'true');
});
