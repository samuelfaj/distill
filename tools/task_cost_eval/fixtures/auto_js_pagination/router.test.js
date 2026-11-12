const test = require('node:test');
const assert = require('node:assert');
const { handle } = require('./router');

test('lists products', () => {
  const response = handle('GET', '/products', {});
  assert.strictEqual(response.status, 200);
  assert.ok(response.body.items.length > 0);
});

test('unknown path', () => {
  assert.strictEqual(handle('GET', '/nope').status, 404);
});
