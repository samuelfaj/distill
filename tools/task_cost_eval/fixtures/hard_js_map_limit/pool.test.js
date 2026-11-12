const test = require('node:test');
const assert = require('node:assert');
const { mapLimit } = require('./pool');

const later = () => new Promise((resolve) => setImmediate(resolve));

test('results keep input order', async () => {
  const results = await mapLimit([3, 1, 2], 2, async (x) => {
    await later();
    return x * 10;
  });
  assert.deepStrictEqual(results, [30, 10, 20]);
});

test('never more than limit calls at once', async () => {
  let active = 0;
  let peak = 0;
  await mapLimit([1, 2, 3, 4, 5, 6, 7], 3, async () => {
    active += 1;
    peak = Math.max(peak, active);
    await later();
    active -= 1;
  });
  assert.ok(peak <= 3, `peak ${peak}`);
});

test('a failure rejects', async () => {
  await assert.rejects(mapLimit([1, 2], 1, async () => { throw new Error('down'); }), /down/);
});
