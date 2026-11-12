#!/usr/bin/env python3
import sys

from _js_common import run_node
from _par_common import attempt, finish, fixture_dir

CHECKS = """
const { mapLimit } = require('./pool');
const tick = async (n = 3) => { for (let i = 0; i < n; i += 1) await new Promise((r) => setImmediate(r)); };
// Rejections of items that never started are expected, not unhandled.
const deferred = () => { let resolve, reject; const promise = new Promise((a, b) => { resolve = a; reject = b; });
                         promise.catch(() => {}); return { promise, resolve, reject }; };
// track() attaches handlers at once, so an early rejection is never unhandled; settle() never hangs:
// a promise that has not settled after a few ticks reports 'pending'.
const track = (p) => p.then((value) => ({ value }), (e) => ({ error: e.message }));
const settle = async (tracked) => Promise.race([tracked, tick(6).then(() => 'pending')]);
await checkAsync('sliding window', async () => {
  const started = [];
  const d = [0, 1, 2, 3, 4].map(deferred);
  const p = track(mapLimit([0, 1, 2, 3, 4], 2, (x, i) => { started.push([x, i]); return d[x].promise; }));
  const steps = [];
  await tick(); steps.push(started.length);
  d[1].resolve('b'); await tick(); steps.push(started.length);
  d[2].resolve('c'); await tick(); steps.push(started.length);
  d[0].resolve('a'); d[3].resolve('d'); await tick(); steps.push(started.length);
  d[4].resolve('e');
  return { steps, started, result: await settle(p) };
});
await checkAsync('stops after failure', async () => {
  const started = [];
  const d = [0, 1, 2, 3, 4, 5].map(deferred);
  const p = track(mapLimit([0, 1, 2, 3, 4, 5], 2, (x) => { started.push(x); return d[x].promise; }));
  await tick();
  d[1].reject(new Error('item 1 failed')); await tick();
  const outcome = await settle(p);
  d[0].resolve('a'); d[2].resolve('c'); d[3].reject(new Error('late')); await tick();
  return { outcome, started };
});
await checkAsync('synchronous throw', async () => {
  const started = [];
  const p = track(mapLimit([0, 1, 2, 3], 1, (x) => { started.push(x); if (x === 1) throw new Error('sync'); return x; }));
  return { outcome: await settle(p), started };
});
await checkAsync('limit above length', async () => {
  const started = [];
  const d = [0, 1, 2].map(deferred);
  const p = track(mapLimit([0, 1, 2], 10, (x) => { started.push(x); return d[x].promise; }));
  await tick();
  const count = started.length;
  d[2].resolve(2); d[0].resolve(0); d[1].resolve(1);
  return { count, outcome: await settle(p) };
});
await checkAsync('empty', async () => settle(track(mapLimit([], 3, () => { throw new Error('never'); }))));
"""


def main(argv):
    root = fixture_dir(argv, 'hard_js_map_limit')
    failures = []
    try:
        out = run_node(root, CHECKS)
    except Exception as error:
        return finish(['import: ' + repr(error)])
    attempt(failures, 'next item starts as soon as any call finishes', lambda: out['sliding window'],
            {'steps': [2, 3, 4, 5], 'started': [[0, 0], [1, 1], [2, 2], [3, 3], [4, 4]],
             'result': {'value': ['a', 'b', 'c', 'd', 'e']}})
    attempt(failures, 'first failure rejects and nothing new starts', lambda: out['stops after failure'],
            {'outcome': {'error': 'item 1 failed'}, 'started': [0, 1]})
    attempt(failures, 'a synchronous throw is a failure', lambda: out['synchronous throw'],
            {'outcome': {'error': 'sync'}, 'started': [0, 1]})
    attempt(failures, 'limit above the number of items', lambda: out['limit above length'],
            {'count': 3, 'outcome': {'value': [0, 1, 2]}})
    attempt(failures, 'empty input', lambda: out['empty'], {'value': []})
    return finish(failures)


if __name__ == '__main__':
    raise SystemExit(main(sys.argv))
